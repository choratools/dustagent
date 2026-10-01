use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::ports::llm::ToolDefinition;

/// Tool definition exposed by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    /// The unique name of the tool.
    pub name: String,
    /// A human-readable description of what the tool does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema defining the input parameters for the tool.
    /// Accepts both `input_schema` and MCP standard `inputSchema`.
    #[serde(alias = "inputSchema", default = "default_input_schema")]
    pub input_schema: Value,
}

fn default_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {}
    })
}

impl McpTool {
    /// Create a new `McpTool`.
    pub fn new(name: impl Into<String>, description: Option<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description,
            input_schema,
        }
    }
}

impl From<McpTool> for ToolDefinition {
    fn from(mcp: McpTool) -> Self {
        Self {
            name: mcp.name,
            description: mcp.description.unwrap_or_default(),
            parameters: mcp.input_schema,
        }
    }
}

impl From<ToolDefinition> for McpTool {
    fn from(td: ToolDefinition) -> Self {
        Self {
            name: td.name,
            description: if td.description.is_empty() {
                None
            } else {
                Some(td.description)
            },
            input_schema: td.parameters,
        }
    }
}

/// JSON-RPC 2.0 Request or Notification object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Create a new JSON-RPC 2.0 request with an integer ID.
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::Number(id.into())),
            method: method.into(),
            params,
        }
    }

    /// Create a new JSON-RPC 2.0 notification (no ID).
    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 Response object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Create a success response.
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::Number(id.into())),
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: Option<u64>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.map(|i| Value::Number(i.into())),
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Extract request ID as `u64` if numeric or numeric string.
    pub fn id_as_u64(&self) -> Option<u64> {
        self.id.as_ref().and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
    }
}

/// JSON-RPC 2.0 Error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref data) = self.data {
            write!(f, "Code {}: {} ({})", self.code, self.message, data)
        } else {
            write!(f, "Code {}: {}", self.code, self.message)
        }
    }
}

/// Abstract port for Model Context Protocol (MCP) clients.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// Initialize the MCP session with the server.
    async fn initialize(&mut self) -> Result<()>;

    /// List all tools provided by the MCP server.
    async fn list_tools(&mut self) -> Result<Vec<McpTool>>;

    /// Execute a tool by name with the given JSON arguments.
    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value>;

    /// Gracefully terminate the MCP client and its underlying connection/process.
    async fn close(&mut self) -> Result<()>;
}

#[async_trait]
impl<M: McpClient + ?Sized> McpClient for Box<M> {
    async fn initialize(&mut self) -> Result<()> {
        (**self).initialize().await
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        (**self).list_tools().await
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        (**self).call_tool(name, arguments).await
    }

    async fn close(&mut self) -> Result<()> {
        (**self).close().await
    }
}

/// A thread-safe mock MCP client for testing.
#[derive(Clone, Default)]
pub struct MockMcpClient {
    pub tools: Vec<McpTool>,
    pub tool_results: Arc<Mutex<HashMap<String, Value>>>,
    pub recorded_calls: Arc<Mutex<Vec<(String, Value)>>>,
    pub initialized: bool,
    pub closed: bool,
}

impl MockMcpClient {
    pub fn new(tools: Vec<McpTool>) -> Self {
        Self {
            tools,
            tool_results: Arc::new(Mutex::new(HashMap::new())),
            recorded_calls: Arc::new(Mutex::new(Vec::new())),
            initialized: false,
            closed: false,
        }
    }

    pub fn with_tool_result(self, tool_name: impl Into<String>, result: Value) -> Self {
        self.tool_results
            .lock()
            .unwrap()
            .insert(tool_name.into(), result);
        self
    }

    pub fn add_tool_result(&self, tool_name: impl Into<String>, result: Value) {
        self.tool_results
            .lock()
            .unwrap()
            .insert(tool_name.into(), result);
    }

    pub fn recorded_calls(&self) -> Vec<(String, Value)> {
        self.recorded_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl McpClient for MockMcpClient {
    async fn initialize(&mut self) -> Result<()> {
        self.initialized = true;
        Ok(())
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        Ok(self.tools.clone())
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.recorded_calls
            .lock()
            .unwrap()
            .push((name.to_string(), arguments.clone()));

        if let Some(res) = self.tool_results.lock().unwrap().get(name) {
            Ok(res.clone())
        } else {
            Ok(serde_json::json!({
                "status": "success",
                "tool": name,
                "arguments": arguments,
            }))
        }
    }

    async fn close(&mut self) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}
