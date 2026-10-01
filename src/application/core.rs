use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use tracing::warn;

use crate::adapters::builtin::BuiltinToolClient;
use crate::adapters::mcp_stdio::McpStdioClient;
use crate::domain::manifest::AppManifest;
use crate::ports::llm::{ChatMessage, LlmProvider, ToolDefinition};
use crate::ports::mcp::McpClient;
use crate::{DustError, Result};

/// Ultra-lightweight execution kernel orchestrating AppManifest, Scoped MCP Tools, and LLM Provider.
pub struct DustCore<P: LlmProvider> {
    manifest: AppManifest,
    provider: P,
    mcp_clients: HashMap<String, Box<dyn McpClient>>,
    max_turns: usize,
}

impl<P: LlmProvider> DustCore<P> {
    /// Creates a new `DustCore` instance with a loaded manifest and LLM provider.
    pub fn new(manifest: AppManifest, provider: P) -> Self {
        let max_turns = manifest.max_turns.unwrap_or(10);
        // Built-in tools are always available under the `builtin` namespace —
        // no manifest declaration required, zero subprocess overhead.
        let mut mcp_clients: HashMap<String, Box<dyn McpClient>> = HashMap::new();
        mcp_clients.insert(
            "builtin".to_string(),
            Box::new(BuiltinToolClient::new()),
        );
        Self {
            manifest,
            provider,
            mcp_clients,
            max_turns,
        }
    }

    /// Loads an `AppManifest` from a file path and creates an uninitialized `DustCore`.
    pub fn load_manifest(manifest_path: impl AsRef<Path>, provider: P) -> Result<Self> {
        let manifest = AppManifest::from_file(manifest_path)?;
        Ok(Self::new(manifest, provider))
    }

    /// Loads an `AppManifest` from a file and initializes all declared scoped MCP clients.
    pub async fn from_manifest_file(manifest_path: impl AsRef<Path>, provider: P) -> Result<Self> {
        let mut core = Self::load_manifest(manifest_path, provider)?;
        core.init_scoped_mcp().await?;
        Ok(core)
    }

    /// Sets the maximum conversation turns for tool calling loops (builder pattern).
    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Registers a scoped MCP client under a given server namespace (builder pattern).
    pub fn with_mcp_client(mut self, name: impl Into<String>, client: Box<dyn McpClient>) -> Self {
        self.mcp_clients.insert(name.into(), client);
        self
    }

    /// Registers a scoped MCP client under a given server namespace.
    pub fn register_mcp_client(&mut self, name: impl Into<String>, client: Box<dyn McpClient>) {
        self.mcp_clients.insert(name.into(), client);
    }

    /// Returns a reference to the loaded application manifest.
    pub fn manifest(&self) -> &AppManifest {
        &self.manifest
    }

    /// Returns a reference to the underlying LLM provider.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Starts ONLY the scoped MCP servers declared in the application manifest.
    pub async fn init_scoped_mcp(&mut self) -> Result<()> {
        for (name, config) in &self.manifest.mcp_servers {
            match McpStdioClient::start_and_init(&config.command, &config.args, config.env.as_ref())
                .await
            {
                Ok(client) => {
                    self.mcp_clients.insert(name.clone(), Box::new(client));
                }
                Err(err) => {
                    eprintln!("[dustagent] Warning: Failed to launch MCP server '{name}': {err}");
                    warn!("Failed to launch MCP server '{name}': {err}");
                }
            }
        }
        Ok(())
    }

    /// Collects and namespaces all tools from the scoped MCP client instances.
    /// Each tool is namespaced as `{server_name}__{tool_name}` to prevent name collisions.
    pub async fn gather_mcp_tools(&mut self) -> Result<Vec<ToolDefinition>> {
        let mut tools = Vec::new();
        for (srv_name, client) in &mut self.mcp_clients {
            match client.list_tools().await {
                Ok(srv_tools) => {
                    for tool in srv_tools {
                        let namespaced_name = format!("{srv_name}__{}", tool.name);
                        let mut td: ToolDefinition = tool.into();
                        td.name = namespaced_name;
                        tools.push(td);
                    }
                }
                Err(err) => {
                    eprintln!("[dustagent] Warning: Failed to list tools from '{srv_name}': {err}");
                    warn!("Failed to list tools from '{srv_name}': {err}");
                }
            }
        }
        Ok(tools)
    }

    /// Dispatches a namespaced tool execution to the appropriate MCP server.
    pub async fn execute_tool(&mut self, full_tool_name: &str, arguments: Value) -> Result<String> {
        let (srv_name, tool_name) = full_tool_name
            .split_once("__")
            .ok_or_else(|| DustError::Mcp(format!("Invalid scoped tool name: {full_tool_name}")))?;

        let client = self
            .mcp_clients
            .get_mut(srv_name)
            .ok_or_else(|| DustError::Mcp(format!("MCP server '{srv_name}' not running")))?;

        let res = client.call_tool(tool_name, arguments).await?;
        Ok(serde_json::to_string(&res)?)
    }

    /// Executes the specialized task in a pure single-shot or micro-loop pipeline.
    pub async fn execute(&mut self, user_input: &str) -> Result<String> {
        let system_prompt = self
            .manifest
            .system_prompt
            .as_deref()
            .unwrap_or("You are a helpful specialized assistant.");

        let mut messages = vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(user_input),
        ];

        let tools = self.gather_mcp_tools().await?;
        let tools_ref = if tools.is_empty() {
            None
        } else {
            Some(tools.as_slice())
        };

        let mut remaining_turns = self.max_turns;

        while remaining_turns > 0 {
            remaining_turns -= 1;
            let resp = self.provider.chat(&messages, tools_ref).await?;

            match resp.tool_calls {
                None => {
                    // Final content reached
                    return Ok(resp.content.unwrap_or_default());
                }
                Some(ref tool_calls) if tool_calls.is_empty() => {
                    // Final content reached
                    return Ok(resp.content.unwrap_or_default());
                }
                Some(ref tool_calls) => {
                    // Append assistant response with requested tool calls
                    messages.push(ChatMessage::assistant(
                        resp.content.clone(),
                        Some(tool_calls.clone()),
                    ));

                    // Execute each tool call and append tool result message
                    for tc in tool_calls {
                        let args_value: Value = tc
                            .parse_arguments()
                            .unwrap_or_else(|_| serde_json::json!({}));

                        let tool_output = match self
                            .execute_tool(&tc.function_name, args_value)
                            .await
                        {
                            Ok(output) => output,
                            Err(err) => serde_json::json!({ "error": err.to_string() }).to_string(),
                        };

                        messages.push(ChatMessage::tool(&tc.id, tool_output));
                    }
                }
            }
        }

        // Return empty string if max turns exhausted without final response
        Ok(String::new())
    }

    /// Cleanly and gracefully shuts down all scoped MCP child clients.
    pub async fn shutdown(&mut self) -> Result<()> {
        for (name, client) in &mut self.mcp_clients {
            if let Err(err) = client.close().await {
                warn!("Error closing MCP client '{name}': {err}");
            }
        }
        self.mcp_clients.clear();
        Ok(())
    }
}
