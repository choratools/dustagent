//! Stateless Responses adapter for an existing Codex ChatGPT login.
//!
//! Only finalized output items are exposed, and only after response.completed.
//! No backend response body or authentication value is included in errors.
use super::codex_auth::{CodexAuth, Credentials};
use crate::application::retry::parse_retry_after;
use crate::error::ProviderFailure;
use crate::ports::llm::{
    ChatMessage, LlmCompletion, LlmProvider, LlmResponse, LlmUsage, ToolCall, ToolDefinition,
};
use crate::{DustError, Result};
use async_trait::async_trait;
use serde_json::{Value, json};

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const MAX_FRAME: usize = 1024 * 1024;

pub struct CodexProvider {
    auth: CodexAuth,
    client: reqwest::Client,
    endpoint: String,
    model: String,
}

impl CodexProvider {
    pub fn new(model: impl Into<String>) -> Result<Self> {
        Ok(Self {
            auth: CodexAuth::load()?,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| {
                    failure(
                        ProviderFailure::Permanent,
                        "Cannot create Codex HTTP client",
                    )
                })?,
            endpoint: ENDPOINT.into(),
            model: model.into(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn base_url(&self) -> &str {
        "https://chatgpt.com/backend-api/codex"
    }

    async fn send(&self, credentials: &Credentials, payload: &Value) -> Result<reqwest::Response> {
        self.client
            .post(&self.endpoint)
            .bearer_auth(&credentials.access_token)
            .header("chatgpt-account-id", &credentials.account_id)
            .header("originator", "codex_cli_rs")
            .header("Accept", "text/event-stream")
            .header("Content-Type", "application/json")
            .json(payload)
            .send()
            .await
            .map_err(|_| failure(ProviderFailure::Transient, "Codex HTTP request interrupted"))
    }
}

fn failure(kind: ProviderFailure, message: &str) -> DustError {
    DustError::Provider {
        kind,
        message: message.into(),
    }
}

fn request(
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[ToolDefinition]>,
) -> Result<Value> {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        match message.role.as_str() {
            "system" => {
                if let Some(text) = &message.content {
                    instructions.push(text.as_str());
                }
            }
            "user" | "assistant" => {
                if let Some(text) = &message.content {
                    input.push(json!({"type":"message", "role":message.role, "content":[{
                        "type": if message.role == "user" {"input_text"} else {"output_text"}, "text":text
                    }]}));
                }
                if let Some(calls) = &message.tool_calls {
                    if message.role != "assistant" {
                        return Err(failure(
                            ProviderFailure::InvalidResponse,
                            "Only assistant messages can request tools",
                        ));
                    }
                    for call in calls {
                        if call.id.is_empty() || call.function_name.is_empty() {
                            return Err(failure(
                                ProviderFailure::InvalidResponse,
                                "Invalid tool call in conversation",
                            ));
                        }
                        input.push(json!({"type":"function_call","call_id":call.id,"name":call.function_name,"arguments":call.arguments}));
                    }
                }
            }
            "tool" => {
                let id = message
                    .tool_call_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        failure(
                            ProviderFailure::InvalidResponse,
                            "Tool output missing call ID",
                        )
                    })?;
                input.push(json!({"type":"function_call_output","call_id":id,"output":message.content.as_deref().unwrap_or("")}));
            }
            _ => {
                return Err(failure(
                    ProviderFailure::InvalidResponse,
                    "Unsupported conversation role",
                ));
            }
        }
    }
    let tools: Vec<_> = tools.unwrap_or_default().iter().map(|tool| json!({
        "type":"function", "name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false
    })).collect();
    Ok(
        json!({"model":model,"instructions":instructions.join("\n\n"),"input":input,
        "tools":tools,"tool_choice":"auto","parallel_tool_calls":true,"store":false,"stream":true,"include":[]}),
    )
}

#[async_trait]
impl LlmProvider for CodexProvider {
    fn model_id(&self) -> Option<&str> {
        Some(self.model())
    }
    fn context_window_tokens(&self) -> Option<usize> {
        super::context::codex_capacity(&self.model)
    }
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LlmResponse> {
        self.chat_with_usage(messages, tools)
            .await
            .map(|completion| completion.response)
    }

