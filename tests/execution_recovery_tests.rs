use async_trait::async_trait;
use dustagent::application::{
    checkpoint,
    retry::RetryConfig,
    validation::{ValidationConfig, ValidationMode},
};
use dustagent::error::ProviderFailure;
use dustagent::{
    AppManifest, ChatMessage, CheckpointPhase, DustCore, DustError, LlmProvider, LlmResponse,
    StopReason, ToolCall, ToolDefinition,
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
struct Provider {
    replies: Mutex<VecDeque<dustagent::Result<LlmResponse>>>,
    calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}
impl Provider {
    fn new(replies: Vec<dustagent::Result<LlmResponse>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            calls: Arc::default(),
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
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider request")
    }
}
fn transient() -> dustagent::Result<LlmResponse> {
    Err(DustError::Provider {
        kind: ProviderFailure::Transient,
        message: "temporary unavailable".into(),
    })
}
fn policy(max: usize) -> AppManifest {
    let mut m = AppManifest::new();
    m.provider_retry = Some(RetryConfig {
        max_retries: max,
        base_delay_ms: 1,
    });
    m
}
fn tool(id: &str, name: &str, args: serde_json::Value) -> dustagent::Result<LlmResponse> {
    Ok(LlmResponse::tool_calls(vec![ToolCall::new(
        id,
        name,
        args.to_string(),
    )]))
}
#[tokio::test]
async fn transient_retry_preserves_messages_and_does_not_spend_another_turn() {
    let provider = Provider::new(vec![transient(), Ok(LlmResponse::text("done"))]);
    let calls = provider.calls.clone();
    let mut core = DustCore::new(policy(2), provider).with_max_turns(1);
    let report = core.execute_report("input").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.turns_used, 1);
    assert_eq!(report.provider_retries.len(), 1);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        serde_json::to_value(&calls[0]).unwrap(),
        serde_json::to_value(&calls[1]).unwrap()
    );
}
#[tokio::test]
async fn permanent_legacy_and_exhausted_failures_stop_with_bounded_requests() {
    for (replies, expected, retries) in [
        (
            vec![Err(DustError::Provider {
                kind: ProviderFailure::Permanent,
                message: "unauthorized".into(),
            })],
            1,
            0,
        ),
        (vec![Err(DustError::Llm("legacy".into()))], 1, 0),
        (vec![transient(), transient(), transient()], 3, 2),
    ] {
        let provider = Provider::new(replies);
        let calls = provider.calls.clone();
        let mut core = DustCore::new(policy(2), provider);
        let report = core.execute_report("input").await;
        assert_eq!(report.stop_reason, StopReason::ExecutionError);
        assert_eq!(report.provider_retries.len(), retries);
        assert_eq!(calls.lock().unwrap().len(), expected);
    }
}
#[tokio::test]
async fn deadline_during_backoff_prevents_an_extra_request() {
    let mut manifest = policy(2);
    manifest.provider_retry.as_mut().unwrap().base_delay_ms = 100;
    let provider = Provider::new(vec![transient()]);
    let calls = provider.calls.clone();
    let mut core = DustCore::new(manifest, provider).with_timeouts(20, 1000);
    assert_eq!(
        core.execute_report("input").await.stop_reason,
        StopReason::TimeLimit
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn model_retry_never_replays_observed_tool_and_state_resumes_then_resets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let mut manifest = policy(1);
    manifest.working_state = true;
    let provider = Provider::new(vec![tool(
        "put",
        "dustagent__state_put",
        json!({"key":"coverage","value":{"remaining":190}}),
    )]);
    let mut core = DustCore::new(manifest.clone(), provider)
        .with_checkpoint(&path)
        .with_max_turns(1);
    let report = core.execute_report("input").await;
    assert_eq!(report.stop_reason, StopReason::TurnLimit);
    assert_eq!(report.working_state.as_ref().unwrap().revision, 1);
    let saved = checkpoint::load(&path).unwrap();
    assert_eq!(saved.phase, CheckpointPhase::Ready);
    let provider = Provider::new(vec![
        transient(),
        Ok(LlmResponse::text("done")),
        Ok(LlmResponse::text("fresh")),
    ]);
    let calls = provider.calls.clone();
    let mut resumed = DustCore::new(manifest, provider).with_checkpoint(&path);
    let report = resumed.resume_report(&saved).await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.tool_calls.len(), 1);
    assert_eq!(report.working_state.as_ref().unwrap().revision, 1);
    assert_eq!(
        report.working_state.as_ref().unwrap().entries["coverage"],
        json!({"remaining":190})
    );
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            serde_json::to_value(&calls[0]).unwrap(),
            serde_json::to_value(&calls[1]).unwrap()
        );
        assert_eq!(calls[0].iter().filter(|m| m.role == "tool").count(), 1);
    }
    assert!(
        resumed
            .execute_tool("dustagent__state_get", json!({"key":"coverage"}))
            .await
            .unwrap()
            .contains("190")
    );
    assert!(
        resumed
            .execute_tool("dustagent__state_list", json!({}))
            .await
            .unwrap()
            .contains("coverage")
    );
    let fresh = resumed.execute_report("fresh input").await;
    assert_eq!(fresh.stop_reason, StopReason::Completed);
    assert_eq!(fresh.working_state.unwrap().revision, 0);
}
#[tokio::test]
async fn working_state_tools_are_opt_in() {
    let mut core = DustCore::new(policy(0), Provider::new(vec![]));
    assert!(
        !core
            .gather_mcp_tools()
            .await
            .unwrap()
            .iter()
            .any(|t| t.name.starts_with("dustagent__state_"))
    );
    let denied = core
        .execute_tool("dustagent__state_put", json!({"key":"x","value":1}))
        .await
        .unwrap();
    assert!(denied.contains("\"isError\":true"));
    let mut manifest = policy(0);
    manifest.working_state = true;
    let mut enabled = DustCore::new(manifest, Provider::new(vec![]));
    assert_eq!(
        enabled
            .gather_mcp_tools()
            .await
            .unwrap()
            .iter()
            .filter(|t| t.name.starts_with("dustagent__state_"))
            .count(),
        3
    );
}
fn feedback(script: &str) -> ValidationConfig {
    ValidationConfig {
        command: "python3".into(),
        args: vec!["-c".into(), script.into()],
        timeout_ms: 5000,
        mode: ValidationMode::Feedback,
    }
}
fn feedback_manifest() -> AppManifest {
    let mut manifest = policy(0);
    manifest.working_state = true;
    manifest.validation = Some(feedback(
        "import json,sys; d=json.load(sys.stdin); print(json.dumps({'decision':'complete' if d['output']=='body' else 'continue','reason':'complete body required'}))",
    ));
    manifest
}
#[tokio::test]
async fn feedback_continue_requests_more_work_and_survives_checkpoint_resume() {
    let manifest = feedback_manifest();
    let provider = Provider::new(vec![
        Ok(LlmResponse::text("title")),
        Ok(LlmResponse::text("body")),
    ]);
    let calls = provider.calls.clone();
    let mut core = DustCore::new(manifest.clone(), provider);
    let report = core.execute_report("input").await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.turns_used, 2);
    assert!(calls.lock().unwrap()[1].iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|s| s.contains("complete body required"))
    }));
    let boundary_dir = tempfile::tempdir().unwrap();
    let boundary_path = boundary_dir.path().join("single-turn.json");
    let mut boundary = DustCore::new(
        manifest.clone(),
        Provider::new(vec![Ok(LlmResponse::text("title"))]),
    )
    .with_checkpoint(&boundary_path)
    .with_max_turns(1);
    assert_eq!(
        boundary.execute_report("original").await.stop_reason,
        StopReason::TurnLimit
    );
    let boundary_seed = checkpoint::load(&boundary_path).unwrap();
    assert_eq!(boundary_seed.phase, CheckpointPhase::Ready);
    let mut boundary_resume = DustCore::new(
        manifest.clone(),
        Provider::new(vec![Ok(LlmResponse::text("body"))]),
    )
    .with_checkpoint(&boundary_path)
    .with_max_turns(1);
    let boundary_report = boundary_resume.resume_report(&boundary_seed).await;
    assert_eq!(boundary_report.stop_reason, StopReason::Completed);
    assert_eq!(boundary_report.turns_used, 2);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let provider = Provider::new(vec![
        tool(
            "put",
            "dustagent__state_put",
            json!({"key":"observed","value":100}),
        ),
        Ok(LlmResponse::text("title")),
    ]);
    let mut first = DustCore::new(manifest.clone(), provider)
        .with_checkpoint(&path)
        .with_max_turns(2);
    assert_eq!(
        first.execute_report("original").await.stop_reason,
        StopReason::TurnLimit
    );
    let saved = checkpoint::load(&path).unwrap();
    assert_eq!(saved.phase, CheckpointPhase::Ready);
    let provider = Provider::new(vec![Ok(LlmResponse::text("body"))]);
    let calls = provider.calls.clone();
    let mut resumed = DustCore::new(manifest, provider)
        .with_checkpoint(&path)
        .with_max_turns(1);
    let report = resumed.resume_report(&saved).await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.turns_used, 3);
    assert_eq!(
        report.working_state.unwrap().entries["observed"],
        json!(100)
    );
    let messages = &calls.lock().unwrap()[0];
    assert!(messages.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|s| s.contains("complete body required"))
    }));
    assert!(
        messages
            .iter()
            .any(|m| m.content.as_deref() == Some("title"))
    );
}
#[tokio::test]
async fn blocked_or_malformed_feedback_terminates_without_another_model_call() {
    for script in [
        "import json,sys; json.load(sys.stdin); print(json.dumps({'decision':'blocked','reason':'cannot verify'}))",
        "import json,sys; json.load(sys.stdin); print('invalid verdict')",
    ] {
        let mut manifest = policy(0);
        manifest.validation = Some(feedback(script));
        let provider = Provider::new(vec![Ok(LlmResponse::text("candidate"))]);
        let calls = provider.calls.clone();
        let mut core = DustCore::new(manifest, provider);
        assert_eq!(
            core.execute_report("input").await.stop_reason,
            StopReason::ValidationFailed
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn packaged_feedback_checker_rebinds_root_when_archive_checkpoint_resumes() {
    use dustagent::application::package;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("package");
    std::fs::create_dir(&root).unwrap();
    let mut manifest = feedback_manifest();
    manifest.package = Some(dustagent::domain::manifest::PackageMetadata {
        name: "feedback-fixture".into(),
        version: "1.0.0".into(),
        dust_version: None,
    });
    manifest.validation.as_mut().unwrap().args = vec!["${DUST_APP_ROOT}/checker.py".into()];
    std::fs::write(root.join("app.json"), manifest.to_json_string().unwrap()).unwrap();
    std::fs::write(root.join("checker.py"), "import json,sys\nd=json.load(sys.stdin)\nassert set(d)=={'input','output','tool_calls','state'}\nassert d['input']=='original packaged input'\nassert d['tool_calls']==[]\nassert d['state']=={'revision':0,'entries':{}}\nprint(json.dumps({'decision':'complete' if d['output']=='body' else 'continue','reason':'packaged checker requires body'}))\n").unwrap();
    let archive = package::pack(&root, Some(&dir.path().join("feedback.dustpkg"))).unwrap();
    let first = package::load(archive.to_str().unwrap(), dir.path()).unwrap();
    let second = package::load(archive.to_str().unwrap(), dir.path()).unwrap();
    assert_ne!(first.root, second.root);
    assert_eq!(first.digest, second.digest);
    let checkpoint_path = dir.path().join("state.json");
    let mut core = DustCore::new(
        first.manifest.clone(),
        Provider::new(vec![Ok(LlmResponse::text("title"))]),
    )
    .with_app_resources(&first.root, first.digest.as_deref())
    .unwrap()
    .with_checkpoint(&checkpoint_path)
    .with_max_turns(1);
    assert_eq!(
        core.execute_report("original packaged input")
            .await
            .stop_reason,
        StopReason::TurnLimit
    );
    let saved = checkpoint::load(&checkpoint_path).unwrap();
    assert_eq!(saved.phase, CheckpointPhase::Ready);
    // Removing the first extraction proves resume cannot accidentally use its path.
    drop(core);
    let old_root = first.root.clone();
    drop(first);
    assert!(!old_root.exists());
    let provider = Provider::new(vec![Ok(LlmResponse::text("body"))]);
    let calls = provider.calls.clone();
    let mut resumed = DustCore::new(second.manifest.clone(), provider)
        .with_app_resources(&second.root, second.digest.as_deref())
        .unwrap()
        .with_checkpoint(&checkpoint_path)
        .with_max_turns(1);
    let report = resumed.resume_report(&saved).await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.turns_used, 2);
    assert!(report.validation.unwrap().passed);
    assert!(report.tool_calls.is_empty());
    assert_eq!(report.working_state.unwrap().revision, 0);
    let messages = &calls.lock().unwrap()[0];
    assert!(
        messages
            .iter()
            .any(|m| m.content.as_deref() == Some("title"))
    );
    assert!(messages.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|text| text.contains("packaged checker requires body"))
    }));
    assert_eq!(
        second.manifest.validation.as_ref().unwrap().args[0],
        "${DUST_APP_ROOT}/checker.py"
    );
}
