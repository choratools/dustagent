use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use dustagent::ports::mcp::{McpClient, McpTool};
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, McpServerConfig, StopReason,
    ToolCall, ToolDefinition, ToolStatus,
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

#[derive(Clone, Default)]
struct ConcurrencyProbe {
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

struct DelayedTool {
    name: String,
    probe: ConcurrencyProbe,
    arguments: Arc<Mutex<Vec<Value>>>,
    schema: Value,
}

#[async_trait]
impl McpClient for DelayedTool {
    async fn initialize(&mut self) -> dustagent::Result<()> {
        Ok(())
    }

    async fn list_tools(&mut self) -> dustagent::Result<Vec<McpTool>> {
        Ok(vec![McpTool::new(
            self.name.clone(),
            None,
            self.schema.clone(),
        )])
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> dustagent::Result<Value> {
        self.arguments.lock().unwrap().push(arguments);
        let active = self.probe.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe.maximum.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        self.probe.active.fetch_sub(1, Ordering::SeqCst);
        Ok(json!({"tool": name}))
    }

    async fn close(&mut self) -> dustagent::Result<()> {
        Ok(())
    }
}

fn execution_tool_call(name: &str, id: &str) -> ToolCall {
    ToolCall::new(id, name, "{}")
}

fn effect_config(tool: &str, read_only: bool) -> McpServerConfig {
    let mut config = McpServerConfig::new("not-started-test-client");
    if read_only {
        config.read_only_tools.push(tool.into());
    }
    config
}

fn tool_schema() -> Value {
    json!({"type":"object","properties":{}})
}

fn dispatch_provider(calls: Vec<ToolCall>) -> Provider {
    Provider::new(vec![
        Ok(LlmResponse::tool_calls(calls)),
        Ok(LlmResponse::text("done")),
    ])
}

#[tokio::test]
async fn declared_read_only_calls_run_in_bounded_parallel_and_keep_order() {
    let probe = ConcurrencyProbe::default();
    let first_args = Arc::new(Mutex::new(Vec::new()));
    let second_args = Arc::new(Mutex::new(Vec::new()));
    let manifest = AppManifest::new()
        .with_mcp_server("first", effect_config("observe", true))
        .with_mcp_server("second", effect_config("observe", true));
    let mut core = DustCore::new(
        manifest,
        dispatch_provider(vec![
            execution_tool_call("first__observe", "one"),
            execution_tool_call("second__observe", "two"),
        ]),
    )
    .with_mcp_client(
        "first",
        Box::new(DelayedTool {
            name: "observe".into(),
            probe: probe.clone(),
            arguments: first_args,
            schema: tool_schema(),
        }),
    )
    .with_mcp_client(
        "second",
        Box::new(DelayedTool {
            name: "observe".into(),
            probe: probe.clone(),
            arguments: second_args,
            schema: tool_schema(),
        }),
    );

    let report = core.execute_report("observe safely").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(probe.maximum.load(Ordering::SeqCst), 2);
    assert_eq!(report.tool_calls.len(), 2);
    assert_eq!(report.tool_calls[0].call_id, "one");
    assert_eq!(
        report.tool_calls[0].output.as_deref(),
        Some(r#"{"tool":"observe"}"#)
    );
    assert_eq!(report.tool_calls[1].call_id, "two");
    assert_eq!(
        report.tool_calls[1].output.as_deref(),
        Some(r#"{"tool":"observe"}"#)
    );
}

#[tokio::test]
async fn undeclared_effect_calls_remain_serial() {
    let probe = ConcurrencyProbe::default();
    let manifest = AppManifest::new()
        .with_mcp_server("first", McpServerConfig::new("not-started-test-client"))
        .with_mcp_server("second", McpServerConfig::new("not-started-test-client"));
    let mut core = DustCore::new(
        manifest,
        dispatch_provider(vec![
            execution_tool_call("first__send", "one"),
            execution_tool_call("second__send", "two"),
        ]),
    )
    .with_mcp_client(
        "first",
        Box::new(DelayedTool {
            name: "send".into(),
            probe: probe.clone(),
            arguments: Arc::new(Mutex::new(Vec::new())),
            schema: tool_schema(),
        }),
    )
    .with_mcp_client(
        "second",
        Box::new(DelayedTool {
            name: "send".into(),
            probe: probe.clone(),
            arguments: Arc::new(Mutex::new(Vec::new())),
            schema: tool_schema(),
        }),
    );

    let report = core.execute_report("send carefully").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(probe.maximum.load(Ordering::SeqCst), 1);
    assert_eq!(report.tool_calls.len(), 2);
}

#[tokio::test]
async fn idempotent_tool_receives_persisted_operation_id() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut config = McpServerConfig::new("not-started-test-client");
    config.idempotent_tools.push("send".into());
    config.idempotency_key_argument = Some("operation_key".into());
    let schema = json!({
        "type":"object",
        "properties":{"operation_key":{"type":"string"}},
        "required":[]
    });
    let manifest = AppManifest::new().with_mcp_server("remote", config);
    let mut core = DustCore::new(
        manifest,
        dispatch_provider(vec![execution_tool_call("remote__send", "send-1")]),
    )
    .with_mcp_client(
        "remote",
        Box::new(DelayedTool {
            name: "send".into(),
            probe: ConcurrencyProbe::default(),
            arguments: recorded.clone(),
            schema,
        }),
    );

    let report = core.execute_report("send once").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    let operation_id = report.tool_calls[0].operation_id.as_deref().unwrap();
    assert_eq!(
        report.tool_calls[0].effect,
        dustagent::ToolEffect::Idempotent
    );
    assert_eq!(
        report.tool_calls[0].arguments["operation_key"],
        operation_id
    );
    assert_eq!(recorded.lock().unwrap()[0]["operation_key"], operation_id);
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
    provider.delay = Duration::from_secs(2);
    let mut core = DustCore::new(AppManifest::new(), provider).with_timeouts(500, 100);
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
    // Allow transcript creation/fsync to finish before timing the 1s tool.
    // The global deadline must still beat the longer per-tool deadline.
    let mut core = DustCore::new(AppManifest::new(), provider).with_timeouts(200, 500);
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
        mode: dustagent::application::validation::ValidationMode::Legacy,
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
