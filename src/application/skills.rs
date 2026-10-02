//! App-local, read-only skill discovery and bounded resource access.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

const MAX_FILE: u64 = 64 * 1024;
const MAX_TOTAL: u64 = 16 * 1024 * 1024;
const MAX_FILES: usize = 4096;

#[derive(Debug, Deserialize)]
struct Metadata {
    name: String,
    description: String,
    compatibility: Option<String>,
    metadata: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone)]
struct Skill {
    directory: PathBuf,
    description: String,
}

#[derive(Debug, Clone)]
pub struct SkillCatalog {
    skills: BTreeMap<String, Skill>,
    digest: String,
}

impl SkillCatalog {
    pub fn load(root: &Path, names: &[String]) -> Result<Self> {
        let root = root.canonicalize().context("invalid app package root")?;
        let mut names = names.to_vec();
        names.sort();
        let mut skills = BTreeMap::new();
        let mut hash = Sha256::new();
        let mut total = 0;
        let mut count = 0;
        if !names.is_empty() {
            reject_symlink(&root.join("skills"))?;
        }
        for name in &names {
            validate_name(name)?;
            if skills.contains_key(name) {
                bail!("duplicate declared skill: {name}");
            }
            let directory = root.join("skills").join(name);
            reject_symlink(&directory)?;
            if !directory.is_dir() {
                bail!("skill directory missing: {name}");
            }
            let text = read_text(&directory, Path::new("SKILL.md"))?;
            let meta = frontmatter(&text)?;
            if meta.name != *name {
                bail!("skill folder and frontmatter name differ: {name}");
            }
            if meta.description.trim().is_empty() || meta.description.chars().count() > 1024 {
                bail!("skill description is empty: {name}");
            }
            if meta
                .compatibility
                .as_ref()
                .is_some_and(|c| c.is_empty() || c.chars().count() > 500)
            {
                bail!("invalid skill compatibility");
            }
            let _ = meta.metadata;
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash_tree(&directory, &directory, &mut hash, &mut total, &mut count)?;
            skills.insert(
                name.clone(),
                Skill {
                    directory,
                    description: meta.description,
                },
            );
        }
        Ok(Self {
            skills,
            digest: format!("{:x}", hash.finalize()),
        })
    }
    pub fn summary(&self) -> String {
        // JSON escaping prevents metadata from breaking the catalog's structure.
        serde_json::to_string(&self.skills.iter().map(|(name, skill)| serde_json::json!({"name":name,"description":skill.description})).collect::<Vec<_>>()).expect("serializable skill metadata")
    }
    pub fn digest(&self) -> String {
        self.digest.clone()
    }
    pub fn read(&self, skill: &str, path: Option<&str>) -> Result<String> {
        let entry = self
            .skills
            .get(skill)
            .context("skill is not declared by this app")?;
        let path = Path::new(path.unwrap_or("SKILL.md"));
        validate_resource(path)?;
        read_text(&entry.directory, path)
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || name.contains("--")
        || name.starts_with('-')
        || name.ends_with('-')
    {
        bail!("invalid skill name: {name}");
    }
    Ok(())
}
fn frontmatter(text: &str) -> Result<Metadata> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        bail!("SKILL.md requires YAML frontmatter");
    }
    let mut yaml = String::new();
    for line in lines {
        if line == "---" {
            let value: serde_yaml::Value =
                serde_yaml::from_str(&yaml).context("invalid skill YAML frontmatter")?;
            return serde_yaml::from_value(value)
                .context("skill frontmatter fields must use the required types");
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    bail!("unterminated skill YAML frontmatter")
}
fn reject_symlink(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("skill symlinks are not allowed: {}", path.display());
    }
    Ok(())
}
fn validate_resource(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("invalid skill resource path");
    }
    if path == Path::new("SKILL.md") {
        return Ok(());
    }
    let first = path.components().next().unwrap().as_os_str();
    if !["references", "scripts", "assets"]
        .iter()
        .any(|s| first == *s)
        || path.components().count() < 2
    {
        bail!("resource must be SKILL.md or inside references/scripts/assets");
    }
    Ok(())
}
fn read_text(directory: &Path, relative: &Path) -> Result<String> {
    if let Some(parent) = directory.parent() {
        reject_symlink(parent)?;
    }
    reject_symlink(directory)?;
    let mut target = directory.to_path_buf();
    for part in relative.components() {
        target.push(part);
        reject_symlink(&target)?;
    }
    let metadata = fs::metadata(&target)?;
    if !metadata.is_file() || metadata.len() > MAX_FILE {
        bail!("skill resource must be a regular file of at most {MAX_FILE} bytes");
    }
    // A bounded read also handles files growing after metadata inspection.
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(&target)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        bail!("skill resource exceeds size limit");
    }
    String::from_utf8(bytes).context("skill resource is not UTF-8 text")
}
fn hash_tree(
    base: &Path,
    directory: &Path,
    hash: &mut Sha256,
    total: &mut u64,
    count: &mut usize,
) -> Result<()> {
    if directory.strip_prefix(base)?.components().count() > 32 {
        bail!("skill resource nesting exceeds limit");
    }
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!("skill symlinks are not allowed");
        }
        if metadata.is_dir() {
            hash_tree(base, &path, hash, total, count)?;
        } else if metadata.is_file() {
            *count += 1;
            *total += metadata.len();
            if *count > MAX_FILES || *total > MAX_TOTAL {
                bail!("skill catalog exceeds content limits");
            }
            let relative = path
                .strip_prefix(base)?
                .to_str()
                .context("skill path is not UTF-8")?
                .replace('\\', "/");
            hash.update((relative.len() as u64).to_le_bytes());
            hash.update(relative.as_bytes());
            use std::io::Read;
            let mut bytes = Vec::new();
            fs::File::open(path)?
                .take(MAX_TOTAL + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_TOTAL || bytes.len() as u64 != metadata.len() {
                bail!("skill resource changed while loading");
            }
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        } else {
            bail!("skill resources must be regular files or directories");
        }
    }
    Ok(())
}
