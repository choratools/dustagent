/// Built-in native tools — no external process, no MCP server, zero overhead.
///
/// Registered under the `builtin` namespace:
///   `builtin__sleep`        — async delay (ms)
///   `builtin__timestamp`    — current UTC time (Unix ms or ISO8601)
///   `builtin__uuid`         — UUID v4
///   `builtin__env_get`      — read an environment variable
///   `builtin__hash`         — SHA-256 hex digest of a string
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::ports::mcp::{McpClient, McpTool};
use crate::{DustError, Result};

/// A zero-process, pure-Rust implementation of core utility tools.
///
/// Implements the same `McpClient` trait as external stdio servers so that
/// `DustCore` can treat built-ins and external MCPs identically.
pub struct BuiltinToolClient;

impl BuiltinToolClient {
    pub fn new() -> Self {
        Self
    }

    /// All tool schemas exposed by the built-in client.
    fn tool_definitions() -> Vec<McpTool> {
        vec![
            McpTool::new(
                "sleep",
                Some("Sleep (async, non-blocking) for the specified number of milliseconds.".into()),
                json!({
                    "type": "object",
                    "properties": {
                        "ms": {
                            "type": "integer",
                            "description": "Duration in milliseconds to sleep.",
                            "minimum": 0,
                            "maximum": 300_000
                        }
                    },
                    "required": ["ms"]
                }),
            ),
            McpTool::new(
                "timestamp",
                Some("Return the current UTC timestamp as Unix epoch milliseconds and ISO-8601 string.".into()),
                json!({
                    "type": "object",
                    "properties": {
                        "format": {
                            "type": "string",
                            "enum": ["unix_ms", "iso8601", "both"],
                            "description": "Output format. Defaults to 'both'.",
                            "default": "both"
                        }
                    }
                }),
            ),
            McpTool::new(
                "uuid",
                Some("Generate a new random UUID v4.".into()),
                json!({ "type": "object", "properties": {} }),
            ),
            McpTool::new(
                "env_get",
                Some("Read the value of an environment variable. Returns null if not set.".into()),
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Environment variable name."
                        }
                    },
                    "required": ["name"]
                }),
            ),
            McpTool::new(
                "hash",
                Some("Compute a SHA-256 hex digest of the given input string.".into()),
                json!({
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "String to hash."
                        }
                    },
                    "required": ["input"]
                }),
            ),
        ]
    }
}

impl Default for BuiltinToolClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl McpClient for BuiltinToolClient {
    async fn initialize(&mut self) -> Result<()> {
        // No-op: built-in tools need no handshake.
        Ok(())
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        Ok(Self::tool_definitions())
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            "sleep" => {
                let ms = arguments["ms"]
                    .as_u64()
                    .ok_or_else(|| DustError::Mcp("builtin::sleep requires `ms` (u64)".into()))?;
                tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
                Ok(json!({ "slept_ms": ms, "status": "ok" }))
            }

            "timestamp" => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| DustError::Mcp(e.to_string()))?;
                let unix_ms = now.as_millis() as u64;

                // ISO-8601: manual construction from epoch (no chrono dependency)
                let secs = now.as_secs();
                let iso = epoch_to_iso8601(secs);

                let fmt = arguments["format"].as_str().unwrap_or("both");
                let result = match fmt {
                    "unix_ms" => json!({ "unix_ms": unix_ms }),
                    "iso8601" => json!({ "iso8601": iso }),
                    _ => json!({ "unix_ms": unix_ms, "iso8601": iso }),
                };
                Ok(result)
            }

            "uuid" => {
                let id = Uuid::new_v4().to_string();
                Ok(json!({ "uuid": id }))
            }

            "env_get" => {
                let var_name = arguments["name"].as_str().ok_or_else(|| {
                    DustError::Mcp("builtin::env_get requires `name` (str)".into())
                })?;
                let value = std::env::var(var_name).ok();
                Ok(json!({ "name": var_name, "value": value }))
            }

            "hash" => {
                let input = arguments["input"]
                    .as_str()
                    .ok_or_else(|| DustError::Mcp("builtin::hash requires `input` (str)".into()))?;
                let digest = sha256_hex(input);
                Ok(json!({ "sha256": digest, "input_len": input.len() }))
            }

            other => Err(DustError::Mcp(format!("Unknown built-in tool: '{other}'"))),
        }
    }

    async fn close(&mut self) -> Result<()> {
        // Nothing to close — no subprocess, no socket.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Minimal helpers — no external crate needed
// ---------------------------------------------------------------------------

/// Converts Unix epoch seconds to a bare ISO-8601 UTC string without `chrono`.
fn epoch_to_iso8601(secs: u64) -> String {
    // Days from 1970-01-01
    let mut days = secs / 86400;
    let time_of_day = secs % 86400;
    let hh = time_of_day / 3600;
    let mm = (time_of_day % 3600) / 60;
    let ss = time_of_day % 60;

    // Gregorian calendar decomposition
    let mut year = 1970u64;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let months = [
        31u64,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for &m in &months {
        if days < m {
            break;
        }
        days -= m;
        month += 1;
    }
    let day = days + 1;
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

/// Minimal SHA-256 — uses `sha2` if available, otherwise a pure-Rust fallback.
/// We add `sha2` to Cargo.toml so this is always available.
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(input.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sleep_short() {
        let mut client = BuiltinToolClient::new();
        let res = client
            .call_tool("sleep", json!({ "ms": 50 }))
            .await
            .unwrap();
        assert_eq!(res["slept_ms"], 50);
        assert_eq!(res["status"], "ok");
    }

    #[tokio::test]
    async fn test_timestamp_both() {
        let mut client = BuiltinToolClient::new();
        let res = client.call_tool("timestamp", json!({})).await.unwrap();
        assert!(res["unix_ms"].as_u64().unwrap() > 0);
        let iso = res["iso8601"].as_str().unwrap();
        assert!(iso.ends_with('Z'));
    }

    #[tokio::test]
    async fn test_uuid_format() {
        let mut client = BuiltinToolClient::new();
        let res = client.call_tool("uuid", json!({})).await.unwrap();
        let id = res["uuid"].as_str().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|&c| c == '-').count(), 4);
    }

    #[tokio::test]
    async fn test_env_get_existing() {
        // SAFETY: test-only, single-threaded test context
        unsafe { std::env::set_var("DUST_TEST_VAR", "hello") };
        let mut client = BuiltinToolClient::new();
        let res = client
            .call_tool("env_get", json!({ "name": "DUST_TEST_VAR" }))
            .await
            .unwrap();
        assert_eq!(res["value"], "hello");
    }

    #[tokio::test]
    async fn test_env_get_missing() {
        let mut client = BuiltinToolClient::new();
        let res = client
            .call_tool("env_get", json!({ "name": "DUST_NONEXISTENT_ZZZ" }))
            .await
            .unwrap();
        assert!(res["value"].is_null());
    }

    #[tokio::test]
    async fn test_hash_known_value() {
        let mut client = BuiltinToolClient::new();
        let res = client
            .call_tool("hash", json!({ "input": "hello" }))
            .await
            .unwrap();
        // SHA-256 of "hello"
        assert_eq!(
            res["sha256"].as_str().unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[tokio::test]
    async fn test_list_tools() {
        let mut client = BuiltinToolClient::new();
        let tools = client.list_tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"sleep"));
        assert!(names.contains(&"timestamp"));
        assert!(names.contains(&"uuid"));
        assert!(names.contains(&"env_get"));
        assert!(names.contains(&"hash"));
    }
}
