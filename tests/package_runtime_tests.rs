use async_trait::async_trait;
use dustagent::application::{checkpoint, package, skills::SkillCatalog};
use dustagent::{
    ChatMessage, DustCore, LlmProvider, LlmResponse, StopReason, ToolCall, ToolDefinition,
};
use serde_json::json;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
};

struct Provider {
    replies: Mutex<VecDeque<LlmResponse>>,
    messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}
impl Provider {
    fn new(replies: Vec<LlmResponse>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            messages: Arc::default(),
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
        self.messages.lock().unwrap().push(messages.to_vec());
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call"))
    }
}
fn fixture(root: &Path) {
    std::fs::create_dir_all(root.join("skills/reading/references")).unwrap();
    std::fs::create_dir_all(root.join("skills/secret")).unwrap();
    std::fs::write(root.join("app.json"), json!({"package":{"name":"fixture","version":"1.0.0"},"skills":["reading"],"system_prompt":"Read evidence","mcp_servers":{}}).to_string()).unwrap();
    std::fs::write(root.join("skills/reading/SKILL.md"), "---\nname: reading\ndescription: Read coverage evidence\n---\nConsult references/coverage.md.").unwrap();
    std::fs::write(
        root.join("skills/reading/references/coverage.md"),
        "observed=100 remaining=190",
    )
    .unwrap();
    std::fs::write(
        root.join("skills/secret/SKILL.md"),
        "---\nname: secret\ndescription: SECRET_MARKER\n---\nSECRET_MARKER",
    )
    .unwrap();
}
fn read_reference() -> LlmResponse {
    LlmResponse::tool_calls(vec![ToolCall::new(
        "read1",
        "dustagent__read_skill",
        r#"{"skill":"reading","path":"references/coverage.md"}"#,
    )])
}

#[tokio::test]
async fn app_owned_catalog_and_tool_never_expose_undeclared_sibling() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let loaded = package::load(dir.path().to_str().unwrap(), dir.path()).unwrap();
    let catalog = SkillCatalog::load(&loaded.root, &loaded.manifest.skills).unwrap();
    assert!(catalog.summary().contains("reading"));
    assert!(!catalog.summary().contains("SECRET_MARKER"));
    assert!(catalog.read("secret", None).is_err());
    assert!(catalog.read("reading", Some("../secret/SKILL.md")).is_err());
    let provider = Provider::new(vec![LlmResponse::text("done")]);
    let captured = provider.messages.clone();
    let mut core = DustCore::new(loaded.manifest.clone(), provider)
        .with_app_resources(&loaded.root, loaded.digest.as_deref())
        .unwrap();
    assert!(
        core.gather_mcp_tools()
            .await
            .unwrap()
            .iter()
            .any(|t| t.name == "dustagent__read_skill")
    );
    let result = core
        .execute_tool(
            "dustagent__read_skill",
            json!({"skill":"reading","path":"references/coverage.md"}),
        )
        .await
        .unwrap();
    assert!(result.contains("observed=100"));
    std::fs::create_dir_all(loaded.root.join("skills/reading/assets")).unwrap();
    std::fs::write(
        loaded.root.join("skills/reading/assets/escaped.txt"),
        "\n".repeat(40000),
    )
    .unwrap();
    let oversized = core
        .execute_tool(
            "dustagent__read_skill",
            json!({"skill":"reading","path":"assets/escaped.txt"}),
        )
        .await
        .unwrap();
    let oversized: serde_json::Value = serde_json::from_str(&oversized).unwrap();
    assert_eq!(oversized["isError"], true);
    let denied = core
        .execute_tool("dustagent__read_skill", json!({"skill":"secret"}))
        .await
        .unwrap();
    assert!(denied.contains("\"isError\":true"));
    assert!(!denied.contains("SECRET_MARKER"));
    assert_eq!(
        core.execute_report("input").await.stop_reason,
        StopReason::Completed
    );
    {
        let messages = captured.lock().unwrap();
        assert!(messages[0].iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|s| s.contains("Read coverage evidence"))
        }));
        assert!(!messages[0].iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|s| s.contains("SECRET_MARKER"))
        }));
    }
    let provider = Provider::new(vec![]);
    let captured = provider.messages.clone();
    let mut unattached = DustCore::new(loaded.manifest, provider);
    assert_eq!(
        unattached.execute_report("input").await.stop_reason,
        StopReason::ExecutionError
    );
    assert!(captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn archive_resume_survives_extraction_path_change_but_rejects_modified_resources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("source");
    fixture(&root);
    let archive = package::pack(&root, Some(&dir.path().join("fixture.dustpkg"))).unwrap();
    let first = package::load(archive.to_str().unwrap(), dir.path()).unwrap();
    let second = package::load(archive.to_str().unwrap(), dir.path()).unwrap();
    assert_ne!(first.root, second.root);
    assert_eq!(first.digest, second.digest);
    let state = dir.path().join("state.json");
    let mut core = DustCore::new(
        first.manifest.clone(),
        Provider::new(vec![read_reference()]),
    )
    .with_app_resources(&first.root, first.digest.as_deref())
    .unwrap()
    .with_checkpoint(&state)
    .with_max_turns(1);
    assert_eq!(
        core.execute_report("original").await.stop_reason,
        StopReason::TurnLimit
    );
    let saved = checkpoint::load(&state).unwrap();
    let provider = Provider::new(vec![LlmResponse::text("complete")]);
    let captured = provider.messages.clone();
    let mut resumed = DustCore::new(second.manifest.clone(), provider)
        .with_app_resources(&second.root, second.digest.as_deref())
        .unwrap()
        .with_checkpoint(&state);
    let report = resumed.resume_report(&saved).await;
    assert_eq!(report.stop_reason, StopReason::Completed);
    assert_eq!(report.tool_calls.len(), 1);
    assert!(captured.lock().unwrap()[0].iter().any(|m| {
        m.role == "tool"
            && m.content
                .as_deref()
                .is_some_and(|s| s.contains("remaining=190"))
    }));
    std::fs::write(
        second.root.join("skills/reading/references/coverage.md"),
        "changed",
    )
    .unwrap();
    let provider = Provider::new(vec![]);
    let captured = provider.messages.clone();
    let mut changed = DustCore::new(second.manifest.clone(), provider)
        .with_app_resources(&second.root, second.digest.as_deref())
        .unwrap()
        .with_checkpoint(&state);
    assert_eq!(
        changed.resume_report(&saved).await.stop_reason,
        StopReason::ExecutionError
    );
    assert!(captured.lock().unwrap().is_empty());
    let provider = Provider::new(vec![]);
    let captured = provider.messages.clone();
    let mut different_package = DustCore::new(first.manifest.clone(), provider)
        .with_app_resources(&first.root, Some("changed-package-digest"))
        .unwrap()
        .with_checkpoint(&state);
    assert_eq!(
        different_package.resume_report(&saved).await.stop_reason,
        StopReason::ExecutionError
    );
    assert!(captured.lock().unwrap().is_empty());
    let installed = package::install(&archive, &dir.path().join("store")).unwrap();
    let loaded = package::load(installed.to_str().unwrap(), dir.path()).unwrap();
    assert_eq!(loaded.digest, first.digest);
}

