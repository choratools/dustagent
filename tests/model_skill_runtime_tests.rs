use async_trait::async_trait;
use dustagent::application::{
    checkpoint,
    skills::{ModelSkillConfig, SkillInclude, SkillLoadingMode},
};
use dustagent::domain::manifest::ModelConfiguration;
use dustagent::{
    AppManifest, ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolDefinition,
};
use std::sync::{Arc, Mutex};

struct Capture {
    model: String,
    messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}
#[async_trait]
impl LlmProvider for Capture {
    fn model_id(&self) -> Option<&str> {
        Some(&self.model)
    }
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _: Option<&[ToolDefinition]>,
    ) -> dustagent::Result<LlmResponse> {
        self.messages.lock().unwrap().push(messages.to_vec());
        Ok(LlmResponse::text("done"))
    }
}
fn provider(model: &str) -> (Capture, Arc<Mutex<Vec<Vec<ChatMessage>>>>) {
    let messages = Arc::new(Mutex::new(Vec::new()));
    (
        Capture {
            model: model.into(),
            messages: messages.clone(),
        },
        messages,
    )
}
fn fixture() -> (tempfile::TempDir, AppManifest) {
    let root = tempfile::tempdir().unwrap();
    for name in ["review", "private"] {
        let dir = root.path().join("skills").join(name);
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("---\nname: {name}\ndescription: Description {name}\n---\nBODY_{name} references/check.md")).unwrap();
        std::fs::write(dir.join("references/check.md"), format!("REFERENCE_{name}")).unwrap();
    }
    let mut manifest = AppManifest::new();
    manifest.skills = vec!["review".into()];
    manifest.default_model = Some("default-model".into());
    (root, manifest)
}
fn policy(mode: SkillLoadingMode) -> ModelConfiguration {
    ModelConfiguration {
        skills: ModelSkillConfig {
            mode,
            ..Default::default()
        },
    }
}
fn selective(skill: &str, path: &str) -> ModelConfiguration {
    ModelConfiguration {
        skills: ModelSkillConfig {
            mode: SkillLoadingMode::Selective,
            include: vec![SkillInclude {
                skill: skill.into(),
                paths: vec![path.into()],
            }],
            ..Default::default()
        },
    }
}
fn submitted(calls: &Arc<Mutex<Vec<Vec<ChatMessage>>>>) -> String {
    calls.lock().unwrap()[0]
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}
#[tokio::test]
async fn default_catalog_never_submits_skill_bodies() {
    let (root, manifest) = fixture();
    let (p, calls) = provider("actual-model");
    let mut core = DustCore::new(manifest, p)
        .with_app_resources(root.path(), None)
        .unwrap();
    assert_eq!(
        core.execute_report("task").await.stop_reason,
        StopReason::Completed
    );
    let text = submitted(&calls);
    assert!(text.contains("Description review"));
    assert!(text.contains("dustagent__read_skill"));
    assert!(!text.contains("BODY_"));
    assert!(!text.contains("REFERENCE_"));
    assert!(!text.contains("Description private"));
}
#[tokio::test]
async fn actual_provider_model_overrides_manifest_default_and_wildcard() {
    let (root, mut manifest) = fixture();
    manifest
        .model_configurations
        .insert("default-model".into(), policy(SkillLoadingMode::Catalog));
    manifest
        .model_configurations
        .insert("*".into(), selective("review", "references/check.md"));
    manifest
        .model_configurations
        .insert("actual-model".into(), policy(SkillLoadingMode::Preload));
    let (p, calls) = provider("actual-model");
    let mut core = DustCore::new(manifest, p)
        .with_app_resources(root.path(), None)
        .unwrap();
    assert_eq!(
        core.execute_report("task").await.stop_reason,
        StopReason::Completed
    );
    let text = submitted(&calls);
    assert!(text.contains("BODY_review"));
    assert!(text.contains("references/check.md"));
    assert!(!text.contains("REFERENCE_review"));
    assert!(!text.contains("BODY_private"));
}
#[tokio::test]
async fn wildcard_selective_submits_only_named_reference_with_provenance() {
    let (root, mut manifest) = fixture();
    manifest
        .model_configurations
        .insert("*".into(), selective("review", "references/check.md"));
    let (p, calls) = provider("unknown-model");
    let mut core = DustCore::new(manifest, p)
        .with_app_resources(root.path(), None)
        .unwrap();
    assert_eq!(
        core.execute_report("task").await.stop_reason,
        StopReason::Completed
    );
    let messages = calls.lock().unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(messages[0][1].content.as_deref().unwrap()).unwrap();
    assert_eq!(metadata["preloaded"][0]["skill"], "review");
    assert_eq!(metadata["preloaded"][0]["path"], "references/check.md");
    assert_eq!(metadata["resources"][0]["preloaded"], false);
    drop(messages);
    let text = submitted(&calls);
    assert!(text.contains("REFERENCE_review"));
    assert!(!text.contains("BODY_review"));
    assert!(!text.contains("REFERENCE_private"));
}

