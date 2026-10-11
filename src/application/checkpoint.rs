//! Durable conversation snapshots. Only snapshots between complete tool batches may resume.
use super::execution::ExecutionReport;
use crate::{
    DustError, Result,
    domain::manifest::AppManifest,
    ports::llm::{ChatMessage, ToolCall},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECONCILED_RESULT_BYTES: usize = 1024 * 1024;
const MAX_RECORDED_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointPhase {
    Ready,
    ToolInFlight,
    ValidationInFlight,
    Finished,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<super::transcript::TranscriptRef>,
    pub version: u32,
    pub manifest_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_hash: Option<String>,
    pub working_directory: PathBuf,
    pub user_input: String,
    pub messages: Vec<ChatMessage>,
    pub report: ExecutionReport,
    pub phase: CheckpointPhase,
}
fn invalid(message: impl Into<String>) -> DustError {
    DustError::Config(format!("Invalid checkpoint: {}", message.into()))
}
fn manifest_hash(manifest: &AppManifest) -> Result<String> {
    // Value's ordered maps canonicalize HashMap fields recursively.
    let value = serde_json::to_value(manifest)?;
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}
impl Checkpoint {
    pub fn new(
        manifest: &AppManifest,
        user_input: &str,
        messages: Vec<ChatMessage>,
        report: ExecutionReport,
        phase: CheckpointPhase,
    ) -> Result<Self> {
        Self::new_in(
            manifest,
            user_input,
            messages,
            report,
            phase,
            &std::env::current_dir()?,
        )
    }
    pub fn new_in(
        manifest: &AppManifest,
        user_input: &str,
        messages: Vec<ChatMessage>,
        report: ExecutionReport,
        phase: CheckpointPhase,
        cwd: &Path,
    ) -> Result<Self> {
        let checkpoint = Self {
            transcript: None,
            version: 1,
            manifest_hash: manifest_hash(manifest)?,
            resource_hash: None,
            working_directory: cwd.canonicalize()?,
            user_input: user_input.into(),
            messages,
            report,
            phase,
        };
        checkpoint.validate_for_in(manifest, cwd)?;
        Ok(checkpoint)
    }
    pub fn validate_for(&self, manifest: &AppManifest) -> Result<()> {
        self.validate_for_in(manifest, &std::env::current_dir()?)
    }
    pub fn validate_for_in(&self, manifest: &AppManifest, cwd: &Path) -> Result<()> {
        self.validate_protocol()?;
        if let Some(state) = &self.report.working_state {
            state.validate()?;
        }
        if self.manifest_hash != manifest_hash(manifest)? {
            return Err(invalid("manifest changed"));
        }
        if self.working_directory != cwd.canonicalize()? {
            return Err(invalid("working directory changed"));
        }
        let prompt = manifest
            .system_prompt
            .as_deref()
            .unwrap_or("You are a helpful specialized assistant.");
        if self.messages.first().and_then(|m| m.content.as_deref()) != Some(prompt) {
            return Err(invalid("system prompt changed"));
        }
        Ok(())
    }
    pub fn validate_resources(&self, hash: Option<&str>) -> Result<()> {
        if self.resource_hash.as_deref() != hash {
            return Err(invalid("package or skill contents changed"));
        }
        Ok(())
    }
    pub fn ensure_resumable(&self) -> Result<()> {
        self.ensure_resumable_with_prompt(None)
    }
    pub fn ensure_resumable_with_prompt(&self, prompt: Option<&str>) -> Result<()> {
        if prompt.is_some_and(|text| text.trim().is_empty()) {
            return Err(invalid("resume followup prompt must not be empty"));
        }
        self.validate_protocol()?;
        match self.phase {
            CheckpointPhase::Ready => Ok(()),
            CheckpointPhase::ToolInFlight => Err(invalid(
                "tool execution was interrupted; replay could repeat side effects",
            )),
            CheckpointPhase::ValidationInFlight => Err(invalid(
                "validation was interrupted; replay could repeat side effects",
            )),
            CheckpointPhase::Finished
                if prompt.is_some_and(|text| !text.trim().is_empty())
                    && self.report.is_complete() =>
            {
                Ok(())
            }
            CheckpointPhase::Finished => Err(invalid(
                "execution already finished; provide a followup prompt to continue a completed execution",
            )),
            CheckpointPhase::Blocked => Err(invalid(
                "execution is blocked and cannot resume automatically",
            )),
        }
    }

    /// Attach an operator-observed result to the earliest uncertain call without replaying it.
    pub fn reconcile_tool_result(&mut self, call_id: &str, result: &str) -> Result<()> {
        let archive_root = if self.transcript.is_some() || self.report.transcript.is_some() {
            Some(super::transcript::configured_root()?)
        } else {
            None
        };
        self.reconcile_tool_result_with_archive_root(call_id, result, archive_root.as_deref())
    }

    fn reconcile_tool_result_with_archive_root(
        &mut self,
        call_id: &str,
        result: &str,
        archive_root: Option<&Path>,
    ) -> Result<()> {
        if !matches!(
            self.phase,
            CheckpointPhase::ToolInFlight | CheckpointPhase::Blocked
        ) {
            return Err(invalid(
                "only an interrupted or blocked checkpoint can be reconciled",
            ));
        }
        if result.len() > MAX_RECONCILED_RESULT_BYTES {
            return Err(invalid("reconciled tool result exceeds 1 MiB"));
        }

        let mut updated = self.clone();
        let uncertain_index = updated
            .report
            .tool_calls
            .iter()
            .position(is_uncertain_tool_record)
            .or_else(|| {
                (updated.phase == CheckpointPhase::ToolInFlight
                    && pending_tool_call_ids(&updated.messages)
                        .first()
                        .is_some_and(|pending| pending == call_id))
                .then_some(updated.report.tool_calls.len())
            });
        let uncertain_index = match uncertain_index {
            Some(index) if index < updated.report.tool_calls.len() => {
                if updated.report.tool_calls[index].call_id != call_id {
                    return Err(invalid(format!(
                        "call ID must match the next uncertain tool call ({})",
                        updated.report.tool_calls[index].call_id
                    )));
                }
                index
            }
            Some(_) => {
                let pending = pending_tool_call_ids(&updated.messages);
                if pending.first().map(String::as_str) != Some(call_id) {
                    return Err(invalid("call ID must match the next pending tool call"));
                }
                let call = updated
                    .messages
                    .iter()
                    .filter(|message| message.role == "assistant")
                    .filter_map(|message| message.tool_calls.as_ref())
                    .flatten()
                    .find(|call| call.id == call_id)
                    .cloned()
                    .ok_or_else(|| invalid("pending call is missing from the conversation"))?;
                let arguments = call.parse_arguments().unwrap_or(serde_json::Value::Null);
                updated
                    .report
                    .tool_calls
                    .push(super::execution::ToolRecord {
                        turn: updated.report.turns_used.max(1),
                        call_id: call.id,
                        name: call.function_name,
                        arguments,
                        effect: super::execution::ToolEffect::ExternalUnknown,
                        operation_id: None,
                        status: super::execution::ToolStatus::InFlight,
                        elapsed_ms: 0,
                        output: None,
                        error: Some(
                            "Legacy checkpoint did not persist the in-flight call record".into(),
                        ),
                        truncated: false,
                    });
                updated.report.warnings.push(format!(
                    "Legacy checkpoint had no persisted record for call {call_id}; it was reconciled from operator-supplied observations."
                ));
                updated.report.tool_calls.len() - 1
            }
            None => {
                return Err(invalid(
                    "checkpoint has no uncertain tool call to reconcile",
                ));
            }
        };
        let has_call = updated.messages.iter().any(|message| {
            message.role == "assistant"
                && message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| calls.iter().any(|call| call.id == call_id))
        });
        if !has_call {
            return Err(invalid("uncertain call is missing from the conversation"));
        }

        let observed_result = ChatMessage::tool(call_id, result);
        let mut reconciliation_archive_messages = vec![observed_result.clone()];
        if let Some(message) = updated.messages.iter_mut().find(|message| {
            message.role == "tool" && message.tool_call_id.as_deref() == Some(call_id)
        }) {
            message.content = Some(result.to_owned());
        } else {
            let pending = pending_tool_call_ids(&updated.messages);
            if pending.first().map(String::as_str) != Some(call_id) {
                return Err(invalid(
                    "reconciled result is not the next conversation tool result",
                ));
            }
            updated.messages.push(observed_result.clone());
        }

        let record = &mut updated.report.tool_calls[uncertain_index];
        let reconciled_turn = record.turn;
        record.status = super::execution::ToolStatus::Reconciled;
        record.error = None;
        let mut recorded_end = result.len().min(MAX_RECORDED_TOOL_OUTPUT_BYTES);
        while !result.is_char_boundary(recorded_end) {
            recorded_end -= 1;
        }
        record.output = Some(result[..recorded_end].to_owned());
        record.truncated = recorded_end < result.len();
        updated.report.warnings.push(format!(
            "Tool call {call_id} was reconciled from operator-supplied external observations; DustAgent did not rerun it."
        ));

        // Serial dispatch persists only the current in-flight record. Later calls in the
        // same assistant batch have no record because they were never dispatched. Parallel
        // dispatch persists every record before starting any call, so uncertain parallel
        // calls remain available for explicit, ordered reconciliation.
        for pending_id in pending_tool_call_ids(&updated.messages) {
            if updated
                .report
                .tool_calls
                .iter()
                .any(|record| record.call_id == pending_id)
            {
                continue;
            }
            let call = updated
                .messages
                .iter()
                .filter(|message| message.role == "assistant")
                .filter_map(|message| message.tool_calls.as_ref())
                .flatten()
                .find(|call| call.id == pending_id)
                .cloned()
                .ok_or_else(|| invalid("pending call is missing from the conversation"))?;
            let reason = "Not dispatched before the earlier tool call was interrupted";
            let not_executed = ChatMessage::tool(
                &pending_id,
                serde_json::json!({"error":"not_executed","reason":reason}).to_string(),
            );
            updated.messages.push(not_executed.clone());
            reconciliation_archive_messages.push(not_executed);
            updated
                .report
                .tool_calls
                .push(super::execution::ToolRecord {
                    turn: reconciled_turn,
                    call_id: pending_id.clone(),
                    name: call.function_name.clone(),
                    arguments: call.parse_arguments().unwrap_or(serde_json::Value::Null),
                    effect: super::execution::ToolEffect::ExternalUnknown,
                    operation_id: None,
                    status: super::execution::ToolStatus::NotExecuted,
                    elapsed_ms: 0,
                    output: None,
                    error: Some(reason.into()),
                    truncated: false,
                });
            updated.report.warnings.push(format!(
                "Tool call {pending_id} was not dispatched before the earlier interruption."
            ));
        }

        if let Some(reference) = updated
            .transcript
            .as_ref()
            .or(updated.report.transcript.as_ref())
        {
            let root =
                archive_root.ok_or_else(|| invalid("transcript archive root is unavailable"))?;
            let store =
                super::transcript::TranscriptStore::open_in(reference, &reference.binding, root)?;
            let mut fork = store.fork()?;
            fork.append(&ChatMessage::system(format!(
                "[dustagent reconciliation] Call {call_id} received an operator-supplied observed result. DustAgent did not rerun the external operation."
            )))?;
            for message in reconciliation_archive_messages {
                fork.append(&message)?;
            }
            let reference = fork.reference();
            updated.transcript = Some(reference.clone());
            updated.report.transcript = Some(reference);
        }

        let has_uncertain_calls = updated
            .report
            .tool_calls
            .iter()
            .any(is_uncertain_tool_record);
        let has_pending_calls = !pending_tool_call_ids(&updated.messages).is_empty();
        if !has_uncertain_calls && !has_pending_calls {
            updated.phase = CheckpointPhase::Ready;
            updated.report.error = None;
        } else {
            updated.phase = CheckpointPhase::Blocked;
        }
        updated.validate_protocol()?;
        *self = updated;
        Ok(())
    }

    fn validate_protocol(&self) -> Result<()> {
        if self.version != 1 {
            return Err(invalid("unsupported version"));
        }
        if self.user_input.trim().is_empty() {
            return Err(invalid("empty original input"));
        }
        if self.messages.first().map(|m| m.role.as_str()) != Some("system") {
            return Err(invalid("missing initial system message"));
        }
        if !self
            .messages
            .iter()
            .any(|m| m.role == "user" && m.content.as_deref() == Some(self.user_input.as_str()))
        {
            return Err(invalid("original user input missing"));
        }
        let mut ids = HashSet::new();
        let mut pending = VecDeque::new();
        for message in &self.messages {
            if !pending.is_empty() && message.role != "tool" {
                return Err(invalid("missing consecutive tool results"));
            }
            match message.role.as_str() {
                "assistant" => {
                    if message.tool_call_id.is_some() {
                        return Err(invalid("assistant has tool result id"));
                    }
                    if let Some(calls) = &message.tool_calls {
                        for call in calls {
                            if call.id.is_empty() || !ids.insert(call.id.as_str()) {
                                return Err(invalid("empty or duplicate tool call id"));
                            }
                            pending.push_back(call.id.as_str());
                        }
                    }
                }
                "tool" => {
                    if message.tool_calls.is_some()
                        || message.tool_call_id.as_deref() != pending.pop_front()
                        || message.tool_call_id.is_none()
                    {
                        return Err(invalid("unexpected or out-of-order tool result"));
                    }
                }
                "system" | "user" => {
                    if message.tool_calls.is_some() || message.tool_call_id.is_some() {
                        return Err(invalid("non-assistant message has tool metadata"));
                    }
                }
                _ => return Err(invalid("unknown message role")),
            }
        }
        if !pending.is_empty()
            && self.phase != CheckpointPhase::ToolInFlight
            && self.phase != CheckpointPhase::Blocked
        {
            return Err(invalid("unresolved tool calls"));
        }
        if self.phase == CheckpointPhase::Ready
            && !matches!(
                self.messages.last().map(|m| m.role.as_str()),
                Some("user" | "tool")
            )
        {
            return Err(invalid("ready snapshot must end with user or tool message"));
        }
        Ok(())
    }
}

