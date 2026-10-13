#![cfg(unix)]

use async_trait::async_trait;
use dustagent::application::package;
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolCall,
    ToolDefinition, ToolStatus,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
};

type CapturedCalls = Arc<Mutex<Vec<(Vec<ChatMessage>, Vec<ToolDefinition>)>>>;

struct Provider {
    replies: Mutex<VecDeque<LlmResponse>>,
    calls: CapturedCalls,
}
impl Provider {
    fn new(replies: Vec<LlmResponse>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            calls: Arc::default(),
        }
    }
}
#[async_trait]
impl LlmProvider for Provider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        self.calls
            .lock()
            .unwrap()
            .push((messages.to_vec(), tools.unwrap_or_default().to_vec()));
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call"))
    }
}
fn is_tool_error(result: dustagent::Result<String>) -> bool {
    match result {
        Err(_) => true,
        Ok(output) => serde_json::from_str::<Value>(&output)
            .ok()
            .is_some_and(|value| value["isError"] == true),
    }
}
fn fixture(root: &Path, script: &str) -> AppManifest {
    std::fs::create_dir_all(root.join("tools")).unwrap();
    std::fs::write(root.join("tools/echo.sh"), script).unwrap();
    std::fs::write(root.join("tools/undeclared.sh"), "printf 'SECRET'").unwrap();
    let manifest = json!({
        "package":{"name":"tool-fixture","version":"2.0.0"},
        "system_prompt":"Use the declared tool.", "max_turns":4, "mcp_servers":{},
        "tools":{"echo":{"description":"Echo JSON", "input_schema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}, "command":"sh", "entrypoint":"tools/echo.sh"}}
    });
    std::fs::write(root.join("app.json"), manifest.to_string()).unwrap();
    AppManifest::from_json_str(&manifest.to_string()).unwrap()
}
fn core(root: &Path, manifest: AppManifest) -> DustCore<Provider> {
    DustCore::new(manifest, Provider::new(vec![]))
        .with_app_resources(root, None)
        .unwrap()
}

#[tokio::test]
async fn declared_tool_executes_with_json_stdin_and_app_cwd_only() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(
        dir.path(),
        "test -f app.json || exit 8\ncat > received.json\ncat received.json\n",
    );
    let mut core = core(dir.path(), manifest);
    let definitions = core.gather_mcp_tools().await.unwrap();
    assert_eq!(
        definitions
            .iter()
            .filter(|t| t.name.starts_with("app__"))
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["app__echo"]
    );
    assert!(is_tool_error(
        core.execute_tool("app__undeclared", json!({})).await
    ));
    let output = core
        .execute_tool("app__echo", json!({"value":"hello"}))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({"value":"hello"})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(dir.path().join("received.json")).unwrap())
            .unwrap(),
        json!({"value":"hello"})
    );
}

#[tokio::test]
async fn schema_rejection_prevents_subprocess_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "touch launched\nprintf '{}'\n");
    let mut core = core(dir.path(), manifest);
    assert!(is_tool_error(
        core.execute_tool("app__echo", json!({"value":42})).await
    ));
    assert!(!dir.path().join("launched").exists());
}

#[tokio::test]
async fn malformed_multiple_json_and_nonzero_outputs_are_errors() {
    for script in ["printf 'not-json'", "printf '{} {}'", "printf '{}'\nexit 9"] {
        let dir = tempfile::tempdir().unwrap();
        let manifest = fixture(dir.path(), script);
        assert!(
            is_tool_error(
                core(dir.path(), manifest)
                    .execute_tool("app__echo", json!({"value":"x"}))
                    .await
            ),
            "script accepted: {script}"
        );
    }
}

