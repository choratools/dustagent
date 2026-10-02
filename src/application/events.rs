//! Optional execution notifications; the core does not depend on ACP.
use super::execution::ToolRecord;
use serde_json::Value;
#[derive(Debug, Clone)]
pub enum ExecutionEvent {
    AssistantMessage {
        content: String,
    },
    ToolStarted {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolFinished {
        record: ToolRecord,
    },
}
