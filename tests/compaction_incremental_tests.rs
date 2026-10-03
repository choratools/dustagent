//! Bounded summary staging preserves complete active batches and archive originals.
use async_trait::async_trait;
use dustagent::application::{
    checkpoint::{self, Checkpoint, CheckpointPhase},
    compaction::{CompactionConfig, estimate},
    transcript::TranscriptStore,
};
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolCall,
    ToolDefinition,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

type Requests = Arc<Mutex<Vec<Vec<ChatMessage>>>>;
struct Model {
    requests: Requests,
    delay_from_call: Option<usize>,
    empty_first: bool,
}
#[async_trait]
impl LlmProvider for Model {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        let count = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(messages.to_vec());
            requests.len()
        };
        if self.delay_from_call.is_some_and(|first| count >= first) {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if tools.is_none() {
            Ok(LlmResponse::text(if self.empty_first && count == 1 {
                ""
            } else {
                "Cumulative continuation facts are unverified; inspect originals for exact evidence."
            }))
        } else {
            Ok(LlmResponse::text(
                "Task continued after the entire batch was summarized.",
            ))
        }
    }
}
fn fixture(
    max_turns: usize,
    delay_from_call: Option<usize>,
    empty_first: bool,
) -> (DustCore<Model>, Checkpoint, Requests, tempfile::TempDir) {
    fixture_with_window(max_turns, delay_from_call, empty_first, 32_768)
}
fn fixture_with_window(
    max_turns: usize,
    delay_from_call: Option<usize>,
    empty_first: bool,
    window: usize,
) -> (DustCore<Model>, Checkpoint, Requests, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = AppManifest::new().with_system_prompt("app instructions");
    manifest.max_turns = Some(max_turns);
    manifest.timeout_ms = Some(if delay_from_call.is_some() {
        1000
    } else {
        10_000
    });
    manifest.compaction = Some(CompactionConfig {
        context_window_tokens: Some(window),
        max_summary_bytes: 512,
        ..Default::default()
    });
    let mut messages = vec![
        ChatMessage::system("app instructions"),
        ChatMessage::user("original current request"),
        ChatMessage::assistant(
            None,
            Some(
                (0..12)
                    .map(|index| {
                        ToolCall::new(
                            format!("call-{index}"),
                            "read_document",
                            format!("{{\"index\":{index}}}"),
                        )
                    })
                    .collect(),
            ),
        ),
    ];
    messages.extend((0..12).map(|index| {
        ChatMessage::tool(
            format!("call-{index}"),
            format!(
                "DOCUMENT-{index}-START\n{}\nDOCUMENT-{index}-END",
                "quoted \"value\" \\ newline\n한글\t".repeat(if window == 4096 { 60 } else { 600 })
            ),
        )
    }));
    let seed = Checkpoint::new(
        &manifest,
        "original current request",
        messages,
        Default::default(),
        CheckpointPhase::Ready,
    )
    .unwrap();
    let requests: Requests = Arc::default();
    let provider = Model {
        requests: requests.clone(),
        delay_from_call,
        empty_first,
    };
    let core =
        DustCore::new(manifest, provider).with_checkpoint(dir.path().join("checkpoint.json"));
    (core, seed, requests, dir)
}
fn cleanup(report: &dustagent::ExecutionReport) {
    std::fs::remove_dir_all(report.transcript.as_ref().unwrap().path.parent().unwrap()).unwrap();
}
fn assert_originals(report: &dustagent::ExecutionReport, originals: &[ChatMessage]) {
    let reference = report.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    for (index, message) in originals.iter().enumerate().skip(3) {
        let serialized = serde_json::to_string(message).unwrap();
        let mut full = String::new();
        let mut offset = 0;
        loop {
            let page = store.read(index as u64 + 1, offset, 16_384).unwrap();
            full.push_str(page["text"].as_str().unwrap());
            if let Some(next) = page["next_offset"].as_u64() {
                offset = next as usize;
            } else {
                break;
            }
        }
        assert_eq!(full, serialized);
    }
}
#[tokio::test]
async fn quote_heavy_parallel_batch_uses_bounded_stages_and_archives_every_original() {
    let (mut core, seed, requests, _dir) = fixture(32, None, false);
    let report = core.resume_report(&seed).await;
    assert_eq!(
        report.stop_reason,
        StopReason::Completed,
        "{:?}",
        report.error
    );
    assert_eq!(report.compactions.len(), 1);
    assert!(report.compaction_attempts.len() > 1);
    assert_eq!(report.turns_used, report.compaction_attempts.len() + 1);
    let config = CompactionConfig {
        context_window_tokens: Some(32_768),
        max_summary_bytes: 512,
        ..Default::default()
    }
    .resolve(None);
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), report.turns_used);
    for request in captured.iter().take(captured.len() - 1) {
        assert!(estimate(request, 0) < 29_492);
        assert!(estimate(request, 0) + 512usize.div_ceil(3) + 128 < 32_768);
        let content = request[1].content.as_deref().unwrap();
        assert!(content.contains("older") || content.contains("OLDER CONVERSATION SOURCE CHUNK"));
    }
    let final_request = captured.last().unwrap();
    assert!(estimate(final_request, 0) < config.trigger_tokens);
    assert!(
        final_request
            .iter()
            .any(|m| m.content.as_deref() == Some("original current request"))
    );
    assert!(
        !final_request
            .iter()
            .any(|m| m.role == "tool" || m.tool_calls.is_some())
    );
    drop(captured);
    assert_originals(&report, &seed.messages);
    cleanup(&report);
}
#[tokio::test]
async fn staged_turn_limit_retains_active_complete_batch_and_archived_partial_notes() {
    let (mut core, seed, requests, dir) = fixture(1, None, false);
    let report = core.resume_report(&seed).await;
    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert_eq!(report.turns_used, 1);
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(report.compactions.is_empty());
    assert_eq!(
        checkpoint::load(dir.path().join("checkpoint.json"))
            .unwrap()
            .messages,
        seed.messages
    );
    let reference = report.transcript.as_ref().unwrap();
    let store = TranscriptStore::open(reference, &reference.binding).unwrap();
    assert_eq!(store.directory(0, 10, None).unwrap()["count"], 0);
    assert_eq!(
        store.search("compaction response", 0, 20).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_originals(&report, &seed.messages);
    cleanup(&report);
}
#[tokio::test]
async fn staged_deadline_does_not_reset_between_chunks_or_dispatch_task() {
    let (mut core, seed, requests, dir) = fixture(32, Some(2), false);
    let report = core.resume_report(&seed).await;
    assert_eq!(report.stop_reason, StopReason::TimeLimit);
    assert_eq!(report.turns_used, 2);
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(report.compactions.is_empty());
    assert_eq!(
        checkpoint::load(dir.path().join("checkpoint.json"))
            .unwrap()
            .messages,
        seed.messages
    );
    assert_originals(&report, &seed.messages);
    cleanup(&report);
}
#[tokio::test]
async fn invalid_first_stage_retries_same_source_then_progresses_with_turn_accounting() {
    let (mut core, seed, requests, _dir) = fixture(32, None, true);
    let report = core.resume_report(&seed).await;
    assert_eq!(
        report.stop_reason,
        StopReason::Completed,
        "{:?}",
        report.error
    );
    assert_eq!(
        serde_json::to_value(&report.compaction_attempts[0]).unwrap()["status"],
        "empty"
    );
    assert_eq!(report.compaction_attempts[1].attempt, 2);
    assert_eq!(
        report.compaction_attempts[2].attempt, 1,
        "retry allowance restarts after a valid chunk"
    );
    assert_eq!(requests.lock().unwrap().len(), report.turns_used);
    let captured = requests.lock().unwrap();
    assert!(
        captured[0][1]
            .content
            .as_deref()
            .unwrap()
            .contains("UTF-8 bytes 0..")
    );
    assert!(
        captured[1][1]
            .content
            .as_deref()
            .unwrap()
            .contains("UTF-8 bytes 0..")
    );
    drop(captured);
    assert_originals(&report, &seed.messages);
    cleanup(&report);
}

#[tokio::test]
async fn small_context_keeps_summary_and_directory_allowance_within_actual_hard_bounds() {
    let (mut core, seed, requests, _dir) = fixture_with_window(32, None, false, 4096);
    let report = core.resume_report(&seed).await;
    assert_eq!(
        report.stop_reason,
        StopReason::Completed,
        "{:?}",
        report.error
    );
    assert_eq!(report.compactions.len(), 1);
    assert!(report.compactions[0].estimated_tokens_after < 2048);
    for request in requests
        .lock()
        .unwrap()
        .iter()
        .take(report.compaction_attempts.len())
    {
        assert!(estimate(request, 0) < 3072);
        assert!(estimate(request, 0) + 512usize.div_ceil(3) + 128 < 4096);
    }
    assert_originals(&report, &seed.messages);
    cleanup(&report);
}
