use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailure {
    Transient,
    Permanent,
    InvalidResponse,
}

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

    #[error("Provider {kind:?}: {message}")]
    Provider {
        kind: ProviderFailure,
        message: String,
    },

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Manifest error: {0}")]
    Manifest(String),

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, DustError>;

impl DustError {
    pub fn is_retryable_provider_failure(&self) -> bool {
        matches!(
            self,
            Self::Provider {
                kind: ProviderFailure::Transient,
                ..
            }
        )
    }
}
