use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dustagent::DustError;
use dustagent::adapters::mcp_stdio::McpStdioClient;
use dustagent::domain::manifest::{AppManifest, McpServerConfig, resolve_manifest_path};
use dustagent::ports::llm::ToolDefinition;
use dustagent::ports::mcp::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpClient, McpTool};
use serde_json::json;
use tempfile::tempdir;

// ============================================================================
// 1. AppManifest & McpServerConfig Domain Tests
// ============================================================================

#[test]
fn test_parse_crawler_manifest() {
    let manifest_path = Path::new("apps/crawler.json");
    let manifest = AppManifest::from_file(manifest_path).expect("apps/crawler.json should parse");

    assert_eq!(manifest.name.as_deref(), Some("crawler"));
    assert!(
        manifest
            .description
            .as_deref()
            .unwrap_or("")
            .contains("Web Scraper")
    );
    assert_eq!(manifest.default_model.as_deref(), Some("gpt-4o-mini"));
    assert!(manifest.system_prompt.is_some());
    assert_eq!(manifest.output_format.as_deref(), Some("raw_json"));

    // Verify MCP servers
    assert!(manifest.mcp_servers.contains_key("fetch"));
    let fetch_srv = &manifest.mcp_servers["fetch"];
    assert_eq!(fetch_srv.command, "uvx");
    assert_eq!(fetch_srv.args, vec!["mcp-server-fetch"]);
    assert!(fetch_srv.env.is_none());
}

#[test]
fn test_parse_patcher_manifest() {
    let manifest_path = Path::new("apps/patcher.json");
    let manifest = AppManifest::from_file(manifest_path).expect("apps/patcher.json should parse");

    assert_eq!(manifest.name.as_deref(), Some("patcher"));
    assert_eq!(manifest.default_model.as_deref(), Some("gpt-4o-mini"));
    assert_eq!(
        manifest.output_format.as_deref(),
        Some("search_replace_patch")
    );
    assert!(manifest.mcp_servers.is_empty());
}

#[test]
fn test_manifest_builder_and_roundtrip_serialization() {
    let mut env = HashMap::new();
    env.insert("API_KEY".to_string(), "secret123".to_string());

    let server_cfg = McpServerConfig::new("python3")
        .with_args(["-m", "my_mcp_server"])
        .with_env(env.clone());

    let manifest = AppManifest::new()
        .with_name("test-app")
        .with_description("A test agent manifest")
        .with_system_prompt("You are a test agent.")
        .with_default_model("gpt-4o")
        .with_mcp_server("python_srv", server_cfg);

    let json_str = manifest
        .to_json_string()
        .expect("Serialization should succeed");

    let deserialized =
        AppManifest::from_json_str(&json_str).expect("Deserialization should succeed");

    assert_eq!(deserialized.name.as_deref(), Some("test-app"));
    assert_eq!(
        deserialized.description.as_deref(),
        Some("A test agent manifest")
    );
    assert_eq!(
        deserialized.system_prompt.as_deref(),
        Some("You are a test agent.")
    );
    assert_eq!(deserialized.default_model.as_deref(), Some("gpt-4o"));

    let srv = deserialized.mcp_servers.get("python_srv").unwrap();
    assert_eq!(srv.command, "python3");
    assert_eq!(srv.args, vec!["-m", "my_mcp_server"]);
    assert_eq!(
        srv.env.as_ref().unwrap().get("API_KEY").unwrap(),
        "secret123"
    );
}

#[test]
fn test_manifest_missing_file_error() {
    let result = AppManifest::from_file("apps/non_existent_app_xyz.json");
    assert!(result.is_err());
    match result.err().unwrap() {
        DustError::Manifest(msg) => {
            assert!(msg.contains("Failed to read manifest file"));
        }
        other => panic!("Expected DustError::Manifest, got: {other:?}"),
    }
}