#[tokio::test]
async fn reference_preload_submits_content_and_disables_skill_lookup() {
    let (root, mut manifest) = fixture();
    let mut config = policy(SkillLoadingMode::Preload);
    config.skills.include_references = true;
    manifest.model_configurations.insert("*".into(), config);
    let (provider, calls) = provider("actual-model");
    let mut core = DustCore::new(manifest, provider)
        .with_app_resources(root.path(), None)
        .unwrap();
    let tools = core.gather_mcp_tools().await.unwrap();
    assert!(
        !tools
            .iter()
            .any(|tool| tool.name == "dustagent__read_skill")
    );
    let result = core
        .execute_tool(
            "dustagent__read_skill",
            serde_json::json!({"skill":"review","path":"references/check.md"}),
        )
        .await
        .unwrap_err();
    assert!(result.to_string().contains("Skill lookup is disabled"));
    assert_eq!(
        core.execute_report("task").await.stop_reason,
        StopReason::Completed
    );
    let text = submitted(&calls);
    assert!(text.contains("BODY_review"));
    assert!(text.contains("REFERENCE_review"));
    assert!(!text.contains("REFERENCE_private"));
    assert!(!text.contains("dustagent__read_skill"));
    assert!(text.contains("Skill lookup is disabled"));
}
#[test]
fn provider_wrappers_forward_model_identity() {
    let (p, _) = provider("arc-model");
    let p = Arc::new(p);
    assert_eq!(p.model_id(), Some("arc-model"));
    let (p, _) = provider("box-model");
    let p: Box<dyn LlmProvider> = Box::new(p);
    assert_eq!(p.model_id(), Some("box-model"));
}

