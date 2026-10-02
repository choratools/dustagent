//! In-memory interactive conversation, distinct from an interrupted-job checkpoint.
use super::{execution::ToolRecord, state::WorkingState};
use crate::ports::llm::ChatMessage;
#[derive(Debug, Default)]
pub struct AgentSession {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) state: WorkingState,
    pub(crate) tool_calls: Vec<ToolRecord>,
    pub(crate) binding: Option<String>,
    pub(crate) blocked: bool,
}
impl AgentSession {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_blocked(&self) -> bool {
        self.blocked
    }
}
pub(crate) fn within_bounds(messages: &[ChatMessage]) -> bool {
    messages.len() <= 2048
        && serde_json::to_vec(messages).is_ok_and(|bytes| bytes.len() <= 8 * 1024 * 1024)
}
