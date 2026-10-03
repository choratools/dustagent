use async_trait::async_trait;
use dustagent::{
    AppManifest, ChatMessage, Checkpoint, CheckpointPhase, DustCore, ExecutionReport, LlmProvider,
    LlmResponse, StopReason, ToolDefinition,
};
struct Model;
#[async_trait]
impl LlmProvider for Model {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        assert_eq!(
            messages.last().unwrap().content.as_deref(),
            Some("original input")
        );
        Ok(LlmResponse::text("legacy resumed"))
    }
}
#[tokio::test]
async fn checkpoint_without_archive_fields_resumes_with_explicit_legacy_warning() {
    let manifest = AppManifest::new();
    let checkpoint = Checkpoint::new(
        &manifest,
        "original input",
        vec![
            ChatMessage::system("You are a helpful specialized assistant."),
            ChatMessage::user("original input"),
        ],
        ExecutionReport::default(),
        CheckpointPhase::Ready,
    )
    .unwrap();
    let mut value = serde_json::to_value(checkpoint).unwrap();
    value.as_object_mut().unwrap().remove("transcript");
    value["report"]
        .as_object_mut()
        .unwrap()
        .remove("transcript");
    value["report"]
        .as_object_mut()
        .unwrap()
        .remove("compactions");
    let legacy: Checkpoint = serde_json::from_value(value).unwrap();
    assert!(legacy.transcript.is_none());
    let dir = tempfile::tempdir().unwrap();
    let mut core = DustCore::new(manifest, Model).with_checkpoint(dir.path().join("resume.json"));
    let report = core.resume_report(&legacy).await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert!(report.transcript.is_some());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("Legacy checkpoint"))
    );
}
