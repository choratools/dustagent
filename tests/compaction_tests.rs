use async_trait::async_trait;
use dustagent::application::{
    compaction::CompactionConfig, session::AgentSession, transcript::TranscriptStore,
};
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolDefinition,
};
use std::sync::{Arc, Mutex};
type Requests = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

struct Model {
    requests: Requests,
    summary: String,
    capacity: Option<usize>,
}
#[async_trait]
impl LlmProvider for Model {
    fn context_window_tokens(&self) -> Option<usize> {
        self.capacity
    }
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        self.requests.lock().unwrap().push(messages.to_vec());
        if tools.is_none()
            && messages[0]
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with("Produce continuation notes")
        {
            return Ok(LlmResponse::text(&self.summary));
        }
        Ok(LlmResponse::text(format!(
            "{}ORIGINAL_END_MARKER",
            "x".repeat(40_000)
        )))
    }
}
fn model(summary: &str) -> Model {
    Model {
        requests: Arc::default(),
        summary: summary.into(),
        capacity: None,
    }
}
#[tokio::test]
async fn oversized_unsummarizable_request_stops_before_provider_and_keeps_original() {
    let mut provider = model("unused");
    provider.capacity = Some(4096);
    let seen = provider.requests.clone();
    let mut core = DustCore::new(AppManifest::new(), provider);
    let input = "x".repeat(15_000);
    let report = core.execute_report(&input).await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert!(report.error.as_deref().unwrap().contains("context budget"));
    assert!(seen.lock().unwrap().is_empty());
    let reference = report.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    assert!(
        store.read(2, 0, 16_384).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains(&input)
    );
    remove_archive(reference);
}
#[tokio::test]
async fn oversized_recent_answer_is_summarized_before_task_dispatch() {
    let provider = model("brief notes");
    let seen = provider.requests.clone();
    let mut manifest = AppManifest::new();
    manifest.compaction = Some(CompactionConfig {
        trigger_tokens: 5000,
        max_summary_bytes: 512,
        ..Default::default()
    });
    let mut core = DustCore::new(manifest, provider);
    let mut session = AgentSession::new();
    core.prompt_report(&mut session, "one").await;
    core.prompt_report(&mut session, "two").await;
    let prior_requests = seen.lock().unwrap().len();
    let report = core.prompt_report(&mut session, "three").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.compactions.len(), 1);
    assert!(report.compactions[0].estimated_tokens_after < 5000);
    assert_eq!(
        seen.lock().unwrap().len(),
        prior_requests + report.turns_used,
        "summary precedes the task dispatch after sufficient compaction"
    );
    remove_archive(report.transcript.as_ref().unwrap());
}
#[tokio::test]
async fn provider_capacity_prevents_premature_compaction_and_override_wins() {
    let mut provider = model("continuation notes");
    provider.capacity = Some(128_000);
    let mut core = DustCore::new(AppManifest::new(), provider);
    let mut session = AgentSession::new();
    let mut archive = None;
    for input in ["one", "two", "three"] {
        let report = core.prompt_report(&mut session, input).await;
        assert_eq!(report.stop_reason, StopReason::Completed);
        assert!(report.compactions.is_empty());
        archive = report.transcript;
    }
    remove_archive(archive.as_ref().unwrap());

    let mut provider = model("continuation notes");
    provider.capacity = Some(128_000);
    let mut manifest = AppManifest::new();
    manifest.compaction = Some(CompactionConfig {
        context_window_tokens: Some(32_768),
        ..Default::default()
    });
    let mut core = DustCore::new(manifest, provider);
    let mut session = AgentSession::new();
    core.prompt_report(&mut session, "one").await;
    core.prompt_report(&mut session, "two").await;
    let report = core.prompt_report(&mut session, "three").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.compactions.len(), 1);
    assert_eq!(report.compactions[0].context_window_tokens, 32_768);
    assert_eq!(report.compactions[0].trigger_tokens, 26_216);
    remove_archive(report.transcript.as_ref().unwrap());
}
#[tokio::test]
async fn compaction_directory_is_cumulative_scoped_and_reopens_with_original_anchors() {
    let provider = model(
        "Investigated authentication and cache behavior; consult originals for exact evidence.",
    );
    let seen = provider.requests.clone();
    let mut core = DustCore::new(AppManifest::new(), provider);
    let mut session = AgentSession::new();
    core.prompt_report(&mut session, "Investigate src/auth.rs refresh_token 401")
        .await;
    core.prompt_report(&mut session, "Check cache_policy and auth_result")
        .await;
    let first = core
        .prompt_report(&mut session, "Continue authentication investigation")
        .await;
    assert_eq!(first.stop_reason, StopReason::Completed);
    assert_eq!(first.compactions.len(), 1);
    let second = core
        .prompt_report(&mut session, "Inspect next cache issue")
        .await;
    assert_eq!(second.stop_reason, StopReason::Completed);
    assert_eq!(second.compactions.len(), 1);
    let reply: serde_json::Value = serde_json::from_str(
        &core
            .execute_tool(
                "dustagent__history_directory",
                serde_json::json!({"limit":1}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reply["isError"], false);
    assert_eq!(reply["result"]["count"], 2);
    assert!(reply["result"]["next_cursor"].as_u64().is_some());
    let bad: serde_json::Value = serde_json::from_str(
        &core
            .execute_tool(
                "dustagent__history_directory",
                serde_json::json!({"path":"/etc/passwd"}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(bad["isError"], true);
    let reference = second.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    let directory = store.directory(0, 10, None).unwrap();
    assert_eq!(directory["count"], 2);
    assert!(directory.to_string().contains("src/auth.rs"));
    for entry in directory["entries"].as_array().unwrap() {
        for keyword in entry["keywords"].as_array().unwrap() {
            let term = keyword["term"].as_str().unwrap();
            let index = keyword["index"].as_u64().unwrap();
            let matches = store.search(term, 0, 20).unwrap();
            assert!(
                matches["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|result| result["index"] == index)
            );
        }
    }
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|request| request.iter().any(|message| message
                .content
                .as_deref()
                .is_some_and(|text| text.contains("history_directory"))))
    );
    let mut other = DustCore::new(AppManifest::new(), model("unused"));
    let other_report = other.execute_report("unrelated session").await;
    let other_directory: serde_json::Value = serde_json::from_str(
        &other
            .execute_tool("dustagent__history_directory", serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(other_directory["result"]["count"], 0);
    remove_archive(other_report.transcript.as_ref().unwrap());
    remove_archive(reference);
}
#[tokio::test]
async fn tighter_budget_drops_recent_answer_and_keeps_directory_within_bounds() {
    async fn attempt(trigger: usize) -> (DustCore<Model>, dustagent::ExecutionReport, Requests) {
        let provider = model("brief");
        let seen = provider.requests.clone();
        let mut manifest = AppManifest::new();
        manifest.compaction = Some(CompactionConfig {
            trigger_tokens: trigger,
            keep_recent_messages: 2,
            max_summary_bytes: 512,
            ..Default::default()
        });
        let mut core = DustCore::new(manifest, provider);
        let mut session = AgentSession::new();
        core.prompt_report(&mut session, "one").await;
        core.prompt_report(&mut session, "two").await;
        let report = core.prompt_report(&mut session, "three").await;
        (core, report, seen)
    }
    let (_, baseline, _) = attempt(0).await;
    assert_eq!(baseline.stop_reason, StopReason::Completed);
    let trigger = baseline.compactions[0].estimated_tokens_after - 1;
    remove_archive(baseline.transcript.as_ref().unwrap());
    let (mut core, failed, seen) = attempt(trigger).await;
    assert_eq!(failed.stop_reason, StopReason::Completed);
    assert_eq!(failed.compactions.len(), 1);
    assert!(failed.compactions[0].estimated_tokens_after < trigger);
    assert_eq!(seen.lock().unwrap().len(), 4);
    let directory: serde_json::Value = serde_json::from_str(
        &core
            .execute_tool("dustagent__history_directory", serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(directory["result"]["count"], 1);
    let reference = failed.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    assert_eq!(store.directory(0, 10, None).unwrap()["count"], 1);
    remove_archive(reference);
}
fn remove_archive(reference: &dustagent::application::transcript::TranscriptRef) {
    std::fs::remove_dir_all(reference.path.parent().unwrap()).unwrap();
}
#[tokio::test]
async fn session_compact_preserves_originals_summary_and_followup_protocol() {
    let model = model(
        "First answer and second answer were completed; retain exact originals when details matter.",
    );
    let seen = model.requests.clone();
    let mut core = DustCore::new(
        AppManifest::new().with_system_prompt("app instructions"),
        model,
    );
    let mut session = AgentSession::new();
    let first = core.prompt_report(&mut session, "first request").await;
    assert_eq!(first.stop_reason, StopReason::Completed);
    assert!(first.compactions.is_empty());
    let second = core.prompt_report(&mut session, "second request").await;
    assert!(second.compactions.is_empty());
    let third = core.prompt_report(&mut session, "latest request").await;
    assert_eq!(third.stop_reason, StopReason::Completed);
    assert_eq!(third.compactions.len(), 1);
    assert_eq!(third.turns_used, 2, "summary call consumes one turn");
    let reference = third.transcript.as_ref().unwrap();
    assert_eq!(
        reference.binding.len(),
        64,
        "only hashed app binding is exposed"
    );
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    let first_answer = store.read(3, 39_000, 16384).unwrap();
    assert!(
        first_answer["text"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_END_MARKER")
    );
    assert!(
        store.search("compaction response", 0, 20).unwrap()["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["index"].is_number())
    );
    let requests = seen.lock().unwrap();
    let active = requests.last().unwrap();
    assert_eq!(active[0].content.as_deref(), Some("app instructions"));
    assert!(
        active
            .iter()
            .any(|m| m.content.as_deref() == Some("first request"))
    );
    assert!(
        active
            .iter()
            .any(|m| m.content.as_deref() == Some("latest request"))
    );
    assert!(active.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|s| s.contains("compacted context"))
    }));
    assert_eq!(requests.len(), 4);
    remove_archive(reference);
}
#[tokio::test]
async fn invalid_summary_keeps_original_archive_and_stops_before_task_request() {
    let model = model("");
    let seen = model.requests.clone();
    let mut core = DustCore::new(AppManifest::new(), model);
    let mut session = AgentSession::new();
    core.prompt_report(&mut session, "one").await;
    core.prompt_report(&mut session, "two").await;
    let report = core.prompt_report(&mut session, "three").await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert!(report.error.as_deref().unwrap().contains("empty summary"));
    assert!(report.compactions.is_empty());
    assert_eq!(seen.lock().unwrap().len(), 5);
    let reference = report.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    assert_eq!(
        store.search("ORIGINAL_END_MARKER", 0, 20).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    remove_archive(reference);
}
#[tokio::test]
async fn disabled_compaction_does_not_call_summary_model() {
    let model = model("unused");
    let seen = model.requests.clone();
    let mut manifest = AppManifest::new();
    manifest.compaction = Some(CompactionConfig {
        enabled: false,
        ..Default::default()
    });
    let mut core = DustCore::new(manifest, model);
    let mut session = AgentSession::new();
    core.prompt_report(&mut session, "one").await;
    core.prompt_report(&mut session, "two").await;
    let report = core.prompt_report(&mut session, "three").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert!(report.compactions.is_empty());
    assert_eq!(seen.lock().unwrap().len(), 3);
    remove_archive(report.transcript.as_ref().unwrap());
}
#[tokio::test]
async fn archive_tampering_blocks_session_before_model_and_cannot_be_bypassed() {
    let model = model("summary");
    let seen = model.requests.clone();
    let mut core = DustCore::new(AppManifest::new(), model);
    let mut session = AgentSession::new();
    let first = core.prompt_report(&mut session, "one").await;
    let reference = first.transcript.unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&reference.path)
        .unwrap()
        .write_all(b"{}\n")
        .unwrap();
    let next = core.prompt_report(&mut session, "two").await;
    assert_eq!(next.stop_reason, StopReason::ExecutionError);
    assert!(session.is_blocked());
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        core.prompt_report(&mut session, "three").await.stop_reason,
        StopReason::ExecutionError
    );
    remove_archive(&reference);
}