#[test]
fn test_manifest_invalid_json_error() {
    let invalid_json = r#"{ "name": "bad", "mcp_servers": "not_an_object" }"#;
    let result = AppManifest::from_json_str(invalid_json);
    assert!(result.is_err());
    match result.err().unwrap() {
        DustError::Manifest(msg) => {
            assert!(msg.contains("Failed to parse manifest JSON"));
        }
        other => panic!("Expected DustError::Manifest, got: {other:?}"),
    }
}

#[test]
fn test_resolve_manifest_path() {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // 1. By app name without extension in apps/
    let resolved = resolve_manifest_path("crawler", &base_dir).expect("Should resolve 'crawler'");
    assert!(resolved.ends_with("apps/crawler.json"));
    assert!(resolved.is_file());

    // 2. By app name with extension in apps/
    let resolved2 =
        resolve_manifest_path("crawler.json", &base_dir).expect("Should resolve 'crawler.json'");
    assert!(resolved2.ends_with("apps/crawler.json"));

    // 3. Direct relative path
    let resolved3 = resolve_manifest_path("apps/crawler.json", &base_dir)
        .expect("Should resolve direct path 'apps/crawler.json'");
    assert!(resolved3.is_file());

    // 4. In a temporary directory
    let dir = tempdir().unwrap();
    let apps_dir = dir.path().join("apps");
    std::fs::create_dir(&apps_dir).unwrap();
    let custom_app = apps_dir.join("custom.json");
    std::fs::write(&custom_app, r#"{"name": "custom"}"#).unwrap();

    let custom_resolved = resolve_manifest_path("custom", dir.path())
        .expect("Should resolve custom manifest in tempdir apps/");
    assert_eq!(custom_resolved, custom_app);

    // 5. Non-existent returns error
    let err = resolve_manifest_path("does_not_exist_at_all", &base_dir);
    assert!(err.is_err());
}

// ============================================================================
// 2. JSON-RPC 2.0 Serialization & Deserialization Tests
// ============================================================================

#[test]
fn test_jsonrpc_request_and_notification_serialization() {
    // Request with params
    let req = JsonRpcRequest::new(1, "initialize", Some(json!({"version": "1.0"})));
    let val = serde_json::to_value(&req).unwrap();
    assert_eq!(val["jsonrpc"], "2.0");
    assert_eq!(val["id"], 1);
    assert_eq!(val["method"], "initialize");
    assert_eq!(val["params"]["version"], "1.0");

    // Request without params
    let req2 = JsonRpcRequest::new(2, "tools/list", None);
    let val2 = serde_json::to_value(&req2).unwrap();
    assert_eq!(val2["id"], 2);
    assert_eq!(val2["method"], "tools/list");
    assert!(val2.get("params").is_none());

    // Notification (no ID)
    let notif = JsonRpcRequest::notification("notifications/initialized", None);
    let val_notif = serde_json::to_value(&notif).unwrap();
    assert_eq!(val_notif["method"], "notifications/initialized");
    assert!(val_notif.get("id").is_none());
}

#[test]
fn test_jsonrpc_response_deserialization() {
    // Success response with numeric ID
    let raw_success = r#"{"jsonrpc": "2.0", "id": 42, "result": {"status": "ok"}}"#;
    let resp: JsonRpcResponse = serde_json::from_str(raw_success).unwrap();
    assert_eq!(resp.id_as_u64(), Some(42));
    assert!(resp.error.is_none());
    assert_eq!(resp.result.unwrap()["status"], "ok");

    // Success response with string ID (some RPC servers return strings)
    let raw_str_id = r#"{"jsonrpc": "2.0", "id": "42", "result": 100}"#;
    let resp_str: JsonRpcResponse = serde_json::from_str(raw_str_id).unwrap();
    assert_eq!(resp_str.id_as_u64(), Some(42));
    assert_eq!(resp_str.result.unwrap(), 100);

    // Error response
    let raw_err =
        r#"{"jsonrpc": "2.0", "id": 99, "error": {"code": -32601, "message": "Method not found"}}"#;
    let resp_err: JsonRpcResponse = serde_json::from_str(raw_err).unwrap();
    assert_eq!(resp_err.id_as_u64(), Some(99));
    let err = resp_err.error.unwrap();
    assert_eq!(
        err,
        JsonRpcError {
            code: -32601,
            message: "Method not found".to_string(),
            data: None,
        }
    );
    assert_eq!(err.code, -32601);
    assert_eq!(err.message, "Method not found");
    assert_eq!(err.to_string(), "Code -32601: Method not found");
}

#[test]
fn test_mcp_tool_schema_and_conversion() {
    // Standard MCP camelCase inputSchema
    let raw_tool_mcp = r#"{
        "name": "fetch",
        "description": "Fetch URL content",
        "inputSchema": {
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"]
        }
    }"#;
    let tool: McpTool = serde_json::from_str(raw_tool_mcp).unwrap();
    assert_eq!(tool.name, "fetch");
    assert_eq!(tool.description.as_deref(), Some("Fetch URL content"));
    assert_eq!(tool.input_schema["type"], "object");
    assert_eq!(tool.input_schema["required"][0], "url");

    // Rust snake_case input_schema
    let raw_tool_snake = r#"{
        "name": "calc",
        "input_schema": { "type": "object" }
    }"#;
    let tool2: McpTool = serde_json::from_str(raw_tool_snake).unwrap();
    assert_eq!(tool2.name, "calc");
    assert!(tool2.description.is_none());
    assert_eq!(tool2.input_schema["type"], "object");

    // Interoperability with ToolDefinition
    let td: ToolDefinition = tool.clone().into();
    assert_eq!(td.name, "fetch");
    assert_eq!(td.description, "Fetch URL content");
    assert_eq!(td.parameters["type"], "object");

    let back_to_mcp: McpTool = td.into();
    assert_eq!(back_to_mcp.name, "fetch");
    assert_eq!(
        back_to_mcp.description.as_deref(),
        Some("Fetch URL content")
    );
}

