use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailure {
    Transient,
    Permanent,
    InvalidResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamTerminal {
    Completed,
    Failed,
    Incomplete,
    Error,
    DoneWithoutCompletion,
    EndedBeforeCompletion,
}

/// Sanitized provider metadata that is safe to surface in logs and reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct ProviderDiagnostics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_terminal: Option<ProviderStreamTerminal>,
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

    #[error("Provider {kind:?}: {message}")]
    ProviderWithDiagnostics {
        kind: ProviderFailure,
        message: String,
        diagnostics: ProviderDiagnostics,
    },

    /// A retryable provider response that may carry an explicit server retry delay.
    #[error("Provider {kind:?}: {message}")]
    ProviderRetryable {
        kind: ProviderFailure,
        message: String,
        retry_after: Option<Duration>,
    },

    /// A retryable provider response with sanitized provider metadata.
    #[error("Provider {kind:?}: {message}")]
    ProviderRetryableWithDiagnostics {
        kind: ProviderFailure,
        message: String,
        retry_after: Option<Duration>,
        diagnostics: ProviderDiagnostics,
    },

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Manifest error: {0}")]
    Manifest(String),

    #[error("Encrypted package requires a passphrase")]
    PackagePassphraseRequired,

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
            } | Self::ProviderWithDiagnostics {
                kind: ProviderFailure::Transient,
                ..
            } | Self::ProviderRetryable {
                kind: ProviderFailure::Transient,
                ..
            } | Self::ProviderRetryableWithDiagnostics {
                kind: ProviderFailure::Transient,
                ..
            }
        )
    }

    pub fn provider_retry_after(&self) -> Option<Duration> {
        match self {
            Self::ProviderRetryable { retry_after, .. } => *retry_after,
            Self::ProviderRetryableWithDiagnostics { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    pub fn provider_diagnostics(&self) -> Option<&ProviderDiagnostics> {
        match self {
            Self::ProviderWithDiagnostics { diagnostics, .. }
            | Self::ProviderRetryableWithDiagnostics { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }
}
