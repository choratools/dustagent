use thiserror::Error;

#[derive(Error, Debug)]
pub enum DustError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Patch error: {0}")]
    Patch(String),

    #[error("MCP error: {0}")]
    Mcp(String),

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Manifest error: {0}")]
    Manifest(String),

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, DustError>;
