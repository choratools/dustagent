use crate::{DustError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    Cancelled,
    TurnLimit,
    TimeLimit,
    ToolTimeout,
    EmptyResponse,
    ExecutionError,
    ValidationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Succeeded,
    Failed,
    TimedOut,
    InvalidArguments,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRecord {
    pub turn: usize,
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub status: ToolStatus,
    pub elapsed_ms: u64,
    pub output: Option<String>,
    pub error: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    pub turn: usize,
    pub elapsed_ms: u64,
    pub assistant_content: Option<String>,
    pub tool_call_count: usize,
    pub error: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    /// Assigned checkpoint destination; startup errors may occur before it is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_path: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction_attempts: Vec<CompactionAttempt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compactions: Vec<super::compaction::CompactionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<super::transcript::TranscriptRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_state: Option<super::state::WorkingState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_retries: Vec<RetryRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_history: Vec<ValidationAttempt>,
    pub stop_reason: StopReason,
    pub output: Option<String>,
    pub turns_used: usize,
    pub elapsed_ms: u64,
    pub tool_calls: Vec<ToolRecord>,
    pub turns: Vec<TurnRecord>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub validation: Option<super::validation::ValidationEvidence>,
}

impl Default for ExecutionReport {
    fn default() -> Self {
        Self {
            checkpoint_path: None,
            compaction_attempts: Vec::new(),
            compactions: Vec::new(),
            transcript: None,
            working_state: None,
            provider_retries: Vec::new(),
            validation_history: Vec::new(),
            stop_reason: StopReason::ExecutionError,
            output: None,
            turns_used: 0,
            elapsed_ms: 0,
            tool_calls: Vec::new(),
            turns: Vec::new(),
            error: None,
            warnings: Vec::new(),
            validation: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStatus {
    Accepted,
    Empty,
    Oversized,
    UnexpectedToolCalls,
    ProviderError,
    Cancelled,
    TimeLimit,
    InputTooLarge,
    NonReducing,
    DirectoryError,
    ArchiveError,
}

/// Diagnostics survive temporary archive deletion without copying conversation content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionAttempt {
    pub turn: usize,
    pub attempt: usize,
    pub status: CompactionStatus,
    pub summary_bytes: Option<usize>,
    pub max_summary_bytes: usize,
    pub tool_call_count: usize,
    pub archive_index: Option<u64>,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

impl ExecutionReport {
    pub fn is_complete(&self) -> bool {
        self.stop_reason == StopReason::Completed
    }
    pub fn into_result(self) -> Result<String> {
        if self.is_complete() {
            return Ok(self.output.unwrap_or_default());
        }
        Err(DustError::Llm(format!(
            "Execution stopped: {:?} after {} turns ({} ms){}",
            self.stop_reason,
            self.turns_used,
            self.elapsed_ms,
            self.error.map(|e| format!(": {e}")).unwrap_or_default()
        )))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRecord {
    pub turn: usize,
    pub attempt: usize,
    pub error: String,
    pub delay_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationAttempt {
    pub turn: usize,
    pub evidence: super::validation::ValidationEvidence,
}
