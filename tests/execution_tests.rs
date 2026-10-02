use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dustagent::ports::mcp::{McpClient, McpTool};
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolCall,
    ToolDefinition, ToolStatus,
};
use serde_json::{Value, json};

#[derive(Clone)]
struct Provider {
    replies: Arc<Mutex<VecDeque<dustagent::Result<LlmResponse>>>>,
    delay: Duration,
}
impl Provider {
    fn new(replies: Vec<dustagent::Result<LlmResponse>>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
            delay: Duration::ZERO,
        }
    }
}
#[async_trait]
impl LlmProvider for Provider {
    async fn chat(
        &self,
        _: &[ChatMessage],
        _: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        tokio::time::sleep(self.delay).await;
        self.replies.lock().unwrap().pop_front().unwrap()
    }
}
struct EvidenceTool;
#[async_trait]
impl McpClient for EvidenceTool {
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
        Ok(
            json!({"body":"observed text", "coverage":{"discovered":290,"observed":100,"remaining":190}}),
        )
    }
    async fn close(&mut self) -> dustagent::Result<()> {
        Ok(())
    }
}
fn observation() -> LlmResponse {
    LlmResponse {
        content: Some("Observation in progress".into()),
        tool_calls: Some(vec![ToolCall::new("call1", "source__observe", "{}")]),
    }
}
#[tokio::test]
async fn exhaustion_retains_actual_evidence_and_partial_text() {
    let provider = Provider::new(vec![Ok(observation())]);
    let mut core = DustCore::new(AppManifest::new(), provider)
        .with_max_turns(1)
        .with_mcp_client("source", Box::new(EvidenceTool));
    let report = core.execute_report("Observe all links").await;
    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert!(!report.is_complete());
    assert_eq!(report.output.as_deref(), Some("Observation in progress"));
    assert_eq!(report.turns_used, 1);
    assert_eq!(report.turns[0].tool_call_count, 1);
    assert_eq!(report.tool_calls[0].status, ToolStatus::Succeeded);
    let result: Value =
        serde_json::from_str(report.tool_calls[0].output.as_ref().unwrap()).unwrap();
    assert_eq!(result["coverage"]["remaining"], 190);
    assert_eq!(result["body"], "observed text");
    assert!(report.into_result().is_err());
}
#[tokio::test]
async fn completed_empty_and_provider_failure_are_distinct() {
    for (response, reason) in [
        (Ok(LlmResponse::text("done")), StopReason::Completed),
        (Ok(LlmResponse::text("  ")), StopReason::EmptyResponse),
        (
            Err(dustagent::DustError::Llm("offline".into())),
            StopReason::ExecutionError,
        ),
    ] {
        let mut core = DustCore::new(AppManifest::new(), Provider::new(vec![response]));
        let report = core.execute_report("task").await;
        assert_eq!(report.stop_reason, reason);
        assert_eq!(report.turns.len(), 1);
        assert_eq!(report.turns_used, 1);
    }
}
#[tokio::test]
async fn provider_timeout_has_a_turn_record() {
    let mut provider = Provider::new(vec![Ok(LlmResponse::text("late"))]);
    provider.delay = Duration::from_secs(1);
    let mut core = DustCore::new(AppManifest::new(), provider).with_timeouts(20, 100);
    let report = core.execute_report("task").await;
    assert_eq!(report.stop_reason, StopReason::TimeLimit);
    assert_eq!(report.turns.len(), 1);
    assert!(report.turns[0].error.is_some());
    assert!(report.output.is_none());
}
#[tokio::test]
async fn tool_timeout_is_terminal_and_client_is_discarded() {
    let provider = Provider::new(vec![Ok(LlmResponse::tool_calls(vec![ToolCall::new(
        "slow",
        "dustagent__sleep",
        r#"{"ms":1000}"#,
    )]))]);
    let mut core = DustCore::new(AppManifest::new(), provider).with_timeouts(500, 15);
    let report = core.execute_report("wait").await;
    assert_eq!(report.stop_reason, StopReason::ToolTimeout);
    assert_eq!(report.tool_calls[0].status, ToolStatus::TimedOut);
    assert!(report.tool_calls[0].output.is_none());
    assert!(
        core.execute_tool("dustagent__timestamp", json!({}))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn global_budget_wins_over_longer_tool_budget() {
    let provider = Provider::new(vec![Ok(LlmResponse::tool_calls(vec![ToolCall::new(
        "slow",
        "dustagent__sleep",
        r#"{"ms":1000}"#,
    )]))]);
    let mut core = DustCore::new(AppManifest::new(), provider).with_timeouts(20, 100);
    let report = core.execute_report("wait").await;
    assert_eq!(report.stop_reason, StopReason::TimeLimit);
    assert_eq!(report.tool_calls[0].status, ToolStatus::TimedOut);
}
#[tokio::test]
async fn later_provider_failure_preserves_observation() {
    let provider = Provider::new(vec![
        Ok(observation()),
        Err(dustagent::DustError::Llm("connection reset".into())),
    ]);
    let mut core = DustCore::new(AppManifest::new(), provider)
        .with_mcp_client("source", Box::new(EvidenceTool));
    let report = core.execute_report("task").await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.tool_calls.len(), 1);
    assert!(report.error.unwrap().contains("connection reset"));
}
#[tokio::test]
async fn invalid_arguments_are_recorded_and_not_run() {
    let provider = Provider::new(vec![
        Ok(LlmResponse::tool_calls(vec![ToolCall::new(
            "bad",
            "dustagent__sleep",
            "not json",
        )])),
        Ok(LlmResponse::text("Recovered")),
    ]);
    let manifest = AppManifest::new();
    let dir = tempfile::tempdir().unwrap();
    let store = dustagent::application::ExperienceStore::new(dir.path());
    let mut core = DustCore::new(manifest.clone(), provider);
    let report = core.execute_report_with_experience("task", &store).await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.tool_calls[0].status, ToolStatus::InvalidArguments);
    assert!(!store.list(&manifest).unwrap()[0].completed);
}