#[tokio::test]
async fn local_tool_failure_is_returned_to_model_for_repair() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "cat\n");
    let provider = Provider::new(vec![
        LlmResponse::tool_calls(vec![ToolCall::new("bad", "app__echo", r#"{"value":4}"#)]),
        LlmResponse::tool_calls(vec![ToolCall::new(
            "fixed",
            "app__echo",
            r#"{"value":"fixed"}"#,
        )]),
        LlmResponse::text("done"),
    ]);
    let calls = provider.calls.clone();
    let mut core = DustCore::new(manifest, provider)
        .with_app_resources(dir.path(), None)
        .unwrap();
    let report = core.execute_report("echo a string").await;
    assert_eq!(
        report.stop_reason,
        StopReason::Completed,
        "report={report:?}"
    );
    assert_eq!(report.tool_calls.len(), 2);
    assert_ne!(report.tool_calls[0].status, ToolStatus::Succeeded);
    assert_eq!(report.tool_calls[1].status, ToolStatus::Succeeded);
    let captured = calls.lock().unwrap();
    assert!(captured[0].1.iter().any(|t| t.name == "app__echo"));
    assert!(captured[1].0.iter().any(|m| m.role == "tool"
        && m.tool_call_id.as_deref() == Some("bad")
        && m.content.as_deref().is_some_and(|s| !s.is_empty())));
    assert!(
        captured[2]
            .0
            .iter()
            .any(|m| m.role == "tool" && m.content.as_deref().is_some_and(|s| s.contains("fixed")))
    );
}

#[tokio::test]
async fn app_tool_timeout_uses_standard_runtime_report() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "sleep 10\nprintf '{}'\n");
    let provider = Provider::new(vec![LlmResponse::tool_calls(vec![ToolCall::new(
        "slow",
        "app__echo",
        r#"{"value":"x"}"#,
    )])]);
    let mut core = DustCore::new(manifest, provider)
        .with_app_resources(dir.path(), None)
        .unwrap()
        .with_timeouts(5000, 50);
    let report = core.execute_report("run").await;
    assert_eq!(
        report.stop_reason,
        StopReason::ToolTimeout,
        "report={report:?}"
    );
    assert_eq!(report.tool_calls[0].status, ToolStatus::TimedOut);
}

#[tokio::test]
async fn tools_and_skills_work_from_plain_and_encrypted_archives_and_installations() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("source");
    fixture(&root, "cat\n");
    std::fs::create_dir_all(root.join("skills/reader/references")).unwrap();
    std::fs::write(
        root.join("skills/reader/SKILL.md"),
        "---\nname: reader\ndescription: Use echo\n---\nUse app__echo and references/help.md.",
    )
    .unwrap();
    std::fs::write(
        root.join("skills/reader/references/help.md"),
        "tool co-packaged",
    )
    .unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(root.join("app.json")).unwrap()).unwrap();
    manifest["skills"] = json!(["reader"]);
    std::fs::write(root.join("app.json"), manifest.to_string()).unwrap();
    let plain = package::pack(&root, Some(&dir.path().join("plain.dustpkg"))).unwrap();
    let encrypted = package::pack_encrypted(
        &root,
        Some(&dir.path().join("encrypted.dustpkg")),
        b"test-secret-password",
    )
    .unwrap();
    let installed = package::install(&plain, &dir.path().join("plain-store")).unwrap();
    let encrypted_installed =
        package::install(&encrypted, &dir.path().join("encrypted-store")).unwrap();
    for (path, private) in [
        (&plain, false),
        (&encrypted, true),
        (&installed, false),
        (&encrypted_installed, true),
    ] {
        let loaded = if private {
            package::load_with_password(path.to_str().unwrap(), dir.path(), b"test-secret-password")
        } else {
            package::load(path.to_str().unwrap(), dir.path())
        }
        .unwrap();
        let mut core = core(&loaded.root, loaded.manifest);
        let output = core
            .execute_tool("app__echo", json!({"value":"packaged"}))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap(),
            json!({"value":"packaged"})
        );
        let reference = core
            .execute_tool(
                "dustagent__read_skill",
                json!({"skill":"reader","path":"references/help.md"}),
            )
            .await
            .unwrap();
        assert!(reference.contains("tool co-packaged"));
    }
}

