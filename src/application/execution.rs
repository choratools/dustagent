use crate::ports::llm::LlmUsage;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_usage: Option<LlmUsage>,
}

/// Aggregate of token counters reported by completed model responses.
/// Missing provider counters are left unavailable instead of being treated as zero.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TokenUsageSummary {
    /// Number of turn and compaction response records that included usage.
    pub responses_with_usage: usize,
    pub input_tokens: Option<u64>,
    /// Cached input is a subset of `input_tokens`.
    pub cached_input_tokens: Option<u64>,
    /// Sum for responses where both input and cached-input counters were reported.
    pub uncached_input_tokens: Option<u64>,
    pub responses_with_uncached_input: usize,
    /// Output includes reasoning tokens when the provider reports them separately.
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// Provider total, or input + output when the provider omitted its total.
    pub total_tokens: Option<u64>,
}

impl TokenUsageSummary {
    fn is_empty(&self) -> bool {
        self.responses_with_usage == 0
            && self.input_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.uncached_input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.reasoning_tokens.is_none()
            && self.total_tokens.is_none()
    }
}

fn add_counter(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default().saturating_add(value));
    }
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
    #[serde(default, skip_serializing_if = "TokenUsageSummary::is_empty")]
    pub token_usage: TokenUsageSummary,
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
            token_usage: TokenUsageSummary::default(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryTokenSource {
    ProviderReported,
    Estimated,
}

/// Diagnostics survive temporary archive deletion without copying conversation content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionAttempt {
    pub turn: usize,
    pub attempt: usize,
    pub status: CompactionStatus,
    pub summary_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_token_source: Option<SummaryTokenSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_usage: Option<LlmUsage>,
    #[serde(default)]
    pub max_summary_tokens: usize,
    pub max_summary_bytes: usize,
    pub tool_call_count: usize,
    pub archive_index: Option<u64>,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

impl ExecutionReport {
    /// Rebuild the persisted aggregate from per-response usage records.
    pub fn refresh_token_usage(&mut self) {
        let usages = self
            .turns
            .iter()
            .filter_map(|turn| turn.provider_usage.as_ref())
            .chain(
                self.compaction_attempts
                    .iter()
                    .filter_map(|attempt| attempt.provider_usage.as_ref()),
            );
        let mut summary = TokenUsageSummary::default();
        for usage in usages.filter(|usage| !usage.is_empty()) {
            summary.responses_with_usage += 1;
            add_counter(&mut summary.input_tokens, usage.input_tokens);
            add_counter(&mut summary.cached_input_tokens, usage.cached_input_tokens);
            if let (Some(input), Some(cached)) = (usage.input_tokens, usage.cached_input_tokens) {
                summary.responses_with_uncached_input += 1;
                add_counter(
                    &mut summary.uncached_input_tokens,
                    Some(input.saturating_sub(cached)),
                );
            }
            add_counter(&mut summary.output_tokens, usage.output_tokens);
            add_counter(&mut summary.reasoning_tokens, usage.reasoning_tokens);
            let total = usage.total_tokens.or_else(|| {
                usage
                    .input_tokens
                    .zip(usage.output_tokens)
                    .map(|(input, output)| input.saturating_add(output))
            });
            add_counter(&mut summary.total_tokens, total);
        }
        self.token_usage = summary;
    }

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

#[cfg(test)]
mod token_usage_tests {
    use super::*;

    #[test]
    fn aggregate_includes_turns_and_compaction_and_keeps_partial_counters_visible() {
        let mut report = ExecutionReport::default();
        report.turns.push(TurnRecord {
            turn: 1,
            elapsed_ms: 0,
            assistant_content: None,
            tool_call_count: 0,
            error: None,
            truncated: false,
            provider_usage: Some(LlmUsage {
                input_tokens: Some(100),
                output_tokens: Some(30),
                total_tokens: Some(130),
                cached_input_tokens: Some(40),
                reasoning_tokens: Some(10),
            }),
        });
        report.turns.push(TurnRecord {
            turn: 2,
            elapsed_ms: 0,
            assistant_content: None,
            tool_call_count: 0,
            error: None,
            truncated: false,
            provider_usage: Some(LlmUsage {
                input_tokens: Some(8),
                ..LlmUsage::default()
            }),
        });
        report.compaction_attempts.push(CompactionAttempt {
            turn: 2,
            attempt: 1,
            status: CompactionStatus::Accepted,
            summary_bytes: Some(10),
            summary_tokens: Some(3),
            summary_token_source: Some(SummaryTokenSource::ProviderReported),
            provider_usage: Some(LlmUsage {
                input_tokens: Some(50),
                output_tokens: Some(20),
                total_tokens: None,
                cached_input_tokens: Some(20),
                reasoning_tokens: Some(5),
            }),
            max_summary_tokens: 10,
            max_summary_bytes: 100,
            tool_call_count: 0,
            archive_index: Some(2),
            elapsed_ms: 1,
            error: None,
        });
        report.provider_retries.push(RetryRecord {
            turn: 2,
            attempt: 1,
            error: "retryable provider error".into(),
            delay_ms: 1,
        });

        report.refresh_token_usage();

        assert_eq!(
            report.token_usage,
            TokenUsageSummary {
                responses_with_usage: 3,
                input_tokens: Some(158),
                cached_input_tokens: Some(60),
                uncached_input_tokens: Some(90),
                responses_with_uncached_input: 2,
                output_tokens: Some(50),
                reasoning_tokens: Some(15),
                total_tokens: Some(200),
            }
        );
        let serialized = serde_json::to_value(&report).unwrap();
        assert_eq!(serialized["token_usage"]["input_tokens"], 158);
        assert_eq!(serialized["token_usage"]["cached_input_tokens"], 60);
        assert_eq!(serialized["token_usage"]["total_tokens"], 200);
    }
}
