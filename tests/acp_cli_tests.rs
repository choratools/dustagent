use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

type Requests = Arc<Mutex<Vec<Value>>>;
fn serve(replies: Vec<(Value, Duration)>) -> (String, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests: Requests = Arc::default();
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
            let length = String::from_utf8(headers)
                .unwrap()
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
    (format!("http://{address}/v1"), requests)
}
fn final_message(text: &str) -> Value {
    json!({"role":"assistant","content":text})
}
fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
}
fn app(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("app.json");
    std::fs::write(
        &path,
        json!({"name":"fixture","working_state":true,"mcp_servers":{}}).to_string(),
    )
    .unwrap();
    path
}
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<Value>,
}
impl Client {
    fn start(dir: &Path, app: &Path, url: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dust"))
            .current_dir(dir)
            .env("OPENAI_API_KEY", "test-only")
            .env("OPENAI_BASE_URL", url)
            .args([
                "acp",
                app.to_str().unwrap(),
                "--max-turns",
                "6",
                "--timeout-ms",
                "10000",
                "--tool-timeout-ms",
                "5000",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let value: Value =
                    serde_json::from_str(&line).expect("ACP stdout must contain only JSON");
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self { child, stdin, rx }
    }
    fn send(&mut self, value: Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{value}").unwrap();
    }
    fn response(&self, id: Value) -> (Value, Vec<Value>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = Vec::new();
        loop {
            let value = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("bounded ACP response");
            if value.get("id") == Some(&id) {
                return (value, events);
            }
            events.push(value);
        }
    }
    fn initialize(&mut self) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"test","version":"1"}}}));
        let (value, _) = self.response(json!(1));
        assert!(value.get("error").is_none(), "{value}");
        value
    }
    fn session(&mut self, dir: &Path, id: Value) -> String {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":{"cwd":dir,"mcpServers":[]}}));
        let (value, _) = self.response(id);
        value["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{value}"))
            .into()
    }
    fn prompt(&mut self, session: &str, id: Value, content: Vec<Value>) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":session,"prompt":content}}));
    }
    fn close(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "ACP did not exit on EOF");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn text(value: &str) -> Value {
    json!({"type":"text","text":value})
}
#[test]
fn negotiation_request_validation_and_client_mcp_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let mut client = Client::start(dir.path(), &app, "http://127.0.0.1:1/v1");
    client.send(json!({"jsonrpc":"2.0","id":"early","method":"session/new","params":{"cwd":dir.path(),"mcpServers":[]}}));
    assert!(client.response(json!("early")).0.get("error").is_some());
    let init = client.initialize();
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], false);
    assert_eq!(
        init["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        false
    );
    writeln!(client.stdin.as_mut().unwrap(), "{{invalid json").unwrap();
    let malformed = client.rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(malformed.get("error").is_some());
    let marker = dir.path().join("unauthorized");
    client.send(json!({"jsonrpc":"2.0","id":"mcp","method":"session/new","params":{"cwd":dir.path(),"mcpServers":[{"name":"rogue","command":"touch","args":[marker],"env":[]}]}}));
    assert!(client.response(json!("mcp")).0.get("error").is_some());
    assert!(!marker.exists());
    let session = client.session(dir.path(), json!("new"));
    for (id, method, params) in [
        (
            10,
            "session/load",
            json!({"sessionId":session,"cwd":dir.path(),"mcpServers":[]}),
        ),
        (
            11,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"image","data":"AA==","mimeType":"image/png"}]}),
        ),
        (
            12,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"resource","resource":{"uri":"file:///etc/passwd","text":"x","mimeType":"text/plain"}}]}),
        ),
    ] {
        client.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        assert!(client.response(json!(id)).0.get("error").is_some());
    }
    client.close();
}
#[test]
fn conversation_state_and_resource_links_survive_only_in_their_session() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let (url, requests) = serve(vec![
        (
            call(
                "put",
                "dustagent__state_put",
                json!({"key":"remember","value":"stored"}),
            ),
            Duration::ZERO,
        ),
        (final_message("first answer"), Duration::ZERO),
        (
            call("get", "dustagent__state_get", json!({"key":"remember"})),
            Duration::ZERO,
        ),
        (final_message("second answer"), Duration::ZERO),
        (
            call(
                "isolated",
                "dustagent__state_get",
                json!({"key":"remember"}),
            ),
            Duration::ZERO,
        ),
        (final_message("isolated answer"), Duration::ZERO),
    ]);
    let mut client = Client::start(dir.path(), &app, &url);
    client.initialize();
    let one = client.session(dir.path(), json!(2));
    let two = client.session(dir.path(), json!(3));
    client.prompt(&one, json!(4), vec![text("original")]);
    let (result, events) = client.response(json!(4));
    assert_eq!(result["result"]["stopReason"], "end_turn");
    assert!(
        events
            .iter()
            .any(|v| v["params"]["update"]["sessionUpdate"] == "tool_call")
    );
    assert!(
        events
            .iter()
            .any(|v| v["params"]["update"]["sessionUpdate"] == "tool_call_update")
    );
    client.prompt(
        &one,
        json!(5),
        vec![
            text("followup"),
            json!({"type":"resource_link","uri":"file:///never/read/private","name":"reference"}),
        ],
    );
    assert_eq!(
        client.response(json!(5)).0["result"]["stopReason"],
        "end_turn"
    );
    client.prompt(&two, json!(6), vec![text("other")]);
    assert_eq!(
        client.response(json!(6)).0["result"]["stopReason"],
        "end_turn"
    );
    let requests = requests.lock().unwrap();
    let continued = requests[2]["messages"].to_string();
    assert!(
        continued.contains("first answer")
            && continued.contains("original")
            && continued.contains("followup")
            && continued.contains("file:///never/read/private")
    );
    assert!(requests[3]["messages"].to_string().contains("stored"));
    let isolated = requests[5]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(isolated.contains("false"));
    assert!(!requests[4]["messages"].to_string().contains("original"));
    client.close();
}
#[test]
fn cancel_model_request_and_eof_are_responsive() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let (url, requests) = serve(vec![(final_message("late"), Duration::from_secs(3))]);
    let mut client = Client::start(dir.path(), &app, &url);
    client.initialize();
    let session = client.session(dir.path(), json!(2));
    client.prompt(&session, json!(3), vec![text("slow")]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    client.prompt(&session, json!(4), vec![text("busy")]);
    assert!(client.response(json!(4)).0.get("error").is_some());
    let started = Instant::now();
    client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}));
    assert_eq!(
        client.response(json!(3)).0["result"]["stopReason"],
        "cancelled"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    client.close();
}
#[test]
fn cancellation_during_tool_blocks_unknown_effect_replay() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let (url, requests) = serve(vec![(
        call("sleep", "dustagent__sleep", json!({"ms":4000})),
        Duration::ZERO,
    )]);
    let mut client = Client::start(dir.path(), &app, &url);
    client.initialize();
    let session = client.session(dir.path(), json!(2));
    client.prompt(&session, json!(3), vec![text("wait")]);
    loop {
        let v = client.rx.recv_timeout(Duration::from_secs(3)).unwrap();
        if v["params"]["update"]["sessionUpdate"] == "tool_call" {
            break;
        }
    }
    client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}));
    assert_eq!(
        client.response(json!(3)).0["result"]["stopReason"],
        "cancelled"
    );
    client.prompt(&session, json!(4), vec![text("again")]);
    assert!(client.response(json!(4)).0.get("error").is_some());
    assert_eq!(requests.lock().unwrap().len(), 1);
    client.close();
}
#[test]
fn archive_package_runs_without_installation() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("pkg");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join("app.json"),
        r#"{"package":{"name":"fixture","version":"1.0.0"},"mcp_servers":{}}"#,
    )
    .unwrap();
    let archive =
        dustagent::application::package::pack(&source, Some(&dir.path().join("fixture.dustpkg")))
            .unwrap();
    let (url, _) = serve(vec![(final_message("archive answer"), Duration::ZERO)]);
    let mut client = Client::start(dir.path(), &archive, &url);
    client.initialize();
    let session = client.session(dir.path(), json!(2));
    client.prompt(&session, json!(3), vec![text("archive")]);
    let (response, events) = client.response(json!(3));
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert!(
        events
            .iter()
            .any(|v| v["params"]["update"]["content"]["text"] == "archive answer")
    );
    client.close();
}
#[test]
fn session_working_directories_are_used_by_real_checker_without_global_change() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let checker = "import os,json,sys; request=json.load(sys.stdin); open('checker-cwd','w').write(os.getcwd()); print(json.dumps({'passed':True,'reason':'cwd observed'}))";
    std::fs::write(&app,json!({"name":"fixture","mcp_servers":{},"validation":{"command":"python3","args":["-c",checker]}}).to_string()).unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let (url, _) = serve(vec![
        (final_message("one"), Duration::ZERO),
        (final_message("two"), Duration::ZERO),
    ]);
    let mut client = Client::start(dir.path(), &app, &url);
    client.initialize();
    let one = client.session(&first, json!(2));
    let two = client.session(&second, json!(3));
    client.prompt(&one, json!(4), vec![text("one")]);
    assert_eq!(
        client.response(json!(4)).0["result"]["stopReason"],
        "end_turn"
    );
    client.prompt(&two, json!(5), vec![text("two")]);
    assert_eq!(
        client.response(json!(5)).0["result"]["stopReason"],
        "end_turn"
    );
    assert_eq!(
        std::fs::read_to_string(first.join("checker-cwd")).unwrap(),
        first.to_str().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(second.join("checker-cwd")).unwrap(),
        second.to_str().unwrap()
    );
    assert!(!dir.path().join("checker-cwd").exists());
    client.close();
}
#[test]
fn eof_during_inflight_model_request_exits_without_waiting_for_model() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let (url, requests) = serve(vec![(final_message("late"), Duration::from_secs(3))]);
    let mut client = Client::start(dir.path(), &app, &url);
    client.initialize();
    let session = client.session(dir.path(), json!(2));
    client.prompt(&session, json!(3), vec![text("slow")]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    client.close();
    assert!(started.elapsed() < Duration::from_secs(2));
}
#[test]
fn cancelled_model_request_allows_followup_without_replaying_tools() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let (url, requests) = serve(vec![
        (final_message("discarded"), Duration::from_millis(150)),
        (final_message("followup accepted"), Duration::ZERO),
    ]);
    let mut client = Client::start(dir.path(), &app, &url);
    client.initialize();
    let session = client.session(dir.path(), json!(2));
    client.prompt(&session, json!(3), vec![text("cancel this")]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}));
    assert_eq!(
        client.response(json!(3)).0["result"]["stopReason"],
        "cancelled"
    );
    client.prompt(&session, json!(4), vec![text("new request")]);
    assert_eq!(
        client.response(json!(4)).0["result"]["stopReason"],
        "end_turn"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1]["messages"].to_string().contains("discarded"));
    client.close();
}
#[test]
fn stdin_half_close_returns_cancelled_prompt_before_stdout_eof() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let (url, requests) = serve(vec![(final_message("late"), Duration::from_secs(3))]);
    let mut client = Client::start(dir.path(), &app, &url);
    client.initialize();
    let session = client.session(dir.path(), json!(2));
    client.prompt(&session, json!("active"), vec![text("in flight")]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    // Keep the stdout reader alive after independently closing the write half.
    client.stdin.take();
    let (response, _) = client.response(json!("active"));
    assert_eq!(response["result"]["stopReason"], "cancelled");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match client
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(value) => assert!(
                value.get("id").is_none(),
                "unexpected extra response: {value}"
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("stdout did not close after cancelled response")
            }
        }
    }
    client.close();
    assert!(started.elapsed() < Duration::from_secs(2));
}
