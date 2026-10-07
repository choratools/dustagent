use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Represents a single message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    /// Role of the message sender: "system", "user", "assistant", or "tool".
    pub role: String,

    /// Text content of the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Tool calls requested by an assistant message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    /// ID of the tool call this message is responding to (when role == "tool").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Creates a new system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Creates a new user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Creates an assistant message with optional text content and tool calls.
    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<ToolCall>>) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Creates an assistant message containing only text content.
    pub fn assistant_text(content: impl Into<String>) -> Self {
        Self::assistant(Some(content.into()), None)
    }

    /// Creates a tool response message.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Defines a callable tool (function) schema for LLM function calling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    /// Name of the function.
    pub name: String,

    /// Description of what the function does.
    pub description: String,

    /// JSON Schema defining the expected parameters.
    pub parameters: Value,
}

impl ToolDefinition {
    /// Creates a new tool definition.
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// A specific tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Unique identifier for this tool call.
    pub id: String,

    /// Name of the function to invoke.
    pub function_name: String,

    /// Raw JSON string of the arguments.
    pub arguments: String,
}

impl ToolCall {
    /// Creates a new tool call.
    pub fn new(
        id: impl Into<String>,
        function_name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            function_name: function_name.into(),
            arguments: arguments.into(),
        }
    }

    /// Parses the JSON arguments into a typed structure or `serde_json::Value`.
    pub fn parse_arguments<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(&self.arguments)
    }
}

/// The response returned by an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LlmResponse {
    /// Text content returned by the LLM.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Tool calls requested by the LLM, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Token counts reported by the provider for one completed response.
/// Providers may omit individual counters or the entire usage object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LlmUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl LlmUsage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.total_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.reasoning_tokens.is_none()
    }
}

/// A model response together with optional provider-reported token usage.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LlmCompletion {
    pub response: LlmResponse,
    pub usage: Option<LlmUsage>,
}

impl LlmResponse {
    /// Creates an LLM response containing text content only.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            tool_calls: None,
        }
    }

    /// Creates an LLM response containing tool calls only.
    pub fn tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            content: None,
            tool_calls: Some(tool_calls),
        }
    }

    /// Creates an LLM response with both content and tool calls.
    pub fn with_content_and_tools(
        content: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
    ) -> Self {
        Self {
            content,
            tool_calls,
        }
    }
}

/// Core port trait for LLM providers.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Actual selected model identifier, when the provider exposes it.
    fn model_id(&self) -> Option<&str> {
        None
    }
    /// Configured deployment's context capacity, when known. This is not token usage.
    fn context_window_tokens(&self) -> Option<usize> {
        None
    }
    /// Sends a conversation history and optional tool definitions to the LLM,
    /// returning the model's response.
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> crate::Result<LlmResponse>;

    /// Sends a conversation and returns provider usage when available.
    /// Existing providers remain compatible and default to no usage metadata.
    async fn chat_with_usage(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> crate::Result<LlmCompletion> {
        self.chat(messages, tools)
            .await
            .map(|response| LlmCompletion {
                response,
                usage: None,
            })
    }
}

#[async_trait]
impl<P: LlmProvider + ?Sized> LlmProvider for Box<P> {
    fn model_id(&self) -> Option<&str> {
        (**self).model_id()
    }
    fn context_window_tokens(&self) -> Option<usize> {
        (**self).context_window_tokens()
    }
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> crate::Result<LlmResponse> {
        (**self).chat(messages, tools).await
    }
    async fn chat_with_usage(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> crate::Result<LlmCompletion> {
        (**self).chat_with_usage(messages, tools).await
    }
}

#[async_trait]
impl<P: LlmProvider + ?Sized> LlmProvider for std::sync::Arc<P> {
    fn model_id(&self) -> Option<&str> {
        (**self).model_id()
    }
    fn context_window_tokens(&self) -> Option<usize> {
        (**self).context_window_tokens()
    }
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> crate::Result<LlmResponse> {
        (**self).chat(messages, tools).await
    }
    async fn chat_with_usage(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> crate::Result<LlmCompletion> {
        (**self).chat_with_usage(messages, tools).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_chat_message_constructors() {
        let sys = ChatMessage::system("sys prompt");
        assert_eq!(sys.role, "system");
        assert_eq!(sys.content.as_deref(), Some("sys prompt"));
        assert!(sys.tool_calls.is_none());
        assert!(sys.tool_call_id.is_none());

        let user = ChatMessage::user("hello");
        assert_eq!(user.role, "user");
        assert_eq!(user.content.as_deref(), Some("hello"));

        let asst_text = ChatMessage::assistant_text("response");
        assert_eq!(asst_text.role, "assistant");
        assert_eq!(asst_text.content.as_deref(), Some("response"));
        assert!(asst_text.tool_calls.is_none());

        let tool_call = ToolCall::new("tc1", "fetch", "{}");
        let asst_tools = ChatMessage::assistant(None, Some(vec![tool_call.clone()]));
        assert_eq!(asst_tools.role, "assistant");
        assert_eq!(asst_tools.tool_calls.as_ref().unwrap().len(), 1);

        let tool_resp = ChatMessage::tool("tc1", "result");
        assert_eq!(tool_resp.role, "tool");
        assert_eq!(tool_resp.tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(tool_resp.content.as_deref(), Some("result"));
    }

    #[test]
    fn test_tool_definition_and_call() {
        let def = ToolDefinition::new(
            "my_fn",
            "A test function",
            json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        );
        assert_eq!(def.name, "my_fn");
        assert_eq!(def.description, "A test function");

        let tc = ToolCall::new("call_1", "my_fn", r#"{"query":"rust"}"#);
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.function_name, "my_fn");
        let parsed: serde_json::Value = tc.parse_arguments().unwrap();
        assert_eq!(parsed["query"], "rust");
    }

    #[test]
    fn test_llm_response_constructors() {
        let resp_text = LlmResponse::text("answer");
        assert_eq!(resp_text.content.as_deref(), Some("answer"));
        assert!(resp_text.tool_calls.is_none());

        let tc = ToolCall::new("c1", "fn1", "{}");
        let resp_tc = LlmResponse::tool_calls(vec![tc.clone()]);
        assert!(resp_tc.content.is_none());
        assert_eq!(resp_tc.tool_calls.as_ref().unwrap().len(), 1);

        let resp_both = LlmResponse::with_content_and_tools(Some("text".into()), Some(vec![tc]));
        assert_eq!(resp_both.content.as_deref(), Some("text"));
        assert_eq!(resp_both.tool_calls.as_ref().unwrap().len(), 1);
    }
}
