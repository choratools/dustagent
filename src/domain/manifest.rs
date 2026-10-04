use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{DustError, Result};

/// Configuration for an individual stdio-based MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
    /// Command executable to run (e.g. "uvx", "npx", "python3").
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional environment variables for the child process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// Opt-in Linux filesystem jail for this server only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chroot: Option<McpChrootConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpChrootConfig {
    pub root: PathBuf,
    /// Numeric, non-root uid:gid used inside the jail.
    pub user: String,
}

impl McpServerConfig {
    /// Create a new MCP server configuration with the given command.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            env: None,
            chroot: None,
        }
    }

    /// Set or append arguments.
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Set environment variables.
    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = Some(env);
        self
    }

    pub fn validate_chroot(&self) -> Result<()> {
        let Some(jail) = &self.chroot else {
            return Ok(());
        };
        if !cfg!(target_os = "linux") {
            return Err(DustError::Manifest(
                "MCP chroot is supported only on Linux".into(),
            ));
        }
        if !jail.root.is_absolute() || !jail.root.is_dir() {
            return Err(DustError::Manifest(
                "MCP chroot root must be an absolute existing directory".into(),
            ));
        }
        if std::fs::canonicalize(&jail.root)? == Path::new("/") {
            return Err(DustError::Manifest(
                "MCP chroot root must not be the host root /".into(),
            ));
        }
        let valid_id = |id: &str| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && id.parse::<u32>().is_ok_and(|id| id > 0 && id < u32::MAX)
        };
        let valid_user = jail
            .user
            .split_once(':')
            .is_some_and(|(uid, gid)| valid_id(uid) && valid_id(gid));
        if !valid_user {
            return Err(DustError::Manifest(
                "MCP chroot user must be non-root numeric uid:gid".into(),
            ));
        }
        if !Path::new(&self.command).is_absolute() {
            return Err(DustError::Manifest(
                "MCP chroot command must be an absolute path inside the jail".into(),
            ));
        }
        if self.env.as_ref().is_some_and(|env| {
            env.keys().any(|name| {
                name.starts_with("LD_")
                    || matches!(
                        name.as_str(),
                        "GLIBC_TUNABLES" | "GCONV_PATH" | "LOCPATH" | "NLSPATH"
                    )
            })
        }) {
            return Err(DustError::Manifest("MCP chroot env cannot configure the host loader (LD_*, GLIBC_TUNABLES, GCONV_PATH, LOCPATH, NLSPATH)".into()));
        }
        Ok(())
    }
}

/// Portable application package identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dust_version: Option<String>,
}

/// Per-model input presentation policy; app capabilities remain unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ModelConfiguration {
    #[serde(default)]
    pub skills: crate::application::skills::ModelSkillConfig,
}

/// Agent-as-an-Application (AaaA) manifest configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AppManifest {
    /// Exact actual model IDs; "*" is the optional fallback.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_configurations: HashMap<String, ModelConfiguration>,
    /// Context compaction policy; omitted uses conservative automatic defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<crate::application::compaction::CompactionConfig>,
    /// Opt-in execution-local JSON memo tools.
    #[serde(default, skip_serializing_if = "is_false")]
    pub working_state: bool,
    /// Bounded provider retry policy; omitted means the runtime default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_retry: Option<crate::application::retry::RetryConfig>,
    /// Distribution metadata for a directory or archive package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageMetadata>,
    /// Explicitly permitted skill folders relative to this application's skills/.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// JSON Schema URI or identifier.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Unique name of the application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// System prompt defining agent persona and operational constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Default LLM model identifier (e.g. "gpt-4o-mini", "claude-3-5-sonnet").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Scoped MCP servers attached to this application.
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Preferred output format (e.g. "raw_json", "search_replace_patch").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    /// Optional max conversation turns for tool calling loop (default: 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    /// Overall execution timeout in milliseconds (default: 300000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Per-tool timeout in milliseconds (default: 30000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_ms: Option<u64>,
    /// Explicit local checker for recorded input/output pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<crate::application::validation::ValidationConfig>,
    /// Sources for external reinforcement research; empty URLs enable discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research: Option<crate::application::research::ResearchConfig>,
}

