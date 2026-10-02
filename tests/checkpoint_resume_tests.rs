use async_trait::async_trait;
use dustagent::application::checkpoint;
use dustagent::ports::mcp::{McpClient, McpTool};
use dustagent::{
    AppManifest, ChatMessage, CheckpointPhase, DustCore, LlmProvider, LlmResponse, StopReason,
    ToolCall, ToolDefinition,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Provider {
    replies: Mutex<VecDeque<dustagent::Result<LlmResponse>>>,
    calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    destroy: Option<std::path::PathBuf>,
}
impl Provider {
    fn new(replies: Vec<dustagent::Result<LlmResponse>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            calls: Arc::default(),
            destroy: None,
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
        if let Some(path) = &self.destroy {
            std::fs::remove_file(path).unwrap();
            std::fs::create_dir(path).unwrap();
        }
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("Unexpected replay/model call")
    }
}
struct CounterTool {
    count: Arc<AtomicUsize>,
    fail: bool,
}
#[async_trait]
impl McpClient for CounterTool {
    async fn initialize(&mut self) -> dustagent::Result<()> {
        Ok(())
    }
    async fn list_tools(&mut self) -> dustagent::Result<Vec<McpTool>> {
        Ok(vec![McpTool::new(
            "observe",
            None,
            json!({"type":"object"}),
        )])
    }
    async fn call_tool(&mut self, _: &str, _: Value) -> dustagent::Result<Value> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(dustagent::DustError::Mcp(
                "connection reset after dispatch".into(),
            ))
        } else {
            Ok(json!({"body":"observed document","coverage":{"observed":100,"remaining":190}}))
        }
    }
    async fn close(&mut self) -> dustagent::Result<()> {
        Ok(())
    }
}
fn observe() -> LlmResponse {
    LlmResponse::tool_calls(vec![ToolCall::new("call1", "source__observe", "{}")])
}
#[tokio::test]
async fn safe_resume_restores_transcript_without_replaying_completed_tool() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let manifest = AppManifest::new().with_system_prompt("Observe documents");
    let count = Arc::new(AtomicUsize::new(0));
    let mut first = DustCore::new(manifest.clone(), Provider::new(vec![Ok(observe())]))
        .with_max_turns(1)
        .with_checkpoint(&path)
        .with_mcp_client(
            "source",
            Box::new(CounterTool {
                count: count.clone(),
                fail: false,
            }),
        );
    assert_eq!(
        first.execute_report("original input").await.stop_reason,
        StopReason::TurnLimit
    );
    let saved = checkpoint::load(&path).unwrap();
    assert_eq!(saved.phase, CheckpointPhase::Ready);
    let provider = Provider::new(vec![Ok(LlmResponse::text("Final from recorded evidence"))]);
    let calls = provider.calls.clone();
    let mut resumed = DustCore::new(manifest, provider)
        .with_max_turns(1)
        .with_checkpoint(&path)
        .with_mcp_client(
            "source",
            Box::new(CounterTool {
                count: count.clone(),
                fail: false,
            }),
        );
    let report = resumed.resume_report(&saved).await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.tool_calls.len(), 1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let messages = &calls.lock().unwrap()[0];
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].content.as_deref(), Some("original input"));
    assert_eq!(messages[3].role, "tool");
    assert!(
        messages[3]
            .content
            .as_ref()
            .unwrap()
            .contains("observed document")
    );
    assert_eq!(
        checkpoint::load(&path).unwrap().phase,
        CheckpointPhase::Finished
    );
}
#[tokio::test]
async fn unknown_tool_outcome_blocks_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let manifest = AppManifest::new();
    let count = Arc::new(AtomicUsize::new(0));
    let mut first = DustCore::new(manifest.clone(), Provider::new(vec![Ok(observe())]))
        .with_checkpoint(&path)
        .with_mcp_client(
            "source",
            Box::new(CounterTool {
                count: count.clone(),
                fail: true,
            }),
        );
    assert_eq!(
        first.execute_report("input").await.stop_reason,
        StopReason::ExecutionError
    );
    let saved = checkpoint::load(&path).unwrap();
    assert_eq!(saved.phase, CheckpointPhase::Blocked);
    let provider = Provider::new(vec![]);
    let calls = provider.calls.clone();
    let mut resumed = DustCore::new(manifest, provider).with_checkpoint(&path);
    assert_eq!(
        resumed.resume_report(&saved).await.stop_reason,
        StopReason::ExecutionError
    );
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn save_failure_before_dispatch_prevents_effect() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let count = Arc::new(AtomicUsize::new(0));
    let mut provider = Provider::new(vec![Ok(observe())]);
    provider.destroy = Some(path.clone());
    let mut core = DustCore::new(AppManifest::new(), provider)
        .with_checkpoint(&path)
        .with_mcp_client(
            "source",
            Box::new(CounterTool {
                count: count.clone(),
                fail: false,
            }),
        );
    let report = core.execute_report("input").await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(report.error.unwrap().contains("Checkpoint save failed"));
}
#[tokio::test]
async fn model_failure_is_resumable_but_tool_timeout_is_not() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.json");
    let mut core = DustCore::new(
        AppManifest::new(),
        Provider::new(vec![Err(dustagent::DustError::Llm(
            "network failure".into(),
        ))]),
    )
    .with_checkpoint(&path);
    assert_eq!(
        core.execute_report("input").await.stop_reason,
        StopReason::ExecutionError
    );
    assert!(checkpoint::load(&path).unwrap().ensure_resumable().is_ok());
    let path = directory.path().join("tool.json");
    let provider = Provider::new(vec![Ok(LlmResponse::tool_calls(vec![ToolCall::new(
        "sleep1",
        "dustagent__sleep",
        r#"{"ms":1000}"#,
    )]))]);
    let mut core = DustCore::new(AppManifest::new(), provider)
        .with_checkpoint(&path)
        .with_timeouts(500, 20);
    assert_eq!(
        core.execute_report("input").await.stop_reason,
        StopReason::ToolTimeout
    );
    let saved = checkpoint::load(&path).unwrap();
    assert_eq!(saved.phase, CheckpointPhase::ToolInFlight);
    assert!(saved.ensure_resumable().is_err());
}
#[tokio::test]
async fn resume_requires_persistent_destination_and_matching_manifest() {
    let manifest = AppManifest::new();
    let saved = dustagent::Checkpoint::new(
        &manifest,
        "input",
        vec![
            ChatMessage::system("You are a helpful specialized assistant."),
            ChatMessage::user("input"),
        ],
        Default::default(),
        CheckpointPhase::Ready,
    )
    .unwrap();
    let mut core = DustCore::new(manifest, Provider::new(vec![]));
    assert_eq!(
        core.resume_report(&saved).await.stop_reason,
        StopReason::ExecutionError
    );
    let directory = tempfile::tempdir().unwrap();
    let mut core = DustCore::new(
        AppManifest::new().with_system_prompt("changed"),
        Provider::new(vec![]),
    )
    .with_checkpoint(directory.path().join("state"));
    assert_eq!(
        core.resume_report(&saved).await.stop_reason,
        StopReason::ExecutionError
    );
}
