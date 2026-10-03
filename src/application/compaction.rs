//! Model-facing context reduction. Original messages live in the independent transcript.
use crate::{DustError, Result, ports::llm::ChatMessage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
    pub trigger_tokens: usize,
    pub keep_recent_messages: usize,
    pub max_summary_bytes: usize,
    #[serde(
        default = "default_summary_retries",
        skip_serializing_if = "is_default_summary_retries"
    )]
    pub max_summary_retries: usize,
}
fn default_summary_retries() -> usize {
    2
}
fn is_default_summary_retries(value: &usize) -> bool {
    *value == 2
}
impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_window_tokens: None,
            trigger_tokens: 0,
            keep_recent_messages: 0,
            max_summary_bytes: 0,
            max_summary_retries: 2,
        }
    }
}
impl CompactionConfig {
    pub fn validate(&self) -> Result<()> {
        if self
            .context_window_tokens
            .is_some_and(|v| !(4096..=10_000_000).contains(&v))
            || (self.trigger_tokens != 0 && !(1024..=1_000_000).contains(&self.trigger_tokens))
            || (self.keep_recent_messages != 0 && !(2..=128).contains(&self.keep_recent_messages))
            || (self.max_summary_bytes != 0 && !(256..=65_536).contains(&self.max_summary_bytes))
            || self.max_summary_retries > 3
        {
            return Err(DustError::Config("Compaction bounds: context_window_tokens 4096..10000000, trigger_tokens 0(auto) or 1024..1000000, keep_recent_messages 0(auto) or 2..128, max_summary_bytes 0(auto) or 256..65536, max_summary_retries 0..3".into()));
        }
        Ok(())
    }
    /// Resolve automatic values from the configured provider's context window.
    /// Reserves are engineering defaults, not a tokenizer or provider guarantee.
    pub fn resolve(&self, provider_window: Option<usize>) -> Self {
        let window = self
            .context_window_tokens
            .or(provider_window)
            .unwrap_or(32_768);
        let reserve = (window / 10).max(1024);
        let input_budget = window.saturating_sub(reserve.saturating_mul(2)).max(1024);
        let mut resolved = self.clone();
        resolved.context_window_tokens = Some(window);
        if resolved.trigger_tokens == 0 {
            resolved.trigger_tokens = input_budget;
        }
        if resolved.max_summary_bytes == 0 {
            // Aim for one eighth of input budget, while bounding output memory.
            resolved.max_summary_bytes = (resolved.trigger_tokens / 8)
                .saturating_mul(3)
                .clamp(256, 65_536);
        }
        resolved
    }
    /// Leave output space even if an explicit trigger is too large.
    pub(crate) fn request_limit(&self) -> usize {
        let window = self.context_window_tokens.unwrap_or(32_768);
        window.saturating_sub((window / 10).max(1024))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub turn: usize,
    pub estimated_tokens_before: usize,
    pub estimated_tokens_after: usize,
    pub archived_through: u64,
    pub messages_before: usize,
    pub messages_after: usize,
    #[serde(default)]
    pub context_window_tokens: usize,
    #[serde(default)]
    pub trigger_tokens: usize,
    #[serde(default)]
    pub max_summary_bytes: usize,
}
/// Serialized UTF-8 bytes / 3, plus message overhead, is an approximate token
/// estimate. This is not a provider tokenizer or a guarantee of the exact limit.
pub fn estimate(messages: &[ChatMessage], tools_bytes: usize) -> usize {
    serde_json::to_vec(messages).map_or(usize::MAX, |v| {
        v.len()
            .saturating_add(tools_bytes)
            .div_ceil(3)
            .saturating_add(messages.len().saturating_mul(16))
    })
}
pub(crate) struct Plan {
    pub preserved: Vec<ChatMessage>,
    pub old: Vec<ChatMessage>,
    pub recent: Vec<ChatMessage>,
}
pub(crate) fn plan(messages: &[ChatMessage], keep: usize, original_input: &str) -> Option<Plan> {
    let leading = messages.iter().take_while(|m| m.role == "system").count();
    let first_user = messages.iter().position(|m| m.role == "user")?;
    let latest_user = messages.iter().rposition(|m| m.role == "user")?;
    let mut cut = messages.len().saturating_sub(keep).max(leading);
    // All tool replies belong to the preceding assistant batch. Retain the batch
    // rather than leaving orphaned results in the replacement context.
    while cut > leading && messages.get(cut).is_some_and(|m| m.role == "tool") {
        cut -= 1;
    }
    let mut preserved = messages[..leading].to_vec();
    let mut old = Vec::new();
    for (index, message) in messages.iter().enumerate().take(cut).skip(leading) {
        if index == first_user
            || index == latest_user
            || (message.role == "user" && message.content.as_deref() == Some(original_input))
        {
            preserved.push(message.clone());
        } else {
            old.push(message.clone());
        }
    }
    if old.is_empty() {
        return None;
    }
    Some(Plan {
        preserved,
        old,
        recent: messages[cut..].to_vec(),
    })
}
/// Retain recent complete batches within a token budget. Explicit message-count
/// overrides remain supported; automatic mode chooses by estimated content size.
pub(crate) fn budget_plan(
    messages: &[ChatMessage],
    config: &CompactionConfig,
    tools_bytes: usize,
    original_input: &str,
) -> Option<Plan> {
    let retention_budget = config.trigger_tokens.saturating_mul(3) / 4;
    let summary_tokens = config.max_summary_bytes.div_ceil(3).saturating_add(128);
    let max_keep = if config.keep_recent_messages == 0 {
        messages.len().saturating_sub(1).max(2)
    } else {
        config.keep_recent_messages
    };
    // Complete-batch retention grows monotonically with `keep`. Binary search
    // avoids serializing a large transcript once per message on long sessions.
    let mut selected = plan(messages, 2, original_input)?;
    let mut low = 3;
    let mut high = max_keep;
    while low <= high {
        let keep = low + (high - low) / 2;
        let candidate = plan(messages, keep, original_input);
        let fits = candidate.as_ref().is_some_and(|candidate| {
            let mut retained = candidate.preserved.clone();
            retained.extend(candidate.recent.clone());
            estimate(&retained, tools_bytes).saturating_add(summary_tokens) < retention_budget
        });
        if fits {
            selected = candidate.unwrap();
            low = keep + 1;
        } else {
            high = keep - 1;
        }
    }
    Some(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::llm::ToolCall;
    #[test]
    fn automatic_budget_reserves_output_and_tool_growth() {
        let config = CompactionConfig::default();
        config.validate().unwrap();
        let resolved = config.resolve(Some(100_000));
        assert_eq!(resolved.trigger_tokens, 80_000);
        assert_eq!(resolved.max_summary_bytes, 30_000);
        assert_eq!(config.resolve(None).context_window_tokens, Some(32_768));
        let explicit = CompactionConfig {
            context_window_tokens: Some(50_000),
            trigger_tokens: 12_000,
            keep_recent_messages: 6,
            max_summary_bytes: 2_000,
            ..config
        };
        let resolved = explicit.resolve(Some(100_000));
        assert_eq!(resolved.context_window_tokens, Some(50_000));
        assert_eq!(resolved.trigger_tokens, 12_000);
        assert_eq!(resolved.keep_recent_messages, 6);
        assert_eq!(resolved.max_summary_bytes, 2_000);
    }
    #[test]
    fn automatic_retention_uses_content_size_and_keeps_current_request() {
        let config = CompactionConfig {
            trigger_tokens: 4_000,
            max_summary_bytes: 512,
            ..CompactionConfig::default()
        }
        .resolve(None);
        let messages = vec![
            ChatMessage::system("system"),
            ChatMessage::user("first"),
            ChatMessage::assistant_text("old".repeat(4_000)),
            ChatMessage::user("current"),
            ChatMessage::assistant_text("recent"),
            ChatMessage::assistant(None, Some(vec![ToolCall::new("1", "a", "{}")])),
            ChatMessage::tool("1", "result"),
        ];
        let selected = budget_plan(&messages, &config, 0, "current").unwrap();
        assert!(
            selected
                .old
                .iter()
                .any(|m| m.content.as_deref().is_some_and(|v| v.len() > 1_000))
        );
        assert!(
            selected
                .preserved
                .iter()
                .chain(selected.recent.iter())
                .any(|m| m.content.as_deref() == Some("current"))
        );
        assert_eq!(selected.recent.last().unwrap().role, "tool");
        assert!(selected.recent.iter().any(|m| m.tool_calls.is_some()));
    }
    #[test]
    fn retains_whole_recent_tool_batch() {
        let m = vec![
            ChatMessage::system("s"),
            ChatMessage::user("u"),
            ChatMessage::assistant_text("old"),
            ChatMessage::assistant(
                None,
                Some(vec![
                    ToolCall::new("1", "a", "{}"),
                    ToolCall::new("2", "b", "{}"),
                ]),
            ),
            ChatMessage::tool("1", "x"),
            ChatMessage::tool("2", "y"),
        ];
        let p = plan(&m, 2, "u").unwrap();
        assert_eq!(p.recent[0].role, "assistant");
        assert_eq!(p.recent.len(), 3);
        assert_eq!(p.preserved[1].content.as_deref(), Some("u"));
    }
}
