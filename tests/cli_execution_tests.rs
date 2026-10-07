use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

fn endpoint(message: Value, delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        let text = String::from_utf8(header).unwrap();
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
        std::thread::sleep(delay);
        let response = json!({
            "choices": [{"message": message}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 2,
                "total_tokens": 12,
                "prompt_tokens_details": {"cached_tokens": 4},
                "completion_tokens_details": {"reasoning_tokens": 1}
            }
        })
        .to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
    });
    format!("http://{address}/v1")
}
fn run(
    message: Value,
    delay: Duration,
    extra: &[&str],
) -> (std::process::Output, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("app.json");
    std::fs::write(&manifest, r#"{"name":"fixture","mcp_servers":{}}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dust"))
        .current_dir(directory.path())
        .env("OPENAI_API_KEY", "local-test-only")
        .env("OPENAI_BASE_URL", endpoint(message, delay))
        .args(["run", manifest.to_str().unwrap()])
        .args(extra)
        .arg("task")
        .output()
        .unwrap();
    (output, directory)
}
fn partial_message() -> Value {
    json!({"role":"assistant","content":"Partial", "tool_calls":[{"id":"call1","type":"function","function":{"name":"dustagent__hash","arguments":"{\"input\":\"body\"}"}}]})
}
#[test]
fn incomplete_json_report_and_atomic_file_preserve_evidence() {
    let (output, directory) = run(
        partial_message(),
        Duration::ZERO,
        &["--json", "--report", "reports/run.json", "--max-turns", "1"],
    );
    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["stop_reason"], "turn_limit");
    assert_eq!(report["output"], "Partial");
    assert_eq!(report["token_usage"]["input_tokens"], 10);
    assert_eq!(report["token_usage"]["cached_input_tokens"], 4);
    assert_eq!(report["token_usage"]["uncached_input_tokens"], 6);
    assert_eq!(report["token_usage"]["output_tokens"], 2);
    assert_eq!(report["token_usage"]["reasoning_tokens"], 1);
    assert_eq!(report["token_usage"]["total_tokens"], 12);
    assert_eq!(report["tool_calls"][0]["status"], "succeeded");
    assert!(report["tool_calls"][0]["output"].is_string());
    let saved: Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("reports/run.json")).unwrap())
            .unwrap();
    assert_eq!(saved, report);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.path().join("reports/run.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[test]
fn incomplete_raw_mode_cannot_feed_partial_text_into_pipeline() {
    let (output, _) = run(partial_message(), Duration::ZERO, &["--max-turns", "1"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TurnLimit"));
}
#[test]
fn completed_and_empty_output_have_different_exit_codes() {
    for (content, code, reason) in [("result", 0, "completed"), ("", 4, "empty_response")] {
        let (output, _) = run(
            json!({"role":"assistant","content":content}),
            Duration::ZERO,
            &["--json"],
        );
        assert_eq!(output.status.code(), Some(code));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["stop_reason"], reason);
    }
}
#[test]
fn slow_provider_returns_timeout_report() {
    let (output, _) = run(
        json!({"role":"assistant","content":"late"}),
        Duration::from_secs(2),
        &["--json", "--timeout-ms", "200"],
    );
    assert_eq!(output.status.code(), Some(3));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["stop_reason"], "time_limit");
    assert_eq!(report["turns_used"], 1);
}
#[test]
fn report_file_failure_does_not_hide_json_evidence() {
    let (output, directory) = run(
        partial_message(),
        Duration::ZERO,
        &["--json", "--report", ".", "--max-turns", "1"],
    );
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["stop_reason"], "turn_limit");
    assert!(report["tool_calls"][0]["output"].is_string());
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("not saved"))
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}
