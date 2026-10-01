use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use dustagent::application::DustCore;
use dustagent::domain::manifest::{AppManifest, McpServerConfig};
use dustagent::ports::llm::{ChatMessage, LlmProvider, LlmResponse, ToolCall, ToolDefinition};
use dustagent::ports::mcp::{McpClient, McpTool};

#[derive(Debug, Clone, PartialEq)]
struct RecordedCall {
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<ToolDefinition>>,
}

#[derive(Clone, Default)]
struct MockLlmProvider {
    responses: Arc<Mutex<VecDeque<LlmResponse>>>,
    recorded_calls: Arc<Mutex<Vec<RecordedCall>>>,
    #[allow(clippy::type_complexity)]
    handler: Arc<
        Mutex<
            Option<
                Arc<
                    dyn Fn(
                            &[ChatMessage],
                            Option<&[ToolDefinition]>,
                        ) -> dustagent::Result<LlmResponse>
                        + Send
                        + Sync,
                >,
            >,
        >,
    >,
}

impl MockLlmProvider {
    fn new() -> Self {
        Self::default()
    }

    fn with_responses(responses: impl IntoIterator<Item = LlmResponse>) -> Self {
        let provider = Self::new();
        for resp in responses {
            provider.responses.lock().unwrap().push_back(resp);
        }
        provider
    }

    fn with_handler<F>(self, handler: F) -> Self
    where
        F: Fn(&[ChatMessage], Option<&[ToolDefinition]>) -> dustagent::Result<LlmResponse>
            + Send
            + Sync
            + 'static,
    {
        *self.handler.lock().unwrap() = Some(Arc::new(handler));
        self
    }

    fn call_count(&self) -> usize {
        self.recorded_calls.lock().unwrap().len()
    }

    fn last_call(&self) -> Option<RecordedCall> {
        self.recorded_calls.lock().unwrap().last().cloned()
    }

    fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.recorded_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        self.recorded_calls.lock().unwrap().push(RecordedCall {
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
        });

        let handler_opt = self.handler.lock().unwrap().clone();
        if let Some(handler) = handler_opt {
            return handler(messages, tools);
        }

        let mut responses = self.responses.lock().unwrap();
        responses.pop_front().ok_or_else(|| {
            dustagent::DustError::Llm("TestLlmProvider: no responses left".to_string())
        })
    }
}

