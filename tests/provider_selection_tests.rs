//! CLI provider selection uses isolated synthetic credential caches; never real account tokens.
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct Fixture {
    dir: tempfile::TempDir,
}
impl Fixture {
    fn new(model: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            json!({"name":"provider-fixture","default_model":model,"mcp_servers":{}}).to_string(),
        )
        .unwrap();
        Self { dir }
    }
    fn auth(&self, content: &str) {
        std::fs::write(self.dir.path().join("auth.json"), content).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dust"));
        command
            .current_dir(self.dir.path())
            .env("CODEX_HOME", self.dir.path())
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        command
    }
    fn run(&self) -> Command {
        let mut command = self.command();
        command.args(["run", "app.json", "--timeout-ms", "1000", "hello"]);
        command
    }
}

fn mock(content: String) -> (String, mpsc::Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
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
        let length: usize = String::from_utf8(headers)
            .unwrap()
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        tx.send(serde_json::from_slice(&body).unwrap()).unwrap();
        let body =
            json!({"choices":[{"message":{"role":"assistant","content":content}}]}).to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    (url, rx)
}
fn error(output: Output, expected: &str) {
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(expected), "unexpected stderr: {stderr}");
    assert!(!stderr.contains("SYNTHETIC_SECRET"));
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("SYNTHETIC_SECRET")
    );
}

#[test]
fn explicit_api_settings_take_precedence_over_malformed_codex_cache() {
    let fixture = Fixture::new(None);
    fixture.auth("malformed SYNTHETIC_SECRET");
    let (url, requests) = mock("API selected".into());
    let output = fixture
        .run()
        .env("OPENAI_API_KEY", "test-only")
        .env("OPENAI_BASE_URL", url)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "API selected"
    );
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(2)).unwrap()["model"],
        "gpt-4o-mini"
    );
}

#[test]
fn base_url_without_key_does_not_fall_back_to_codex_or_contact_server() {
    let fixture = Fixture::new(None);
    fixture.auth(&json!({"auth_mode":"chatgpt","tokens":{"access_token":"SYNTHETIC_SECRET","account_id":"fake"}}).to_string());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    error(
        fixture
            .run()
            .env(
                "OPENAI_BASE_URL",
                format!("http://{}", listener.local_addr().unwrap()),
            )
            .output()
            .unwrap(),
        "OPENAI_API_KEY is missing",
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn missing_and_malformed_cache_errors_are_actionable_and_redacted() {
    let fixture = Fixture::new(None);
    error(fixture.run().output().unwrap(), "codex login");
    fixture.auth("{SYNTHETIC_SECRET");
    error(fixture.run().output().unwrap(), "JSON");
}

#[test]
fn incomplete_and_unsupported_cached_auth_fail_before_network() {
    let fixture = Fixture::new(None);
    fixture.auth(
        &json!({"auth_mode":"unsupported","tokens":{"access_token":"SYNTHETIC_SECRET"}})
            .to_string(),
    );
    error(fixture.run().output().unwrap(), "OAuth");
    fixture.auth(
        &json!({"OPENAI_API_KEY":{},"tokens":{"access_token":"SYNTHETIC_SECRET"}}).to_string(),
    );
    error(fixture.run().output().unwrap(), "account ID");
}

#[test]
fn empty_explicit_settings_fail_instead_of_using_cached_credentials() {
    let fixture = Fixture::new(None);
    fixture.auth("SYNTHETIC_SECRET");
    for name in ["OPENAI_API_KEY", "OPENAI_BASE_URL"] {
        error(
            fixture.run().env(name, "  ").output().unwrap(),
            &format!("{name} is set but empty"),
        );
    }
}

#[test]
fn manifest_model_and_cli_override_reach_actual_api_request() {
    let fixture = Fixture::new(Some("manifest-model"));
    for (override_model, expected) in [(None, "manifest-model"), (Some("cli-model"), "cli-model")] {
        let (url, requests) = mock("ok".into());
        let mut command = fixture.command();
        command
            .env("OPENAI_API_KEY", "test-only")
            .env("OPENAI_BASE_URL", url)
            .args(["run", "app.json"]);
        if let Some(model) = override_model {
            command.args(["--model", model]);
        }
        let output = command
            .args(["--timeout-ms", "1000", "hello"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            requests.recv_timeout(Duration::from_secs(2)).unwrap()["model"],
            expected
        );
    }
}

#[test]
fn scaffold_request_tells_model_which_provider_model_to_generate() {
    let fixture = Fixture::new(None);
    let (url, requests) = mock(
        json!({"name":"generated","default_model":"scaffold-model","mcp_servers":{}}).to_string(),
    );
    let output = fixture
        .command()
        .env("OPENAI_API_KEY", "test-only")
        .env("OPENAI_BASE_URL", url)
        .args([
            "new",
            "generated",
            "generate concise answers",
            "--stdout",
            "--model",
            "scaffold-model",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request["model"], "scaffold-model");
    assert!(
        request["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Use default_model=scaffold-model")
    );
    let manifest: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(manifest["default_model"], "scaffold-model");
}
