use dustagent::application::execution::ToolRecord;
use dustagent::{
    AppManifest, ChatMessage, Checkpoint, CheckpointPhase, ExecutionReport, ToolEffect, ToolStatus,
};
use std::process::Command;

#[test]
fn reconcile_command_records_observed_result_without_running_tools() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = AppManifest::new().with_system_prompt("fixture prompt");
    let checkpoint_path = directory.path().join("state.json");
    let result_path = directory.path().join("observed.txt");
    let mut checkpoint = Checkpoint::new_in(
        &manifest,
        "send request",
        vec![
            ChatMessage::system("fixture prompt"),
            ChatMessage::user("send request"),
            ChatMessage::assistant(
                None,
                Some(vec![dustagent::ToolCall::new(
                    "call-1",
                    "remote__send",
                    "{}",
                )]),
            ),
            ChatMessage::tool("call-1", "transport outcome unknown"),
        ],
        ExecutionReport::default(),
        CheckpointPhase::Blocked,
        directory.path(),
    )
    .unwrap();
    checkpoint.report.tool_calls.push(ToolRecord {
        turn: 1,
        call_id: "call-1".into(),
        name: "remote__send".into(),
        arguments: serde_json::json!({}),
        effect: ToolEffect::ExternalUnknown,
        operation_id: Some("stable-operation-id".into()),
        status: ToolStatus::TimedOut,
        elapsed_ms: 100,
        output: None,
        error: Some("Tool deadline exceeded".into()),
        truncated: false,
    });
    dustagent::application::checkpoint::save(&checkpoint_path, &checkpoint).unwrap();
    std::fs::write(&result_path, "Observed externally: accepted").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dust"))
        .current_dir(directory.path())
        .args([
            "reconcile",
            checkpoint_path.to_str().unwrap(),
            "--call-id",
            "call-1",
            "--result-file",
            result_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("No MCP tool was executed"));
    let saved = dustagent::application::checkpoint::load(&checkpoint_path).unwrap();
    assert_eq!(saved.phase, CheckpointPhase::Ready);
    assert_eq!(saved.report.tool_calls[0].status, ToolStatus::Reconciled);
    assert_eq!(
        saved.messages.last().unwrap().content.as_deref(),
        Some("Observed externally: accepted")
    );
}
