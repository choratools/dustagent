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