#[tokio::test]
async fn changed_tool_script_rejects_resume_before_model_call() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "cat\n");
    let state = dir.path().join("checkpoint.json");
    let provider = Provider::new(vec![LlmResponse::tool_calls(vec![ToolCall::new(
        "first",
        "app__echo",
        r#"{"value":"saved"}"#,
    )])]);
    let mut initial = DustCore::new(manifest.clone(), provider)
        .with_app_resources(dir.path(), None)
        .unwrap()
        .with_checkpoint(&state)
        .with_max_turns(1);
    assert_eq!(
        initial.execute_report("run").await.stop_reason,
        StopReason::TurnLimit
    );
    let saved = dustagent::application::checkpoint::load(&state).unwrap();
    std::fs::write(
        dir.path().join("tools/echo.sh"),
        "printf '{\"changed\":true}'",
    )
    .unwrap();
    let provider = Provider::new(vec![]);
    let calls = provider.calls.clone();
    let mut resumed = DustCore::new(manifest, provider)
        .with_app_resources(dir.path(), None)
        .unwrap()
        .with_checkpoint(&state);
    assert_eq!(
        resumed.resume_report(&saved).await.stop_reason,
        StopReason::ExecutionError
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn traversal_and_symlink_entrypoints_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "cat\n");
    let mut raw = serde_json::to_value(&manifest).unwrap();
    for entrypoint in ["../escape.sh", "/tmp/escape.sh", "tools/../app.json"] {
        raw["tools"]["echo"]["entrypoint"] = json!(entrypoint);
        assert!(AppManifest::from_json_str(&raw.to_string()).is_err());
    }
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::remove_file(dir.path().join("tools/echo.sh")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("tools/echo.sh")).unwrap();
    assert!(
        DustCore::new(manifest, Provider::new(vec![]))
            .with_app_resources(dir.path(), None)
            .is_err()
    );
}

#[tokio::test]
async fn oversized_stdout_is_rejected_without_hanging_on_open_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(
        dir.path(),
        "printf '\"'\nhead -c 1100000 /dev/zero | tr '\\000' x\nprintf '\"'\n",
    );
    let mut core = core(dir.path(), manifest);
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        core.execute_tool("app__echo", json!({"value":"x"})),
    )
    .await;
    assert!(is_tool_error(outcome.expect("oversized output hung")));
}

#[tokio::test]
async fn cancellation_preserves_unknown_outcome_without_automatic_replay() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "touch started\nexec sleep 10\n");
    let state = dir.path().join("state.json");
    let provider = Provider::new(vec![LlmResponse::tool_calls(vec![ToolCall::new(
        "cancelled",
        "app__echo",
        r#"{"value":"x"}"#,
    )])]);
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    let mut core = DustCore::new(manifest.clone(), provider)
        .with_app_resources(dir.path(), None)
        .unwrap()
        .with_checkpoint(&state)
        .with_cancellation(receiver);
    let marker = dir.path().join("started");
    let cancel_task = tokio::spawn(async move {
        for _ in 0..100 {
            if marker.exists() {
                cancel.send(true).unwrap();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("tool never started");
    });
    let report = core.execute_report("run").await;
    cancel_task.await.unwrap();
    assert_eq!(report.stop_reason, StopReason::Cancelled);
    assert_eq!(report.tool_calls[0].status, ToolStatus::TimedOut);
    let checkpoint = dustagent::application::checkpoint::load(&state).unwrap();
    let provider = Provider::new(vec![]);
    let calls = provider.calls.clone();
    let mut resumed = DustCore::new(manifest, provider)
        .with_app_resources(dir.path(), None)
        .unwrap();
    assert_eq!(
        resumed.resume_report(&checkpoint).await.stop_reason,
        StopReason::ExecutionError
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn app_namespace_cannot_be_claimed_by_mcp_server() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = fixture(dir.path(), "cat\n");
    let mut raw = serde_json::to_value(manifest).unwrap();
    raw["mcp_servers"] = json!({"app":{"command":"sh"}});
    assert!(AppManifest::from_json_str(&raw.to_string()).is_err());
}
