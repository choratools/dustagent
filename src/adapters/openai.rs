use crate::ports::llm::{ChatMessage, LlmProvider, LlmResponse, ToolCall, ToolDefinition};
use crate::{DustError, Result};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Production OpenAI-compatible LLM Gateway adapter.
pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// Creates a new `OpenAiProvider` by reading credentials from environment variables:
    /// - `OPENAI_API_KEY` (required)
    /// - `OPENAI_BASE_URL` (optional, default: "https://api.openai.com/v1")
    pub fn new(model: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            DustError::Config("OPENAI_API_KEY environment variable is required.".to_string())
        })?;

        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        Ok(Self::with_config(api_key, base_url, model))
    }

    /// Creates an `OpenAiProvider` with explicit configuration values.
    pub fn with_config(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::with_client(api_key, base_url, model, reqwest::Client::new())
    }

    /// Creates an `OpenAiProvider` with explicit configuration and custom reqwest client.
    pub fn with_client(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            api_key: api_key.into(),
            base_url,
            model: model.into(),
            client,
        }
    }

    /// Returns the configured model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the base URL for the OpenAI API.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[derive(Serialize)]
struct OpenAiWireMessage<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiWireToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
struct OpenAiWireToolCall<'a> {
    id: &'a str,
    r#type: &'static str,
    function: OpenAiWireFunction<'a>,
}

#[derive(Serialize)]
struct OpenAiWireFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct OpenAiWireTool<'a> {
    r#type: &'static str,
    function: OpenAiWireToolFunction<'a>,
}

#[derive(Serialize)]
struct OpenAiWireToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LlmResponse> {
        let url = format!("{}/chat/completions", self.base_url);

        // Serialize conversation messages into OpenAI wire format
        let wire_messages: Vec<OpenAiWireMessage> = messages
            .iter()
            .map(|msg| {
                let tool_calls = msg.tool_calls.as_ref().map(|calls| {
                    calls
                        .iter()
                        .map(|tc| OpenAiWireToolCall {
                            id: &tc.id,
                            r#type: "function",
                            function: OpenAiWireFunction {
                                name: &tc.function_name,
                                arguments: &tc.arguments,
                            },
                        })
                        .collect()
                });

                OpenAiWireMessage {
                    role: &msg.role,
                    content: msg.content.as_deref(),
                    tool_calls,
                    tool_call_id: msg.tool_call_id.as_deref(),
                }
            })
            .collect();

        let mut payload = serde_json::json!({
            "model": self.model,
            "messages": wire_messages,
            "temperature": 0.0,
        });

        // Attach tools if provided
        if let Some(tool_defs) = tools.filter(|t| !t.is_empty()) {
            let wire_tools: Vec<OpenAiWireTool> = tool_defs
                .iter()
                .map(|td| OpenAiWireTool {
                    r#type: "function",
                    function: OpenAiWireToolFunction {
                        name: &td.name,
                        description: &td.description,
                        parameters: &td.parameters,
                    },
                })
                .collect();
            payload["tools"] = serde_json::json!(wire_tools);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| DustError::Llm(format!("OpenAI HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read response body>".to_string());
            return Err(DustError::Llm(format!(
                "OpenAI API error ({status}): {body}"
            )));
        }

        let resp_json: Value = resp
            .json()
            .await
            .map_err(|e| DustError::Llm(format!("Failed to parse OpenAI JSON response: {e}")))?;

        let choice = resp_json
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| DustError::Llm("OpenAI response missing 'choices[0]'".to_string()))?;

        let message = choice
            .get("message")
            .ok_or_else(|| DustError::Llm("OpenAI choice missing 'message'".to_string()))?;

        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string());

        let tool_calls =
            if let Some(tc_array) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
                let mut calls = Vec::new();
                for tc in tc_array {
                    let id = tc
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let fn_obj = tc.get("function");
                    let name = fn_obj
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = fn_obj
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}")
                        .to_string();
                    calls.push(ToolCall::new(id, name, args));
                }
                if calls.is_empty() { None } else { Some(calls) }
            } else {
                None
            };

        Ok(LlmResponse {
            content,
            tool_calls,
        })
    }
}

/// A recorded chat call made to the `MockLlmProvider`.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCall {
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<ToolDefinition>>,
}