// ============================================================================
// 3. Tokio McpStdioClient Integration Tests (Mock MCP Subprocess)
// ============================================================================

/// Python mock MCP server script responding to initialize, notifications, tools/list, and tools/call.
const MOCK_MCP_SERVER_SCRIPT: &str = r#"
import sys
import json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    req_id = req.get("id")

    if method == "initialize":
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-mcp-server", "version": "1.0.0"}
            }
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        # Handshake notification received, no response
        pass
    elif method == "tools/list":
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echoes back the input message",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"msg": {"type": "string"}},
                            "required": ["msg"]
                        }
                    },
                    {
                        "name": "add",
                        "description": "Adds two numbers",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "a": {"type": "number"},
                                "b": {"type": "number"}
                            },
                            "required": ["a", "b"]
                        }
                    }
                ]
            }
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        params = req.get("params", {})
        tool_name = params.get("name")
        args = params.get("arguments", {})

        if tool_name == "echo":
            msg = args.get("msg", "")
            resp = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [{"type": "text", "text": f"echo: {msg}"}]
                }
            }
        elif tool_name == "add":
            a = args.get("a", 0)
            b = args.get("b", 0)
            resp = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {"sum": a + b}
            }
        else:
            resp = {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32601,
                    "message": f"Tool '{tool_name}' not found"
                }
            }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    else:
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32601, "message": f"Method '{method}' not found"}
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

