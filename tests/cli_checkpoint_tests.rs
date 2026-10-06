use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

fn serve(replies: Vec<(Value, Duration)>) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = count.clone();
    let captured = requests.clone();
    std::thread::spawn(move || {
        for (message, delay) in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let text = String::from_utf8(headers).unwrap();
            let length = text
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            observed.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(delay);
            let body = json!({"choices":[{"message":message}]}).to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    (format!("http://{address}/v1"), count, requests)
}
fn manifest(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("app.json");
    std::fs::write(&path, r#"{"name":"fixture","mcp_servers":{}}"#).unwrap();
    path
}
fn command(dir: &std::path::Path, app: &std::path::Path, url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dust"));
    cmd.current_dir(dir)
        .env("OPENAI_API_KEY", "test-only")
        .env("OPENAI_BASE_URL", url)
        .args(["run", app.to_str().unwrap()]);
    cmd
}
fn tool_message() -> Value {
    json!({"role":"assistant","content":"observing", "tool_calls":[{"id":"call1","type":"function","function":{"name":"dustagent__hash","arguments":"{\"input\":\"observed body\"}"}}]})
}

fn announced_checkpoint(output: &std::process::Output) -> std::path::PathBuf {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let path = stderr
        .lines()
        .find_map(|line| line.strip_prefix("[dustagent] checkpoint: "))
        .unwrap_or_else(|| panic!("checkpoint path missing from stderr: {stderr}"));
    let path = std::path::PathBuf::from(path);
    assert!(path.is_absolute());
    path
}

#[test]
fn default_runs_preserve_unique_private_checkpoints_without_changing_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let app = manifest(dir.path());
    let temp_root = dir.path().join("runtime-tmp");
    std::fs::create_dir(&temp_root).unwrap();
    let (url, calls, _) = serve(vec![
        (json!({"role":"assistant","content":"done"}), Duration::ZERO),
        (json!({"role":"assistant","content":"done"}), Duration::ZERO),
    ]);
    let json_output = command(dir.path(), &app, &url)
        .env("TMPDIR", &temp_root)
        .args(["--json", "input"])
        .output()
        .unwrap();
    assert!(json_output.status.success(), "{:?}", json_output);
    let first_path = announced_checkpoint(&json_output);
    let first_report: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(
        first_report["checkpoint_path"],
        first_path.to_str().unwrap()
    );
    let plain_output = command(dir.path(), &app, &url)
        .env("TMPDIR", &temp_root)
        .args(["--report", "plain-report.json", "input"])
        .output()
        .unwrap();
    assert!(plain_output.status.success(), "{:?}", plain_output);
    assert_eq!(
        String::from_utf8(plain_output.stdout.clone())
            .unwrap()
            .trim(),
        "done"
    );
    let second_path = announced_checkpoint(&plain_output);
    let second_report: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("plain-report.json")).unwrap())
            .unwrap();
    assert_eq!(
        second_report["checkpoint_path"],
        second_path.to_str().unwrap()
    );
    assert_ne!(first_path, second_path);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    for path in [first_path, second_path] {
        assert!(path.starts_with(&temp_root));
        assert_eq!(path.file_name().unwrap(), "state.json");
        assert!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("dust-run-")
        );
        let saved = dustagent::application::checkpoint::load(&path).unwrap();
        assert_eq!(saved.phase, dustagent::CheckpointPhase::Finished);
        assert_eq!(saved.report.output.as_deref(), Some("done"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn default_checkpoint_can_resume_in_a_fresh_process_without_replaying_tools() {
    let dir = tempfile::tempdir().unwrap();
    let app = manifest(dir.path());
    let (url, _, _) = serve(vec![(tool_message(), Duration::ZERO)]);
    let first = command(dir.path(), &app, &url)
        .env("TMPDIR", dir.path())
        .args(["--json", "--max-turns", "1", "original automatic input"])
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(2));
    let path = announced_checkpoint(&first);
    let first_report: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first_report["checkpoint_path"], path.to_str().unwrap());
    let (url, _, captured) = serve(vec![(
        json!({"role":"assistant","content":"resumed automatic run"}),
        Duration::ZERO,
    )]);
    let resumed = command(dir.path(), &app, &url)
        .env("TMPDIR", dir.path())
        .args([
            "--json",
            "--resume",
            path.to_str().unwrap(),
            "--max-turns",
            "1",
        ])
        .output()
        .unwrap();
    assert!(resumed.status.success(), "{:?}", resumed);
    let report: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(report["checkpoint_path"], path.to_str().unwrap());
    assert_eq!(report["turns_used"], 2);
    assert_eq!(report["tool_calls"].as_array().unwrap().len(), 1);
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["messages"][1]["content"],
        "original automatic input"
    );
    assert_eq!(requests[0]["messages"][3]["role"], "tool");
    let saved = dustagent::application::checkpoint::load(&path).unwrap();
    assert_eq!(saved.phase, dustagent::CheckpointPhase::Finished);
}
#[test]
fn fresh_process_resumes_original_transcript_with_additional_budget() {
    let dir = tempfile::tempdir().unwrap();
    let app = manifest(dir.path());
    let (url, _, _) = serve(vec![(tool_message(), Duration::ZERO)]);
    let first = command(dir.path(), &app, &url)
        .args([
            "--json",
            "--checkpoint",
            "state.json",
            "--max-turns",
            "1",
            "original input",
        ])
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(2));
    let (url, _, captured) = serve(vec![(
        json!({"role":"assistant","content":"final"}),
        Duration::ZERO,
    )]);
    let resumed = command(dir.path(), &app, &url)
        .args(["--json", "--resume", "state.json", "--max-turns", "1"])
        .output()
        .unwrap();
    assert_eq!(resumed.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(report["turns_used"], 2);
    assert_eq!(report["tool_calls"].as_array().unwrap().len(), 1);
    let requests = captured.lock().unwrap();
    assert_eq!(requests[0]["messages"][1]["content"], "original input");
    assert_eq!(requests[0]["messages"][3]["role"], "tool");
    let checkpoint =
        dustagent::application::checkpoint::load(dir.path().join("state.json")).unwrap();
    assert_eq!(checkpoint.phase, dustagent::CheckpointPhase::Finished);
    let rejected = command(dir.path(), &app, "http://127.0.0.1:1/v1")
        .args(["--json", "--resume", "state.json"])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
}
#[test]
fn concurrent_cli_lease_rejects_before_startup() {
    let dir = tempfile::tempdir().unwrap();
    let app = manifest(dir.path());
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dir.path().join(".state.json.lock"))
        .unwrap();
    lease.try_lock().unwrap();
    let output = command(dir.path(), &app, "http://127.0.0.1:1/v1")
        .args(["--checkpoint", "state.json", "input"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("already in use"));
    assert!(!dir.path().join("state.json").exists());
}
#[test]
fn completed_checkpoint_is_not_overwritten_by_fresh_run_or_report() {
    let dir = tempfile::tempdir().unwrap();
    let app = manifest(dir.path());
    let (url, _, _) = serve(vec![(
        json!({"role":"assistant","content":"done"}),
        Duration::ZERO,
    )]);
    let output = command(dir.path(), &app, &url)
        .args(["--checkpoint", "state.json", "input"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let before = std::fs::read(dir.path().join("state.json")).unwrap();
    let output = command(dir.path(), &app, "http://127.0.0.1:1/v1")
        .args(["--checkpoint", "state.json", "input"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read(dir.path().join("state.json")).unwrap(),
        before
    );
    let output = command(dir.path(), &app, "http://127.0.0.1:1/v1")
        .args(["--resume", "state.json", "--report", "./state.json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read(dir.path().join("state.json")).unwrap(),
        before
    );
}
#[test]
fn process_kill_releases_lease_and_preserves_safe_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let app = manifest(dir.path());
    let (url, calls, _) = serve(vec![
        (tool_message(), Duration::ZERO),
        (
            json!({"role":"assistant","content":"late"}),
            Duration::from_secs(3),
        ),
    ]);
    let mut child = command(dir.path(), &app, &url)
        .args(["--checkpoint", "state.json", "original input"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while calls.load(Ordering::SeqCst) < 2 {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    let path = dir.path().join("state.json");
    let saved = dustagent::application::checkpoint::load(&path).unwrap();
    assert_eq!(saved.phase, dustagent::CheckpointPhase::Ready);
    assert_eq!(saved.report.tool_calls.len(), 1);
    child.kill().unwrap();
    child.wait().unwrap();
    let (url, _, _) = serve(vec![(
        json!({"role":"assistant","content":"resumed"}),
        Duration::ZERO,
    )]);
    let output = command(dir.path(), &app, &url)
        .args(["--json", "--resume", "state.json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["output"], "resumed");
    assert_eq!(report["tool_calls"].as_array().unwrap().len(), 1);
}

#[test]
fn completed_cli_checkpoint_accepts_followup_without_replaying_history() {
    let dir = tempfile::tempdir().unwrap();
    let app = manifest(dir.path());
    let (url, _, _) = serve(vec![(
        json!({"role":"assistant","content":"old answer"}),
        Duration::ZERO,
    )]);
    let first = command(dir.path(), &app, &url)
        .args(["--checkpoint", "state.json", "original task"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let (url, _, captured) = serve(vec![(
        json!({"role":"assistant","content":"fixed answer"}),
        Duration::ZERO,
    )]);
    let next = command(dir.path(), &app, &url)
        .args([
            "--json",
            "--resume",
            "state.json",
            "--max-turns",
            "1",
            "Return JSON instead",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    let report: Value = serde_json::from_slice(&next.stdout).unwrap();
    assert_eq!(report["output"], "fixed answer");
    assert_eq!(report["turns_used"], 2);
    let requests = captured.lock().unwrap();
    let messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(messages[1]["content"], "original task");
    assert_eq!(messages[2]["content"], "old answer");
    assert_eq!(messages.last().unwrap()["content"], "Return JSON instead");
    let saved = dustagent::application::checkpoint::load(dir.path().join("state.json")).unwrap();
    assert_eq!(saved.user_input, "original task");
    assert_eq!(
        saved
            .messages
            .iter()
            .filter(|m| m.content.as_deref() == Some("Return JSON instead"))
            .count(),
        1
    );
    let rejected = command(dir.path(), &app, "http://127.0.0.1:1/v1")
        .args(["--resume", "state.json"])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
}

#[test]
fn ready_cli_checkpoint_appends_followup_after_observed_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let app = manifest(dir.path());
    let (url, _, _) = serve(vec![(tool_message(), Duration::ZERO)]);
    let first = command(dir.path(), &app, &url)
        .args([
            "--checkpoint",
            "state.json",
            "--max-turns",
            "1",
            "original task",
        ])
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(2));
    let (url, _, captured) = serve(vec![(
        json!({"role":"assistant","content":"corrected"}),
        Duration::ZERO,
    )]);
    let next = command(dir.path(), &app, &url)
        .args([
            "--json",
            "--resume",
            "state.json",
            "--max-turns",
            "1",
            "Use JSON",
        ])
        .output()
        .unwrap();
    assert!(next.status.success());
    let report: Value = serde_json::from_slice(&next.stdout).unwrap();
    assert_eq!(report["tool_calls"].as_array().unwrap().len(), 1);
    let requests = captured.lock().unwrap();
    let messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(messages[messages.len() - 2]["role"], "tool");
    assert_eq!(messages.last().unwrap()["content"], "Use JSON");
}
