//! Subprocess coverage for the context/archive boundary; no remote services.
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Requests = Arc<Mutex<Vec<Value>>>;
fn serve(replies: Vec<Value>) -> (String, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests: Requests = Arc::default();
    let captured = requests.clone();
    std::thread::spawn(move || {
        for message in replies {
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
fn answer(content: &str) -> Value {
    json!({"role":"assistant","content":content})
}
fn call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]})
}
fn app(dir: &Path, enabled: bool) -> PathBuf {
    let script = r#"import sys,json
for line in sys.stdin:
 q=json.loads(line)
 if 'id' not in q: continue
 m=q['method']
 if m=='initialize': r={'protocolVersion':'2024-11-05','capabilities':{},'serverInfo':{'name':'large','version':'1'}}
 elif m=='tools/list': r={'tools':[{'name':'large','description':'large fixture result','inputSchema':{'type':'object','properties':{}}}]}
 else: r={'content':[{'type':'text','text':'x'*100000+'ORIGINAL_BEYOND_64K'}]}
 print(json.dumps({'jsonrpc':'2.0','id':q['id'],'result':r}),flush=True)
"#;
    let path = dir.join("app.json");
    std::fs::write(&path,json!({"name":"compact-fixture","system_prompt":"Keep the user's exact request.","mcp_servers":{"fixture":{"command":"python3","args":["-u","-c",script]}},"compaction":{"enabled":enabled,"trigger_tokens":24000,"keep_recent_messages":2,"max_summary_bytes":1024}}).to_string()).unwrap();
    path
}
fn run(dir: &Path, app: &Path, url: &str, input: &str) -> (Value, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_dust"))
        .current_dir(dir)
        .env("OPENAI_API_KEY", "test-only")
        .env("OPENAI_BASE_URL", url)
        .env("DUST_HISTORY_HOME", dir.join("history"))
        .args([
            "run",
            app.to_str().unwrap(),
            "--json",
            "--max-turns",
            "12",
            "--timeout-ms",
            "10000",
            input,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (report, output.status.success())
}
fn archive(report: &Value) -> String {
    let path = report["transcript"]["path"]
        .as_str()
        .expect("report references original archive");
    std::fs::read_to_string(path).unwrap()
}
#[test]
fn disabled_compaction_preserves_untruncated_original_without_summary_call() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), false);
    let (url, requests) = serve(vec![
        call("large", "fixture__large", json!({})),
        answer("done"),
    ]);
    let (report, success) = run(dir.path(), &app, &url, "retain original");
    assert!(success, "{report}");
    assert_eq!(report["stop_reason"], "completed");
    assert!(report["compactions"].as_array().is_none_or(Vec::is_empty));
    assert!(archive(&report).contains("ORIGINAL_BEYOND_64K"));
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.get("tools").is_some())
    );
}

#[test]
fn compact_keeps_request_and_safe_tool_batches_and_native_history_recovers_original() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), true);
    let (url, requests) = serve(vec![
        call("large", "fixture__large", json!({})),
        call("time", "dustagent__timestamp", json!({"format":"both"})),
        call("large-two", "fixture__large", json!({})),
        call("large-three", "fixture__large", json!({})),
        call("large-again", "fixture__large", json!({})),
        answer(""),
        answer(
            "Earlier tool returned a long fixture result. The archive holds its original at index 4.",
        ),
        call(
            "directory",
            "dustagent__history_directory",
            json!({"limit":1}),
        ),
        call(
            "search",
            "dustagent__history_search",
            json!({"query":"ORIGINAL_BEYOND_64K"}),
        ),
        call(
            "read",
            "dustagent__history_read",
            json!({"index":4,"offset":99500,"limit":1024}),
        ),
        answer("original recovered"),
    ]);
    let input = "USER_REQUEST_MUST_SURVIVE: recover the exact archived marker";
    let (report, success) = run(dir.path(), &app, &url, input);
    assert!(success, "{report}");
    assert_eq!(report["output"], "original recovered");
    assert_eq!(report["compactions"].as_array().unwrap().len(), 1);
    let record = &report["compactions"][0];
    assert!(
        record["estimated_tokens_after"].as_u64().unwrap()
            < record["estimated_tokens_before"].as_u64().unwrap()
    );
    assert!(archive(&report).contains("ORIGINAL_BEYOND_64K"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 11);
    assert!(
        requests[5].get("tools").is_none(),
        "summary must not dispatch tools"
    );
    assert!(
        requests[5]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Produce continuation notes")
    );
    assert!(
        requests[6].get("tools").is_none(),
        "retry is also tool-less"
    );
    assert!(
        requests[6]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("empty summary")
    );
    assert_eq!(report["compaction_attempts"][0]["status"], "empty");
    assert_eq!(report["compaction_attempts"][1]["status"], "accepted");
    let reduced = &requests[7]["messages"];
    assert!(reduced.to_string().contains("history_directory"));
    let directory_reply = requests[8]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "directory")
        .unwrap();
    let directory: Value =
        serde_json::from_str(directory_reply["content"].as_str().unwrap()).unwrap();
    assert_eq!(directory["isError"], false);
    assert_eq!(directory["result"]["count"], 1);
    assert!(
        !directory["result"]["entries"][0]["keywords"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        reduced
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "user" && message["content"] == input)
    );
    assert!(reduced.to_string().len() < 100000);
    // The currently retained tool batch is complete: latest large call and reply.
    assert!(
        reduced
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["tool_call_id"] == "large-again")
    );
    assert!(
        requests[9]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == "search")
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_BEYOND_64K"),
        "search returns matching original snippets"
    );
    assert!(
        requests[10]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == "read")
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_BEYOND_64K"),
        "read returns original beyond report truncation"
    );
}
#[test]
fn rejected_empty_compaction_preserves_full_original_and_reports_failure() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), true);
    let (url, requests) = serve(vec![
        call("large", "fixture__large", json!({})),
        call("time", "dustagent__timestamp", json!({"format":"both"})),
        call("large-two", "fixture__large", json!({})),
        call("large-three", "fixture__large", json!({})),
        call("large-again", "fixture__large", json!({})),
        answer(""),
        answer(""),
        answer(""),
    ]);
    let (report, success) = run(
        dir.path(),
        &app,
        &url,
        "empty summary must preserve original",
    );
    assert!(!success, "{report}");
    assert_ne!(report["stop_reason"], "completed");
    assert!(report["compactions"].as_array().is_none_or(Vec::is_empty));
    assert!(archive(&report).contains("ORIGINAL_BEYOND_64K"));
    assert_eq!(requests.lock().unwrap().len(), 8);
}

