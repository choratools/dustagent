//! Local AaaA packages: tar.gz containers, never executable installers.
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::domain::manifest::{AppManifest, resolve_manifest_path};
use crate::error::{DustError, Result};

const MAX_ENTRIES: usize = 4096;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE: u64 = MAX_TOTAL + 8 * 1024 * 1024;

pub struct LoadedApp {
    pub manifest: AppManifest,
    pub root: PathBuf,
    pub digest: Option<String>,
    _guard: Option<TempDir>,
}

fn invalid(message: impl Into<String>) -> DustError {
    DustError::Manifest(message.into())
}
fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && value != "."
        && value != ".."
}
fn semver(value: &str) -> Result<(u64, u64, u64)> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid("Package version must be numeric X.Y.Z"));
    }
    Ok((
        parts[0].parse().map_err(|_| invalid("Invalid version"))?,
        parts[1].parse().map_err(|_| invalid("Invalid version"))?,
        parts[2].parse().map_err(|_| invalid("Invalid version"))?,
    ))
}
fn validate_manifest(manifest: &AppManifest) -> Result<()> {
    let meta = manifest
        .package
        .as_ref()
        .ok_or_else(|| invalid("Package requires package metadata in app.json"))?;
    if !safe_component(&meta.name) {
        return Err(invalid("Unsafe package name"));
    }
    semver(&meta.version)?;
    if let Some(requirement) = &meta.dust_version {
        let current = semver(env!("CARGO_PKG_VERSION"))?;
        let compatible = if let Some(minimum) = requirement.strip_prefix(">=") {
            current >= semver(minimum)?
        } else {
            current == semver(requirement)?
        };
        if !compatible {
            return Err(invalid(
                "Package is incompatible with this DustAgent version",
            ));
        }
    }
    Ok(())
}
fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}
fn inventory(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(
        root: &Path,
        relative: &Path,
        files: &mut Vec<PathBuf>,
        entries: &mut usize,
        total: &mut u64,
    ) -> Result<()> {
        if relative.components().count() > 64 {
            return Err(invalid("Package directory depth exceeds 64"));
        }
        for item in fs::read_dir(root.join(relative))? {
            let item = item?;
            *entries += 1;
            if *entries > MAX_ENTRIES {
                return Err(invalid("Too many package entries"));
            }
            let path = relative.join(item.file_name());
            let metadata = fs::symlink_metadata(item.path())?;
            if metadata.is_dir() {
                walk(root, &path, files, entries, total)?;
            } else if metadata.is_file() {
                *total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| invalid("Package size overflow"))?;
                if metadata.len() > MAX_FILE || *total > MAX_TOTAL {
                    return Err(invalid("Package size limit exceeded"));
                }
                files.push(path);
            } else {
                return Err(invalid("Package symlinks and special files are forbidden"));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, Path::new(""), &mut files, &mut 0, &mut 0)?;
    files.sort();
    Ok(files)
}
fn bounded_read(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(invalid("Package file grew beyond its size limit"));
    }
    Ok(bytes)
}
fn content_digest(root: &Path, files: &[PathBuf]) -> Result<String> {
    let mut hash = Sha256::new();
    let mut total = 0u64;
    for path in files {
        let name = path
            .to_str()
            .ok_or_else(|| invalid("Package paths must be UTF-8"))?;
        let bytes = bounded_read(&root.join(path))?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL {
            return Err(invalid("Package contents grew beyond total size limit"));
        }
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn directory(root: &Path, guard: Option<TempDir>) -> Result<LoadedApp> {
    let root = root.canonicalize()?;
    let files = inventory(&root)?;
    if !files.iter().any(|p| p == Path::new("app.json")) {
        return Err(invalid("Package root must contain app.json"));
    }
    let manifest = AppManifest::from_file(root.join("app.json"))?;
    validate_manifest(&manifest)?;
    super::skills::SkillCatalog::load(&root, &manifest.skills)
        .map_err(|e| invalid(format!("Invalid package skills: {e}")))?;
    let digest = Some(content_digest(&root, &files)?);
    Ok(LoadedApp {
        manifest,
        root,
        digest,
        _guard: guard,
    })
}
fn extract(source: &Path) -> Result<LoadedApp> {
    let staging = tempfile::tempdir()?;
    let source_file = File::open(source)?;
    if source_file.metadata()?.len() > MAX_ARCHIVE {
        return Err(invalid("Compressed package exceeds 72 MiB limit"));
    }
    let mut archive = tar::Archive::new(GzDecoder::new(source_file.take(MAX_ARCHIVE)));
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for (index, item) in archive.entries()?.raw(true).enumerate() {
        if index >= MAX_ENTRIES {
            return Err(invalid("Too many archive entries"));
        }
        let mut item = item?;
        let path = item.path()?.into_owned();
        if !safe_relative(&path) || !seen.insert(path.clone()) {
            return Err(invalid("Unsafe or duplicate archive path"));
        }
        let kind = item.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(invalid("Archive links and special files are forbidden"));
        }
        let size = item.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("Archive size overflow"))?;
        if size > MAX_FILE || total > MAX_TOTAL {
            return Err(invalid("Archive size limit exceeded"));
        }
        let target = staging.path().join(path);
        if kind.is_dir() {
            fs::create_dir_all(target)?;
        } else {
            fs::create_dir_all(
                target
                    .parent()
                    .ok_or_else(|| invalid("Missing archive parent"))?,
            )?;
            let mut file = File::options().write(true).create_new(true).open(target)?;
            let copied = std::io::copy(&mut item.by_ref().take(MAX_FILE + 1), &mut file)?;
            if copied != size {
                return Err(invalid("Truncated archive entry"));
            }
            file.sync_all()?;
        }
    }
    let root = staging.path().to_path_buf();
    directory(&root, Some(staging))
}
/// Load a directory, tar.gz .dustpkg, legacy JSON, or installed package name.
pub fn load(source: &str, base: &Path) -> Result<LoadedApp> {
    let direct = PathBuf::from(source);
    let path = if direct.exists() {
        direct
    } else {
        base.join(source)
    };
    if path.is_dir() {
        return directory(&path, None);
    }
    if path.is_file() && path.extension().is_some_and(|e| e == "dustpkg") {
        return extract(&path);
    }
    if let Ok(path) = resolve_manifest_path(source, base) {
        let root = path.canonicalize()?.parent().unwrap().to_path_buf();
        let manifest = AppManifest::from_file(&path)?;
        if manifest.package.is_some() {
            return directory(&root, None);
        }
        return Ok(LoadedApp {
            manifest,
            root,
            digest: None,
            _guard: None,
        });
    }
    if safe_component(source) {
        let local = base.join("apps").join(source);
        if local.is_dir() {
            return directory(&local, None);
        }
        let installed = default_store()?.join(source);
        if installed.is_dir() {
            return directory(&installed, None);
        }
    }
    Err(invalid(format!("App or package '{source}' not found")))
}
pub fn default_store() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DUST_PACKAGE_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home =
        std::env::var_os("HOME").ok_or_else(|| invalid("HOME is required for package storage"))?;
    Ok(PathBuf::from(home).join(".dustagent/packages"))
}
/// Pack root contents deterministically; refuses an existing destination.
pub fn pack(source: &Path, output: Option<&Path>) -> Result<PathBuf> {
    let loaded = directory(source, None)?;
    let meta = loaded.manifest.package.as_ref().unwrap();
    let output = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{}-{}.dustpkg", meta.name, meta.version)));
    let output_parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    if output_parent.starts_with(&loaded.root) {
        return Err(invalid(
            "Package output must be outside its source directory",
        ));
    }
    let files = inventory(&loaded.root)?;
    let mut staging = tempfile::NamedTempFile::new_in(
        output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    {
        let encoder = GzEncoder::new(staging.as_file_mut(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut total = 0u64;
        for path in files {
            let bytes = bounded_read(&loaded.root.join(&path))?;
            total += bytes.len() as u64;
            if total > MAX_TOTAL {
                return Err(invalid("Package contents grew beyond total size limit"));
            }
            let mut header = tar::Header::new_ustar();
            header.set_path(&path)?;
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_mtime(0);
            header.set_cksum();
            archive.append_data(&mut header, path, &bytes[..])?;
        }
        archive.into_inner()?.finish()?.flush()?;
    }
    staging.as_file().sync_all()?;
    staging
        .persist_noclobber(&output)
        .map_err(|e| DustError::Io(e.error))?;
    #[cfg(unix)]
    File::open(&output_parent)?.sync_all()?;
    Ok(output)
}
/// Install without hooks and without replacing any existing package.
pub fn install(source: &Path, store: &Path) -> Result<PathBuf> {
    let loaded = if source.is_dir() {
        directory(source, None)?
    } else {
        extract(source)?
    };
    let name = &loaded.manifest.package.as_ref().unwrap().name;
    fs::create_dir_all(store)?;
    let target = store.join(name);
    let staging = tempfile::tempdir_in(store)?;
    let mut total = 0u64;
    for path in inventory(&loaded.root)? {
        let destination = staging.path().join(&path);
        fs::create_dir_all(destination.parent().unwrap())?;
        let bytes = bounded_read(&loaded.root.join(path))?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL {
            return Err(invalid("Package contents grew beyond total size limit"));
        }
        let mut file = File::options()
            .create_new(true)
            .write(true)
            .open(destination)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    // Serialize publication with an OS lock so cooperating installers cannot overwrite.
    let lease = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.join(".install.lock"))?;
    lease.lock()?;
    if target.exists() {
        return Err(invalid("Package is already installed"));
    }
    fs::rename(staging.path(), &target)?;
    #[cfg(unix)]
    File::open(store)?.sync_all()?;
    Ok(target)
}