    async fn chat_with_usage(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LlmCompletion> {
        let payload = request(&self.model, messages, tools)?;
        let credentials = self.auth.credentials().await?;
        let mut response = self.send(&credentials, &payload).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            drop(response);
            let fresh = self
                .auth
                .refresh_after_unauthorized(&credentials.access_token)
                .await?;
            response = self.send(&fresh, &payload).await?;
        }
        let status = response.status();
        if !status.is_success() {
            let kind =
                if status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error() {
                    ProviderFailure::Transient
                } else {
                    ProviderFailure::Permanent
                };
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after);
            if kind == ProviderFailure::Transient && retry_after.is_some() {
                return Err(DustError::ProviderRetryable {
                    kind,
                    message: format!("Codex API error ({status})"),
                    retry_after,
                });
            }
            return Err(failure(kind, &format!("Codex API error ({status})")));
        }
        let mut parser = SseParser::default();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            failure(
                ProviderFailure::Transient,
                "Codex response stream interrupted",
            )
        })? {
            if let Some(result) = parser.push(&chunk)? {
                return Ok(LlmCompletion {
                    response: result,
                    usage: parser.usage.clone(),
                });
            }
        }
        Err(failure(
            ProviderFailure::Transient,
            "Codex stream ended before completion",
        ))
    }
}

#[derive(Default)]
struct SseParser {
    total: usize,
    frame: Vec<u8>,
    line: Vec<u8>,
    items: Vec<Value>,
    usage: Option<LlmUsage>,
}
impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Result<Option<LlmResponse>> {
        self.total = self.total.checked_add(bytes.len()).ok_or_else(|| {
            failure(
                ProviderFailure::InvalidResponse,
                "Codex response exceeds limit",
            )
        })?;
        if self.total > MAX_RESPONSE {
            return Err(failure(
                ProviderFailure::InvalidResponse,
                "Codex response exceeds limit",
            ));
        }
        for &byte in bytes {
            if byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                if self.line.is_empty() {
                    if !self.frame.is_empty() {
                        let frame = std::mem::take(&mut self.frame);
                        if let Some(response) = self.event(&frame)? {
                            return Ok(Some(response));
                        }
                    }
                } else if self.line.starts_with(b"data:") {
                    let mut data = &self.line[5..];
                    if data.first() == Some(&b' ') {
                        data = &data[1..];
                    }
                    if !self.frame.is_empty() {
                        self.frame.push(b'\n');
                    }
                    if self.frame.len() + data.len() > MAX_FRAME {
                        return Err(failure(
                            ProviderFailure::InvalidResponse,
                            "Codex SSE frame exceeds limit",
                        ));
                    }
                    self.frame.extend_from_slice(data);
                }
                self.line.clear();
            } else {
                if self.line.len() >= MAX_FRAME {
                    return Err(failure(
                        ProviderFailure::InvalidResponse,
                        "Codex SSE line exceeds limit",
                    ));
                }
                self.line.push(byte);
            }
        }
        Ok(None)
    }
    fn event(&mut self, data: &[u8]) -> Result<Option<LlmResponse>> {
        if data == b"[DONE]" {
            return Err(failure(
                ProviderFailure::Transient,
                "Codex stream ended before completion",
            ));
        }
        let event: Value = serde_json::from_slice(data)
            .map_err(|_| failure(ProviderFailure::InvalidResponse, "Invalid Codex SSE JSON"))?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                let item = event.get("item").filter(|v| v.is_object()).ok_or_else(|| {
                    failure(
                        ProviderFailure::InvalidResponse,
                        "Codex completed item missing",
                    )
                })?;
                self.items.push(item.clone());
            }
            Some("response.completed") => {
                let response =
                    event
                        .get("response")
                        .filter(|v| v.is_object())
                        .ok_or_else(|| {
                            failure(
                                ProviderFailure::InvalidResponse,
                                "Codex completion missing response",
                            )
                        })?;
                if response
                    .get("id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(failure(
                        ProviderFailure::InvalidResponse,
                        "Codex completion missing response ID",
                    ));
                }
                if response
                    .get("status")
                    .is_some_and(|status| status.as_str() != Some("completed"))
                {
                    return Err(failure(
                        ProviderFailure::InvalidResponse,
                        "Codex response did not complete",
                    ));
                }
                self.usage = parse_usage(response.get("usage"));
                let items = if let Some(output) = response.get("output") {
                    let output = output.as_array().ok_or_else(|| {
                        failure(
                            ProviderFailure::InvalidResponse,
                            "Invalid Codex response output",
                        )
                    })?;
                    // Codex may send finalized items separately and leave the terminal
                    // output array empty. Its own client consumes output_item.done.
                    if output.is_empty() {
                        &self.items
                    } else {
                        output
                    }
                } else {
                    &self.items
                };
                return Ok(Some(parse_output(items)?));
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                // Backend bodies can contain user inputs and credentials; never echo them.
                return Err(failure(
                    ProviderFailure::Permanent,
                    "Codex response failed or was incomplete",
                ));
            }
            Some(_) => {}
            None => {
                return Err(failure(
                    ProviderFailure::InvalidResponse,
                    "Codex SSE event missing type",
                ));
            }
        }
        Ok(None)
    }
}

