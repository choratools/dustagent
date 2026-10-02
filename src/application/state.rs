//! Bounded, execution-local JSON working state. This module performs no filesystem I/O.
use crate::{DustError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_KEY_CHARS: usize = 256;
const MAX_VALUE_BYTES: usize = 16 * 1024;
const MAX_ENTRIES: usize = 256;
const MAX_TOTAL_BYTES: usize = 256 * 1024;
const PAGE_SIZE: usize = 20;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkingState {
    pub revision: u64,
    pub entries: BTreeMap<String, Value>,
}

impl WorkingState {
    pub fn get(&self, key: &str) -> Result<Value> {
        validate_key(key, false)?;
        Ok(match self.entries.get(key) {
            Some(value) => json!({"found":true,"value":value,"revision":self.revision}),
            None => json!({"found":false,"value":null,"revision":self.revision}),
        })
    }

    pub fn put(&mut self, key: String, value: Value) -> Result<Value> {
        validate_key(&key, false)?;
        validate_value(&value)?;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| config("working state revision overflow"))?;
        let mut candidate = self.clone();
        candidate.revision = revision;
        candidate.entries.insert(key, value);
        candidate.validate()?;
        *self = candidate;
        Ok(json!({"revision":revision}))
    }

    pub fn list(&self, prefix: &str, cursor: Option<&str>) -> Result<Value> {
        validate_key(prefix, true)?;
        if let Some(cursor) = cursor {
            validate_key(cursor, false)?;
        }
        let mut matching = self.entries.keys().filter(|key| {
            key.starts_with(prefix) && cursor.is_none_or(|cursor| key.as_str() > cursor)
        });
        let keys: Vec<&String> = matching.by_ref().take(PAGE_SIZE).collect();
        let next_cursor = if matching.next().is_some() {
            keys.last().copied()
        } else {
            None
        };
        Ok(json!({"keys":keys,"revision":self.revision,"next_cursor":next_cursor}))
    }

    pub fn validate(&self) -> Result<()> {
        if self.entries.len() > MAX_ENTRIES {
            return Err(config("working state exceeds 256 entries"));
        }
        for (key, value) in &self.entries {
            validate_key(key, false)?;
            validate_value(value)?;
        }
        if serde_json::to_vec(self)?.len() > MAX_TOTAL_BYTES {
            return Err(config("working state exceeds 256 KiB"));
        }
        Ok(())
    }
}

fn config(message: &str) -> DustError {
    DustError::Config(message.into())
}
fn validate_key(key: &str, allow_empty: bool) -> Result<()> {
    if (!allow_empty && key.is_empty()) || key.chars().count() > MAX_KEY_CHARS || key.contains('\0')
    {
        return Err(config(
            "state key/prefix must contain at most 256 characters, no NUL, and keys must be nonempty",
        ));
    }
    Ok(())
}
fn validate_value(value: &Value) -> Result<()> {
    if serde_json::to_vec(value)?.len() > MAX_VALUE_BYTES {
        return Err(config("state value exceeds 16 KiB"));
    }
    Ok(())
}
