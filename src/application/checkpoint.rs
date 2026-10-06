//! Durable conversation snapshots. Only snapshots between complete tool batches may resume.
use super::execution::ExecutionReport;
use crate::{DustError, Result, domain::manifest::AppManifest, ports::llm::ChatMessage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 32 * 1024 * 1024;
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
    use crate::ports::llm::ToolCall;
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
}
