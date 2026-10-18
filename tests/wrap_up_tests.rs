use async_trait::async_trait;
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolCall,
    ToolDefinition,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Probe {
    requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    finish_at: Option<usize>,
    delay: Duration,
}
#[async_trait]
impl LlmProvider for Probe {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        let turn = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(messages.to_vec());
            requests.len()
        };
        tokio::time::sleep(self.delay).await;
        if self.finish_at == Some(turn) {
            Ok(LlmResponse::text(r#"{"status":"complete"}"#))
        } else {
            Ok(LlmResponse {
                content: None,
                tool_calls: Some(vec![ToolCall::new(
                    format!("call-{turn}"),
                    "dustagent__hash",
                    r#"{"input":"body"}"#,
                )]),
            })
        }
    }
}
fn manifest(enabled: Option<bool>) -> AppManifest {
    let mut manifest = AppManifest::new();
    manifest.max_turns = Some(2);
    manifest.wrap_up_on_limit = enabled;
    manifest
}
fn notices(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .filter(|m| {
            m.content
                .as_deref()
                .is_some_and(|s| s.starts_with("[dustagent turn limit: wrap-up]"))
        })
        .count()
}

#[tokio::test]
async fn disabled_preserves_original_limit_and_legacy_serialization() {
    let manifest = manifest(None);
    assert!(
        serde_json::to_value(&manifest)
            .unwrap()
            .get("wrap_up_on_limit")
            .is_none()
    );
    let probe = Probe::default();
    let report = DustCore::new(manifest, probe.clone())
        .execute_report("Return JSON only")
        .await;
    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert_eq!(report.turns_used, 2);
    assert_eq!(probe.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn wrap_up_can_finish_on_third_extra_turn_with_original_contract() {
    let probe = Probe {
        finish_at: Some(5),
        ..Probe::default()
    };
    let mut app = manifest(Some(true));
    app.output_schema = Some(serde_json::from_str(r#"{"type":"object","properties":{"status":{"type":"string"}},"required":["status"],"additionalProperties":false}"#).unwrap());
    let report = DustCore::new(app, probe.clone())
        .execute_report("Return JSON only")
        .await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.turns_used, 5);
    assert_eq!(report.tool_calls.len(), 4);
    assert!(report.tool_calls.iter().all(|call| call.error.is_none()));
    assert_eq!(report.output.as_deref(), Some(r#"{"status":"complete"}"#));
    let requests = probe.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(notices(request), usize::from(index >= 2));
        assert!(
            request
                .iter()
                .any(|m| m.role == "user" && m.content.as_deref() == Some("Return JSON only"))
        );
        assert!(request.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .is_some_and(|s| s.starts_with("[dustagent output contract]"))
        }));
    }
}

#[tokio::test]
async fn wrap_up_is_bounded_even_when_model_keeps_using_tools() {
    let probe = Probe::default();
    let report = DustCore::new(manifest(Some(true)), probe.clone())
        .execute_report("task")
        .await;
    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert_eq!(report.turns_used, 5);
    assert_eq!(probe.requests.lock().unwrap().len(), 5);
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|s| s.contains("granted 3 wrap-up turns"))
            .count(),
        1
    );
}

#[tokio::test]
async fn builder_can_disable_manifest_wrap_up() {
    let probe = Probe::default();
    let report = DustCore::new(manifest(Some(true)), probe.clone())
        .with_wrap_up_on_limit(false)
        .execute_report("task")
        .await;
    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert_eq!(probe.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn completion_before_limit_does_not_enter_wrap_up() {
    let probe = Probe {
        finish_at: Some(2),
        ..Probe::default()
    };
    let report = DustCore::new(manifest(Some(true)), probe.clone())
        .execute_report("task")
        .await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.turns_used, 2);
    assert!(
        probe
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| notices(r) == 0)
    );
}

#[tokio::test]
async fn wrap_up_never_extends_execution_deadline() {
    let probe = Probe {
        delay: Duration::from_millis(100),
        ..Probe::default()
    };
    let report = DustCore::new(manifest(Some(true)), probe.clone())
        .with_timeouts(20, 20)
        .execute_report("task")
        .await;
    assert_eq!(report.stop_reason, StopReason::TimeLimit);
    assert!(probe.requests.lock().unwrap().len() <= 1);
    assert!(
        !report
            .warnings
            .iter()
            .any(|s| s.contains("granted 3 wrap-up turns"))
    );
}
