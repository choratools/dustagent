use crate::{DustError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
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