#[tokio::test]
async fn partial_loading_modes_still_expose_and_allow_reference_lookup() {
    for config in [
        policy(SkillLoadingMode::Catalog),
        policy(SkillLoadingMode::Preload),
        selective("review", "SKILL.md"),
    ] {
        let (root, mut manifest) = fixture();
        manifest.model_configurations.insert("*".into(), config);
        let (provider, _) = provider("actual-model");
        let mut core = DustCore::new(manifest, provider)
            .with_app_resources(root.path(), None)
            .unwrap();
        assert!(
            core.gather_mcp_tools()
                .await
                .unwrap()
                .iter()
                .any(|tool| tool.name == "dustagent__read_skill")
        );
        let content = core
            .execute_tool(
                "dustagent__read_skill",
                serde_json::json!({"skill":"review","path":"references/check.md"}),
            )
            .await
            .unwrap();
        assert!(content.contains("REFERENCE_review"));
    }
}
#[test]
fn invalid_policy_missing_path_and_out_of_scope_skill_fail_before_chat() {
    for config in [
        selective("review", "references/missing.md"),
        selective("private", "SKILL.md"),
        policy(SkillLoadingMode::Selective),
    ] {
        let (root, mut manifest) = fixture();
        manifest
            .model_configurations
            .insert("actual-model".into(), config);
        let (p, calls) = provider("actual-model");
        assert!(
            DustCore::new(manifest, p)
                .with_app_resources(root.path(), None)
                .is_err()
        );
        assert!(calls.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn checkpoint_rejects_changed_effective_model_skill_policy_before_chat() {
    let (root, mut manifest) = fixture();
    manifest
        .model_configurations
        .insert("first".into(), policy(SkillLoadingMode::Preload));
    manifest
        .model_configurations
        .insert("second".into(), policy(SkillLoadingMode::Catalog));
    let path = root.path().join("checkpoint.json");
    let (p, _) = provider("first");
    let mut core = DustCore::new(manifest.clone(), p)
        .with_app_resources(root.path(), None)
        .unwrap()
        .with_checkpoint(&path);
    assert_eq!(
        core.execute_report("task").await.stop_reason,
        StopReason::Completed
    );
    let mut saved = checkpoint::load(&path).unwrap();
    saved.phase = dustagent::CheckpointPhase::Ready;
    saved.messages.push(ChatMessage::user("continue"));
    let (p, calls) = provider("second");
    let mut resumed = DustCore::new(manifest, p)
        .with_app_resources(root.path(), None)
        .unwrap()
        .with_checkpoint(&path);
    let report = resumed.resume_report(&saved).await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    let error = report.error.unwrap();
    assert!(error.contains("skill contents changed"), "{error}");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn cli_model_override_selects_policy_in_actual_http_request() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::time::Duration;
    let (root, mut manifest) = fixture();
    manifest
        .model_configurations
        .insert("default-model".into(), policy(SkillLoadingMode::Catalog));
    manifest
        .model_configurations
        .insert("cli-model".into(), policy(SkillLoadingMode::Preload));
    let manifest_path = root.path().join("app.json");
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        let header = String::from_utf8(header).unwrap();
        let length = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        sender
            .send(serde_json::from_slice::<serde_json::Value>(&body).unwrap())
            .unwrap();
        let response =
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":"done"}}]})
                .to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_dust"))
        .current_dir(root.path())
        .env("OPENAI_API_KEY", "local-test-only")
        .env("OPENAI_BASE_URL", format!("http://{address}/v1"))
        .env("DUST_HISTORY_HOME", root.path().join("history"))
        .args([
            "run",
            manifest_path.to_str().unwrap(),
            "--model",
            "cli-model",
            "task",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    server.join().unwrap();
    assert_eq!(request["model"], "cli-model");
    let sent = request["messages"].to_string();
    assert!(sent.contains("BODY_review"));
    assert!(!sent.contains("REFERENCE_review"));
    assert!(!sent.contains("BODY_private"));
    assert_eq!(request["messages"][1]["role"], "system");
    let prompt: serde_json::Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(prompt["preloaded"][0]["skill"], "review");
    assert_eq!(prompt["preloaded"][0]["path"], "SKILL.md");
}

#[tokio::test]
async fn preload_over_context_budget_is_rejected_before_provider_request() {
    let (root, mut manifest) = fixture();
    std::fs::write(
        root.path().join("skills/review/SKILL.md"),
        format!(
            "---\nname: review\ndescription: Description review\n---\n{}",
            "x".repeat(24_000)
        ),
    )
    .unwrap();
    manifest.compaction = Some(dustagent::application::compaction::CompactionConfig {
        context_window_tokens: Some(4096),
        ..Default::default()
    });
    manifest
        .model_configurations
        .insert("actual-model".into(), policy(SkillLoadingMode::Preload));
    let (p, calls) = provider("actual-model");
    let mut core = DustCore::new(manifest, p)
        .with_app_resources(root.path(), None)
        .unwrap();
    let report = core.execute_report("task").await;
    assert_eq!(report.stop_reason, StopReason::ExecutionError);
    assert!(report.error.unwrap().contains("context budget"));
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn cli_checkpoint_resume_accepts_same_policy_and_rejects_changed_policy_before_http() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::time::Duration;
    let (root, mut manifest) = fixture();
    manifest
        .model_configurations
        .insert("first".into(), policy(SkillLoadingMode::Preload));
    manifest
        .model_configurations
        .insert("second".into(), policy(SkillLoadingMode::Catalog));
    let manifest_path = root.path().join("app.json");
    let checkpoint_path = root.path().join("checkpoint.json");
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        for message in [
            serde_json::json!({"role":"assistant","tool_calls":[{"id":"hash1","type":"function","function":{"name":"dustagent__hash","arguments":"{\"input\":\"body\"}"}}]}),
            serde_json::json!({"role":"assistant","content":"done"}),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            sender
                .send(serde_json::from_slice::<serde_json::Value>(&body).unwrap())
                .unwrap();
            let response = serde_json::json!({"choices":[{"message":message}]}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        }
    });
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dust"))
            .current_dir(root.path())
            .env("OPENAI_API_KEY", "local-test-only")
            .env("OPENAI_BASE_URL", format!("http://{address}/v1"))
            .env("DUST_HISTORY_HOME", root.path().join("history"))
            .args(["run", manifest_path.to_str().unwrap()])
            .args(extra)
            .output()
            .unwrap()
    };
    let first = run(&[
        "--model",
        "first",
        "--checkpoint",
        checkpoint_path.to_str().unwrap(),
        "--max-turns",
        "1",
        "task",
    ]);
    assert_eq!(
        first.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let request = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(request["model"], "first");
    let seed = checkpoint::load(&checkpoint_path).unwrap();
    assert_eq!(seed.phase, dustagent::CheckpointPhase::Ready);
    let changed = run(&[
        "--model",
        "second",
        "--resume",
        checkpoint_path.to_str().unwrap(),
    ]);
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("skill contents changed"));
    assert!(receiver.try_recv().is_err());
    let same = run(&[
        "--model",
        "first",
        "--resume",
        checkpoint_path.to_str().unwrap(),
    ]);
    assert!(
        same.status.success(),
        "{}",
        String::from_utf8_lossy(&same.stderr)
    );
    let request = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(request["model"], "first");
    assert!(request["messages"].to_string().contains("BODY_review"));
    server.join().unwrap();
}