fn is_uncertain_tool_record(record: &super::execution::ToolRecord) -> bool {
    matches!(
        record.status,
        super::execution::ToolStatus::InFlight | super::execution::ToolStatus::TimedOut
    ) || (record.status == super::execution::ToolStatus::Failed && record.output.is_none())
}

fn pending_tool_call_ids(messages: &[ChatMessage]) -> Vec<String> {
    let mut pending = VecDeque::new();
    for message in messages {
        if message.role == "assistant" {
            if let Some(calls) = &message.tool_calls {
                pending.extend(calls.iter().map(|call: &ToolCall| call.id.clone()));
            }
        } else if message.role == "tool"
            && let Some(call_id) = &message.tool_call_id
            && pending.front() == Some(call_id)
        {
            pending.pop_front();
        }
    }
    pending.into_iter().collect()
}
pub fn load(path: impl AsRef<Path>) -> Result<Checkpoint> {
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_BYTES {
        return Err(invalid("file exceeds 32 MiB"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("file exceeds 32 MiB"));
    }
    let checkpoint: Checkpoint = serde_json::from_slice(&bytes)?;
    checkpoint.validate_protocol()?;
    Ok(checkpoint)
}
pub fn save(path: impl AsRef<Path>, checkpoint: &Checkpoint) -> Result<()> {
    checkpoint.validate_protocol()?;
    let bytes = serde_json::to_vec(checkpoint)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("encoded snapshot exceeds 32 MiB"));
    }
    let path = path.as_ref();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = parent.join(format!(".dust-checkpoint-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolEffect, ToolStatus, application::execution::ToolRecord, ports::llm::ToolCall};
    fn fixture() -> (AppManifest, Checkpoint) {
        let manifest = AppManifest::new().with_system_prompt("prompt");
        let checkpoint = Checkpoint::new(
            &manifest,
            "task",
            vec![ChatMessage::system("prompt"), ChatMessage::user("task")],
            ExecutionReport::default(),
            CheckpointPhase::Ready,
        )
        .unwrap();
        (manifest, checkpoint)
    }

    fn uncertain_record(call_id: &str) -> ToolRecord {
        ToolRecord {
            turn: 1,
            call_id: call_id.into(),
            name: "remote__send".into(),
            arguments: serde_json::json!({}),
            effect: ToolEffect::ExternalUnknown,
            operation_id: Some(format!("op-{call_id}")),
            status: ToolStatus::TimedOut,
            elapsed_ms: 10,
            output: None,
            error: Some("Tool deadline exceeded".into()),
            truncated: false,
        }
    }

    fn blocked_checkpoint(call_ids: &[&str]) -> Checkpoint {
        let (_, mut checkpoint) = fixture();
        let calls = call_ids
            .iter()
            .map(|id| ToolCall::new(*id, "remote__send", "{}"))
            .collect::<Vec<_>>();
        checkpoint
            .messages
            .push(ChatMessage::assistant(None, Some(calls.clone())));
        for call in calls {
            checkpoint
                .messages
                .push(ChatMessage::tool(&call.id, "transport outcome unknown"));
            checkpoint
                .report
                .tool_calls
                .push(uncertain_record(&call.id));
        }
        checkpoint.phase = CheckpointPhase::Blocked;
        checkpoint
    }
    #[test]
    fn atomic_private_roundtrip() {
        let (manifest, mut cp) = fixture();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run.json");
        save(&path, &cp).unwrap();
        cp.report.turns_used = 2;
        save(&path, &cp).unwrap();
        let loaded = load(&path).unwrap();
        loaded.validate_for(&manifest).unwrap();
        loaded.ensure_resumable().unwrap();
        assert_eq!(loaded.report.turns_used, 2);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn binding_and_version_checks() {
        let (mut manifest, cp) = fixture();
        manifest.max_turns = Some(20);
        assert!(cp.validate_for(&manifest).is_err());
        let (manifest, mut cp) = fixture();
        cp.working_directory = PathBuf::from("/different");
        assert!(cp.validate_for(&manifest).is_err());
        cp.version = 2;
        assert!(cp.ensure_resumable().is_err());
    }
    #[test]
    fn interrupted_and_finished_cannot_resume() {
        for phase in [
            CheckpointPhase::ToolInFlight,
            CheckpointPhase::ValidationInFlight,
            CheckpointPhase::Finished,
            CheckpointPhase::Blocked,
        ] {
            let (_, mut cp) = fixture();
            cp.phase = phase;
            assert!(cp.ensure_resumable().is_err());
        }
    }
    #[test]
    fn protocol_requires_ordered_unique_tool_results() {
        let (_, mut cp) = fixture();
        cp.messages.push(ChatMessage::assistant(
            None,
            Some(vec![
                ToolCall::new("a", "fetch", "{}"),
                ToolCall::new("b", "fetch", "{}"),
            ]),
        ));
        assert!(cp.ensure_resumable().is_err());
        cp.messages.push(ChatMessage::tool("b", "result"));
        assert!(cp.ensure_resumable().is_err());
        cp.messages.pop();
        cp.messages
            .extend([ChatMessage::tool("a", "one"), ChatMessage::tool("b", "two")]);
        cp.ensure_resumable().unwrap();
        cp.messages.push(ChatMessage::assistant(
            None,
            Some(vec![ToolCall::new("a", "fetch", "{}")]),
        ));
        assert!(cp.ensure_resumable().is_err());
    }
    #[test]
    fn corrupt_and_unknown_fields_rejected() {
        let (_, cp) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run");
        fs::write(&path, "broken").unwrap();
        assert!(load(&path).is_err());
        let mut value = serde_json::to_value(cp).unwrap();
        value["extra"] = true.into();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(load(&path).is_err());
    }
    #[test]
    fn manifest_hash_ignores_map_insertion_order() {
        use crate::domain::manifest::McpServerConfig;
        let first = AppManifest::new()
            .with_mcp_server("a", McpServerConfig::new("a"))
            .with_mcp_server("b", McpServerConfig::new("b"));
        let second = AppManifest::new()
            .with_mcp_server("b", McpServerConfig::new("b"))
            .with_mcp_server("a", McpServerConfig::new("a"));
        assert_eq!(
            manifest_hash(&first).unwrap(),
            manifest_hash(&second).unwrap()
        );
    }
    #[test]
    fn oversized_snapshot_rejected_before_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized");
        File::create(&path).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn reconciliation_replaces_observed_error_and_unblocks_without_replay() {
        let mut checkpoint = blocked_checkpoint(&["call-1"]);
        checkpoint
            .reconcile_tool_result("call-1", r#"{"accepted":true}"#)
            .unwrap();

        assert_eq!(checkpoint.phase, CheckpointPhase::Ready);
        assert_eq!(
            checkpoint.messages.last().unwrap().content.as_deref(),
            Some("{\"accepted\":true}")
        );
        assert_eq!(
            checkpoint.report.tool_calls[0].status,
            ToolStatus::Reconciled
        );
        assert_eq!(
            checkpoint.report.tool_calls[0].operation_id.as_deref(),
            Some("op-call-1")
        );
        assert_eq!(
            checkpoint.report.tool_calls[0].output.as_deref(),
            Some("{\"accepted\":true}")
        );
        assert!(checkpoint.report.error.is_none());
        checkpoint.ensure_resumable().unwrap();
    }

    #[test]
    fn reconciliation_requires_uncertain_calls_in_order() {
        let mut checkpoint = blocked_checkpoint(&["call-1", "call-2"]);
        let original = checkpoint.messages.clone();
        assert!(
            checkpoint
                .reconcile_tool_result("call-2", "observed")
                .is_err()
        );
        assert_eq!(checkpoint.messages, original);

        checkpoint
            .reconcile_tool_result("call-1", "first observed")
            .unwrap();
        assert_eq!(checkpoint.phase, CheckpointPhase::Blocked);
        checkpoint
            .reconcile_tool_result("call-2", "second observed")
            .unwrap();
        assert_eq!(checkpoint.phase, CheckpointPhase::Ready);
    }

    #[test]
    fn reconciliation_marks_later_serial_calls_not_executed_after_process_interruption() {
        let (_, mut checkpoint) = fixture();
        checkpoint.messages.push(ChatMessage::assistant(
            None,
            Some(vec![
                ToolCall::new("call-1", "remote__send", "{}"),
                ToolCall::new("call-2", "remote__send", "{}"),
            ]),
        ));
        let mut in_flight = uncertain_record("call-1");
        in_flight.status = ToolStatus::InFlight;
        checkpoint.report.tool_calls.push(in_flight);
        checkpoint.phase = CheckpointPhase::ToolInFlight;

        checkpoint
            .reconcile_tool_result("call-1", "first was accepted")
            .unwrap();

        assert_eq!(checkpoint.phase, CheckpointPhase::Ready);
        assert_eq!(checkpoint.report.tool_calls.len(), 2);
        assert_eq!(
            checkpoint.report.tool_calls[0].status,
            ToolStatus::Reconciled
        );
        assert_eq!(
            checkpoint.report.tool_calls[1].status,
            ToolStatus::NotExecuted
        );
        assert_eq!(
            checkpoint.messages[3].tool_call_id.as_deref(),
            Some("call-1")
        );
        assert_eq!(
            checkpoint.messages[4].tool_call_id.as_deref(),
            Some("call-2")
        );
        checkpoint.ensure_resumable().unwrap();
    }

    #[test]
    fn reconciliation_accepts_legacy_in_flight_checkpoint_without_tool_record() {
        let (_, mut checkpoint) = fixture();
        checkpoint.messages.push(ChatMessage::assistant(
            None,
            Some(vec![ToolCall::new("legacy-call", "remote__send", "{}")]),
        ));
        checkpoint.phase = CheckpointPhase::ToolInFlight;

        checkpoint
            .reconcile_tool_result("legacy-call", "operator verified no duplicate")
            .unwrap();

        assert_eq!(checkpoint.phase, CheckpointPhase::Ready);
        assert_eq!(checkpoint.report.tool_calls.len(), 1);
        assert_eq!(
            checkpoint.report.tool_calls[0].status,
            ToolStatus::Reconciled
        );
        assert!(checkpoint.report.tool_calls[0].operation_id.is_none());
        checkpoint.ensure_resumable().unwrap();
    }

    #[test]
    fn reconciliation_forks_transcript_and_preserves_original_archive() {
        let directory = tempfile::tempdir().unwrap();
        let mut archive = super::super::transcript::TranscriptStore::create_in(
            directory.path(),
            "fixture-binding",
        )
        .unwrap();
        archive.append(&ChatMessage::system("prompt")).unwrap();
        archive.append(&ChatMessage::user("task")).unwrap();
        archive
            .append(&ChatMessage::assistant(
                None,
                Some(vec![ToolCall::new("call-1", "remote__send", "{}")]),
            ))
            .unwrap();
        archive
            .append(&ChatMessage::tool("call-1", "transport outcome unknown"))
            .unwrap();
        let original = archive.reference();
        let mut checkpoint = blocked_checkpoint(&["call-1"]);
        checkpoint.transcript = Some(original.clone());
        checkpoint.report.transcript = Some(original.clone());

        checkpoint
            .reconcile_tool_result_with_archive_root(
                "call-1",
                "operator checked: accepted",
                Some(directory.path()),
            )
            .unwrap();

        let updated = checkpoint.transcript.as_ref().unwrap();
        assert_ne!(updated.path, original.path);
        assert_eq!(updated.count, original.count + 2);
        let original_store = super::super::transcript::TranscriptStore::open_in(
            &original,
            &original.binding,
            directory.path(),
        )
        .unwrap();
        assert_eq!(original_store.reference(), original);
        let updated_store = super::super::transcript::TranscriptStore::open_in(
            updated,
            &updated.binding,
            directory.path(),
        )
        .unwrap();
        assert_eq!(updated_store.reference(), *updated);
    }
}