#[derive(Clone, Default)]
struct MockMcpClient {
    tools: Vec<McpTool>,
    tool_results: Arc<Mutex<HashMap<String, Value>>>,
    recorded_calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl MockMcpClient {
    fn new(tools: Vec<McpTool>) -> Self {
        Self {
            tools,
            tool_results: Arc::new(Mutex::new(HashMap::new())),
            recorded_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_tool_result(self, tool_name: impl Into<String>, result: Value) -> Self {
        self.tool_results
            .lock()
            .unwrap()
            .insert(tool_name.into(), result);
        self
    }

    fn recorded_calls(&self) -> Vec<(String, Value)> {
        self.recorded_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl McpClient for MockMcpClient {
    async fn initialize(&mut self) -> dustagent::Result<()> {
        Ok(())
    }

    async fn list_tools(&mut self) -> dustagent::Result<Vec<McpTool>> {
        Ok(self.tools.clone())
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> dustagent::Result<Value> {
        self.recorded_calls
            .lock()
            .unwrap()
            .push((name.to_string(), arguments.clone()));

        if let Some(res) = self.tool_results.lock().unwrap().get(name) {
            Ok(res.clone())
        } else {
            Ok(json!({ "status": "success", "tool": name }))
        }
    }

    async fn close(&mut self) -> dustagent::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_single_shot_execution_without_tools() {
    let manifest = AppManifest {
        schema: Some("dustagent/app-v1".to_string()),
        name: Some("summarizer".to_string()),
        description: Some("Concise text summarizer".to_string()),
        system_prompt: Some("You are a concise summarizer.".to_string()),
        default_model: Some("gpt-4o-mini".to_string()),
        mcp_servers: HashMap::new(),
        output_format: Some("text".to_string()),
        max_turns: None,
    };

    let expected_output = "The quick brown fox jumps over the lazy dog.";
    let mock_llm = MockLlmProvider::with_responses(vec![LlmResponse::text(expected_output)]);

    let mut core = DustCore::new(manifest, mock_llm.clone());

    let user_input = "Please summarize this paragraph.";
    let result = core
        .execute(user_input)
        .await
        .expect("Execution should succeed");

    assert_eq!(result, expected_output);
    assert_eq!(mock_llm.call_count(), 1);

    let recorded = mock_llm
        .last_call()
        .expect("A call should have been recorded");
    assert_eq!(recorded.messages.len(), 2);
    assert_eq!(
        recorded.messages[0],
        ChatMessage::system("You are a concise summarizer.")
    );
    assert_eq!(recorded.messages[1], ChatMessage::user(user_input));
    assert!(recorded.tools.is_none() || recorded.tools.as_ref().unwrap().is_empty());

    core.shutdown().await.expect("Shutdown should succeed");
}

#[tokio::test]
async fn test_multiturn_tool_execution() {
    let manifest = AppManifest {
        schema: Some("dustagent/app-v1".to_string()),
        name: Some("crawler".to_string()),
        description: Some("Web content extractor".to_string()),
        system_prompt: Some("You are a headless web crawler.".to_string()),
        default_model: Some("gpt-4o-mini".to_string()),
        mcp_servers: HashMap::new(),
        output_format: Some("raw_json".to_string()),
        max_turns: None,
    };

    // Prepare mock MCP client exposing "fetch" tool
    let fetch_tool = McpTool::new(
        "fetch",
        Some("Fetch webpage content by URL".to_string()),
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" }
            },
            "required": ["url"]
        }),
    );
    let mock_mcp = MockMcpClient::new(vec![fetch_tool]).with_tool_result(
        "fetch",
        json!({
            "status": 200,
            "html": "<html><body><h1>Example Heading</h1><p>Hello Rust!</p></body></html>"
        }),
    );

    // Mock LLM turn 1: Call tool `fetcher__fetch`
    // Mock LLM turn 2: Return final extracted content
    let tool_call = ToolCall::new(
        "call_crawl_999",
        "fetcher__fetch",
        r#"{"url":"https://example.com"}"#,
    );
    let mock_llm = MockLlmProvider::with_responses(vec![
        LlmResponse::tool_calls(vec![tool_call]),
        LlmResponse::text(r#"{"heading":"Example Heading","body":"Hello Rust!"}"#),
    ]);

    let mut core = DustCore::new(manifest, mock_llm.clone())
        .with_mcp_client("fetcher", Box::new(mock_mcp.clone()));

    let user_input = "Extract heading and body from https://example.com";
    let result = core
        .execute(user_input)
        .await
        .expect("Multi-turn execution should succeed");

    assert_eq!(
        result,
        r#"{"heading":"Example Heading","body":"Hello Rust!"}"#
    );

    // Verify LLM interactions
    assert_eq!(mock_llm.call_count(), 2);
    let calls = mock_llm.recorded_calls();

    // Turn 1 should have tools available
    assert!(calls[0].tools.is_some());
    let tools = calls[0].tools.as_ref().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "fetcher__fetch");

    // Turn 2 messages should include assistant tool call & tool result
    let turn2_messages = &calls[1].messages;
    assert_eq!(turn2_messages.len(), 4);
    assert_eq!(turn2_messages[0].role, "system");
    assert_eq!(turn2_messages[1].role, "user");
    assert_eq!(turn2_messages[2].role, "assistant");
    assert_eq!(turn2_messages[3].role, "tool");
    assert_eq!(
        turn2_messages[3].tool_call_id,
        Some("call_crawl_999".to_string())
    );

    // Verify MCP tool execution
    let mcp_calls = mock_mcp.recorded_calls();
    assert_eq!(mcp_calls.len(), 1);
    assert_eq!(mcp_calls[0].0, "fetch");
    assert_eq!(mcp_calls[0].1, json!({"url": "https://example.com"}));

    core.shutdown().await.expect("Shutdown should succeed");
}

#[tokio::test]
async fn test_load_manifest_from_file_and_execute() {
    let manifest_path = Path::new("apps/patcher.json");
    assert!(manifest_path.exists(), "apps/patcher.json must exist");

    let mock_llm = MockLlmProvider::with_responses(vec![LlmResponse::text(
        "<<<<<<< SEARCH\nfoo\n=======\nbar\n>>>>>>> REPLACE",
    )]);

    let mut core = DustCore::load_manifest(manifest_path, mock_llm.clone())
        .expect("Should load manifest file");

    assert_eq!(core.manifest().name.as_deref(), Some("patcher"));

    let output = core
        .execute("Change foo to bar")
        .await
        .expect("Execution should succeed");

    assert_eq!(output, "<<<<<<< SEARCH\nfoo\n=======\nbar\n>>>>>>> REPLACE");
    assert_eq!(mock_llm.call_count(), 1);
}

#[tokio::test]
async fn test_tool_execution_error_handling() {
    let manifest = AppManifest {
        schema: None,
        name: Some("error_tester".to_string()),
        description: None,
        system_prompt: Some("You handle errors.".to_string()),
        default_model: None,
        mcp_servers: HashMap::new(),
        output_format: None,
        max_turns: None,
    };

    // Tool call for non-existent server
    let tool_call = ToolCall::new("call_missing_1", "nonexistent__tool", "{}");

    let mock_llm = MockLlmProvider::with_responses(vec![
        LlmResponse::tool_calls(vec![tool_call]),
        LlmResponse::text("Handled missing tool gracefully"),
    ]);

    let mut core = DustCore::new(manifest, mock_llm.clone());

    let result = core
        .execute("Trigger error")
        .await
        .expect("Should not crash on tool error");
    assert_eq!(result, "Handled missing tool gracefully");

    // The tool message should contain the error JSON
    let calls = mock_llm.recorded_calls();
    assert_eq!(calls.len(), 2);
    let tool_msg = &calls[1].messages[3];
    assert_eq!(tool_msg.role, "tool");
    assert!(
        tool_msg.content.as_ref().unwrap().contains("error"),
        "Tool content should contain error details"
    );
}

#[tokio::test]
async fn test_max_turns_limit() {
    let manifest = AppManifest {
        schema: None,
        name: Some("loop_tester".to_string()),
        description: None,
        system_prompt: None,
        default_model: None,
        mcp_servers: HashMap::new(),
        output_format: None,
        max_turns: None,
    };

    // Infinite tool calling loop
    let mock_llm = MockLlmProvider::new().with_handler(|_msgs, _tools| {
        Ok(LlmResponse::tool_calls(vec![ToolCall::new(
            "loop_id",
            "fake__ping",
            "{}",
        )]))
    });

    let mut core = DustCore::new(manifest, mock_llm.clone()).with_max_turns(3);

    let result = core
        .execute("Infinite loop")
        .await
        .expect("Should terminate on max turns");
    // Max turns reached without final content returns empty string
    assert_eq!(result, "");
    assert_eq!(mock_llm.call_count(), 3);
}

#[test]
fn test_mcp_server_config_creation() {
    let config = McpServerConfig::new("uvx").with_args(["mcp-server-fetch".to_string()]);
    assert_eq!(config.command, "uvx");
    assert_eq!(config.args, vec!["mcp-server-fetch"]);
}