fn serve_once() -> String {
    serve_message("package complete", true)
}
fn serve_message(content: &str, expect_tools: bool) -> String {
    let content = content.to_owned();
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        let text = String::from_utf8(headers).unwrap();
        let length = text
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if expect_tools {
            assert!(
                request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["function"]["name"] == "dustagent__read_skill")
            );
        }
        let response =
            json!({"choices":[{"message":{"role":"assistant","content":content}}]}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).unwrap();
    });
    format!("http://{address}/v1")
}
#[test]
fn cli_packs_installs_and_runs_all_three_package_sources() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    fixture(&source);
    let archive = dir.path().join("fixture.dustpkg");
    let store = dir.path().join("store");
    let result = Command::new(env!("CARGO_BIN_EXE_dust"))
        .args([
            "pack",
            source.to_str().unwrap(),
            "--output",
            archive.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = Command::new(env!("CARGO_BIN_EXE_dust"))
        .args([
            "install",
            archive.to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for app in [
        source.to_str().unwrap(),
        archive.to_str().unwrap(),
        "fixture",
    ] {
        let url = serve_once();
        let result = Command::new(env!("CARGO_BIN_EXE_dust"))
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env("DUST_PACKAGE_HOME", &store)
            .env("OPENAI_API_KEY", "test-only")
            .env("OPENAI_BASE_URL", url)
            .args(["run", app, "--json", "original input"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["output"], "package complete");
    }
}

#[tokio::test]
async fn native_namespace_cannot_be_overridden_by_mcp_configuration() {
    use dustagent::{AppManifest, McpServerConfig};
    let parsed = AppManifest::from_json_str(
        r#"{"mcp_servers":{"dustagent":{"command":"not-a-real-command"}}}"#,
    );
    assert!(parsed.is_err());
    let provider = Provider::new(vec![]);
    let calls = provider.messages.clone();
    let manifest =
        AppManifest::new().with_mcp_server("dustagent", McpServerConfig::new("not-a-real-command"));
    let mut core = DustCore::new(manifest, provider);
    assert!(
        core.init_scoped_mcp()
            .await
            .unwrap_err()
            .to_string()
            .contains("reserved")
    );
    assert!(core.gather_mcp_tools().await.is_err());
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn cli_new_creates_packable_package_and_stdout_does_not_write_files() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let generated = json!({"name":"model-name","skills":["model-skill"],"system_prompt":"generated","mcp_servers":{}}).to_string();
    let url = serve_message(&generated, false);
    let result = Command::new(env!("CARGO_BIN_EXE_dust"))
        .current_dir(dir.path())
        .env("OPENAI_API_KEY", "test-only")
        .env("OPENAI_BASE_URL", url)
        .args(["new", "generated", "Create fixture"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let root = dir.path().join("apps/generated");
    assert!(root.join("skills").is_dir());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("app.json")).unwrap()).unwrap();
    assert_eq!(manifest["package"]["name"], "generated");
    assert_eq!(manifest["package"]["version"], "0.1.0");
    assert_eq!(manifest["skills"], json!([]));
    assert!(package::pack(&root, Some(&dir.path().join("generated.dustpkg"))).is_ok());
    let duplicate = Command::new(env!("CARGO_BIN_EXE_dust"))
        .current_dir(dir.path())
        .env("OPENAI_API_KEY", "test-only")
        .env("OPENAI_BASE_URL", "http://127.0.0.1:1/v1")
        .args(["new", "generated", "Duplicate fixture"])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already exists"));
    let url = serve_message(&generated, false);
    let stdout = Command::new(env!("CARGO_BIN_EXE_dust"))
        .current_dir(dir.path())
        .env("OPENAI_API_KEY", "test-only")
        .env("OPENAI_BASE_URL", url)
        .args(["new", "stdout-fixture", "Create fixture", "--stdout"])
        .output()
        .unwrap();
    assert!(
        stdout.status.success(),
        "{}",
        String::from_utf8_lossy(&stdout.stderr)
    );
    let manifest: serde_json::Value = serde_json::from_slice(&stdout.stdout).unwrap();
    assert_eq!(manifest["package"]["name"], "stdout-fixture");
    assert!(!dir.path().join("apps/stdout-fixture").exists());
}
