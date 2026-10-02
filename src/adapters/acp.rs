//! ACP v1 stdio adapter. Client inputs never grant additional application tools.
use crate::{
    AutoProvider, DustCore, DustError, Result,
    application::{
        events::ExecutionEvent,
        execution::{ExecutionReport, StopReason, ToolStatus},
        package::{self, LoadedApp},
        session::AgentSession,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, watch},
    task::{Id, JoinError, JoinSet},
};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_TEXT: usize = 256 * 1024;
#[derive(Debug, Clone)]
pub struct AcpOptions {
    pub app: String,
    pub model: Option<String>,
    pub max_turns: Option<usize>,
    pub timeout_ms: Option<u64>,
    pub tool_timeout_ms: Option<u64>,
}
type Core = DustCore<AutoProvider>;
struct Session {
    core: Option<Core>,
    history: Option<AgentSession>,
    cancel: watch::Sender<bool>,
    ready: bool,
}
struct Completion {
    session_id: String,
    request_id: Value,
    core: Core,
    history: AgentSession,
    ready: bool,
    response: Value,
}
// A task panic/abort loses the in-memory execution state. Drop that session rather
// than leave its request permanently busy or permit continuation with unknown state.
fn fail_job(
    failure: &JoinError,
    pending: &mut HashMap<Id, (String, Value)>,
    sessions: &mut HashMap<String, Session>,
    active: &mut HashSet<String>,
) -> Option<Value> {
    let (session_id, request_id) = pending.remove(&failure.id())?;
    sessions.remove(&session_id);
    active.remove(&id_key(&request_id));
    Some(error(
        request_id,
        -32000,
        "Prompt task failed; affected session was closed",
    ))
}
fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.into()}})
}
fn result(id: Value, value: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":value})
}
fn update(id: &str, value: Value) -> Value {
    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":id,"update":value}})
}
fn id_key(id: &Value) -> String {
    id.to_string()
}
fn input(params: &Value) -> std::result::Result<String, String> {
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or("prompt must be an array")?;
    if blocks.is_empty() {
        return Err("prompt must not be empty".into());
    }
    let mut output = String::new();
    for block in blocks {
        let part = match block.get("type").and_then(Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .ok_or("text must be a string")?
                .to_owned(),
            Some("resource_link") => {
                let uri = block
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or("resource link requires uri")?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("resource link requires name")?;
                for field in ["title", "description", "mimeType"] {
                    if block.get(field).is_some_and(|v| !v.is_string()) {
                        return Err(format!("{field} must be a string"));
                    }
                }
                format!(
                    "Resource link (metadata only; content not fetched): {}",
                    json!({"uri":uri,"name":name,"title":block.get("title"),"description":block.get("description"),"mimeType":block.get("mimeType")})
                )
            }
            _ => {
                return Err(
                    "Unsupported prompt content type; only text and resource_link are accepted"
                        .into(),
                );
            }
        };
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&part);
        if output.len() > MAX_TEXT {
            return Err("Prompt exceeds 256 KiB".into());
        }
    }
    Ok(output)
}
fn session_directory(params: &Value, app: &LoadedApp) -> std::result::Result<PathBuf, String> {
    let cwd = PathBuf::from(
        params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or("cwd must be a string")?,
    );
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err("cwd must be an existing absolute directory".into());
    }
    let servers = params
        .get("mcpServers")
        .and_then(Value::as_array)
        .ok_or("mcpServers must be an array")?;
    let mut seen = HashSet::new();
    for server in servers {
        if server.get("type").is_some_and(|v| v != "stdio") {
            return Err("Only declared stdio MCP servers are accepted".into());
        }
        let name = server
            .get("name")
            .and_then(Value::as_str)
            .ok_or("MCP name must be a string")?;
        if !seen.insert(name) {
            return Err("Duplicate MCP server name".into());
        }
        let declared = app
            .manifest
            .mcp_servers
            .get(name)
            .ok_or("MCP server is not declared by this app")?;
        let command = server
            .get("command")
            .and_then(Value::as_str)
            .ok_or("MCP command must be a string")?;
        let args: Vec<String> =
            serde_json::from_value(server.get("args").cloned().ok_or("MCP args required")?)
                .map_err(|_| "MCP args must be strings")?;
        let env = server
            .get("env")
            .and_then(Value::as_array)
            .ok_or("MCP env must be an array")?;
        let mut variables = HashMap::new();
        for item in env {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or("MCP env name required")?;
            let value = item
                .get("value")
                .and_then(Value::as_str)
                .ok_or("MCP env value required")?;
            if variables
                .insert(name.to_owned(), value.to_owned())
                .is_some()
            {
                return Err("Duplicate MCP environment name".into());
            }
        }
        if command != declared.command
            || args != declared.args
            || variables != declared.env.clone().unwrap_or_default()
        {
            return Err("Client MCP configuration differs from the app declaration".into());
        }
    }
    cwd.canonicalize().map_err(|e| e.to_string())
}
fn event_update(
    event: ExecutionEvent,
    ids: &mut HashMap<String, String>,
    prefix: &str,
    sequence: &mut u64,
) -> Value {
    match event {
        ExecutionEvent::AssistantMessage { content } => {
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":content}})
        }
        ExecutionEvent::ToolStarted {
            call_id,
            name,
            arguments,
        } => {
            *sequence += 1;
            let id = format!("{prefix}-{}", sequence);
            ids.insert(call_id, id.clone());
            json!({"sessionUpdate":"tool_call","toolCallId":id,"title":name,"status":"in_progress","rawInput":arguments})
        }
        ExecutionEvent::ToolFinished { record } => {
            let id = ids
                .get(&record.call_id)
                .cloned()
                .unwrap_or_else(|| format!("{prefix}-unobserved"));
            json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":if record.status == ToolStatus::Succeeded {"completed"} else {"failed"},"rawOutput":record.output,"content":record.error.map(|text| vec![json!({"type":"content","content":{"type":"text","text":text}})]).unwrap_or_default()})
        }
    }
}
// Retains partial bytes across select cancellation and caps allocation before parsing.
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    frame: &mut Vec<u8>,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(frame.len());
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(available.len());
        let finished = available[count - 1] == b'\n';
        let count = count.min((MAX_FRAME + 1).saturating_sub(frame.len()));
        frame.extend_from_slice(&available[..count]);
        reader.consume(count);
        if finished || frame.len() > MAX_FRAME {
            return Ok(frame.len());
        }
    }
}
pub async fn serve_stdio(options: AcpOptions) -> Result<()> {
    serve(tokio::io::stdin(), tokio::io::stdout(), options).await
}
/// Newline-delimited JSON-RPC transport; the reader remains active during prompts.
pub async fn serve<R, W>(reader: R, mut writer: W, options: AcpOptions) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let app = Arc::new(package::load(&options.app, &std::env::current_dir()?)?);
    let total = options
        .timeout_ms
        .or(app.manifest.timeout_ms)
        .unwrap_or(300_000);
    let tool = options
        .tool_timeout_ms
        .or(app.manifest.tool_timeout_ms)
        .unwrap_or(30_000);
    if !(1..=86_400_000).contains(&total)
        || !(1..=86_400_000).contains(&tool)
        || options.max_turns == Some(0)
    {
        return Err(DustError::Config("Invalid ACP execution budget".into()));
    }
    let (tx, mut rx) = mpsc::channel::<Value>(64);
    let mut writer_task = tokio::spawn(async move {
        while let Some(value) = rx.recv().await {
            let mut bytes = serde_json::to_vec(&value)?;
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
            writer.flush().await?;
        }
        Ok::<(), DustError>(())
    });
    let mut reader = BufReader::new(reader);
    let mut sessions: HashMap<String, Session> = HashMap::new();
    let mut jobs = JoinSet::new();
    let mut pending = HashMap::new();
    let mut active = HashSet::new();
    let mut initialized = false;
    let mut next_session = 0u64;
    let mut next_prompt = 0u64;
    let mut writer_finished = None;
    let mut frame = Vec::new();
    loop {
        tokio::select! {
            written = &mut writer_task => { writer_finished = Some(written); break; }
            completion = jobs.join_next_with_id(), if !jobs.is_empty() => {
                match completion {
                    Some(Ok((task_id, done))) => {
                        let done: Completion = done;
                        pending.remove(&task_id);
                        active.remove(&id_key(&done.request_id));
                        let response = done.response;
                        if let Some(slot) = sessions.get_mut(&done.session_id) { slot.core = Some(done.core); slot.history = Some(done.history); slot.ready = done.ready; }
                        let _ = tx.send(response).await;
                    }
                    Some(Err(failure)) => if let Some(response) = fail_job(&failure, &mut pending, &mut sessions, &mut active) { let _ = tx.send(response).await; },
                    None => {}
                }
            }
            read = read_frame(&mut reader, &mut frame) => {
                match read { Ok(0) | Err(_) => break, Ok(_) => {} }
                if frame.len() > MAX_FRAME { tx.send(error(Value::Null,-32600,"Frame exceeds 1 MiB")).await.ok(); break; }
                let envelope: Value = match serde_json::from_slice(&frame) { Ok(v) => v, Err(_) => { tx.send(error(Value::Null,-32700,"Invalid JSON")).await.ok(); frame.clear(); continue; } };
                frame.clear();
                let id = envelope.get("id").cloned();
                let valid_id = id.as_ref().is_none_or(|v| v.is_null() || v.is_string() || v.is_i64() || v.is_u64());
                let method = envelope.get("method").and_then(Value::as_str);
                if !envelope.is_object() || envelope.get("jsonrpc") != Some(&json!("2.0")) || method.is_none() || !valid_id || envelope.get("params").is_some_and(|v| !v.is_object()) {
                    tx.send(error(Value::Null,-32600,"Invalid request envelope")).await.ok(); continue;
                }
                let method = method.unwrap();
                let params = envelope.get("params").cloned().unwrap_or_else(|| json!({}));
                if method == "session/cancel" && id.is_none() { if let Some(slot) = params.get("sessionId").and_then(Value::as_str).and_then(|sid| sessions.get(sid)) { let _ = slot.cancel.send(true); } continue; }
                let Some(id) = id else { continue; };
                if active.contains(&id_key(&id)) { tx.send(error(id,-32600,"Request ID already active")).await.ok(); continue; }
                let response = if method == "initialize" {
                    if initialized { error(id,-32600,"Connection already initialized") }
                    else if params.get("protocolVersion").and_then(Value::as_u64).is_none() || params.get("clientCapabilities").is_some_and(|v| !v.is_object()) { error(id,-32602,"Invalid initialization parameters") }
                    else { initialized = true; result(id,json!({"protocolVersion":1,"agentCapabilities":{"loadSession":false,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},"mcpCapabilities":{"http":false,"sse":false}},"agentInfo":{"name":"dustagent","version":env!("CARGO_PKG_VERSION")},"authMethods":[]})) }
                } else if !initialized { error(id,-32000,"Initialize the connection first") }
                else if method == "session/new" {
                    if sessions.len() >= 8 { error(id,-32000,"Session limit reached") }
                    else { match session_directory(&params,&app) {
                        Err(message) => error(id,-32602,message),
                        Ok(cwd) => {
                            let model = options.model.clone().or_else(|| app.manifest.default_model.clone());
                            let core = AutoProvider::new(model).and_then(|provider| DustCore::new(app.manifest.clone(),provider).with_timeouts(total,tool).with_app_resources(&app.root,app.digest.as_deref())).and_then(|core| core.with_working_directory(&cwd));
                            match core { Err(e) => error(id,-32000,e.to_string()), Ok(mut core) => { if let Some(turns) = options.max_turns { core = core.with_max_turns(turns); } next_session += 1; let sid = format!("dust-{next_session}"); let (cancel,_) = watch::channel(false); sessions.insert(sid.clone(),Session{core:Some(core),history:Some(AgentSession::new()),cancel,ready:false}); result(id,json!({"sessionId":sid})) } }
                        }
                    } }
                } else if method == "session/prompt" {
                    let sid = params.get("sessionId").and_then(Value::as_str).unwrap_or("").to_owned();
                    match (sessions.get_mut(&sid), input(&params)) {
                        (_,Err(message)) => error(id,-32602,message),
                        (None,_) => error(id,-32602,"Unknown session"),
                        (Some(slot),Ok(text)) if slot.core.is_some() => {
                            let mut core = slot.core.take().unwrap(); let mut history = slot.history.take().unwrap(); let ready = slot.ready;
                            let _ = slot.cancel.send(false); let cancel = slot.cancel.subscribe();
                            let (events,mut event_rx) = mpsc::unbounded_channel(); core = core.with_events(events).with_cancellation(cancel.clone());
                            let send = tx.clone(); let app_guard = app.clone(); let request_id = id.clone(); let session_id = sid.clone(); active.insert(id_key(&id));
                            next_prompt += 1; let prefix = format!("{sid}-p{next_prompt}");
                            let job_metadata = (sid.clone(), id.clone());
                            let handle = jobs.spawn(async move {
                                let mut ids = HashMap::new(); let mut event_sequence = 0;
                                let _guard = app_guard; let started = tokio::time::Instant::now(); let mut ready = ready; let mut cancel_init = cancel;
                                let report = { let operation = async {
                                    if !ready {
                                        let startup = tokio::select! { result = tokio::time::timeout(Duration::from_millis(total),core.init_scoped_mcp()) => match result { Ok(Ok(())) => None, Ok(Err(e)) => Some((StopReason::ExecutionError,e.to_string())), Err(_) => Some((StopReason::TimeLimit,"MCP startup exceeded budget".into())) }, _ = cancel_init.changed() => Some((StopReason::Cancelled,"Cancelled during MCP startup".into())) };
                                        if let Some((reason,message)) = startup { let _ = tokio::time::timeout(Duration::from_secs(5),core.shutdown()).await; return ExecutionReport { stop_reason:reason,error:Some(message),..Default::default() }; }
                                        ready = true;
                                    }
                                    let remaining = total.saturating_sub(started.elapsed().as_millis() as u64); if remaining == 0 { return ExecutionReport { stop_reason:StopReason::TimeLimit,error:Some("MCP startup exhausted execution budget".into()),..Default::default() }; } core.set_timeouts(remaining,tool); core.prompt_report(&mut history,&text).await
                                };
                                tokio::pin!(operation);
                                loop { tokio::select! { report = &mut operation => break report, event = event_rx.recv() => if let Some(event) = event && send.send(update(&sid,event_update(event, &mut ids, &prefix, &mut event_sequence))).await.is_err() { break ExecutionReport { error:Some("ACP writer closed".into()),..Default::default() }; } } } };
                                while let Ok(event) = event_rx.try_recv() { let _ = send.send(update(&sid,event_update(event, &mut ids, &prefix, &mut event_sequence))).await; }
                                let response = match report.stop_reason { StopReason::Completed => result(id,json!({"stopReason":"end_turn"})), StopReason::TurnLimit => result(id,json!({"stopReason":"max_turn_requests"})), StopReason::Cancelled => result(id,json!({"stopReason":"cancelled"})), _ => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"DustAgent execution did not complete","data":report}}) };
                                Completion {session_id,request_id,core,history,ready,response}
                            });
                            pending.insert(handle.id(), job_metadata);
                            continue;
                        }
                        (Some(_),_) => error(id,-32000,"Session already has an active prompt"),
                    }
                } else { error(id,-32601,"Method not supported") };
                if tx.send(response).await.is_err() { break; }
            }
        }
    }
    for slot in sessions.values() {
        let _ = slot.cancel.send(true);
    }
    let cleanup = async {
        while let Some(done) = jobs.join_next_with_id().await {
            match done {
                Ok((task_id, mut done)) => {
                    pending.remove(&task_id);
                    let _ = tx.send(done.response).await;
                    let _ =
                        tokio::time::timeout(Duration::from_secs(5), done.core.shutdown()).await;
                }
                Err(failure) => {
                    if let Some(response) =
                        fail_job(&failure, &mut pending, &mut sessions, &mut active)
                    {
                        let _ = tx.send(response).await;
                    }
                }
            }
        }
        for slot in sessions.values_mut() {
            if let Some(core) = slot.core.as_mut() {
                let _ = tokio::time::timeout(Duration::from_secs(5), core.shutdown()).await;
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(6), cleanup)
        .await
        .is_err()
    {
        jobs.abort_all();
    }
    drop(sessions);
    drop(jobs);
    drop(tx);
    let written = match writer_finished {
        Some(result) => result,
        None => match tokio::time::timeout(Duration::from_secs(2), &mut writer_task).await {
            Ok(result) => result,
            Err(_) => {
                writer_task.abort();
                return Err(DustError::Config("ACP writer shutdown timed out".into()));
            }
        },
    };
    match written {
        Ok(result) => result,
        Err(e) => Err(DustError::Config(format!("ACP writer failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    #[test]
    fn content_is_explicit_and_bounded() {
        assert_eq!(
            input(&json!({"prompt":[{"type":"text","text":"hello"}]})).unwrap(),
            "hello"
        );
        assert!(input(&json!({"prompt":[{"type":"image","data":"abc"}]})).is_err());
        assert!(input(&json!({"prompt":[{"type":"text","text":"x".repeat(MAX_TEXT+1)}]})).is_err());
        let resource = input(&json!({"prompt":[{"type":"resource_link","name":"sample","uri":"file:///etc/passwd"}]})).unwrap();
        assert!(resource.contains("metadata only"));
        assert!(resource.contains("file:///etc/passwd"));
        assert!(input(&json!({"prompt":[{"type":"resource_link","name":"sample","uri":"x","description":1}]})).is_err());
    }
    #[tokio::test]
    async fn failed_job_releases_request_and_closes_session() {
        let mut jobs = JoinSet::<()>::new();
        let handle = jobs.spawn(std::future::pending());
        let request_id = json!("request");
        let mut pending = HashMap::from([(handle.id(), ("session".into(), request_id.clone()))]);
        let (cancel, _) = watch::channel(false);
        let mut sessions = HashMap::from([(
            "session".into(),
            Session {
                core: None,
                history: None,
                cancel,
                ready: false,
            },
        )]);
        let mut active = HashSet::from([id_key(&request_id)]);
        handle.abort();
        let failure = jobs.join_next_with_id().await.unwrap().unwrap_err();
        let response = fail_job(&failure, &mut pending, &mut sessions, &mut active).unwrap();
        assert_eq!(response["id"], request_id);
        assert_eq!(response["error"]["code"], -32000);
        assert!(sessions.is_empty());
        assert!(pending.is_empty());
        assert!(active.is_empty());
    }
    #[test]
    fn tool_ids_are_unique_when_provider_reuses_ids() {
        let mut ids = HashMap::new();
        let mut sequence = 0;
        let start = || ExecutionEvent::ToolStarted {
            call_id: "reused".into(),
            name: "tool".into(),
            arguments: json!({}),
        };
        let first = event_update(start(), &mut ids, "dust-1-p1", &mut sequence);
        let second = event_update(start(), &mut ids, "dust-1-p1", &mut sequence);
        assert_ne!(first["toolCallId"], second["toolCallId"]);
        let record = crate::ToolRecord {
            turn: 1,
            call_id: "reused".into(),
            name: "tool".into(),
            arguments: json!({}),
            status: ToolStatus::Succeeded,
            elapsed_ms: 0,
            output: Some("ok".into()),
            error: None,
            truncated: false,
        };
        let finished = event_update(
            ExecutionEvent::ToolFinished { record },
            &mut ids,
            "dust-1-p1",
            &mut sequence,
        );
        assert_eq!(second["toolCallId"], finished["toolCallId"]);
        let third = event_update(start(), &mut HashMap::new(), "dust-1-p2", &mut 0);
        assert_ne!(first["toolCallId"], third["toolCallId"]);
    }
    #[tokio::test]
    async fn frame_limit_precedes_full_line_allocation() {
        let (mut writer, reader) = tokio::io::duplex(2048);
        let task = tokio::spawn(async move {
            let _ = writer.write_all(&vec![b'x'; MAX_FRAME * 2]).await;
        });
        let mut frame = Vec::new();
        assert_eq!(
            read_frame(&mut BufReader::new(reader), &mut frame)
                .await
                .unwrap(),
            MAX_FRAME + 1
        );
        task.abort();
    }
    #[tokio::test]
    async fn protocol_initialization_errors_and_capabilities() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("app.json");
        std::fs::write(
            &manifest,
            json!({"name":"test","system_prompt":"test"}).to_string(),
        )
        .unwrap();
        let (mut input_writer, input_reader) = tokio::io::duplex(8192);
        let (output_writer, output_reader) = tokio::io::duplex(8192);
        let options = AcpOptions {
            app: manifest.to_string_lossy().into_owned(),
            model: None,
            max_turns: None,
            timeout_ms: None,
            tool_timeout_ms: None,
        };
        let task = tokio::spawn(serve(input_reader, output_writer, options));
        let requests = [
            json!({"jsonrpc":"2.0","id":0,"method":"session/new","params":{}}),
            json!({"jsonrpc":"2.0","id":null,"method":"initialize","params":{"protocolVersion":1}}),
            json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{}}),
            json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"relative","mcpServers":[]}}),
        ];
        for request in requests {
            input_writer
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }
        input_writer.write_all(b"broken\n").await.unwrap();
        drop(input_writer);
        let mut output = BufReader::new(output_reader).lines();
        let mut values = Vec::new();
        while let Some(line) = output.next_line().await.unwrap() {
            values.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(values[0]["error"]["code"], -32000);
        assert_eq!(values[1]["result"]["protocolVersion"], 1);
        assert!(values[1]["id"].is_null());
        assert_eq!(
            values[1]["result"]["agentCapabilities"]["loadSession"],
            false
        );
        assert_eq!(values[2]["error"]["code"], -32601);
        assert_eq!(values[3]["error"]["code"], -32602);
        assert_eq!(values[4]["error"]["code"], -32700);
        task.await.unwrap().unwrap();
    }
    #[test]
    fn mcp_configuration_cannot_expand_app_authority() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("app.json");
        std::fs::write(&manifest,json!({"name":"test","system_prompt":"test","mcp_servers":{"tool":{"command":"echo","args":["safe"]}}}).to_string()).unwrap();
        let app = package::load(manifest.to_str().unwrap(), temp.path()).unwrap();
        let mut request = json!({"cwd":temp.path(),"mcpServers":[{"name":"tool","command":"echo","args":["safe"],"env":[]}]});
        assert!(session_directory(&request, &app).is_ok());
        request["mcpServers"][0]["args"] = json!(["unsafe"]);
        assert!(session_directory(&request, &app).is_err());
        request["mcpServers"][0]["name"] = json!("other");
        assert!(session_directory(&request, &app).is_err());
    }
}