#[test]
fn acp_history_is_session_scoped_and_cannot_read_a_caller_chosen_path() {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), false);
    let external = dir.path().join("external.txt");
    std::fs::write(&external, "EXTERNAL_FILE_MUST_NOT_BE_READ").unwrap();
    let (url, requests) = serve(vec![
        answer("session one answer"),
        call(
            "search",
            "dustagent__history_search",
            json!({"query":"SESSION_ONE_PRIVATE_MARKER"}),
        ),
        call(
            "bad-path",
            "dustagent__history_read",
            json!({"index":1,"path":external}),
        ),
        answer("session two answer"),
    ]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_dust"))
        .current_dir(dir.path())
        .env("OPENAI_API_KEY", "test-only")
        .env("OPENAI_BASE_URL", url)
        .env("DUST_HISTORY_HOME", dir.path().join("history"))
        .args([
            "acp",
            app.to_str().unwrap(),
            "--max-turns",
            "6",
            "--timeout-ms",
            "10000",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx
                .send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let request =
        |input: &mut std::process::ChildStdin, id: usize, method: &str, params: Value| -> Value {
            writeln!(
                input,
                "{}",
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            )
            .unwrap();
            loop {
                let value = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                if value["id"] == id {
                    assert!(value.get("error").is_none(), "{value}");
                    return value["result"].clone();
                }
            }
        };
    request(
        &mut input,
        1,
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"test","version":"1"}}),
    );
    let one = request(
        &mut input,
        2,
        "session/new",
        json!({"cwd":dir.path(),"mcpServers":[]}),
    )["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let two = request(
        &mut input,
        3,
        "session/new",
        json!({"cwd":dir.path(),"mcpServers":[]}),
    )["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        request(
            &mut input,
            4,
            "session/prompt",
            json!({"sessionId":one,"prompt":[{"type":"text","text":"SESSION_ONE_PRIVATE_MARKER"}]})
        )["stopReason"],
        "end_turn"
    );
    assert_eq!(
        request(
            &mut input,
            5,
            "session/prompt",
            json!({"sessionId":two,"prompt":[{"type":"text","text":"search own history only"}]})
        )["stopReason"],
        "end_turn"
    );
    drop(input);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("ACP did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let search_result = requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "search")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    let search = serde_json::from_str::<Value>(search_result).unwrap();
    assert!(
        search["result"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["role"] != "user"),
        "other session user records must not be searchable"
    );
    let read_result = requests[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "bad-path")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(!read_result.contains("EXTERNAL_FILE_MUST_NOT_BE_READ"));
    assert!(
        requests[3]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .all(|message| !message.to_string().contains("SESSION_ONE_PRIVATE_MARKER"))
    );
    assert_eq!(
        serde_json::from_str::<Value>(read_result).unwrap()["isError"],
        true
    );
    let directories: Vec<_> = std::fs::read_dir(dir.path().join("history"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        directories.len(),
        2,
        "each ACP session has its own transcript"
    );
    let originals: Vec<_> = directories
        .iter()
        .map(|path| std::fs::read_to_string(path.join("transcript.jsonl")).unwrap())
        .collect();
    assert_eq!(
        originals
            .iter()
            .filter(|text| text.lines().any(|line| {
                let record: Value = serde_json::from_str(line).unwrap();
                record["message"]["role"] == "user"
                    && record["message"]["content"] == "SESSION_ONE_PRIVATE_MARKER"
            }))
            .count(),
        1
    );
}

#[test]
fn default_policy_handles_one_large_latest_tool_result_without_unsummarizable_failure() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), true);
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&app).unwrap()).unwrap();
    manifest.as_object_mut().unwrap().remove("compaction");
    std::fs::write(&app, manifest.to_string()).unwrap();
    let (url, requests) = serve(vec![
        call("large", "fixture__large", json!({})),
        answer("done"),
    ]);
    let (report, success) = run(dir.path(), &app, &url, "single large latest batch");
    assert!(success, "{report}");
    assert_eq!(report["output"], "done");
    assert!(archive(&report).contains("ORIGINAL_BEYOND_64K"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let feedback = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "large")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(
        feedback.len() < 20000,
        "model-facing result is bounded while original remains full"
    );
    assert!(feedback.contains("original record 4 is preserved"));
}
