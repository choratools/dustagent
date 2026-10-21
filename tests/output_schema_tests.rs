use async_trait::async_trait;
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, OutputSchema, StopReason,
    ToolDefinition,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Provider {
    replies: Mutex<VecDeque<LlmResponse>>,
    calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

impl Provider {
    fn new(replies: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().map(LlmResponse::text).collect()),
            calls: Arc::default(),
        }
    }
}

#[async_trait]
impl LlmProvider for Provider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        self.calls.lock().unwrap().push(messages.to_vec());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .map(Ok)
            .expect("unexpected model request")
    }
}

fn manifest() -> AppManifest {
    let mut manifest = AppManifest::new();
    manifest.name = Some("json-writer".into());
    manifest.output_schema = Some(
        serde_json::from_value::<OutputSchema>(serde_json::json!({
            "type":"object",
            "required":["title", "count"],
            "additionalProperties":false,
            "properties":{
                "title":{"type":"string","minLength":3},
                "count":{"type":"integer","minimum":1,"maximum":10}
            }
        }))
        .unwrap(),
    );
    manifest.max_turns = Some(3);
    manifest
}

#[tokio::test]
async fn invalid_json_and_schema_errors_are_returned_then_repaired() {
    let provider = Provider::new([
        "Here is your answer: {\"title\":\"x\",\"count\":20}",
        "{\"title\":\"x\",\"count\":20}",
        "{\"title\":\"Valid title\",\"count\":3}",
    ]);
    let calls = provider.calls.clone();
    let mut core = DustCore::new(manifest(), provider);

    let report = core.execute_report("create a result").await;

    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(
        report.output.as_deref(),
        Some("{\"title\":\"Valid title\",\"count\":3}")
    );
    assert_eq!(report.validation_history.len(), 3);
    assert_eq!(
        report.validation_history[0].evidence.decision,
        Some(dustagent::ValidationDecision::Continue)
    );
    assert!(
        report.validation_history[0]
            .evidence
            .reason
            .contains("not valid JSON")
    );
    assert!(
        report.validation_history[1]
            .evidence
            .reason
            .contains("$/title: string must contain at least 3")
    );
    assert!(
        report.validation_history[1]
            .evidence
            .reason
            .contains("$/count: number must be at most 10")
    );
    assert_eq!(
        report.validation_history[2].evidence.decision,
        Some(dustagent::ValidationDecision::Complete)
    );

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    let first_system = calls[0]
        .iter()
        .find(|message| {
            message.role == "system"
                && message
                    .content
                    .as_deref()
                    .is_some_and(|content| content.contains("output contract"))
        })
        .unwrap();
    assert!(
        first_system
            .content
            .as_deref()
            .unwrap()
            .contains("output contract")
    );
    let second_messages = serde_json::to_string(&calls[1]).unwrap();
    assert!(second_messages.contains("not valid JSON"));
    let third_messages = serde_json::to_string(&calls[2]).unwrap();
    assert!(third_messages.contains("$/title: string must contain at least 3"));
    assert!(third_messages.contains("$/count: number must be at most 10"));
}

#[tokio::test]
async fn schema_feedback_consumes_the_existing_turn_budget() {
    let provider = Provider::new(["not json"]);
    let mut config = manifest();
    config.max_turns = Some(1);
    let mut core = DustCore::new(config, provider);

    let report = core.execute_report("create a result").await;

    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert_eq!(report.turns_used, 1);
    assert_eq!(report.validation_history.len(), 1);
    assert!(!report.validation_history[0].evidence.passed);
}

#[tokio::test]
async fn empty_final_response_gets_schema_feedback_and_can_be_repaired() {
    let provider = Provider::new(["", "{\"title\":\"Valid title\",\"count\":3}"]);
    let mut core = DustCore::new(manifest(), provider);

    let report = core.execute_report("create a result").await;

    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.validation_history.len(), 2);
    assert!(
        report.validation_history[0]
            .evidence
            .reason
            .contains("output is empty")
    );
}

#[tokio::test]
async fn external_checker_runs_only_after_the_native_schema_passes() {
    let directory = tempfile::tempdir().unwrap();
    let mut manifest = manifest();
    manifest.validation = Some(dustagent::application::validation::ValidationConfig {
        command: "python3".into(),
        args: vec![
            "-c".into(),
            "import json,pathlib,sys; json.load(sys.stdin); pathlib.Path('checker-ran').write_text('yes'); print(json.dumps({'passed':True,'reason':'additional check passed'}))".into(),
        ],
        timeout_ms: 5000,
        mode: dustagent::application::validation::ValidationMode::Legacy,
    });
    let provider = Provider::new([
        "{\"title\":\"x\",\"count\":20}",
        "{\"title\":\"Valid title\",\"count\":3}",
    ]);
    let mut core = DustCore::new(manifest, provider)
        .with_working_directory(directory.path())
        .unwrap();

    let report = core.execute_report("create a result").await;

    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.validation_history.len(), 2);
    assert_eq!(
        report.validation_history[0].evidence.decision,
        Some(dustagent::ValidationDecision::Continue)
    );
    assert!(report.validation_history[1].evidence.passed);
    assert_eq!(
        report.validation_history[1].evidence.reason,
        "additional check passed"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("checker-ran")).unwrap(),
        "yes"
    );
}

#[test]
fn manifest_checks_schema_declarations_and_rejects_unknown_keywords() {
    let manifest = AppManifest::from_json_str(
        r#"{"output_schema":{"type":"array","minItems":1,"items":{"type":"string"}}}"#,
    )
    .unwrap();
    assert!(manifest.output_schema.is_some());

    assert!(
        AppManifest::from_json_str(r#"{"output_schema":{"type":"string","pattern":"("}}"#).is_err()
    );
    assert!(
        AppManifest::from_json_str(r#"{"output_schema":{"type":"string","format":"email"}}"#)
            .is_err()
    );
}