/// A thread-safe mock provider for unit & integration tests without network or API keys.
#[derive(Clone, Default)]
pub struct MockLlmProvider {
    responses: Arc<Mutex<VecDeque<LlmResponse>>>,
    recorded_calls: Arc<Mutex<Vec<RecordedCall>>>,
    #[allow(clippy::type_complexity)]
    handler: Arc<
        Mutex<
            Option<
                Arc<
                    dyn Fn(&[ChatMessage], Option<&[ToolDefinition]>) -> Result<LlmResponse>
                        + Send
                        + Sync,
                >,
            >,
        >,
    >,
}

impl MockLlmProvider {
    /// Creates an empty MockLlmProvider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a MockLlmProvider with a sequence of pre-queued responses.
    pub fn with_responses(responses: impl IntoIterator<Item = LlmResponse>) -> Self {
        let provider = Self::new();
        for resp in responses {
            provider.add_response(resp);
        }
        provider
    }

    /// Adds a response to the back of the response queue.
    pub fn add_response(&self, response: LlmResponse) {
        self.responses.lock().unwrap().push_back(response);
    }

    /// Sets a dynamic handler closure for generating responses.
    pub fn with_handler<F>(self, handler: F) -> Self
    where
        F: Fn(&[ChatMessage], Option<&[ToolDefinition]>) -> Result<LlmResponse>
            + Send
            + Sync
            + 'static,
    {
        *self.handler.lock().unwrap() = Some(Arc::new(handler));
        self
    }

    /// Returns all recorded calls made to this provider.
    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.recorded_calls.lock().unwrap().clone()
    }

    /// Returns the number of chat calls made so far.
    pub fn call_count(&self) -> usize {
        self.recorded_calls.lock().unwrap().len()
    }

    /// Returns the most recent call, if any.
    pub fn last_call(&self) -> Option<RecordedCall> {
        self.recorded_calls.lock().unwrap().last().cloned()
    }

    /// Returns the messages from the most recent call, if any.
    pub fn last_messages(&self) -> Option<Vec<ChatMessage>> {
        self.last_call().map(|c| c.messages)
    }

    /// Clears all recorded calls and queued responses.
    pub fn clear(&self) {
        self.responses.lock().unwrap().clear();
        self.recorded_calls.lock().unwrap().clear();
    }
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LlmResponse> {
        // Record the incoming request
        let call = RecordedCall {
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
        };
        self.recorded_calls.lock().unwrap().push(call);

        // Check if dynamic handler is registered
        let handler_opt = self.handler.lock().unwrap().clone();
        if let Some(handler) = handler_opt {
            return handler(messages, tools);
        }

        // Otherwise pop from queued responses
        let mut responses = self.responses.lock().unwrap();
        responses.pop_front().ok_or_else(|| {
            DustError::Llm("MockLlmProvider: no queued responses available".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_provider_config() {
        let provider =
            OpenAiProvider::with_config("fake-key", "https://api.openai.com/v1/", "gpt-4o");
        assert_eq!(provider.model(), "gpt-4o");
        assert_eq!(provider.base_url(), "https://api.openai.com/v1");
    }

    #[test]
    fn test_openai_provider_missing_key() {
        // Temporarily clear OPENAI_API_KEY
        let original = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }

        let res = OpenAiProvider::new("gpt-4o-mini");
        assert!(res.is_err());

        // Restore if it existed
        if let Some(val) = original {
            unsafe {
                std::env::set_var("OPENAI_API_KEY", val);
            }
        }
    }

    #[tokio::test]
    async fn test_mock_llm_provider_queue() {
        let mock = MockLlmProvider::with_responses(vec![
            LlmResponse::text("first"),
            LlmResponse::text("second"),
        ]);

        let msgs = vec![ChatMessage::user("hi")];
        let r1 = mock.chat(&msgs, None).await.unwrap();
        assert_eq!(r1.content.as_deref(), Some("first"));

        let r2 = mock.chat(&msgs, None).await.unwrap();
        assert_eq!(r2.content.as_deref(), Some("second"));

        let r3 = mock.chat(&msgs, None).await;
        assert!(r3.is_err());
        assert_eq!(mock.call_count(), 3);
        assert_eq!(mock.last_messages().unwrap().len(), 1);

        mock.clear();
        assert_eq!(mock.call_count(), 0);
    }

    #[tokio::test]
    async fn test_mock_llm_provider_handler() {
        let mock = MockLlmProvider::new().with_handler(|msgs, _tools| {
            let last_user = msgs.iter().rev().find(|m| m.role == "user");
            let echo = last_user
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            Ok(LlmResponse::text(format!("Echo: {echo}")))
        });

        let msgs = vec![ChatMessage::user("ping")];
        let res = mock.chat(&msgs, None).await.unwrap();
        assert_eq!(res.content.as_deref(), Some("Echo: ping"));
    }
}
