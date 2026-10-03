//! Capacity hints, never exact token counts. Custom gateways must supply app overrides.
use serde_json::Value;
use std::{io::Read, path::Path};

/// Exact known IDs only: a similarly named deployment may have a different limit.
pub(crate) fn official_capacity(model: &str) -> Option<usize> {
    match model {
        "gpt-4o-mini" | "gpt-4o-mini-2024-07-18" | "gpt-4o" => Some(128_000),
        "gpt-6.1-sol" => Some(1_050_000),
        _ => None,
    }
}

pub(crate) fn codex_capacity(model: &str) -> Option<usize> {
    let root = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex")))?;
    read_codex_capacity(&root.join("models_cache.json"), model)
}

fn read_codex_capacity(path: &Path, model: &str) -> Option<usize> {
    // Bound both allocation and reads; this file contains metadata, not credentials.
    const MAX: u64 = 4 * 1024 * 1024;
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX as usize {
        return None;
    }
    let cache: Value = serde_json::from_slice(&bytes).ok()?;
    let entry = cache
        .get("models")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(model))?;
    let window = usize::try_from(entry.get("context_window")?.as_u64()?).ok()?;
    if !(4096..=2_000_000).contains(&window) {
        return None;
    }
    let percent = match entry.get("effective_context_window_percent") {
        None | Some(Value::Null) => 100,
        Some(value) => value.as_u64().filter(|n| (1..=100).contains(n))?,
    };
    let effective = window.saturating_mul(percent as usize) / 100;
    (effective >= 4096).then_some(effective)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_is_exact_and_cache_is_bounded() {
        assert_eq!(official_capacity("gpt-4o-mini"), Some(128_000));
        assert_eq!(official_capacity("local/gpt-4o-mini"), None);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models_cache.json");
        std::fs::write(&path, r#"{"models":[{"slug":"test","context_window":272000,"effective_context_window_percent":95}]}"#).unwrap();
        assert_eq!(read_codex_capacity(&path, "test"), Some(258_400));
        assert_eq!(read_codex_capacity(&path, "other"), None);
        std::fs::write(&path, r#"{"models":[{"slug":"test","context_window":272000,"effective_context_window_percent":101}]}"#).unwrap();
        assert_eq!(read_codex_capacity(&path, "test"), None);
        std::fs::write(&path, vec![b' '; 4 * 1024 * 1024 + 1]).unwrap();
        assert_eq!(read_codex_capacity(&path, "test"), None);
    }
}