fn parse_output(items: &[Value]) -> Result<LlmResponse> {
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if item.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                let parts = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        failure(
                            ProviderFailure::InvalidResponse,
                            "Invalid Codex message content",
                        )
                    })?;
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => texts.push(
                            part.get("text")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    failure(ProviderFailure::InvalidResponse, "Invalid Codex text")
                                })?
                                .to_owned(),
                        ),
                        Some("refusal") => {
                            return Err(failure(
                                ProviderFailure::Permanent,
                                "Codex refused the request",
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                let string = |key| {
                    item.get(key)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            failure(
                                ProviderFailure::InvalidResponse,
                                "Invalid Codex function call",
                            )
                        })
                };
                let id = string("call_id")?;
                if calls.iter().any(|call: &ToolCall| call.id == id) {
                    return Err(failure(
                        ProviderFailure::InvalidResponse,
                        "Duplicate Codex function call ID",
                    ));
                }
                calls.push(ToolCall::new(id, string("name")?, string("arguments")?));
            }
            Some("reasoning") => {}
            _ => {
                return Err(failure(
                    ProviderFailure::InvalidResponse,
                    "Unsupported Codex output item",
                ));
            }
        }
    }
    Ok(LlmResponse {
        content: if texts.is_empty() {
            None
        } else {
            Some(texts.concat())
        },
        tool_calls: if calls.is_empty() { None } else { Some(calls) },
    })
}