impl AppManifest {
    /// Create an empty manifest with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load and deserialize an `AppManifest` from a JSON file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path_ref = path.as_ref();
        let content = std::fs::read_to_string(path_ref).map_err(|e| {
            DustError::Manifest(format!(
                "Failed to read manifest file '{}': {}",
                path_ref.display(),
                e
            ))
        })?;
        Self::from_json_str(&content)
    }

    /// Deserialize an `AppManifest` from a JSON string.
    pub fn from_json_str(content: &str) -> Result<Self> {
        let manifest: Self = serde_json::from_str(content)
            .map_err(|e| DustError::Manifest(format!("Failed to parse manifest JSON: {e}")))?;
        manifest.validate_native_namespace()?;
        manifest.validate_model_configurations()?;
        for config in manifest.mcp_servers.values() {
            config.validate_chroot()?;
        }
        if let Some(config) = &manifest.compaction {
            config.validate()?;
        }
        if let Some(policy) = &manifest.provider_retry {
            policy.validate()?;
        }
        Ok(manifest)
    }

    pub fn validate_model_configurations(&self) -> Result<()> {
        if self.model_configurations.len() > 128 {
            return Err(DustError::Manifest(
                "At most 128 model configurations are allowed".into(),
            ));
        }
        for (model, config) in &self.model_configurations {
            if model.is_empty() || model.len() > 256 || model.trim() != model {
                return Err(DustError::Manifest(
                    "Model configuration keys must be nonempty model IDs or *".into(),
                ));
            }
            config.skills.validate().map_err(|error| {
                DustError::Manifest(format!("Invalid model configuration {model}: {error:#}"))
            })?;
        }
        Ok(())
    }

    /// Native tool namespace cannot be replaced by a configured MCP server.
    pub fn validate_native_namespace(&self) -> Result<()> {
        if self.mcp_servers.contains_key("dustagent") {
            return Err(DustError::Manifest(
                "MCP namespace dustagent is reserved for native tools".into(),
            ));
        }
        Ok(())
    }

    /// Serialize this manifest to a pretty-printed JSON string.
    pub fn to_json_string(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(Into::into)
    }

    /// Set application name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set application description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set system prompt.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Set default model.
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Attach an MCP server configuration.
    pub fn with_mcp_server(mut self, name: impl Into<String>, config: McpServerConfig) -> Self {
        self.mcp_servers.insert(name.into(), config);
        self
    }
}

/// Resolves an application manifest path by checking:
/// 1. Direct path (`app_name_or_path`)
/// 2. Direct path relative to `base_dir`
/// 3. `{base_dir}/apps/{app_name_or_path}.json`
/// 4. `{base_dir}/apps/{app_name_or_path}`
/// 5. `{base_dir}/{app_name_or_path}.json`
pub fn resolve_manifest_path(app_name_or_path: &str, base_dir: &Path) -> Result<PathBuf> {
    // 1. Check if direct path exists as is
    let direct = PathBuf::from(app_name_or_path);
    if direct.is_file() {
        return Ok(direct);
    }

    // 2. Check direct path relative to base_dir
    let direct_base = base_dir.join(app_name_or_path);
    if direct_base.is_file() {
        return Ok(direct_base);
    }

    // 3. Check apps/{app_name}.json relative to base_dir
    let in_apps_with_json = base_dir
        .join("apps")
        .join(format!("{app_name_or_path}.json"));
    if in_apps_with_json.is_file() {
        return Ok(in_apps_with_json);
    }

    // 4. Check apps/{app_name} relative to base_dir
    let in_apps_direct = base_dir.join("apps").join(app_name_or_path);
    if in_apps_direct.is_file() {
        return Ok(in_apps_direct);
    }

    // 5. Check {app_name}.json relative to base_dir
    let in_base_with_json = base_dir.join(format!("{app_name_or_path}.json"));
    if in_base_with_json.is_file() {
        return Ok(in_base_with_json);
    }

    Err(DustError::Manifest(format!(
        "App manifest '{app_name_or_path}' not found in '{}' or apps/ directory",
        base_dir.display()
    )))
}

fn is_false(value: &bool) -> bool {
    !*value
}
