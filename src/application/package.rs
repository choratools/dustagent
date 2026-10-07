//! Local AaaA packages: tar.gz containers, never executable installers.
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zeroize::{Zeroize, Zeroizing};

use crate::domain::manifest::{AppManifest, resolve_manifest_path};
use crate::error::{DustError, Result};

const MAX_ENTRIES: usize = 4096;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE: u64 = MAX_TOTAL + 8 * 1024 * 1024;
const MAX_ENCRYPTED_ARCHIVE: u64 = MAX_ARCHIVE + 512;
const ENCRYPTED_MAGIC: &[u8; 8] = b"DUSTENC1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MIN_PASSPHRASE_BYTES: usize = 12;
const MAX_PASSPHRASE_BYTES: usize = 1024;
const ARGON2_MEMORY_KIB: u32 = 19 * 1024;
const ARGON2_ITERATIONS: u32 = 2;

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
struct EncryptedHeader {
    name: String,
    version: String,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
    payload_offset: usize,
}

fn validate_passphrase(passphrase: &[u8]) -> Result<()> {
    if passphrase.len() < MIN_PASSPHRASE_BYTES || passphrase.len() > MAX_PASSPHRASE_BYTES {
        return Err(invalid(format!(
            "Package passphrase must be between {MIN_PASSPHRASE_BYTES} and {MAX_PASSPHRASE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn derive_key(passphrase: &[u8], salt: &[u8; SALT_LEN]) -> Result<Zeroizing<[u8; 32]>> {
    validate_passphrase(passphrase)?;
    let params = Params::new(ARGON2_MEMORY_KIB, ARGON2_ITERATIONS, 1, Some(32))
        .map_err(|_| invalid("Could not configure package key derivation"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(passphrase, salt, &mut key[..])
        .map_err(|_| invalid("Could not derive package encryption key"))?;
    Ok(key)
}

fn encrypted_header(bytes: &[u8]) -> Result<Option<EncryptedHeader>> {
    if !bytes.starts_with(ENCRYPTED_MAGIC) {
        return Ok(None);
    }
    if bytes.len() as u64 > MAX_ENCRYPTED_ARCHIVE || bytes.len() < 12 {
        return Err(invalid("Encrypted package header or size is invalid"));
    }
    let name_len = u16::from_be_bytes([bytes[8], bytes[9]]) as usize;
    let version_len = u16::from_be_bytes([bytes[10], bytes[11]]) as usize;
    if name_len == 0 || name_len > 128 || version_len == 0 || version_len > 62 {
        return Err(invalid("Encrypted package metadata is invalid"));
    }
    let name_start = 12;
    let version_start = name_start + name_len;
    let salt_start = version_start + version_len;
    let nonce_start = salt_start + SALT_LEN;
    let payload_offset = nonce_start + NONCE_LEN;
    if bytes.len() < payload_offset + TAG_LEN {
        return Err(invalid("Encrypted package is truncated"));
    }
    let name = std::str::from_utf8(&bytes[name_start..version_start])
        .map_err(|_| invalid("Encrypted package name is not UTF-8"))?
        .to_owned();
    if !safe_component(&name) {
        return Err(invalid("Encrypted package name is unsafe"));
    }
    let version = std::str::from_utf8(&bytes[version_start..salt_start])
        .map_err(|_| invalid("Encrypted package version is not UTF-8"))?
        .to_owned();
    semver(&version)?;
    let salt = bytes[salt_start..nonce_start]
        .try_into()
        .map_err(|_| invalid("Encrypted package salt is invalid"))?;
    let nonce = bytes[nonce_start..payload_offset]
        .try_into()
        .map_err(|_| invalid("Encrypted package nonce is invalid"))?;
    Ok(Some(EncryptedHeader {
        name,
        version,
        salt,
        nonce,
        payload_offset,
    }))
}

fn encrypt_archive(
    archive: &[u8],
    name: &str,
    version: &str,
    passphrase: &[u8],
) -> Result<Vec<u8>> {
    validate_passphrase(passphrase)?;
    if archive.len() as u64 > MAX_ARCHIVE {
        return Err(invalid("Compressed package exceeds archive size limit"));
    }
    if !safe_component(name) || version.len() > 62 {
        return Err(invalid("Package encryption metadata is invalid"));
    }
    semver(version)?;
    let name_bytes = name.as_bytes();
    let version_bytes = version.as_bytes();
    let mut salt = *uuid::Uuid::new_v4().as_bytes();
    let nonce_uuid = uuid::Uuid::new_v4();
    let nonce: [u8; NONCE_LEN] = nonce_uuid.as_bytes()[..NONCE_LEN]
        .try_into()
        .expect("fixed nonce length");
    let mut header =
        Vec::with_capacity(12 + name_bytes.len() + version_bytes.len() + SALT_LEN + NONCE_LEN);
    header.extend_from_slice(ENCRYPTED_MAGIC);
    header.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
    header.extend_from_slice(&(version_bytes.len() as u16).to_be_bytes());
    header.extend_from_slice(name_bytes);
    header.extend_from_slice(version_bytes);
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce);

    let key = derive_key(passphrase, &salt)?;
    salt.zeroize();
    let cipher = Aes256Gcm::new_from_slice(&key[..])
        .map_err(|_| invalid("Could not initialize package encryption"))?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: archive,
                aad: &header,
            },
        )
        .map_err(|_| invalid("Package encryption failed"))?;
    if header.len().saturating_add(ciphertext.len()) as u64 > MAX_ENCRYPTED_ARCHIVE {
        return Err(invalid("Encrypted package exceeds archive size limit"));
    }
    header.extend_from_slice(&ciphertext);
    Ok(header)
}

fn decrypt_archive(
    bytes: &[u8],
    header: &EncryptedHeader,
    passphrase: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    let key = derive_key(passphrase, &header.salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key[..])
        .map_err(|_| invalid("Could not initialize package decryption"))?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&header.nonce),
                Payload {
                    msg: &bytes[header.payload_offset..],
                    aad: &bytes[..header.payload_offset],
                },
            )
            .map_err(|_| invalid("Wrong passphrase or encrypted package authentication failed"))?,
    );
    if plaintext.len() as u64 > MAX_ARCHIVE {
        return Err(invalid("Decrypted package exceeds archive size limit"));
    }
    Ok(plaintext)
}

