use dustagent::domain::manifest::{AppManifest, McpChrootConfig, McpServerConfig};
use dustagent::{DustCore, LlmProvider, McpClient, McpStdioClient};
use serde_json::json;

fn config(root: &std::path::Path) -> McpServerConfig {
    let mut config = McpServerConfig::new("/busybox").with_args(["sh", "/server.sh"]);
    config.chroot = Some(McpChrootConfig {
        root: root.to_owned(),
        user: "65534:65534".into(),
    });
    config
}

#[cfg(target_os = "linux")]
#[test]
fn config_validates_jail_boundary_and_preserves_legacy_serialization() {
    let temp = tempfile::tempdir().unwrap();
    let mut jailed = config(temp.path());
    assert!(jailed.validate_chroot().is_ok());
    for user in [
        "0:1",
        "1:0",
        "root:root",
        "1",
        "1:1:1",
        "-1:1",
        "4294967295:1",
    ] {
        jailed.chroot.as_mut().unwrap().user = user.into();
        assert!(jailed.validate_chroot().is_err(), "{user}");
    }
    jailed.chroot.as_mut().unwrap().user = "1:1".into();
    for name in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "GLIBC_TUNABLES",
        "GCONV_PATH",
    ] {
        jailed.env = Some(std::collections::HashMap::from([(
            name.into(),
            "unsafe".into(),
        )]));
        assert!(jailed.validate_chroot().is_err());
    }
    jailed.env = None;
    jailed.command = "node".into();
    assert!(jailed.validate_chroot().is_err());
    jailed.command = "/node".into();
    jailed.chroot.as_mut().unwrap().root = "/".into();
    assert!(jailed.validate_chroot().is_err());
    jailed.chroot.as_mut().unwrap().root = "relative".into();
    assert!(jailed.validate_chroot().is_err());
    assert!(
        serde_json::to_value(McpServerConfig::new("node"))
            .unwrap()
            .get("chroot")
            .is_none()
    );
    assert!(AppManifest::from_json_str(&json!({"mcp_servers":{"files":{"command":"node","chroot":{"root":temp.path(),"user":"1:1"}}}}).to_string()).is_err());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn chroot_configuration_rejects_unsupported_platform() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        config(temp.path())
            .validate_chroot()
            .unwrap_err()
            .to_string()
            .contains("only on Linux")
    );
}

struct NoModel;
#[async_trait::async_trait]
impl LlmProvider for NoModel {
    async fn chat(
        &self,
        _: &[dustagent::ChatMessage],
        _: Option<&[dustagent::ToolDefinition]>,
    ) -> dustagent::Result<dustagent::LlmResponse> {
        panic!("startup must never call model")
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn jail_missing_target_is_fatal_for_core_without_host_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let mut jailed = config(temp.path());
    jailed.command = "/bin/echo".into(); // exists on the host, deliberately absent in jail
    let manifest = AppManifest::new().with_mcp_server("files", jailed);
    let mut core = DustCore::new(manifest, NoModel);
    let error = core.init_scoped_mcp().await.unwrap_err().to_string();
    assert!(
        error.contains("Required jailed MCP server 'files' failed"),
        "{error}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_chroot_static_fixture_or_permission_denial() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    if !std::path::Path::new("/usr/bin/busybox").is_file() {
        eprintln!("SKIP actual chroot fixture: /usr/bin/busybox unavailable");
        return;
    }
    std::fs::copy("/usr/bin/busybox", temp.path().join("busybox")).unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), temp.path().join("outside-link")).unwrap();
    std::fs::write(temp.path().join("server.sh"), r#"while IFS= read -r line; do
case "$line" in
*'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}';;
*'"method":"tools/call"'*)
 uid=$(/busybox id -u); gid=$(/busybox id -g); groups=$(/busybox id -G)
 if [ -e /etc/passwd ] || [ -e /../../etc/passwd ] || [ -e /outside-link ]; then visible=true; else visible=false; fi
 printf '{"jsonrpc":"2.0","id":2,"result":{"uid":"%s","gid":"%s","groups":"%s","cwd":"%s","host_visible":%s,"explicit":"%s","home":"%s"}}\n' "$uid" "$gid" "$groups" "$PWD" "$visible" "$EXPLICIT" "$HOME";;
esac
done
"#).unwrap();
    let jailed = config(temp.path()).with_env(std::collections::HashMap::from([(
        "EXPLICIT".into(),
        "kept".into(),
    )]));
    let mut client = McpStdioClient::from_config(&jailed).unwrap();
    match client.initialize().await {
        Err(error) => {
            // Real capability probe distinguishes permission denial from a broken prepared jail.
            let launcher = ["/usr/sbin/chroot", "/usr/bin/chroot", "/sbin/chroot"]
                .into_iter()
                .find(|path| std::path::Path::new(path).is_file())
                .expect("host chroot launcher");
            let probe = std::process::Command::new(launcher)
                .env_clear()
                .args(["--userspec=+65534:+65534", "--groups=", "--"])
                .arg(temp.path())
                .args(["/busybox", "true"])
                .output()
                .unwrap();
            assert!(
                !probe.status.success(),
                "fixture failed with usable chroot: {error}"
            );
            let stderr = String::from_utf8_lossy(&probe.stderr);
            assert!(
                stderr.contains("Operation not permitted") || stderr.contains("Permission denied"),
                "unexpected probe failure: {stderr}"
            );
            eprintln!(
                "Actual chroot denied by host privileges; verified startup fails without fallback: {error}"
            );
        }
        Ok(()) => {
            let result = client.call_tool("inspect", json!({})).await.unwrap();
            assert_eq!(result["uid"], "65534");
            assert_eq!(result["gid"], "65534");
            assert_eq!(result["groups"], "65534");
            assert_eq!(result["cwd"], "/");
            assert_eq!(result["host_visible"], false);
            assert_eq!(result["explicit"], "kept");
            assert_eq!(result["home"], "");
            client.close().await.unwrap();
        }
    }
}

struct BrokenDiscovery;
#[async_trait::async_trait]
impl McpClient for BrokenDiscovery {
    async fn initialize(&mut self) -> dustagent::Result<()> {
        Ok(())
    }
    async fn list_tools(&mut self) -> dustagent::Result<Vec<dustagent::McpTool>> {
        Err(dustagent::DustError::Mcp(
            "fixture discovery failure".into(),
        ))
    }
    async fn call_tool(
        &mut self,
        _: &str,
        _: serde_json::Value,
    ) -> dustagent::Result<serde_json::Value> {
        panic!("no tool execution")
    }
    async fn close(&mut self) -> dustagent::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn required_jailed_tool_discovery_error_is_fatal() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = AppManifest::new().with_mcp_server("files", config(temp.path()));
    let mut core = DustCore::new(manifest, NoModel);
    core.register_mcp_client("files", Box::new(BrokenDiscovery));
    let error = core.gather_mcp_tools().await.unwrap_err().to_string();
    assert!(
        error.contains("Required jailed MCP server 'files' tool discovery failed"),
        "{error}"
    );
}
