use async_trait::async_trait;
use dustagent::application::{events::ExecutionEvent, session::AgentSession};
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolCall,
    ToolDefinition,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
struct Provider {
    replies: Mutex<VecDeque<LlmResponse>>,
    seen: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    slow_first: bool,
}
#[async_trait]
impl LlmProvider for Provider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        let first = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(messages.to_vec());
            seen.len() == 1
        };
        if first && self.slow_first {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider call"))
    }
}
fn provider(replies: Vec<LlmResponse>) -> Provider {
    Provider {
        replies: Mutex::new(replies.into()),
        seen: Arc::default(),
        slow_first: false,
    }
}
#[tokio::test]
async fn completed_prompt_preserves_conversation_state_and_events() {
    let model = provider(vec![
        LlmResponse::tool_calls(vec![ToolCall::new(
            "memo1",
            "dustagent__state_put",
            r#"{"key":"page","value":"observed"}"#,
        )]),
        LlmResponse::text("first answer"),
        LlmResponse::text("second answer"),
    ]);
    let seen = model.seen.clone();
    let mut manifest = AppManifest::new();
    manifest.working_state = true;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut core = DustCore::new(manifest, model).with_events(tx);
    let mut session = AgentSession::new();
    let first = core.prompt_report(&mut session, "first question").await;
    assert_eq!(first.stop_reason, StopReason::Completed);
    let second = core.prompt_report(&mut session, "follow up").await;
    assert_eq!(second.stop_reason, StopReason::Completed);
    assert_eq!(second.turns_used, 1);
    assert_eq!(second.tool_calls.len(), 1);
    assert_eq!(second.working_state.unwrap().entries["page"], "observed");
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[2].last().unwrap().content.as_deref(),
        Some("follow up")
    );
    assert!(
        seen[2]
            .iter()
            .any(|m| m.content.as_deref() == Some("first answer"))
    );
    assert!(matches!(
        rx.try_recv().unwrap(),
        ExecutionEvent::ToolStarted { .. }
    ));
    assert!(matches!(
        rx.try_recv().unwrap(),
        ExecutionEvent::ToolFinished { .. }
    ));
    assert!(matches!(
        rx.try_recv().unwrap(),
        ExecutionEvent::AssistantMessage { .. }
    ));
    assert!(!session.is_blocked());
}
#[tokio::test]
async fn cancelling_model_request_allows_followup() {
    let mut model = provider(vec![LlmResponse::text("followup answer")]);
    model.slow_first = true;
    let (tx, rx) = watch::channel(false);
    let mut core = DustCore::new(AppManifest::new(), model).with_cancellation(rx);
    let mut session = AgentSession::new();
    let cancel = async {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        tx.send(true).unwrap();
    };
    let (report, ()) = tokio::join!(core.prompt_report(&mut session, "cancel me"), cancel);
    assert_eq!(report.stop_reason, StopReason::Cancelled);
    assert!(!session.is_blocked());
    tx.send(false).unwrap();
    assert_eq!(
        core.prompt_report(&mut session, "new question")
            .await
            .stop_reason,
        StopReason::Completed
    );
}
#[tokio::test]
async fn cancelling_inflight_tool_blocks_followup() {
    let model = provider(vec![LlmResponse::tool_calls(vec![ToolCall::new(
        "sleep1",
        "dustagent__sleep",
        r#"{"ms":10000}"#,
    )])]);
    let (tx, rx) = watch::channel(false);
    let (events, mut receiver) = mpsc::unbounded_channel();
    let mut core = DustCore::new(AppManifest::new(), model)
        .with_events(events)
        .with_cancellation(rx);
    let mut session = AgentSession::new();
    let cancel = async {
        while let Some(event) = receiver.recv().await {
            if matches!(event, ExecutionEvent::ToolStarted { .. }) {
                tx.send(true).unwrap();
                break;
            }
        }
    };
    let (report, ()) = tokio::join!(core.prompt_report(&mut session, "sleep"), cancel);
    assert_eq!(report.stop_reason, StopReason::Cancelled);
    assert!(session.is_blocked());
    tx.send(false).unwrap();
    assert_eq!(
        core.prompt_report(&mut session, "again").await.stop_reason,
        StopReason::ExecutionError
    );
}
#[tokio::test]
async fn session_binding_and_transcript_bound_fail_before_new_model_dispatch() {
    let mut session = AgentSession::new();
    let mut core = DustCore::new(
        AppManifest::new().with_system_prompt("one"),
        provider(vec![LlmResponse::text("ok")]),
    );
    assert_eq!(
        core.prompt_report(&mut session, "hi").await.stop_reason,
        StopReason::Completed
    );
    let model = provider(vec![]);
    let seen = model.seen.clone();
    let mut other = DustCore::new(AppManifest::new().with_system_prompt("two"), model);
    assert_eq!(
        other.prompt_report(&mut session, "hi").await.stop_reason,
        StopReason::ExecutionError
    );
    assert!(seen.lock().unwrap().is_empty());
    let mut session = AgentSession::new();
    let model = provider(vec![]);
    let seen = model.seen.clone();
    let mut core = DustCore::new(AppManifest::new(), model);
    assert_eq!(
        core.prompt_report(&mut session, &"x".repeat(8 * 1024 * 1024))
            .await
            .stop_reason,
        StopReason::ExecutionError
    );
    assert!(session.is_blocked());
    assert!(seen.lock().unwrap().is_empty());
}
#[cfg(unix)]
#[tokio::test]
async fn explicit_cwd_reaches_checker_without_mutating_process_cwd() {
    use dustagent::application::validation::{ValidationConfig, ValidationMode};
    let before = std::env::current_dir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("expected"), "yes").unwrap();
    let mut manifest = AppManifest::new();
    manifest.validation = Some(ValidationConfig { command: "python3".into(), args: vec!["-c".into(), "import json,pathlib; print(json.dumps({'passed':pathlib.Path('expected').read_text()=='yes','reason':'cwd checked'}))".into()], timeout_ms: 5000, mode: ValidationMode::Legacy });
    let mut core = DustCore::new(manifest, provider(vec![LlmResponse::text("result")]))
        .with_working_directory(directory.path())
        .unwrap();
    assert_eq!(
        core.prompt_report(&mut AgentSession::new(), "check")
            .await
            .stop_reason,
        StopReason::Completed
    );
    assert_eq!(std::env::current_dir().unwrap(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_startup_uses_separate_session_directories() {
    use dustagent::adapters::mcp_stdio::McpStdioClient;
    use dustagent::ports::mcp::McpClient;
    let before = std::env::current_dir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let args = vec!["-u".into(), "-c".into(), r#"import sys,json,os
for line in sys.stdin:
 q=json.loads(line)
 if 'id' not in q: continue
 m=q['method']
 result={'protocolVersion':'2024-11-05','capabilities':{},'serverInfo':{'name':'cwd','version':'1'}} if m=='initialize' else {'content':[{'type':'text','text':os.getcwd()}]}
 print(json.dumps({'jsonrpc':'2.0','id':q['id'],'result':result}),flush=True)
"#.into()];
    let (first, second) = tokio::join!(
        McpStdioClient::start_and_init_in("python3", &args, None, a.path()),
        McpStdioClient::start_and_init_in("python3", &args, None, b.path())
    );
    let mut first = first.unwrap();
    let mut second = second.unwrap();
    let first_result = first.call_tool("pwd", serde_json::json!({})).await.unwrap();
    let second_result = second
        .call_tool("pwd", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        first_result["content"][0]["text"],
        a.path().to_str().unwrap()
    );
    assert_eq!(
        second_result["content"][0]["text"],
        b.path().to_str().unwrap()
    );
    first.close().await.unwrap();
    second.close().await.unwrap();
    assert_eq!(std::env::current_dir().unwrap(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_running_checker_blocks_session() {
    use dustagent::application::validation::{ValidationConfig, ValidationMode};
    let directory = tempfile::tempdir().unwrap();
    let mut manifest = AppManifest::new();
    manifest.validation = Some(ValidationConfig {
        command: "python3".into(),
        args: vec![
            "-c".into(),
            "import pathlib,time; pathlib.Path('started').write_text('yes'); time.sleep(30)".into(),
        ],
        timeout_ms: 60000,
        mode: ValidationMode::Legacy,
    });
    let (tx, rx) = watch::channel(false);
    let mut core = DustCore::new(manifest, provider(vec![LlmResponse::text("candidate")]))
        .with_cancellation(rx)
        .with_working_directory(directory.path())
        .unwrap();
    let mut session = AgentSession::new();
    let cancel = async {
        for _ in 0..500 {
            if directory.path().join("started").exists() {
                tx.send(true).unwrap();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("checker did not start");
    };
    let (report, ()) = tokio::join!(core.prompt_report(&mut session, "validate"), cancel);
    assert_eq!(report.stop_reason, StopReason::Cancelled);
    assert!(session.is_blocked());
}

#[tokio::test]
async fn unknown_tool_transport_error_blocks_session_without_checkpoint() {
    use dustagent::ports::mcp::{McpClient, McpTool};
    struct Broken;
    #[async_trait]
    impl McpClient for Broken {
        async fn initialize(&mut self) -> dustagent::Result<()> {
            Ok(())
        }
        async fn list_tools(&mut self) -> dustagent::Result<Vec<McpTool>> {
            Ok(vec![McpTool::new(
                "write",
                None,
                serde_json::json!({"type":"object"}),
            )])
        }
        async fn call_tool(
            &mut self,
            _: &str,
            _: serde_json::Value,
        ) -> dustagent::Result<serde_json::Value> {
            Err(dustagent::DustError::Mcp(
                "connection lost after dispatch".into(),
            ))
        }
        async fn close(&mut self) -> dustagent::Result<()> {
            Ok(())
        }
    }
    let model = provider(vec![LlmResponse::tool_calls(vec![ToolCall::new(
        "write1",
        "remote__write",
        "{}",
    )])]);
    let seen = model.seen.clone();
    let mut core =
        DustCore::new(AppManifest::new(), model).with_mcp_client("remote", Box::new(Broken));
    let mut session = AgentSession::new();
    let report = core.prompt_report(&mut session, "write").await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert!(session.is_blocked());
    assert_eq!(
        core.prompt_report(&mut session, "retry").await.stop_reason,
        StopReason::ExecutionError
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}
