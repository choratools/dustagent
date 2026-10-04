pub mod adapters;
pub mod application;
pub mod domain;
pub mod error;
pub mod ports;

pub use adapters::builtin::BuiltinToolClient;
pub use adapters::fuzzy_patch::FuzzyPatcher;
pub use adapters::mcp_stdio::McpStdioClient;
pub use adapters::openai::OpenAiProvider;
pub use adapters::provider::AutoProvider;
pub use application::core::DustCore;
pub use application::skills::{ModelSkillConfig, SkillInclude, SkillLoadingMode};
pub use domain::manifest::{
    AppManifest, McpChrootConfig, McpServerConfig, ModelConfiguration, PackageMetadata,
    resolve_manifest_path,
};
pub use domain::patch::{LineRange, PatchError, SearchReplaceBlock, extract_blocks};
pub use error::{DustError, ProviderFailure, Result};
pub use ports::llm::{ChatMessage, LlmProvider, LlmResponse, ToolCall, ToolDefinition};
pub use ports::mcp::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpClient, McpTool};
pub use ports::patcher::CodePatcher;

pub use application::execution::{ExecutionReport, StopReason, ToolRecord, ToolStatus, TurnRecord};

pub use application::checkpoint::{Checkpoint, CheckpointPhase};

pub use application::package::LoadedApp;
pub use application::skills::SkillCatalog;

pub use application::retry::RetryConfig;
pub use application::state::WorkingState;
pub use application::validation::{ValidationDecision, ValidationMode};

pub use application::events::ExecutionEvent;
pub use application::session::AgentSession;