#[tokio::test]
async fn actual_validation_failure_is_not_completion_or_a_reusable_example() {
    let mut manifest = AppManifest::new();
    manifest.validation = Some(dustagent::application::validation::ValidationConfig {
        command: "python3".into(),
        args: vec!["-c".into(), "import json,sys; json.load(sys.stdin); print(json.dumps({'passed':False,'reason':'required invariant failed'}))".into()],
        timeout_ms: 5000,
    });
    let provider = Provider::new(vec![Ok(LlmResponse::text("convincing but incorrect"))]);
    let dir = tempfile::tempdir().unwrap();
    let store = dustagent::application::ExperienceStore::new(dir.path());
    let mut core = DustCore::new(manifest.clone(), provider);
    let report = core.execute_report_with_experience("task", &store).await;
    assert_eq!(report.stop_reason, StopReason::ValidationFailed);
    assert!(!report.validation.as_ref().unwrap().passed);
    assert_eq!(report.output.as_deref(), Some("convincing but incorrect"));
    assert!(!store.list(&manifest).unwrap()[0].completed);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn actual_stdio_timeout_reaches_call_then_kills_and_reaps_server() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("called.json");
    let script = r#"
import json,os,sys,time
for line in sys.stdin:
    request=json.loads(line)
    if 'id' not in request:
        continue
    method=request['method']
    if method=='initialize':
        result={}
    elif method=='tools/list':
        result={'tools':[{'name':'observe','inputSchema':{'type':'object'}}]}
    else:
        with open(sys.argv[1],'w') as handle:
            json.dump({'pid':os.getpid(),'method':method},handle)
        time.sleep(10)
        result={'body':'late'}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#;
    let args = vec![
        "-u".into(),
        "-c".into(),
        script.into(),
        marker.to_string_lossy().into_owned(),
    ];
    let client = tokio::time::timeout(
        Duration::from_secs(5),
        dustagent::McpStdioClient::start_and_init("python3", &args, None),
    )
    .await
    .unwrap()
    .unwrap();
    let provider = Provider::new(vec![Ok(observation())]);
    let mut core = DustCore::new(AppManifest::new(), provider)
        .with_timeouts(2000, 100)
        .with_mcp_client("source", Box::new(client));
    let report = core.execute_report("observe").await;
    assert_eq!(report.stop_reason, StopReason::ToolTimeout);
    let reached: Value = serde_json::from_slice(&std::fs::read(marker).unwrap()).unwrap();
    assert_eq!(reached["method"], "tools/call");
    let pid = reached["pid"].as_u64().unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "Timed-out server must be reaped"
    );
    assert!(
        core.execute_tool("source__observe", json!({}))
            .await
            .is_err()
    );
}