fn read_package_bytes(source: &Path) -> Result<Vec<u8>> {
    let file = File::open(source)?;
    if file.metadata()?.len() > MAX_ENCRYPTED_ARCHIVE {
        return Err(invalid("Package exceeds compressed archive size limit"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_ENCRYPTED_ARCHIVE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ENCRYPTED_ARCHIVE
        || (!bytes.starts_with(ENCRYPTED_MAGIC) && bytes.len() as u64 > MAX_ARCHIVE)
    {
        return Err(invalid("Package exceeds compressed archive size limit"));
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
fn extract_archive(source: impl Read) -> Result<LoadedApp> {
    let staging = tempfile::tempdir()?;
    let mut archive = tar::Archive::new(GzDecoder::new(source));
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

fn extract_bytes(bytes: &[u8], passphrase: Option<&[u8]>) -> Result<LoadedApp> {
    let Some(header) = encrypted_header(bytes)? else {
        return extract_archive(Cursor::new(bytes));
    };
    let passphrase = passphrase.ok_or(crate::DustError::PackagePassphraseRequired)?;
    let plaintext = decrypt_archive(bytes, &header, passphrase)?;
    let loaded = extract_archive(Cursor::new(&plaintext[..]))?;
    let metadata = loaded
        .manifest
        .package
        .as_ref()
        .ok_or_else(|| invalid("Encrypted package manifest has no package metadata"))?;
    if metadata.name != header.name || metadata.version != header.version {
        return Err(invalid(
            "Encrypted package header does not match its manifest",
        ));
    }
    Ok(loaded)
}

fn extract(source: &Path, passphrase: Option<&[u8]>) -> Result<LoadedApp> {
    let bytes = read_package_bytes(source)?;
    extract_bytes(&bytes, passphrase)
}
/// Load a directory, tar.gz .dustpkg, legacy JSON, or installed package name.
pub fn load(source: &str, base: &Path) -> Result<LoadedApp> {
    load_with_optional_password(source, base, None)
}

/// Load an encrypted .dustpkg with a passphrase; plaintext package sources are also supported.
pub fn load_with_password(source: &str, base: &Path, passphrase: &[u8]) -> Result<LoadedApp> {
    load_with_optional_password(source, base, Some(passphrase))
}

fn load_with_optional_password(
    source: &str,
    base: &Path,
    passphrase: Option<&[u8]>,
) -> Result<LoadedApp> {
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
        return extract(&path, passphrase);
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
        let installed_archive = default_store()?.join(format!("{source}.dustpkg"));
        if installed_archive.is_file() {
            return extract(&installed_archive, passphrase);
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
fn build_archive(root: &Path) -> Result<Vec<u8>> {
    let files = inventory(root)?;
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let mut total = 0u64;
    for path in files {
        let bytes = bounded_read(&root.join(&path))?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("Package size overflow"))?;
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
    let bytes = archive.into_inner()?.finish()?;
    if bytes.len() as u64 > MAX_ARCHIVE {
        return Err(invalid("Compressed package exceeds archive size limit"));
    }
    Ok(bytes)
}

fn output_path(
    root: &Path,
    name: &str,
    version: &str,
    output: Option<&Path>,
) -> Result<(PathBuf, PathBuf)> {
    let output = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{name}-{version}.dustpkg")));
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let canonical_parent = parent.canonicalize()?;
    if canonical_parent.starts_with(root) {
        return Err(invalid(
            "Package output must be outside its source directory",
        ));
    }
    Ok((output, canonical_parent))
}

fn publish_archive(
    root: &Path,
    name: &str,
    version: &str,
    output: Option<&Path>,
    bytes: &[u8],
) -> Result<PathBuf> {
    let (output, output_parent) = output_path(root, name, version, output)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    staging.write_all(bytes)?;
    staging.as_file().sync_all()?;
    staging
        .persist_noclobber(&output)
        .map_err(|e| DustError::Io(e.error))?;
    #[cfg(unix)]
    File::open(&output_parent)?.sync_all()?;
    Ok(output)
}

/// Pack root contents deterministically; refuses an existing destination.
pub fn pack(source: &Path, output: Option<&Path>) -> Result<PathBuf> {
    let loaded = directory(source, None)?;
    let meta = loaded.manifest.package.as_ref().unwrap();
    let archive = Zeroizing::new(build_archive(&loaded.root)?);
    publish_archive(&loaded.root, &meta.name, &meta.version, output, &archive)
}

/// Pack and encrypt the complete package archive with a passphrase.
pub fn pack_encrypted(source: &Path, output: Option<&Path>, passphrase: &[u8]) -> Result<PathBuf> {
    let loaded = directory(source, None)?;
    let meta = loaded.manifest.package.as_ref().unwrap();
    let archive = Zeroizing::new(build_archive(&loaded.root)?);
    let encrypted = encrypt_archive(&archive, &meta.name, &meta.version, passphrase)?;
    publish_archive(&loaded.root, &meta.name, &meta.version, output, &encrypted)
}
/// Install without hooks and without replacing any existing package.
pub fn install(source: &Path, store: &Path) -> Result<PathBuf> {
    if source.is_file() {
        let bytes = read_package_bytes(source)?;
        if let Some(header) = encrypted_header(&bytes)? {
            return install_encrypted(&bytes, &header, store);
        }
    }
    let loaded = if source.is_dir() {
        directory(source, None)?
    } else {
        extract(source, None)?
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
    if target.exists() || store.join(format!("{name}.dustpkg")).exists() {
        return Err(invalid("Package is already installed"));
    }
    fs::rename(staging.path(), &target)?;
    #[cfg(unix)]
    File::open(store)?.sync_all()?;
    Ok(target)
}

fn install_encrypted(bytes: &[u8], header: &EncryptedHeader, store: &Path) -> Result<PathBuf> {
    fs::create_dir_all(store)?;
    let target = store.join(format!("{}.dustpkg", header.name));
    let mut staging = tempfile::NamedTempFile::new_in(store)?;
    staging.write_all(bytes)?;
    staging.as_file().sync_all()?;
    let lease = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.join(".install.lock"))?;
    lease.lock()?;
    if store.join(&header.name).exists() || target.exists() {
        return Err(invalid("Package is already installed"));
    }
    staging
        .persist_noclobber(&target)
        .map_err(|e| DustError::Io(e.error))?;
    #[cfg(unix)]
    File::open(store)?.sync_all()?;
    Ok(target)
}
