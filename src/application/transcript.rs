//! Append-only originals, separate from the model's compacted conversation.
use crate::{DustError, Result, ports::llm::ChatMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

const MAX_RECORD: usize = 16 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const PAGE: usize = 16 * 1024;
fn invalid() -> DustError {
    DustError::Config("Transcript archive is invalid or exceeds its limits".into())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptRef {
    pub path: PathBuf,
    pub count: u64,
    pub hash: String,
    pub binding: String,
}
#[derive(Debug)]
pub struct TranscriptStore {
    reference: TranscriptRef,
    bytes: u64,
    entries: Vec<DirectoryEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryKeyword {
    pub term: String,
    pub index: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryHighlight {
    pub index: u64,
    pub role: String,
    pub preview: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub id: u64,
    pub from: u64,
    pub through: u64,
    /// Model summary is navigation advice, never verified evidence.
    pub description: String,
    pub keywords: Vec<DirectoryKeyword>,
    pub highlights: Vec<DirectoryHighlight>,
}
#[derive(Serialize, Deserialize)]
struct Record {
    index: u64,
    message: ChatMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory: Option<DirectoryEntry>,
}

fn root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("DUST_HISTORY_HOME") {
        return Ok(PathBuf::from(p));
    }
    std::env::var_os("HOME")
        .map(|p| PathBuf::from(p).join(".dustagent/history"))
        .ok_or_else(|| {
            DustError::Config("Set HOME or DUST_HISTORY_HOME to preserve transcripts".into())
        })
}
fn hash(previous: &str, line: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(previous.as_bytes());
    digest.update(line);
    format!("{:x}", digest.finalize())
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
impl TranscriptStore {
    pub fn create(binding: &str) -> Result<Self> {
        Self::create_in(&root()?, binding)
    }
    pub fn create_in(root: &Path, binding: &str) -> Result<Self> {
        private_dir(root)?;
        let directory = root.canonicalize()?.join(uuid::Uuid::new_v4().to_string());
        private_dir(&directory)?;
        let path = directory.join("transcript.jsonl");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?.sync_all()?;
        Ok(Self {
            reference: TranscriptRef {
                path,
                count: 0,
                hash: hash(binding, &[]),
                binding: binding.into(),
            },
            bytes: 0,
            entries: Vec::new(),
        })
    }
    pub fn open(reference: &TranscriptRef, binding: &str) -> Result<Self> {
        Self::open_in(reference, binding, &root()?)
    }
    pub fn open_in(reference: &TranscriptRef, binding: &str, root: &Path) -> Result<Self> {
        if reference.binding != binding {
            return Err(invalid());
        }
        let path = &reference.path;
        if path.file_name().and_then(|s| s.to_str()) != Some("transcript.jsonl") {
            return Err(invalid());
        }
        let parent = path.parent().ok_or_else(invalid)?;
        if fs::symlink_metadata(parent)?.file_type().is_symlink()
            || fs::symlink_metadata(path)?.file_type().is_symlink()
            || parent.parent().ok_or_else(invalid)?.canonicalize()? != root.canonicalize()?
        {
            return Err(invalid());
        }
        if !fs::metadata(path)?.is_file() {
            return Err(invalid());
        }
        let bytes = fs::metadata(path)?.len();
        if bytes > MAX_ARCHIVE {
            return Err(invalid());
        }
        let mut count = 0;
        let mut entries: Vec<DirectoryEntry> = Vec::new();
        let mut digest = hash(binding, &[]);
        visit(path, |line, record| {
            count += 1;
            if record.index != count {
                return Err(invalid());
            }
            if let Some(entry) = record.directory {
                let from = entries.last().map_or(1, |previous| previous.through + 1);
                if entry.id != entries.len() as u64 + 1
                    || entry.from != from
                    || entry.through >= record.index
                    || entry.from > entry.through
                    || entry.description.chars().count() > 512
                    || entry.keywords.len() > 12
                    || entry.highlights.len() > 10
                    || entry.keywords.iter().any(|k| {
                        k.index < entry.from
                            || k.index > entry.through
                            || k.term.is_empty()
                            || k.term.len() > 160
                    })
                    || entry.highlights.iter().any(|h| {
                        h.index < entry.from
                            || h.index > entry.through
                            || h.preview.chars().count() > 160
                            || h.role.chars().count() > 32
                    })
                {
                    return Err(invalid());
                }
                entries.push(entry);
            }
            digest = hash(&digest, line);
            Ok(true)
        })?;
        if count != reference.count || digest != reference.hash {
            return Err(invalid());
        }
        verify_anchors(path, &entries)?;
        Ok(Self {
            reference: reference.clone(),
            bytes,
            entries,
        })
    }
    /// Copy immutable checkpoint history before adding a continuation request.
    /// A failed checkpoint save must not invalidate the previous archive reference.
    pub fn fork(&self) -> Result<Self> {
        let root = self
            .reference
            .path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(invalid)?;
        Self::open_in(&self.reference, &self.reference.binding, root)?;
        let mut fork = Self::create_in(root, &self.reference.binding)?;
        let source = File::open(&self.reference.path)?;
        let mut target = OpenOptions::new().write(true).open(&fork.reference.path)?;
        let bytes = std::io::copy(&mut source.take(MAX_ARCHIVE + 1), &mut target)?;
        if bytes > MAX_ARCHIVE || bytes != self.bytes {
            return Err(invalid());
        }
        target.sync_all()?;
        fork.reference.count = self.reference.count;
        fork.reference.hash = self.reference.hash.clone();
        Self::open_in(&fork.reference, &self.reference.binding, root)
    }
    pub fn count(&self) -> u64 {
        self.reference.count
    }
    pub fn reference(&self) -> TranscriptRef {
        self.reference.clone()
    }
    pub fn append(&mut self, message: &ChatMessage) -> Result<u64> {
        self.append_record(message, None)
    }
    fn append_record(
        &mut self,
        message: &ChatMessage,
        directory: Option<DirectoryEntry>,
    ) -> Result<u64> {
        let index = self.reference.count.checked_add(1).ok_or_else(invalid)?;
        let mut line = serde_json::to_vec(&Record {
            index,
            message: message.clone(),
            directory,
        })?;
        line.push(b'\n');
        if line.len() > MAX_RECORD || self.bytes + line.len() as u64 > MAX_ARCHIVE {
            return Err(invalid());
        }
        let metadata = fs::symlink_metadata(&self.reference.path)?;
        if metadata.file_type().is_symlink() || metadata.len() != self.bytes {
            return Err(invalid());
        }
        let mut file = OpenOptions::new().append(true).open(&self.reference.path)?;
        file.write_all(&line)?;
        file.sync_data()?;
        self.reference.hash = hash(&self.reference.hash, &line);
        self.reference.count = index;
        self.bytes += line.len() as u64;
        Ok(index)
    }
    /// Index only newly archived originals. Entries themselves share the archive hash chain.
    pub fn index_compaction(&mut self, summary: &str) -> Result<DirectoryEntry> {
        let entry = self.prepare_compaction(summary)?;
        self.append_directory(entry.clone())?;
        Ok(entry)
    }
    pub fn prepare_compaction(&self, summary: &str) -> Result<DirectoryEntry> {
        let from = self.entries.last().map_or(1, |entry| entry.through + 1);
        let through = self.count();
        if from > through {
            return Err(invalid());
        }
        let eligible =
            |record: &Record| {
                record.index >= from
                    && record.index <= through
                    && record.directory.is_none()
                    && !(record.message.role == "system"
                        && record.message.content.as_deref().is_some_and(|text| {
                            text.starts_with("[dustagent compaction response:")
                        }))
            };
        let mut originals = 0usize;
        visit(&self.reference.path, |_, record| {
            if eligible(&record) {
                originals += 1;
            }
            Ok(true)
        })?;
        let n = originals.min(10);
        let positions: Vec<usize> = (0..n)
            .map(|i| {
                if n <= 1 {
                    0
                } else {
                    i * (originals - 1) / (n - 1)
                }
            })
            .collect();
        let mut seen = 0usize;
        let mut anchors = Vec::new();
        let mut highlights = Vec::new();
        visit(&self.reference.path, |_, record| {
            if eligible(&record) {
                if positions.contains(&seen) {
                    let text = serde_json::to_string(&record.message)?;
                    let mut terms = Vec::new();
                    if let Some(calls) = &record.message.tool_calls {
                        for call in calls {
                            add_terms(&mut terms, &call.function_name);
                            add_terms(&mut terms, &call.arguments);
                        }
                    }
                    add_terms(&mut terms, record.message.content.as_deref().unwrap_or(""));
                    anchors.push((record.index, terms));
                    highlights.push(DirectoryHighlight {
                        index: record.index,
                        role: record.message.role.chars().take(32).collect(),
                        preview: text.chars().take(160).collect(),
                    });
                }
                seen += 1;
            }
            Ok(true)
        })?;
        let mut keywords: Vec<DirectoryKeyword> = Vec::new();
        for round in 0..6 {
            for (index, terms) in &anchors {
                if let Some(term) = terms.get(round)
                    && keywords.len() < 12
                    && !keywords.iter().any(|k| &k.term == term)
                {
                    keywords.push(DirectoryKeyword {
                        term: term.clone(),
                        index: *index,
                    });
                }
            }
        }
        let entry = DirectoryEntry {
            id: self.entries.len() as u64 + 1,
            from,
            through,
            description: summary.chars().take(512).collect(),
            keywords,
            highlights,
        };
        Ok(entry)
    }
    pub fn append_directory(&mut self, entry: DirectoryEntry) -> Result<()> {
        if entry.id != self.entries.len() as u64 + 1
            || entry.from
                != self
                    .entries
                    .last()
                    .map_or(1, |previous| previous.through + 1)
            || entry.through != self.count()
            || entry.from > entry.through
            || entry.description.chars().count() > 512
            || entry.keywords.len() > 12
            || entry.highlights.len() > 10
            || entry
                .keywords
                .iter()
                .any(|k| k.term.is_empty() || k.term.len() > 160)
            || entry
                .highlights
                .iter()
                .any(|h| h.preview.chars().count() > 160 || h.role.chars().count() > 32)
        {
            return Err(invalid());
        }
        verify_anchors(&self.reference.path, std::slice::from_ref(&entry))?;
        self.append_record(
            &ChatMessage::system(
                "[dustagent original directory: navigation metadata; description is unverified]",
            ),
            Some(entry.clone()),
        )?;
        self.entries.push(entry);
        Ok(())
    }
    /// Cursor is the next directory entry position, independent of original record indexes.
    pub fn directory(&self, cursor: usize, limit: usize, query: Option<&str>) -> Result<Value> {
        if cursor > self.entries.len()
            || limit == 0
            || limit > 10
            || query.is_some_and(|q| q.is_empty() || q.chars().count() > 256)
        {
            return Err(invalid());
        }
        let mut results = Vec::new();
        let mut next = cursor;
        while next < self.entries.len() {
            let entry = &self.entries[next];
            let mut matched_by = Vec::new();
            if let Some(q) = query {
                if entry.description.contains(q) {
                    matched_by.push("description");
                }
                if entry.keywords.iter().any(|k| k.term.contains(q)) {
                    matched_by.push("keyword");
                }
                if entry.highlights.iter().any(|h| h.preview.contains(q)) {
                    matched_by.push("preview");
                }
            }
            if query.is_none() || !matched_by.is_empty() {
                let mut item = serde_json::to_value(entry)?;
                item["matched_by"] = json!(matched_by);
                let mut candidate = results.clone();
                candidate.push(item.clone());
                if serde_json::to_vec(
                    &json!({"entries":candidate,"next_cursor":next+1,"count":self.entries.len()}),
                )?
                .len()
                    > PAGE
                {
                    break;
                }
                results.push(item);
            }
            next += 1;
            if results.len() == limit {
                break;
            }
        }
        Ok(
            json!({"entries":results,"next_cursor":if next < self.entries.len(){Some(next)}else{None},"count":self.entries.len()}),
        )
    }
    pub fn directory_hint(&self, max_bytes: usize) -> String {
        let Some(entry) = self.entries.last() else {
            return String::new();
        };
        self.directory_hint_for(entry, max_bytes)
    }
    pub fn directory_hint_for(&self, entry: &DirectoryEntry, max_bytes: usize) -> String {
        let hint = format!(
            "Use dustagent__history_directory; {} older entries. Latest #{} records {}–{}; keywords: {}; unverified: {}",
            entry.id.saturating_sub(1),
            entry.id,
            entry.from,
            entry.through,
            entry
                .keywords
                .iter()
                .take(4)
                .map(|k| format!("{}@{}", clip_bytes(&k.term, 24), k.index))
                .collect::<Vec<_>>()
                .join(", "),
            entry.description.chars().take(120).collect::<String>()
        );
        let mut end = max_bytes.min(hint.len());
        while !hint.is_char_boundary(end) {
            end -= 1;
        }
        hint[..end].to_string()
    }
    /// UTF-8 byte offsets refer to the original message serialized as JSON.
    pub fn read(&self, index: u64, offset: usize, limit: usize) -> Result<Value> {
        if index == 0 || index > self.count() || limit == 0 || limit > PAGE {
            return Err(invalid());
        }
        let mut output = None;
        visit(&self.reference.path, |_, record| {
            if record.index != index {
                return Ok(true);
            }
            let text = serde_json::to_string(&record.message)?;
            if offset > text.len() || !text.is_char_boundary(offset) {
                return Err(invalid());
            }
            let mut end = (offset + limit).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == offset && offset < text.len() {
                return Err(invalid());
            }
            output = Some(
                json!({"index":index,"offset":offset,"text":&text[offset..end],"next_offset": if end < text.len() {Some(end)} else {None},"total_bytes":text.len()}),
            );
            Ok(false)
        })?;
        output.ok_or_else(invalid)
    }
    /// Literal, case-sensitive search; cursor is the last examined record index.
    pub fn search(&self, query: &str, cursor: u64, limit: usize) -> Result<Value> {
        if query.is_empty()
            || query.chars().count() > 256
            || limit == 0
            || limit > 20
            || cursor > self.count()
        {
            return Err(invalid());
        }
        let encoded_query = serde_json::to_string(query)?;
        let needle = &encoded_query[1..encoded_query.len() - 1];
        let mut results = Vec::new();
        let mut scanned = cursor;
        let mut examined = 0usize;
        let mut examined_bytes = 0usize;
        visit(&self.reference.path, |line, record| {
            if record.index <= cursor {
                return Ok(true);
            }
            scanned = record.index;
            examined += 1;
            examined_bytes += line.len();
            let text = serde_json::to_string(&record.message)?;
            if let Some(position) = text.find(needle) {
                let start = text[..position]
                    .char_indices()
                    .rev()
                    .nth(80)
                    .map_or(0, |(i, _)| i);
                let snippet: String = text[start..].chars().take(512).collect();
                results.push(
                    json!({"index":record.index,"role":record.message.role,"snippet":snippet}),
                );
            }
            Ok(results.len() < limit && examined < 1000 && examined_bytes < 4 * 1024 * 1024)
        })?;
        Ok(
            json!({"results":results,"next_cursor":if scanned < self.count() {Some(scanned)} else {None},"count":self.count()}),
        )
    }
}
fn verify_anchors(path: &Path, entries: &[DirectoryEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut current = 0;
    let mut found = 0usize;
    visit(path, |_, record| {
        while current < entries.len() && entries[current].through < record.index {
            current += 1;
        }
        if current == entries.len() {
            return Ok(false);
        }
        let entry = &entries[current];
        if record.index < entry.from {
            return Ok(true);
        }
        let keywords: Vec<_> = entry
            .keywords
            .iter()
            .filter(|k| k.index == record.index)
            .collect();
        let highlights: Vec<_> = entry
            .highlights
            .iter()
            .filter(|h| h.index == record.index)
            .collect();
        if !keywords.is_empty() || !highlights.is_empty() {
            if record.directory.is_some()
                || (record.message.role == "system"
                    && record
                        .message
                        .content
                        .as_deref()
                        .is_some_and(|s| s.starts_with("[dustagent compaction response:")))
            {
                return Err(invalid());
            }
            for keyword in keywords {
                let exists = record
                    .message
                    .content
                    .as_deref()
                    .is_some_and(|s| s.contains(&keyword.term))
                    || record.message.tool_calls.as_ref().is_some_and(|calls| {
                        calls.iter().any(|call| {
                            call.function_name.contains(&keyword.term)
                                || call.arguments.contains(&keyword.term)
                        })
                    });
                if !exists {
                    return Err(invalid());
                }
                found += 1;
            }
            let text = serde_json::to_string(&record.message)?;
            for highlight in highlights {
                if highlight.role != record.message.role.chars().take(32).collect::<String>()
                    || highlight.preview != text.chars().take(160).collect::<String>()
                {
                    return Err(invalid());
                }
                found += 1;
            }
        }
        Ok(true)
    })?;
    if found
        != entries
            .iter()
            .map(|e| e.keywords.len() + e.highlights.len())
            .sum::<usize>()
    {
        return Err(invalid());
    }
    Ok(())
}
fn clip_bytes(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn add_terms(terms: &mut Vec<String>, text: &str) {
    for term in text.split(|c: char| !(c.is_alphanumeric() || "_./:-".contains(c))) {
        let term = term.trim_matches(['.', '/', ':', '-']);
        if term.chars().count() < 3 || term.len() > 160 || terms.iter().any(|t| t == term) {
            continue;
        }
        terms.push(term.to_string());
        terms.sort_by_key(|t| std::cmp::Reverse(term_score(t)));
        terms.truncate(6);
    }
}
fn term_score(term: &str) -> usize {
    if term.contains('/') || term.contains('.') || term.contains("__") {
        3
    } else if term.contains('_') || term.chars().any(char::is_uppercase) {
        2
    } else {
        1
    }
}
fn visit(path: &Path, mut callback: impl FnMut(&[u8], Record) -> Result<bool>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_ARCHIVE {
        return Err(invalid());
    }
    let mut reader = BufReader::new(File::open(path)?);
    loop {
        let mut line = Vec::new();
        // `take` bounds allocation even when an attacker replaces a file with a giant line.
        let n =
            std::io::Read::take(&mut reader, MAX_RECORD as u64 + 1).read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if n > MAX_RECORD || line.last() != Some(&b'\n') {
            return Err(invalid());
        }
        let record: Record = serde_json::from_slice(&line).map_err(|_| invalid())?;
        if !callback(&line, record)? {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparing_directory_does_not_commit_and_reopen_checks_real_anchors() {
        let root = tempfile::tempdir().unwrap();
        let mut store = TranscriptStore::create_in(root.path(), "app").unwrap();
        store
            .append(&ChatMessage::user("real_path.rs original"))
            .unwrap();
        let before = store.reference();
        let prepared = store
            .prepare_compaction("model-only invented_topic")
            .unwrap();
        assert_eq!(store.reference(), before);
        assert_eq!(store.directory(0, 10, None).unwrap()["count"], 0);
        assert!(
            store
                .directory_hint_for(&prepared, 256)
                .contains("real_path.rs")
        );
        store.append_directory(prepared).unwrap();
        let match_page = store.directory(0, 10, Some("invented_topic")).unwrap();
        assert_eq!(
            match_page["entries"][0]["matched_by"],
            json!(["description"])
        );
        let mut lines = Vec::new();
        visit(&store.reference.path, |_, mut record| {
            if let Some(entry) = &mut record.directory {
                entry.keywords[0].term = "invented_keyword".into();
            }
            let mut line = serde_json::to_vec(&record)?;
            line.push(b'\n');
            lines.push(line);
            Ok(true)
        })
        .unwrap();
        let mut reference = store.reference();
        reference.hash = hash("app", &[]);
        let mut bytes = Vec::new();
        for line in lines {
            reference.hash = hash(&reference.hash, &line);
            bytes.extend(line);
        }
        fs::write(&reference.path, bytes).unwrap();
        assert!(TranscriptStore::open_in(&reference, "app", root.path()).is_err());
    }
    #[test]
    fn directory_roundtrip_anchors_and_bounded_navigation() {
        let root = tempfile::tempdir().unwrap();
        let mut store = TranscriptStore::create_in(root.path(), "app").unwrap();
        for i in 0..25 {
            store
                .append(&ChatMessage::user(format!(
                    "조사{i} src/module_{i}.rs 확인"
                )))
                .unwrap();
        }
        store
            .append(&ChatMessage::system(
                "[dustagent compaction response: unverified model output] invented_path.rs",
            ))
            .unwrap();
        let first = store
            .index_compaction("목표 및 실패 이유\n후속 조사")
            .unwrap();
        assert_eq!((first.from, first.through), (1, 26));
        assert_eq!(first.highlights.first().unwrap().index, 1);
        assert_eq!(first.highlights.last().unwrap().index, 25);
        assert!(!first.keywords.iter().any(|k| k.term.contains("invented")));
        for keyword in &first.keywords {
            let page = store.read(keyword.index, 0, PAGE).unwrap();
            assert!(page["text"].as_str().unwrap().contains(&keyword.term));
        }
        store
            .append(&ChatMessage::user("마지막 last_path.rs"))
            .unwrap();
        let second = store.index_compaction("둘째").unwrap();
        assert_eq!(second.id, 2);
        assert_eq!(second.from, 27);
        let page = store.directory(0, 1, None).unwrap();
        assert_eq!(page["entries"][0]["id"], 1);
        assert_eq!(page["next_cursor"], 1);
        assert_eq!(store.directory(1, 1, None).unwrap()["entries"][0]["id"], 2);
        assert_eq!(
            store.directory(0, 10, Some("last_path.rs")).unwrap()["entries"][0]["id"],
            2
        );
        assert_eq!(
            store.directory(0, 10, Some("이유\n후속")).unwrap()["entries"][0]["id"],
            1
        );
        for bytes in 0..300 {
            assert!(store.directory_hint(bytes).len() <= bytes);
        }
        let reopened = TranscriptStore::open_in(&store.reference(), "app", root.path()).unwrap();
        assert_eq!(
            reopened.directory(0, 10, None).unwrap(),
            store.directory(0, 10, None).unwrap()
        );
        assert!(store.directory(0, 11, None).is_err());
        assert!(store.directory(3, 1, None).is_err());
        assert!(store.directory(0, 1, Some(&"가".repeat(257))).is_err());
        let mut contents = fs::read_to_string(&store.reference.path).unwrap();
        contents = contents.replace("둘째", "변경");
        fs::write(&store.reference.path, contents).unwrap();
        assert!(TranscriptStore::open_in(&store.reference(), "app", root.path()).is_err());
    }
    #[test]
    fn directory_indexes_arguments_and_keeps_hint_terms() {
        use crate::ports::llm::ToolCall;
        let root = tempfile::tempdir().unwrap();
        let mut store = TranscriptStore::create_in(root.path(), "app").unwrap();
        store
            .append(&ChatMessage::assistant(
                None,
                Some(vec![ToolCall {
                    id: "call1".into(),
                    function_name: "x".repeat(20000),
                    arguments:
                        json!({"url":"https://example.org/last_page", "file":"src/recovery.rs"})
                            .to_string(),
                }]),
            ))
            .unwrap();
        let entry = store.index_compaction(&"가".repeat(512)).unwrap();
        assert!(entry.keywords.iter().all(|k| k.term.len() <= 160));
        assert!(
            entry
                .keywords
                .iter()
                .any(|k| k.term.contains("example.org"))
        );
        let hint = store.directory_hint(256);
        assert!(hint.contains("example.org"));
        assert!(hint.len() <= 256);
        assert!(store.directory(0, 10, None).unwrap()["next_cursor"].is_null());
        assert!(TranscriptStore::open_in(&store.reference(), "app", root.path()).is_ok());
    }
    #[test]
    fn directory_pages_stay_bounded_and_progress() {
        let root = tempfile::tempdir().unwrap();
        let mut store = TranscriptStore::create_in(root.path(), "app").unwrap();
        for _ in 0..12 {
            for i in 0..10 {
                store
                    .append(&ChatMessage::user(format!(
                        "{} module_{i}.rs",
                        "가".repeat(400)
                    )))
                    .unwrap();
            }
            store.index_compaction(&"한".repeat(512)).unwrap();
        }
        let mut cursor = 0;
        let mut total = 0;
        loop {
            let page = store.directory(cursor, 10, None).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE);
            let n = page["entries"].as_array().unwrap().len();
            assert!(n > 0);
            total += n;
            match page["next_cursor"].as_u64() {
                Some(next) => {
                    assert!(next as usize > cursor);
                    cursor = next as usize;
                }
                None => break,
            }
        }
        assert_eq!(total, 12);
    }
    #[test]
    fn message_content_cannot_forge_directory_metadata() {
        let root = tempfile::tempdir().unwrap();
        let mut store = TranscriptStore::create_in(root.path(), "app").unwrap();
        store
            .append(&ChatMessage::user(
                r#"{"directory":{"id":1,"from":1,"through":1}}"#,
            ))
            .unwrap();
        let opened = TranscriptStore::open_in(&store.reference(), "app", root.path()).unwrap();
        assert_eq!(opened.directory(0, 10, None).unwrap()["count"], 0);
    }
    #[test]
    fn originals_pages_search_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TranscriptStore::create_in(dir.path(), "app").unwrap();
        store
            .append(&ChatMessage::user("안녕 needle".repeat(1000)))
            .unwrap();
        store
            .append(&ChatMessage::assistant_text("needle second"))
            .unwrap();
        let page = store.read(1, 0, 100).unwrap();
        assert!(page["next_offset"].is_number());
        let next = page["next_offset"].as_u64().unwrap() as usize;
        assert!(store.read(1, next, 100).is_ok());
        let found = store.search("needle", 0, 1).unwrap();
        assert_eq!(found["results"][0]["index"], 1);
        assert_eq!(
            store.search("needle", 1, 1).unwrap()["results"][0]["index"],
            2
        );
        assert!(TranscriptStore::open_in(&store.reference(), "app", dir.path()).is_ok());
        assert!(TranscriptStore::open_in(&store.reference(), "other", dir.path()).is_err());
        fs::write(&store.reference.path, b"{}\n").unwrap();
        assert!(TranscriptStore::open_in(&store.reference(), "app", dir.path()).is_err());
    }
    #[test]
    fn oversized_is_not_truncated_and_sessions_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = TranscriptStore::create_in(dir.path(), "app").unwrap();
        let b = TranscriptStore::create_in(dir.path(), "app").unwrap();
        assert!(
            a.append(&ChatMessage::user("x".repeat(MAX_RECORD)))
                .is_err()
        );
        assert_eq!(a.count(), 0);
        a.append(&ChatMessage::user("private")).unwrap();
        assert_eq!(b.search("private", 0, 20).unwrap()["results"], json!([]));
        assert!(a.read(1, 0, PAGE + 1).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&a.reference.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn search_advances_without_matches_and_rejects_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TranscriptStore::create_in(dir.path(), "app").unwrap();
        for _ in 0..1001 {
            store.append(&ChatMessage::user("nothing")).unwrap();
        }
        let page = store.search("missing", 0, 20).unwrap();
        assert_eq!(page["next_cursor"], 1000);
        assert!(store.search("missing", 1000, 20).unwrap()["next_cursor"].is_null());
        #[cfg(unix)]
        {
            let reference = store.reference();
            let alternate = dir.path().join("elsewhere");
            fs::rename(&reference.path, &alternate).unwrap();
            std::os::unix::fs::symlink(&alternate, &reference.path).unwrap();
            assert!(TranscriptStore::open_in(&reference, "app", dir.path()).is_err());
            assert!(store.append(&ChatMessage::user("anything")).is_err());
        }
    }
    #[test]
    fn search_matches_literal_newlines_quotes_and_backslashes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TranscriptStore::create_in(dir.path(), "app").unwrap();
        let original = "first\nsecond \"quoted\" C:\\folder";
        store.append(&ChatMessage::user(original)).unwrap();
        for query in ["first\nsecond", "\"quoted\"", "C:\\folder", original] {
            let page = store.search(query, 0, 20).unwrap();
            assert_eq!(page["results"][0]["index"], 1, "query {query:?}");
        }
        assert!(
            store.search("first\\nsecond", 0, 20).unwrap()["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