fn parse_usage(value: Option<&Value>) -> Option<LlmUsage> {
    let value = value?.as_object()?;
    let count = |key: &str| value.get(key).and_then(Value::as_u64);
    let cached_input_tokens = value
        .get("input_tokens_details")
        .and_then(|v| v.get("cached_tokens"))
        .and_then(Value::as_u64);
    let reasoning_tokens = value
        .get("output_tokens_details")
        .and_then(|v| v.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    let usage = LlmUsage {
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        total_tokens: count("total_tokens"),
        cached_input_tokens,
        reasoning_tokens,
    };
    (!usage.is_empty()).then_some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(value: Value) -> Vec<u8> {
        format!("data: {value}\r\n\r\n").into_bytes()
    }
    #[test]
    fn split_stream_and_done_items() {
        let mut parser = SseParser::default();
        let mut bytes = event(
            json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"안녕"}]}}),
        );
        bytes.extend(event(json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call123","name":"dustagent__sleep","arguments":"{\"ms\":1}"}})));
        bytes.extend(event(
            json!({"type":"response.completed","response":{"id":"r1"}}),
        ));
        let mut result = None;
        for byte in bytes {
            if let Some(r) = parser.push(&[byte]).unwrap() {
                result = Some(r);
            }
        }
        let result = result.unwrap();
        assert_eq!(result.content.as_deref(), Some("안녕"));
        assert_eq!(result.tool_calls.unwrap()[0].id, "call123");
    }
    #[test]
    fn completed_output_not_duplicated() {
        let item = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]});
        let mut parser = SseParser::default();
        assert!(
            parser
                .push(&event(
                    json!({"type":"response.output_item.done","item":item})
                ))
                .unwrap()
                .is_none()
        );
        assert_eq!(parser.push(&event(json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[item]}}))).unwrap().unwrap().content.as_deref(),Some("hello"));
    }
    #[test]
    fn empty_terminal_output_keeps_finalized_items_and_completion_requires_id() {
        let item = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"final"}]});
        let mut parser = SseParser::default();
        assert!(
            parser
                .push(&event(
                    json!({"type":"response.output_item.done","item":item})
                ))
                .unwrap()
                .is_none()
        );
        let result=parser.push(&event(json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[]}}))).unwrap().unwrap();
        assert_eq!(result.content.as_deref(), Some("final"));
        for response in [json!({}), json!({"id":""}), json!({"id":"r1","status":4})] {
            assert!(
                SseParser::default()
                    .push(&event(
                        json!({"type":"response.completed","response":response})
                    ))
                    .is_err()
            );
        }
    }
    #[test]
    fn failures_do_not_expose_body_or_partial_text() {
        for kind in ["response.failed", "response.incomplete", "error"] {
            let mut parser = SseParser::default();
            let error = parser
                .push(&event(
                    json!({"type":kind,"error":{"message":"secret_token"}}),
                ))
                .unwrap_err();
            assert!(!error.to_string().contains("secret_token"));
        }
        assert!(SseParser::default().push(&event(json!({"type":"response.completed","response":{"status":"incomplete","output":[]}}))).is_err());
    }
    #[test]
    fn frame_and_total_limits() {
        assert!(
            SseParser::default()
                .push(&vec![b'x'; MAX_FRAME + 1])
                .is_err()
        );
        let mut parser = SseParser {
            total: MAX_RESPONSE,
            ..Default::default()
        };
        assert!(parser.push(b"x").is_err());
    }
    #[test]
    fn request_tool_history_and_non_strict_schemas() {
        let call = ToolCall::new("c1", "sleep", "{}");
        let payload = request(
            "model",
            &[
                ChatMessage::system("instructions"),
                ChatMessage::user("hi"),
                ChatMessage::assistant(None, Some(vec![call])),
                ChatMessage::tool("c1", "ok"),
            ],
            Some(&[ToolDefinition::new(
                "sleep",
                "sleep",
                json!({"type":"object"}),
            )]),
        )
        .unwrap();
        assert_eq!(payload["instructions"], "instructions");
        assert_eq!(payload["input"][1]["call_id"], "c1");
        assert_eq!(payload["input"][2]["type"], "function_call_output");
        assert_eq!(payload["tools"][0]["strict"], false);
        assert_eq!(payload["store"], false);
        assert_eq!(payload["stream"], true);
        assert!(payload.get("temperature").is_none());
    }
    async fn mock_server(
        replies: Vec<(u16, String)>,
    ) -> (
        String,
        tokio::sync::mpsc::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let task = tokio::spawn(async move {
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .map(|s| s.trim().parse().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                tx.send(String::from_utf8(request).unwrap()).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (endpoint, rx, task)
    }
    fn fixture_auth(endpoint: String) -> (tempfile::TempDir, CodexAuth) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth.json");
        std::fs::write(&path,json!({"tokens":{"access_token":"fake-access","account_id":"fake-account","refresh_token":"fake-refresh"}}).to_string()).unwrap();
        let auth = CodexAuth::from_path(path, endpoint).unwrap();
        (temp, auth)
    }
    fn completed(text: &str) -> String {
        String::from_utf8(event(json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}]}}))).unwrap()
    }
    #[tokio::test]
    async fn http_headers_payload_and_tool_roundtrip() {
        let (endpoint, mut rx, server) = mock_server(vec![(200, completed("done"))]).await;
        let (_temp, auth) = fixture_auth("http://127.0.0.1:1/token".into());
        let provider = CodexProvider {
            auth,
            client: reqwest::Client::new(),
            endpoint,
            model: "test-model".into(),
        };
        let response = provider
            .chat(
                &[
                    ChatMessage::system("sys"),
                    ChatMessage::assistant(
                        None,
                        Some(vec![ToolCall::new("call-id", "sleep", "{}")]),
                    ),
                    ChatMessage::tool("call-id", "ok"),
                ],
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.content.as_deref(), Some("done"));
        let request = rx.recv().await.unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(
            headers
                .to_lowercase()
                .contains("authorization: bearer fake-access")
        );
        assert!(
            headers
                .to_lowercase()
                .contains("chatgpt-account-id: fake-account")
        );
        assert!(headers.to_lowercase().contains("originator: codex_cli_rs"));
        let payload: Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["input"][0]["call_id"], "call-id");
        assert_eq!(payload["input"][1]["call_id"], "call-id");
        server.await.unwrap();
    }
    #[tokio::test]
    async fn http_status_and_truncated_stream_fail_closed() {
        for (status, body, expected) in [
            (429, "secret-body".into(), ProviderFailure::Transient),
            (403, "secret-body".into(), ProviderFailure::Permanent),
            (
                200,
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".into(),
                ProviderFailure::Transient,
            ),
        ] {
            let (endpoint, _rx, server) = mock_server(vec![(status, body)]).await;
            let (_temp, auth) = fixture_auth("http://127.0.0.1:1/token".into());
            let provider = CodexProvider {
                auth,
                client: reqwest::Client::new(),
                endpoint,
                model: "test".into(),
            };
            let error = provider
                .chat(&[ChatMessage::user("test")], None)
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("secret-body"));
            assert!(matches!(error,DustError::Provider{kind,..} if kind==expected));
            server.await.unwrap();
        }
    }
    #[tokio::test]
    async fn unauthorized_refreshes_once_and_retries_with_new_token() {
        let (refresh_endpoint, mut refresh_rx, refresh_server) = mock_server(vec![(
            200,
            json!({"access_token":"new-access","refresh_token":"new-refresh"}).to_string(),
        )])
        .await;
        let (endpoint, mut rx, server) =
            mock_server(vec![(401, "do not echo".into()), (200, completed("done"))]).await;
        let (temp, auth) = fixture_auth(refresh_endpoint);
        let provider = CodexProvider {
            auth,
            client: reqwest::Client::new(),
            endpoint,
            model: "test".into(),
        };
        assert_eq!(
            provider
                .chat(&[ChatMessage::user("test")], None)
                .await
                .unwrap()
                .content
                .as_deref(),
            Some("done")
        );
        assert!(rx.recv().await.unwrap().contains("Bearer fake-access"));
        assert!(rx.recv().await.unwrap().contains("Bearer new-access"));
        assert!(refresh_rx.recv().await.unwrap().contains("fake-refresh"));
        let cache: Value =
            serde_json::from_slice(&std::fs::read(temp.path().join("auth.json")).unwrap()).unwrap();
        assert_eq!(cache["tokens"]["access_token"], "new-access");
        server.await.unwrap();
        refresh_server.await.unwrap();
    }
    #[tokio::test]
    async fn dropping_inference_cancels_stream_without_internal_replay() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            started_tx.send(()).unwrap();
            // Dropping the HTTP response closes this request rather than launching background work.
            loop {
                if stream.read(&mut buffer).await.unwrap() == 0 {
                    break;
                }
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        let (_temp, auth) = fixture_auth("http://127.0.0.1:1/token".into());
        let provider = CodexProvider {
            auth,
            client: reqwest::Client::new(),
            endpoint,
            model: "test".into(),
        };
        let pending =
            tokio::spawn(async move { provider.chat(&[ChatMessage::user("test")], None).await });
        started_rx.await.unwrap();
        pending.abort();
        let _ = pending.await;
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
