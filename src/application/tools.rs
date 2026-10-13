//! App-scoped callable tools. Programs receive JSON on stdin and return one JSON value.
use super::output_schema::{OutputSchema, OutputType};
use crate::{
    error::{DustError, Result},
    ports::llm::ToolDefinition,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Component, Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const MAX_FILES: usize = 4096;
const MAX_ASSET_BYTES: usize = 64 * 1024 * 1024;
const MAX_INPUT: usize = 1024 * 1024;
const MAX_STDOUT: usize = 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppToolConfig {
    pub description: String,
    pub input_schema: OutputSchema,
    pub command: String,
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

fn failure(message: impl Into<String>) -> DustError {
    DustError::Config(message.into())
}

pub fn validate_configs(configs: &HashMap<String, AppToolConfig>) -> Result<()> {
    if configs.len() > 128 {
        return Err(failure("app tools exceed 128 declarations"));
    }
    for (name, config) in configs {
        if name.is_empty()
            || name.len() > 59
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(failure(format!("invalid app tool name: {name}")));
        }
        if config.description.trim().is_empty() || config.description.len() > 8192 {
            return Err(failure(format!(
                "app tool {name} requires a bounded nonempty description"
            )));
        }
        if config.command.trim().is_empty()
            || config.command.len() > 4096
            || config.command.contains('\0')
        {
            return Err(failure(format!("invalid command for app tool {name}")));
        }
        let components = Path::new(&config.entrypoint)
            .components()
            .collect::<Vec<_>>();
        if components.len() < 2
            || components.first() != Some(&Component::Normal(std::ffi::OsStr::new("tools")))
            || !components.iter().all(|c| matches!(c, Component::Normal(_)))
            || config.entrypoint.contains('\0')
            || config.entrypoint.contains('\\')
        {
            return Err(failure(format!(
                "app tool {name} entrypoint must be a relative file beneath tools/"
            )));
        }
        if config.args.len() > 128
            || config
                .args
                .iter()
                .any(|a| a.len() > 8192 || a.contains('\0'))
        {
            return Err(failure(format!("invalid arguments for app tool {name}")));
        }
        config.input_schema.validate()?;
        if config.input_schema.schema_type != OutputType::Object {
            return Err(failure(format!(
                "app tool {name} input_schema must have type object"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ToolCatalog {
    root: PathBuf,
    configs: BTreeMap<String, AppToolConfig>,
    digest: String,
}

impl ToolCatalog {
    pub fn load(root: &Path, configs: &HashMap<String, AppToolConfig>) -> Result<Self> {
        validate_configs(configs)?;
        let root = root.canonicalize()?;
        let configs: BTreeMap<_, _> = configs
            .iter()
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect();
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(&configs)?);
        if !configs.is_empty() {
            let mut files = Vec::new();
            collect_assets(&root.join("tools"), &mut files, 0, &mut 0)?;
            files.sort();
            let mut total = 0usize;
            for file in files {
                let metadata = std::fs::symlink_metadata(&file)?;
                let size =
                    usize::try_from(metadata.len()).map_err(|_| failure("tool asset too large"))?;
                total = total
                    .checked_add(size)
                    .ok_or_else(|| failure("tool assets too large"))?;
                if total > MAX_ASSET_BYTES {
                    return Err(failure("tool assets exceed 64 MiB"));
                }
                let relative = file
                    .strip_prefix(&root)
                    .map_err(|_| failure("tool asset escaped app root"))?;
                let name = relative
                    .to_str()
                    .ok_or_else(|| failure("tool asset paths must be UTF-8"))?;
                hash.update((name.len() as u64).to_le_bytes());
                hash.update(name.as_bytes());
                hash.update((size as u64).to_le_bytes());
                let mut bytes = Vec::new();
                std::io::Read::take(std::fs::File::open(&file)?, (size as u64) + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() != size {
                    return Err(failure("tool asset changed while loading"));
                }
                hash.update(bytes);
            }
            for (name, config) in &configs {
                let path = root.join(&config.entrypoint);
                if !std::fs::symlink_metadata(&path)?.file_type().is_file() {
                    return Err(failure(format!(
                        "app tool {name} entrypoint must be a regular file"
                    )));
                }
            }
        }
        Ok(Self {
            root,
            configs,
            digest: format!("{:x}", hash.finalize()),
        })
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.configs
            .iter()
            .map(|(name, c)| {
                ToolDefinition::new(
                    format!("app__{name}"),
                    &c.description,
                    serde_json::to_value(&c.input_schema).expect("serializable schema"),
                )
            })
            .collect()
    }
    pub fn digest(&self) -> String {
        self.digest.clone()
    }
    pub fn contains(&self, name: &str) -> bool {
        name.strip_prefix("app__")
            .is_some_and(|key| self.configs.contains_key(key))
    }
    pub async fn call(&self, name: &str, arguments: Value) -> Result<String> {
        let key = name.strip_prefix("app__").unwrap_or(name);
        let config = self
            .configs
            .get(key)
            .ok_or_else(|| failure(format!("undeclared app tool: {name}")))?;
        let input = serde_json::to_vec(&arguments)?;
        if input.len() > MAX_INPUT {
            return Err(failure("app tool JSON input exceeds 1 MiB"));
        }
        let evidence = config
            .input_schema
            .validate_output(std::str::from_utf8(&input).expect("JSON UTF-8"))?;
        if !evidence.passed {
            return Err(failure(format!(
                "app tool {key} invalid input: {}",
                evidence.reason
            )));
        }
        // Revalidate identity before executing mutable filesystem resources.
        let current = Self::load(
            &self.root,
            &self
                .configs
                .iter()
                .map(|(n, c)| (n.clone(), c.clone()))
                .collect(),
        )?;
        if current.digest != self.digest {
            return Err(failure("app tool resources changed since initialization"));
        }
        let mut child = tokio::process::Command::new(&config.command)
            .args(&config.args)
            .arg(self.root.join(&config.entrypoint))
            .current_dir(&self.root)
            // Local Python imports must not mutate the immutable tools tree.
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let write = async move {
            stdin.write_all(&input).await?;
            stdin.write_all(b"\n").await?;
            stdin.shutdown().await
        };
        let (_, stdout, stderr, status) = tokio::try_join!(
            write,
            read_bounded(stdout, MAX_STDOUT),
            read_bounded(stderr, MAX_STDERR),
            child.wait()
        )?;
        if !status.success() {
            return Err(failure(format!(
                "app tool {key} exited with {status}: {}",
                String::from_utf8_lossy(&stderr)
            )));
        }
        let value: Value = serde_json::from_slice(&stdout).map_err(|e| {
            failure(format!(
                "app tool {key} must return exactly one JSON value: {e}"
            ))
        })?;
        Ok(serde_json::to_string(&value)?)
    }
}

fn collect_assets(
    path: &Path,
    files: &mut Vec<PathBuf>,
    depth: usize,
    entries: &mut usize,
) -> Result<()> {
    *entries += 1;
    if *entries > MAX_FILES {
        return Err(failure("tool assets exceed entry count limit"));
    }
    if depth > 64 {
        return Err(failure("tool assets exceed directory depth limit"));
    }
    let kind = std::fs::symlink_metadata(path)?.file_type();
    if kind.is_symlink() {
        return Err(failure("tool assets must not contain symlinks"));
    }
    if kind.is_file() {
        files.push(path.to_owned());
        if files.len() > MAX_FILES {
            return Err(failure("tool assets exceed file count limit"));
        }
    } else if kind.is_dir() {
        for entry in std::fs::read_dir(path)? {
            collect_assets(&entry?.path(), files, depth + 1, entries)?;
        }
    } else {
        return Err(failure("tool assets must be regular files or directories"));
    }
    Ok(())
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > limit {
            return Err(std::io::Error::other(format!(
                "app tool output exceeds {limit} bytes"
            )));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}
