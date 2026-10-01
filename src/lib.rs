pub mod adapters;
pub mod application;
pub mod domain;
pub mod error;
pub mod ports;

pub use adapters::fuzzy_patch::FuzzyPatcher;
pub use adapters::mcp_stdio::McpStdioClient;
pub use adapters::openai::{MockLlmProvider, OpenAiProvider};
pub use application::core::DustCore;
pub use domain::manifest::{AppManifest, McpServerConfig, resolve_manifest_path};
pub use domain::patch::{LineRange, PatchError, SearchReplaceBlock, extract_blocks};
pub use error::{DustError, Result};
pub use ports::llm::{ChatMessage, LlmProvider, LlmResponse, ToolCall, ToolDefinition};
pub use ports::mcp::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpClient, McpTool};
pub use ports::patcher::CodePatcher;
