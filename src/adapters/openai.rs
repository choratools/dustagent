use crate::error::ProviderFailure;
use crate::ports::llm::{ChatMessage, LlmProvider, LlmResponse, ToolCall, ToolDefinition};
use crate::{DustError, Result};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

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
    fn model_id(&self) -> Option<&str> {
        Some(self.model())
    }
    fn context_window_tokens(&self) -> Option<usize> {
        if self.base_url == "https://api.openai.com/v1" {
            super::context::official_capacity(&self.model)
        } else {
            None
        }
    }
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
            .map_err(|e| DustError::Provider {
                kind: if e.is_connect() || e.is_timeout() || e.is_body() || e.is_request() {
                    ProviderFailure::Transient
                } else {
                    ProviderFailure::Permanent
                },
                message: format!("OpenAI HTTP request failed: {}", e.without_url()),
            })?;

        let status = resp.status();
        if !status.is_success() {
            // Status is decisive; do not expose arbitrary server bodies or credentials.
            let kind = match status.as_u16() {
                408 | 429 | 500 | 502 | 503 | 504 => ProviderFailure::Transient,
                _ => ProviderFailure::Permanent,
            };
            return Err(DustError::Provider {
                kind,
                message: format!("OpenAI API error ({status})"),
            });
        }
        let bytes = resp.bytes().await.map_err(|e| DustError::Provider {
            kind: ProviderFailure::Transient,
            message: format!("Failed to read OpenAI response body: {}", e.without_url()),
        })?;
        let resp_json: Value = serde_json::from_slice(&bytes).map_err(|e| DustError::Provider {
            kind: ProviderFailure::InvalidResponse,
            message: format!("Failed to parse OpenAI JSON response: {e}"),
        })?;

        let choice = resp_json
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| DustError::Provider {
                kind: ProviderFailure::InvalidResponse,
                message: "OpenAI response missing 'choices[0]'".into(),
            })?;

        let message = choice.get("message").ok_or_else(|| DustError::Provider {
            kind: ProviderFailure::InvalidResponse,
            message: "OpenAI choice missing 'message'".into(),
        })?;

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
}
