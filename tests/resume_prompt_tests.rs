use async_trait::async_trait;
use dustagent::application::checkpoint;
use dustagent::{
    AppManifest, ChatMessage, CheckpointPhase, DustCore, LlmProvider, LlmResponse, StopReason,
    ToolDefinition,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

struct Provider {
    replies: Mutex<VecDeque<dustagent::Result<LlmResponse>>>,
    calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    destroy: Option<std::path::PathBuf>,
}
impl Provider {
    fn new(replies: Vec<dustagent::Result<LlmResponse>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            calls: Arc::default(),
            destroy: None,
        }
    }
}
#[async_trait]
impl LlmProvider for Provider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        self.calls.lock().unwrap().push(messages.to_vec());
        if let Some(path) = &self.destroy {
            std::fs::remove_file(path).unwrap();
            std::fs::create_dir(path).unwrap();
        }
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("Unexpected replay/model call")
    }
}

#[tokio::test]
async fn failed_followup_is_saved_once_and_resume_keeps_original_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let manifest = AppManifest::new().with_system_prompt("Respond");
    let mut first = DustCore::new(
        manifest.clone(),
        Provider::new(vec![Ok(LlmResponse::text("old"))]),
    )
    .with_checkpoint(&path);
    assert!(first.execute_report("original").await.is_complete());
    let saved = checkpoint::load(&path).unwrap();
    let mut failed = DustCore::new(
        manifest.clone(),
        Provider::new(vec![Err(dustagent::DustError::Config("offline".into()))]),
    )
    .with_checkpoint(&path);
    let report = failed
        .resume_report_with_prompt(&saved, Some("Fix format"))
        .await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert!(report.output.is_none());
    let saved = checkpoint::load(&path).unwrap();
    assert_eq!(saved.user_input, "original");
    assert_eq!(saved.phase, CheckpointPhase::Ready);
    let provider = Provider::new(vec![Ok(LlmResponse::text("fixed"))]);
    let calls = provider.calls.clone();
    let mut resumed = DustCore::new(manifest, provider).with_checkpoint(&path);
    assert!(resumed.resume_report(&saved).await.is_complete());
    assert_eq!(
        calls.lock().unwrap()[0]
            .iter()
            .filter(|m| m.content.as_deref() == Some("Fix format"))
            .count(),
        1
    );
    let archived = std::fs::read_to_string(&saved.transcript.unwrap().path).unwrap();
    assert_eq!(archived.matches("Fix format").count(), 1);
}

