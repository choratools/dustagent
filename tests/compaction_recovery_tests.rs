use async_trait::async_trait;
use dustagent::application::{
    compaction::CompactionConfig, session::AgentSession, transcript::TranscriptStore,
};
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolCall,
    ToolDefinition,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Calls {
    summary: usize,
    task: usize,
}
struct Scripted {
    responses: Mutex<VecDeque<LlmResponse>>,
    calls: Arc<Mutex<Calls>>,
    delay: Duration,
}
#[async_trait]
impl LlmProvider for Scripted {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        if tools.is_none()
            && messages[0]
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with("Produce continuation notes")
        {
            self.calls.lock().unwrap().summary += 1;
            tokio::time::sleep(self.delay).await;
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::text("")))
        } else {
            self.calls.lock().unwrap().task += 1;
            Ok(LlmResponse::text(format!(
                "{}ORIGINAL_END_MARKER",
                "x".repeat(40_000)
            )))
        }
    }
}
fn fixture(
    responses: Vec<LlmResponse>,
    max_turns: usize,
    retries: usize,
    delay: Duration,
) -> (DustCore<Scripted>, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut manifest = AppManifest::new();
    manifest.max_turns = Some(max_turns);
    manifest.timeout_ms = Some(1000);
    manifest.compaction = Some(CompactionConfig {
        max_summary_bytes: 512,
        max_summary_retries: retries,
        ..Default::default()
    });
    let model = Scripted {
        responses: Mutex::new(responses.into()),
        calls: calls.clone(),
        delay,
    };
    (DustCore::new(manifest, model), calls)
}
async fn seed(core: &mut DustCore<Scripted>, session: &mut AgentSession) {
    assert_eq!(
        core.prompt_report(session, "Investigate src/auth.rs refresh_token 401")
            .await
            .stop_reason,
        StopReason::Completed
    );
    assert_eq!(
        core.prompt_report(session, "Check auth_result")
            .await
            .stop_reason,
        StopReason::Completed
    );
}
fn statuses(report: &dustagent::ExecutionReport) -> Vec<String> {
    report
        .compaction_attempts
        .iter()
        .map(|a| {
            serde_json::to_value(a).unwrap()["status"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}
fn cleanup(report: &dustagent::ExecutionReport) {
    std::fs::remove_dir_all(report.transcript.as_ref().unwrap().path.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn empty_then_oversized_then_valid_recovers_without_replaying_task() {
    let (mut core, calls) = fixture(
        vec![
            LlmResponse::text(""),
            LlmResponse::text("z".repeat(513)),
            LlmResponse::text("Authentication investigation complete; consult original evidence."),
        ],
        8,
        2,
        Duration::ZERO,
    );
    let mut session = AgentSession::new();
    seed(&mut core, &mut session).await;
    let report = core
        .prompt_report(&mut session, "Continue investigation")
        .await;
    assert_eq!(
        report.stop_reason,
        StopReason::Completed,
        "{:?}",
        report.error
    );
    assert_eq!(statuses(&report), ["empty", "oversized", "accepted"]);
    assert_eq!(report.turns_used, 4);
    assert_eq!(report.compactions.len(), 1);
    assert_eq!(calls.lock().unwrap().task, 3);
    for (index, attempt) in report.compaction_attempts.iter().enumerate() {
        assert_eq!(attempt.attempt, index + 1);
        assert_eq!(attempt.max_summary_bytes, 512);
        assert!(attempt.archive_index.is_some());
    }
    assert_eq!(report.compaction_attempts[1].summary_bytes, Some(513));
    let reference = report.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    assert_eq!(
        store.search("compaction response", 0, 20).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(store.directory(0, 10, None).unwrap()["count"], 1);
    assert!(
        store.read(3, 39_000, 16_384).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_END_MARKER")
    );
    cleanup(&report);
}
#[tokio::test]
async fn unexpected_summary_tools_are_rejected_without_execution() {
    let invalid = LlmResponse::tool_calls(vec![ToolCall::new(
        "must-not-run",
        "dustagent__sleep",
        "{\"ms\":60000}",
    )]);
    let (mut core, calls) = fixture(
        vec![invalid, LlmResponse::text("Keep auth evidence.")],
        8,
        2,
        Duration::ZERO,
    );
    let mut session = AgentSession::new();
    seed(&mut core, &mut session).await;
    let report = core.prompt_report(&mut session, "Continue").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(statuses(&report), ["unexpected_tool_calls", "accepted"]);
    assert_eq!(report.compaction_attempts[0].tool_call_count, 1);
    assert!(report.tool_calls.is_empty());
    assert_eq!(calls.lock().unwrap().task, 3);
    cleanup(&report);
}
#[tokio::test]
async fn exhausted_retries_preserve_originals_and_classify_every_attempt() {
    let (mut core, calls) = fixture(vec![], 8, 2, Duration::ZERO);
    let mut session = AgentSession::new();
    seed(&mut core, &mut session).await;
    let report = core.prompt_report(&mut session, "Continue").await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert_eq!(statuses(&report), ["empty", "empty", "empty"]);
    assert!(report.error.as_deref().unwrap().contains("empty"));
    assert!(report.compactions.is_empty());
    assert_eq!(calls.lock().unwrap().task, 2);
    let reference = report.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    assert_eq!(store.directory(0, 10, None).unwrap()["count"], 0);
    assert!(
        store.read(3, 39_000, 16_384).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_END_MARKER")
    );
    cleanup(&report);
}
#[tokio::test]
async fn turn_limit_stops_summary_retries_without_extra_provider_calls() {
    let (mut core, calls) = fixture(vec![], 2, 3, Duration::ZERO);
    let mut session = AgentSession::new();
    seed(&mut core, &mut session).await;
    let report = core.prompt_report(&mut session, "Continue").await;
    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert_eq!(statuses(&report), ["empty", "empty"]);
    assert_eq!(calls.lock().unwrap().summary, 2);
    assert_eq!(calls.lock().unwrap().task, 2);
    cleanup(&report);
}
#[tokio::test]
async fn explicit_zero_retry_stops_after_one_invalid_summary() {
    let (mut core, calls) = fixture(vec![], 8, 0, Duration::ZERO);
    let mut session = AgentSession::new();
    seed(&mut core, &mut session).await;
    let report = core.prompt_report(&mut session, "Continue").await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert_eq!(statuses(&report), ["empty"]);
    assert_eq!(calls.lock().unwrap().summary, 1);
    cleanup(&report);
}

#[tokio::test]
async fn time_limit_interrupts_summary_and_prevents_retry() {
    let (mut core, calls) = fixture(
        vec![LlmResponse::text("valid notes")],
        8,
        2,
        Duration::from_secs(2),
    );
    let mut session = AgentSession::new();
    seed(&mut core, &mut session).await;
    let report = core.prompt_report(&mut session, "Continue").await;
    assert_eq!(report.stop_reason, StopReason::TimeLimit);
    assert_eq!(statuses(&report), ["time_limit"]);
    assert_eq!(calls.lock().unwrap().summary, 1);
    assert_eq!(calls.lock().unwrap().task, 2);
    assert!(report.compaction_attempts[0].archive_index.is_none());
    cleanup(&report);
}
#[tokio::test]
async fn later_failed_compaction_preserves_previous_directory_and_success_records() {
    let (mut core, calls) = fixture(
        vec![LlmResponse::text("Authentication evidence retained.")],
        8,
        2,
        Duration::ZERO,
    );
    let mut session = AgentSession::new();
    seed(&mut core, &mut session).await;
    let accepted = core.prompt_report(&mut session, "Continue").await;
    assert_eq!(accepted.stop_reason, StopReason::Completed);
    assert_eq!(statuses(&accepted), ["accepted"]);
    let failed = core.prompt_report(&mut session, "Next issue").await;
    assert_eq!(failed.stop_reason, StopReason::ExecutionError);
    assert_eq!(statuses(&failed), ["empty", "empty", "empty"]);
    assert_eq!(calls.lock().unwrap().task, 3);
    let reference = failed.transcript.as_ref().unwrap();
    let reopened = TranscriptStore::open(reference, &reference.binding).unwrap();
    assert_eq!(reopened.directory(0, 10, None).unwrap()["count"], 1);
    assert_eq!(
        reopened.search("compaction response", 0, 20).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(accepted.compactions.len(), 1);
    cleanup(&failed);
}

#[tokio::test]
async fn utf8_summary_over_auto_token_cap_recovers_and_report_survives_archive_cleanup() {
    let oversized = "가".repeat(3278);
    assert_eq!(oversized.len(), 9834);
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut manifest = AppManifest::new();
    manifest.max_turns = Some(8);
    let model = Scripted {
        responses: Mutex::new(
            vec![
                LlmResponse::text(&oversized),
                LlmResponse::text("Authentication evidence retained."),
            ]
            .into(),
        ),
        calls: calls.clone(),
        delay: Duration::ZERO,
    };
    let mut core = DustCore::new(manifest, model);
    let mut session = AgentSession::new();
    seed(&mut core, &mut session).await;
    let report = core.prompt_report(&mut session, "Continue").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(statuses(&report), ["oversized", "accepted"]);
    assert_eq!(report.compaction_attempts[0].summary_bytes, Some(9834));
    assert_eq!(report.compaction_attempts[0].summary_tokens, Some(3278));
    assert_eq!(report.compaction_attempts[0].max_summary_tokens, 3277);
    assert_eq!(report.compaction_attempts[0].max_summary_bytes, 26_216);
    assert_eq!(calls.lock().unwrap().summary, 2);
    assert_eq!(calls.lock().unwrap().task, 3);
    let reference = report.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    let index = report.compaction_attempts[0].archive_index.unwrap();
    assert!(
        store.read(index, 0, 16384).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains(&oversized)
    );
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(
        !serialized.contains(&oversized),
        "diagnosis records must not contain the raw summary body"
    );
    cleanup(&report);
    assert!(!reference.path.exists());
    let decoded: dustagent::ExecutionReport = serde_json::from_str(&serialized).unwrap();
    assert_eq!(statuses(&decoded), ["oversized", "accepted"]);
    assert_eq!(decoded.compaction_attempts[0].summary_bytes, Some(9834));
    assert_eq!(
        serde_json::to_value(&decoded).unwrap(),
        serde_json::to_value(&report).unwrap()
    );
}

struct SummaryFailure(Scripted);
#[async_trait]
impl LlmProvider for SummaryFailure {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        if tools.is_none()
            && messages[0]
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with("Produce continuation notes")
        {
            self.0.calls.lock().unwrap().summary += 1;
            return Err(dustagent::DustError::Llm(
                "fixture summary provider failure".into(),
            ));
        }
        self.0.chat(messages, tools).await
    }
}
#[tokio::test]
async fn summary_provider_error_is_diagnosed_without_validation_retry() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let provider = SummaryFailure(Scripted {
        responses: Mutex::default(),
        calls: calls.clone(),
        delay: Duration::ZERO,
    });
    let mut core = DustCore::new(AppManifest::new(), provider);
    let mut session = AgentSession::new();
    for input in ["Investigate authentication", "Check auth_result"] {
        assert_eq!(
            core.prompt_report(&mut session, input).await.stop_reason,
            StopReason::Completed
        );
    }
    let report = core.prompt_report(&mut session, "Continue").await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert_eq!(statuses(&report), ["provider_error"]);
    assert_eq!(calls.lock().unwrap().summary, 1);
    assert_eq!(calls.lock().unwrap().task, 2);
    let attempt = &report.compaction_attempts[0];
    assert!(attempt.summary_bytes.is_none());
    assert!(attempt.archive_index.is_none());
    assert!(
        attempt
            .error
            .as_deref()
            .unwrap()
            .contains("fixture summary provider failure")
    );
    let reference = report.transcript.as_ref().unwrap();
    let reopened = TranscriptStore::open(reference, &reference.binding).unwrap();
    assert!(
        reopened.read(3, 39_000, 16_384).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_END_MARKER")
    );
    cleanup(&report);
}
