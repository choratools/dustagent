//! File-backed Codex OAuth credentials. Secret values never implement printable Debug.
use crate::error::{DustError, Result};
use serde_json::Value;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

const MAX_CACHE: u64 = 1024 * 1024;
const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
static REFRESH_LOCK: Mutex<()> = Mutex::const_new(());
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Clone)]
pub struct Credentials {
    pub access_token: String,
    pub account_id: String,
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials([redacted])")
    }
}
#[derive(Clone)]
pub struct CodexAuth {
    inner: Arc<Inner>,
}
struct Inner {
    path: PathBuf,
    endpoint: String,
    client: reqwest::Client,
    state: Mutex<State>,
}
struct State {
    credentials: Credentials,
}
fn error(message: &str) -> DustError {
    DustError::Config(message.into())
}
fn cache_path() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home).join("auth.json"));
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".codex/auth.json"))
        .ok_or_else(|| error("Cannot locate Codex credentials; set CODEX_HOME or HOME"))
}
fn read_cache(path: &Path) -> Result<(Vec<u8>, Value)> {
    let mut file = std::fs::File::open(path).map_err(|_| {
        error("Cannot read Codex auth.json; run codex login (keyring-only login is unsupported)")
    })?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_CACHE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error("Cannot read Codex credential cache"))?;
    if bytes.len() as u64 > MAX_CACHE {
        return Err(error("Codex credential cache exceeds 1 MiB"));
    }
    let json =
        serde_json::from_slice(&bytes).map_err(|_| error("Invalid Codex credential cache JSON"))?;
    Ok((bytes, json))
}
fn credentials(json: &Value) -> Result<Credentials> {
    if let Some(mode) = json.get("auth_mode").and_then(Value::as_str)
        && mode != "chatgpt"
    {
        return Err(error("Codex cache is not a ChatGPT OAuth login"));
    }
    let tokens = json
        .get("tokens")
        .ok_or_else(|| error("No ChatGPT credentials in Codex cache; run codex login"))?;
    let get = |name| {
        tokens
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned)
    };
    Ok(Credentials {
        access_token: get("access_token")
            .ok_or_else(|| error("Codex cache lacks an access token"))?,
        account_id: get("account_id").ok_or_else(|| error("Codex cache lacks an account ID"))?,
    })
}
impl CodexAuth {
    pub fn load() -> Result<Self> {
        Self::from_path(cache_path()?, TOKEN_ENDPOINT.into())
    }
    pub fn cached_api_key() -> Result<Option<String>> {
        let (_, json) = read_cache(&cache_path()?)?;
        Ok(json
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned))
    }
    pub(crate) fn from_path(path: PathBuf, endpoint: String) -> Result<Self> {
        let (_, json) = read_cache(&path)?;
        let credentials = credentials(&json)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| error("Cannot initialize Codex authentication client"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                endpoint,
                client,
                state: Mutex::new(State { credentials }),
            }),
        })
    }
    pub async fn credentials(&self) -> Result<Credentials> {
        Ok(self.inner.state.lock().await.credentials.clone())
    }
    /// Refresh runs in an owned task: cancelling an inference cannot interrupt token rotation persistence.
    pub async fn refresh_after_unauthorized(&self, used_access_token: &str) -> Result<Credentials> {
        let inner = self.inner.clone();
        let used = used_access_token.to_owned();
        tokio::spawn(async move { refresh(inner, used).await })
            .await
            .map_err(|_| error("Codex token refresh task failed"))?
    }
}
async fn refresh(inner: Arc<Inner>, used: String) -> Result<Credentials> {
    let _refresh_guard = REFRESH_LOCK.lock().await;
    let mut state = inner.state.lock().await;
    if state.credentials.access_token != used {
        return Ok(state.credentials.clone());
    }
    let (original, mut json) = read_cache(&inner.path)?;
    let current = credentials(&json)?;
    if current.access_token != used {
        state.credentials = current.clone();
        return Ok(current);
    }
    let refresh_token = json["tokens"]["refresh_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| error("Codex cache lacks a refresh token; run codex login"))?;
    let response = inner.client.post(&inner.endpoint).json(&serde_json::json!({"grant_type":"refresh_token", "client_id":CLIENT_ID, "refresh_token":refresh_token})).send().await.map_err(|_| error("Codex token refresh transport failed"))?;
    if !response.status().is_success() {
        return Err(error("Codex token refresh rejected; run codex login"));
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error("Codex token refresh response failed"))?
    {
        if body.len() + chunk.len() > MAX_CACHE as usize {
            return Err(error("Codex token refresh response exceeds limit"));
        }
        body.extend_from_slice(&chunk);
    }
    let updated: Value =
        serde_json::from_slice(&body).map_err(|_| error("Invalid Codex token refresh response"))?;
    let access = updated
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| error("Codex refresh response lacks access token"))?;
    json["tokens"]["access_token"] = Value::String(access.into());
    for field in ["refresh_token", "id_token"] {
        if let Some(value) = updated
            .get(field)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            json["tokens"][field] = Value::String(value.into());
        }
    }
    let renewed = credentials(&json)?;
    persist(&inner.path, &original, &json)?;
    state.credentials = renewed.clone();
    Ok(renewed)
}
fn persist(path: &Path, original: &[u8], json: &Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| error("Invalid Codex credential path"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| error("Cannot create refreshed Codex credential cache"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| error("Cannot restrict Codex credential permissions"))?;
    }
    let bytes = serde_json::to_vec_pretty(json)
        .map_err(|_| error("Cannot encode refreshed Codex credentials"))?;
    temp.write_all(&bytes)
        .and_then(|_| temp.as_file().sync_all())
        .map_err(|_| error("Cannot persist refreshed Codex credentials"))?;
    if read_cache(path)?.0 != original {
        return Err(error(
            "Codex credentials changed during refresh; retry or run codex login",
        ));
    }
    temp.persist(path)
        .map_err(|_| error("Cannot replace Codex credential cache"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, br#"{"auth_mode":"chatgpt","tokens":{"access_token":"secret-a","refresh_token":"secret-r","account_id":"account","id_token":"id"},"metadata":{"keep":true}}"#).unwrap();
        (dir, path)
    }
    #[tokio::test]
    async fn loads_and_redacts() {
        let (_dir, path) = fixture();
        let auth = CodexAuth::from_path(path, TOKEN_ENDPOINT.into()).unwrap();
        let c = auth.credentials().await.unwrap();
        assert_eq!(c.access_token, "secret-a");
        assert!(!format!("{c:?}").contains("secret-a"));
    }
    #[test]
    fn bounds_and_invalid_json_do_not_echo_secrets() {
        let (_dir, path) = fixture();
        std::fs::write(&path, b"secret-invalid-json").unwrap();
        assert!(
            !read_cache(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("secret-invalid")
        );
        std::fs::write(&path, vec![b'x'; MAX_CACHE as usize + 1]).unwrap();
        assert!(read_cache(&path).is_err());
    }
    #[test]
    fn preserves_metadata_and_rejects_concurrent_replacement() {
        let (_dir, path) = fixture();
        let (bytes, mut json) = read_cache(&path).unwrap();
        json["tokens"]["access_token"] = Value::String("new".into());
        persist(&path, &bytes, &json).unwrap();
        let (newbytes, newjson) = read_cache(&path).unwrap();
        assert_eq!(newjson["metadata"]["keep"], true);
        assert!(persist(&path, &bytes, &json).is_err());
        assert_eq!(read_cache(&path).unwrap().0, newbytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[tokio::test]
    async fn concurrent_refresh_rotates_once_and_survives_caller_cancellation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_dir, path) = fixture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let auth = CodexAuth::from_path(path.clone(), endpoint.clone()).unwrap();
        let second = CodexAuth::from_path(path.clone(), endpoint).unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).contains("refresh_token"));
            seen_tx.send(()).unwrap();
            tokio::time::sleep(Duration::from_millis(40)).await;
            let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","id_token":"new-id"}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let task = tokio::spawn(async move { auth.refresh_after_unauthorized("secret-a").await });
        seen_rx.await.unwrap();
        task.abort();
        let c = second.refresh_after_unauthorized("secret-a").await.unwrap();
        assert_eq!(c.access_token, "new-access");
        let (_, json) = read_cache(&path).unwrap();
        assert_eq!(json["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(json["tokens"]["id_token"], "new-id");
        server.await.unwrap();
    }
}
