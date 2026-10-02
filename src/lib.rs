pub mod adapters;
pub mod application;
pub mod domain;
pub mod error;
pub mod ports;

pub use adapters::builtin::BuiltinToolClient;
pub use adapters::fuzzy_patch::FuzzyPatcher;
pub use adapters::mcp_stdio::McpStdioClient;
pub use adapters::openai::OpenAiProvider;
pub use application::core::DustCore;
pub use domain::manifest::{AppManifest, McpServerConfig, resolve_manifest_path};
pub use domain::patch::{LineRange, PatchError, SearchReplaceBlock, extract_blocks};
pub use error::{DustError, Result};
pub use ports::llm::{ChatMessage, LlmProvider, LlmResponse, ToolCall, ToolDefinition};
pub use ports::mcp::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpClient, McpTool};
pub use ports::patcher::CodePatcher;

pub use application::execution::{ExecutionReport, StopReason, ToolRecord, ToolStatus, TurnRecord};

pub use application::checkpoint::{Checkpoint, CheckpointPhase};