#[tokio::test]
async fn test_mcp_stdio_client_full_lifecycle() {
    let mut client = McpStdioClient::spawn(
        "python3",
        &["-c".to_string(), MOCK_MCP_SERVER_SCRIPT.to_string()],
        None,
    )
    .expect("Should spawn mock python MCP server");

    assert!(!client.is_initialized());

    // 1. Explicit initialize handshake
    client
        .initialize()
        .await
        .expect("initialize handshake should succeed");
    assert!(client.is_initialized());

    // Calling initialize a second time should be idempotent
    client
        .initialize()
        .await
        .expect("idempotent initialize should succeed");

    // 2. List tools
    let tools = client
        .list_tools()
        .await
        .expect("tools/list should succeed");
    assert_eq!(tools.len(), 2);

    let echo_tool = tools.iter().find(|t| t.name == "echo").unwrap();
    assert_eq!(
        echo_tool.description.as_deref(),
        Some("Echoes back the input message")
    );
    assert_eq!(
        echo_tool.input_schema["properties"]["msg"]["type"],
        "string"
    );

    let add_tool = tools.iter().find(|t| t.name == "add").unwrap();
    assert_eq!(add_tool.description.as_deref(), Some("Adds two numbers"));

    // 3. Call tool 'echo'
    let echo_res = client
        .call_tool("echo", json!({"msg": "Hello DustAgent"}))
        .await
        .expect("call_tool echo should succeed");
    assert_eq!(echo_res["content"][0]["text"], "echo: Hello DustAgent");

    // 4. Call tool 'add'
    let add_res = client
        .call_tool("add", json!({"a": 15, "b": 27}))
        .await
        .expect("call_tool add should succeed");
    assert_eq!(add_res["sum"], 42);

    // 5. Clean graceful close
    client.close().await.expect("close should succeed");
    assert!(!client.is_initialized());
}

#[tokio::test]
async fn test_mcp_stdio_client_auto_initialize_on_tool_call() {
    // Test that list_tools or call_tool automatically initializes if not explicitly done
    let mut client = McpStdioClient::spawn(
        "python3",
        &["-c".to_string(), MOCK_MCP_SERVER_SCRIPT.to_string()],
        None,
    )
    .expect("Should spawn mock python MCP server");

    assert!(!client.is_initialized());

    // Directly call list_tools without calling initialize first
    let tools = client
        .list_tools()
        .await
        .expect("list_tools should auto-init");
    assert!(client.is_initialized());
    assert_eq!(tools.len(), 2);

    client.close().await.expect("close should succeed");
}

#[tokio::test]
async fn test_mcp_stdio_client_error_response_handling() {
    let mut client = McpStdioClient::new(
        "python3",
        &["-c".to_string(), MOCK_MCP_SERVER_SCRIPT.to_string()],
        None,
    )
    .await
    .expect("McpStdioClient::new should spawn and init");

    // Call unknown tool
    let err = client
        .call_tool("non_existent_tool", json!({}))
        .await
        .expect_err("Calling unknown tool must fail with MCP error");

    match err {
        DustError::Mcp(msg) => {
            assert!(msg.contains("Tool 'non_existent_tool' not found"));
        }
        other => panic!("Expected DustError::Mcp, got: {other:?}"),
    }

    client.close().await.expect("close should succeed");
}

#[tokio::test]
async fn test_mcp_stdio_client_from_config() {
    let config = McpServerConfig::new("python3")
        .with_args(["-c".to_string(), MOCK_MCP_SERVER_SCRIPT.to_string()]);

    let mut client = McpStdioClient::from_config_and_init(&config)
        .await
        .expect("Should spawn and init from config");

    let tools = client.list_tools().await.expect("Should list tools");
    assert_eq!(tools.len(), 2);

    client.close().await.expect("Should close cleanly");
}

#[tokio::test]
async fn test_mcp_stdio_client_handles_unexpected_server_termination() {
    // Child process that immediately exits with status code 3
    let mut client = McpStdioClient::spawn(
        "python3",
        &["-c".to_string(), "import sys; sys.exit(3)".to_string()],
        None,
    )
    .expect("Spawn should succeed");

    let result = client.initialize().await;
    assert!(result.is_err());
    match result.err().unwrap() {
        DustError::Mcp(msg) => {
            assert!(msg.contains("terminated unexpectedly"));
        }
        other => panic!("Expected DustError::Mcp, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_mcp_stdio_client_drop_terminates_child_process() {
    // Spawns a process sleeping for 60 seconds
    let client = McpStdioClient::spawn(
        "python3",
        &["-c".to_string(), "import time; time.sleep(60)".to_string()],
        None,
    )
    .expect("Spawn sleeping process should succeed");

    // Drop client immediately; Drop trait should kill process without hanging
    drop(client);
    // If drop didn't kill or hung, this test would time out or leak
}
