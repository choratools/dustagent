use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::domain::manifest::McpServerConfig;
use crate::ports::mcp::{McpClient, McpTool};
use crate::{DustError, Result};

/// Stdio-based MCP Client executing over child process standard I/O streams using JSON-RPC 2.0.
pub struct McpStdioClient {
    child: Option<Child>,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    req_id: AtomicU64,
    initialized: bool,
}

impl McpStdioClient {
    /// Spawns a child process without initializing the MCP session immediately.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
    ) -> Result<Self> {
        Self::spawn_with_stderr(command, args, env, false)
    }

    /// Spawns a child process with configurable stderr inheritance.
    pub fn spawn_with_stderr(
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
        inherit_stderr: bool,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        if let Some(env_map) = env {
            cmd.envs(env_map);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        if inherit_stderr {
            cmd.stderr(Stdio::inherit());
        } else {
            cmd.stderr(Stdio::null());
        }
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| DustError::Mcp(format!("Failed to spawn MCP server '{command}': {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DustError::Mcp("Failed to capture MCP stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DustError::Mcp("Failed to capture MCP stdout".to_string()))?;

        let lines = BufReader::new(stdout).lines();

        Ok(Self {
            child: Some(child),
            stdin,
            lines,
            req_id: AtomicU64::new(1),
            initialized: false,
        })
    }

    /// Spawns an uninitialized client from an `McpServerConfig`.
    pub fn from_config(config: &McpServerConfig) -> Result<Self> {
        Self::spawn(&config.command, &config.args, config.env.as_ref())
    }

    /// Spawns a child process and completes the MCP initialization handshake.
    pub async fn new(
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
    ) -> Result<Self> {
        let mut client = Self::spawn(command, args, env)?;
        client.initialize().await?;
        Ok(client)
    }

    /// Spawns a child process and completes the MCP initialization handshake (alias for new).
    pub async fn start_and_init(
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
    ) -> Result<Self> {
        Self::new(command, args, env).await
    }

    /// Spawns a child process from config and completes the MCP initialization handshake.
    pub async fn from_config_and_init(config: &McpServerConfig) -> Result<Self> {
        let mut client = Self::from_config(config)?;
        client.initialize().await?;
        Ok(client)
    }

    /// Check if the MCP session has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Sends a JSON-RPC 2.0 request and returns the parsed result.
    pub async fn send_rpc(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.req_id.fetch_add(1, Ordering::SeqCst);
        let mut req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        if let Some(p) = params {
            req["params"] = p;
        }

        let mut payload = serde_json::to_string(&req)?;
        payload.push('\n');

        self.stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| DustError::Mcp(format!("Failed to write to MCP stdin: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| DustError::Mcp(format!("Failed to flush MCP stdin: {e}")))?;

        loop {
            let line = match self.lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => {
                    let status_msg = if let Some(ref mut child) = self.child {
                        match child.try_wait() {
                            Ok(Some(status)) => format!(" (process exited with status {status})"),
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    };
                    return Err(DustError::Mcp(format!(
                        "MCP server terminated unexpectedly while executing '{method}'{status_msg}"
                    )));
                }
                Err(e) => {
                    return Err(DustError::Mcp(format!(
                        "Error reading from MCP stdout while executing '{method}': {e}"
                    )));
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let resp: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("Ignored unparseable line from MCP server: {trimmed} ({e})");
                    continue;
                }
            };

            // If this is a response with an ID, verify it matches
            if let Some(resp_id) = resp.get("id") {
                let matches = resp_id.as_u64() == Some(id)
                    || resp_id.as_str().and_then(|s| s.parse::<u64>().ok()) == Some(id);

                if matches {
                    if let Some(err) = resp.get("error") {
                        return Err(DustError::Mcp(format!("MCP RPC Error ({method}): {err}")));
                    }
                    return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            // Skip notifications or responses for different IDs
        }
    }

    /// Sends a JSON-RPC 2.0 notification without waiting for a response.
    pub async fn send_notification(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let mut req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if let Some(p) = params {
            req["params"] = p;
        }

        let mut payload = serde_json::to_string(&req)?;
        payload.push('\n');

        self.stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| {
                DustError::Mcp(format!("Failed to write notification to MCP stdin: {e}"))
            })?;
        self.stdin.flush().await.map_err(|e| {
            DustError::Mcp(format!("Failed to flush notification to MCP stdin: {e}"))
        })?;
        Ok(())
    }
}

#[async_trait]
impl McpClient for McpStdioClient {
    async fn initialize(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }

        self.send_rpc(
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "dustagent",
                    "version": "0.1.0"
                }
            })),
        )
        .await?;

        self.send_notification("notifications/initialized", None)
            .await?;
        self.initialized = true;
        Ok(())
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        if !self.initialized {
            self.initialize().await?;
        }

        let res = self.send_rpc("tools/list", None).await?;
        let mut tools = Vec::new();

        let tool_array = res
            .get("tools")
            .and_then(|t| t.as_array())
            .or_else(|| res.as_array());

        if let Some(tool_list) = tool_array {
            for t in tool_list {
                let name = t
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let description = t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string());
                let input_schema = t
                    .get("inputSchema")
                    .or_else(|| t.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));

                tools.push(McpTool::new(name, description, input_schema));
            }
        }

        Ok(tools)
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        if !self.initialized {
            self.initialize().await?;
        }

        self.send_rpc(
            "tools/call",
            Some(serde_json::json!({
                "name": name,
                "arguments": arguments,
            })),
        )
        .await
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.initialized = false;
        Ok(())
    }
}

impl Drop for McpStdioClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}