#[tokio::test]
async fn uncertain_and_noncomplete_finished_checkpoints_reject_followup_before_model() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = AppManifest::new().with_system_prompt("Respond");
    for phase in [
        CheckpointPhase::ToolInFlight,
        CheckpointPhase::ValidationInFlight,
        CheckpointPhase::Blocked,
        CheckpointPhase::Finished,
    ] {
        let saved = checkpoint::Checkpoint::new(
            &manifest,
            "original",
            vec![
                ChatMessage::system("Respond"),
                ChatMessage::user("original"),
            ],
            Default::default(),
            phase,
        )
        .unwrap();
        let provider = Provider::new(vec![]);
        let calls = provider.calls.clone();
        let mut core = DustCore::new(manifest.clone(), provider)
            .with_checkpoint(dir.path().join("state.json"));
        assert_eq!(
            core.resume_report_with_prompt(&saved, Some("fix"))
                .await
                .stop_reason,
            StopReason::ExecutionError
        );
        assert!(calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn completed_checkpoint_needs_nonempty_followup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let manifest = AppManifest::new().with_system_prompt("Respond");
    let mut first = DustCore::new(
        manifest.clone(),
        Provider::new(vec![Ok(LlmResponse::text("old"))]),
    )
    .with_checkpoint(&path);
    assert!(first.execute_report("original").await.is_complete());
    let saved = checkpoint::load(&path).unwrap();
    let provider = Provider::new(vec![Ok(LlmResponse::text("new"))]);
    let calls = provider.calls.clone();
    let mut core = DustCore::new(manifest, provider).with_checkpoint(&path);
    assert_eq!(
        core.resume_report(&saved).await.stop_reason,
        StopReason::ExecutionError
    );
    assert_eq!(
        core.resume_report_with_prompt(&saved, Some(" \n"))
            .await
            .stop_reason,
        StopReason::ExecutionError
    );
    assert!(calls.lock().unwrap().is_empty());
    assert!(
        core.resume_report_with_prompt(&saved, Some("Fix"))
            .await
            .is_complete()
    );
    let captured = calls.lock().unwrap();
    assert_eq!(captured[0].last().unwrap().content.as_deref(), Some("Fix"));
}

#[tokio::test]
async fn failed_initial_followup_checkpoint_save_keeps_previous_archive_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let manifest = AppManifest::new().with_system_prompt("Respond");
    let mut first = DustCore::new(
        manifest.clone(),
        Provider::new(vec![Ok(LlmResponse::text("old"))]),
    )
    .with_checkpoint(&path);
    assert!(first.execute_report("original").await.is_complete());
    let saved = checkpoint::load(&path).unwrap();
    let original = saved.transcript.as_ref().unwrap();
    let before = std::fs::read(&original.path).unwrap();
    let provider = Provider::new(vec![]);
    let calls = provider.calls.clone();
    let mut core = DustCore::new(manifest.clone(), provider).with_checkpoint(dir.path());
    let failure = core.resume_report_with_prompt(&saved, Some("Fix")).await;
    assert_eq!(failure.stop_reason, StopReason::ExecutionError);
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(&original.path).unwrap(), before);
    let store =
        dustagent::application::transcript::TranscriptStore::open(original, &original.binding)
            .unwrap();
    assert_eq!(store.reference(), *original);
    let untouched = checkpoint::load(&path).unwrap();
    assert_eq!(untouched.transcript, saved.transcript);
    let mut valid = DustCore::new(
        manifest,
        Provider::new(vec![Ok(LlmResponse::text("fixed"))]),
    )
    .with_checkpoint(&path);
    assert!(
        valid
            .resume_report_with_prompt(&untouched, Some("Fix"))
            .await
            .is_complete()
    );
    let updated = checkpoint::load(&path).unwrap();
    assert_ne!(updated.transcript.unwrap().path, original.path);
}

#[test]
fn archive_fork_preserves_directory_anchors_and_source_records() {
    use dustagent::application::transcript::TranscriptStore;
    let root = tempfile::tempdir().unwrap();
    let mut source = TranscriptStore::create_in(root.path(), "binding").unwrap();
    source
        .append(&ChatMessage::user(
            "Review src/main.rs and references/checklist.md",
        ))
        .unwrap();
    source
        .append(&ChatMessage::assistant_text("Observed document"))
        .unwrap();
    source
        .index_compaction("Relevant document and source")
        .unwrap();
    let original_ref = source.reference();
    let original_directory = source.directory(0, 10, None).unwrap();
    let mut fork = source.fork().unwrap();
    assert_eq!(fork.count(), source.count());
    assert_eq!(fork.reference().hash, original_ref.hash);
    assert_ne!(fork.reference().path, original_ref.path);
    assert_eq!(fork.directory(0, 10, None).unwrap(), original_directory);
    assert_eq!(
        fork.read(1, 0, 1000).unwrap(),
        source.read(1, 0, 1000).unwrap()
    );
    fork.append(&ChatMessage::user("Followup")).unwrap();
    assert_eq!(source.reference(), original_ref);
    assert_eq!(
        TranscriptStore::open_in(&original_ref, "binding", root.path())
            .unwrap()
            .directory(0, 10, None)
            .unwrap(),
        original_directory
    );
    TranscriptStore::open_in(&fork.reference(), "binding", root.path()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(fork.reference().path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
