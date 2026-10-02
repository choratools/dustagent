//! Opt-in, locally curated examples. Completion is never treated as approval.
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    domain::manifest::AppManifest,
    error::{DustError, Result},
};

const MAX_FILE: u64 = 8 * 1024 * 1024;
const MAX_TEXT: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experience {
    pub id: String,
    pub input: String,
    pub output: String,
    pub completed: bool,
    pub approved: bool,
    #[serde(default)]
    pub reviewed: bool,
    #[serde(default)]
    pub review_reason: Option<String>,
    #[serde(default)]
    pub validation: Option<super::validation::ValidationEvidence>,
    #[serde(default)]
    pub sources: Vec<super::research::SourceEvidence>,
    pub timestamp_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Research {
        id: String,
        sources: Vec<super::research::SourceEvidence>,
    },
    Validation {
        id: String,
        evidence: super::validation::ValidationEvidence,
    },
    Record {
        experience: Experience,
    },
    Approval {
        id: String,
        approved: bool,
    },
    Review {
        id: String,
        reuse: bool,
        reason: String,
    },
}

pub struct ExperienceStore {
    root: PathBuf,
}

impl ExperienceStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, manifest: &AppManifest) -> Result<PathBuf> {
        // Value's default ordered maps canonicalize nested HashMaps too.
        let canonical = serde_json::to_vec(&serde_json::to_value(manifest)?)?;
        Ok(self
            .root
            .join(format!("{:x}.jsonl", Sha256::digest(canonical))))
    }

    fn append(&self, manifest: &AppManifest, event: Event) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        let path = self.path(manifest)?;
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) + bytes.len() as u64 > MAX_FILE {
            return Err(DustError::Config(
                "Experience store exceeds 8 MiB; archive it before recording more".into(),
            ));
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&bytes)?;
        Ok(())
    }

    pub fn record(
        &self,
        manifest: &AppManifest,
        input: &str,
        output: &str,
        completed: bool,
    ) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| DustError::Config(e.to_string()))?;
        let mut digest = Sha256::new();
        digest.update(now.as_nanos().to_le_bytes());
        digest.update(input.as_bytes());
        digest.update(output.as_bytes());
        let id = format!("{:x}", digest.finalize());
        let experience = Experience {
            id: id.clone(),
            input: truncate(input),
            output: truncate(output),
            completed: completed && input.len() <= MAX_TEXT && output.len() <= MAX_TEXT,
            approved: false,
            reviewed: false,
            review_reason: None,
            validation: None,
            sources: Vec::new(),
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| DustError::Config(e.to_string()))?
                .as_millis()
                .min(u64::MAX as u128) as u64,
        };
        self.append(manifest, Event::Record { experience })?;
        Ok(id)
    }

    pub fn list(&self, manifest: &AppManifest) -> Result<Vec<Experience>> {
        let file = match fs::File::open(self.path(manifest)?) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        if file.metadata()?.len() > MAX_FILE {
            return Err(DustError::Config("Experience store exceeds 8 MiB".into()));
        }
        let mut experiences: Vec<Experience> = vec![];
        for line in BufReader::new(file.take(MAX_FILE + 1)).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(&line)? {
                Event::Research { id, sources } => {
                    if let Some(e) = experiences.iter_mut().find(|e| e.id == id) {
                        e.sources = sources;
                    }
                }
                Event::Validation { id, evidence } => {
                    if let Some(e) = experiences.iter_mut().find(|e| e.id == id) {
                        e.validation = Some(evidence);
                    }
                }
                Event::Record { mut experience } => {
                    experience.approved = false;
                    experience.completed &=
                        experience.input.len() <= MAX_TEXT && experience.output.len() <= MAX_TEXT;
                    experience.input = truncate(&experience.input);
                    experience.output = truncate(&experience.output);
                    experiences.push(experience);
                }
                Event::Review { id, reuse, reason } => {
                    if let Some(e) = experiences.iter_mut().find(|e| e.id == id) {
                        e.reviewed = true;
                        e.review_reason = Some(reason);
                        e.approved = reuse && e.completed && !e.output.trim().is_empty();
                    }
                }
                Event::Approval { id, approved } => {
                    if let Some(e) = experiences.iter_mut().find(|e| e.id == id) {
                        e.approved = approved && e.completed && !e.output.trim().is_empty();
                    }
                }
            }
        }
        Ok(experiences)
    }

    pub fn approve(&self, manifest: &AppManifest, id: &str, approved: bool) -> Result<()> {
        let experiences = self.list(manifest)?;
        let e = experiences
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| DustError::Config(format!("Unknown experience id: {id}")))?;
        if !e.completed || e.output.trim().is_empty() {
            return Err(DustError::Config(
                "Only completed, nonempty experiences can be curated".into(),
            ));
        }
        self.append(
            manifest,
            Event::Approval {
                id: id.into(),
                approved,
            },
        )
    }

    pub fn record_sources(
        &self,
        manifest: &AppManifest,
        id: &str,
        sources: Vec<super::research::SourceEvidence>,
    ) -> Result<()> {
        if !self.list(manifest)?.iter().any(|e| e.id == id) {
            return Err(DustError::Config("Unknown record".into()));
        }
        self.append(
            manifest,
            Event::Research {
                id: id.into(),
                sources,
            },
        )
    }

    pub fn record_validation(
        &self,
        manifest: &AppManifest,
        id: &str,
        evidence: super::validation::ValidationEvidence,
    ) -> Result<()> {
        if !self.list(manifest)?.iter().any(|e| e.id == id) {
            return Err(DustError::Config("Unknown record".into()));
        }
        self.append(
            manifest,
            Event::Validation {
                id: id.into(),
                evidence,
            },
        )
    }

    /// Persist an automatic review; identifiers remain internal bookkeeping.
    pub fn review(
        &self,
        manifest: &AppManifest,
        id: &str,
        reuse: bool,
        reason: &str,
    ) -> Result<()> {
        let records = self.list(manifest)?;
        let e = records
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| DustError::Config("Unknown record".into()))?;
        if reuse && (!e.completed || e.output.trim().is_empty()) {
            return Err(DustError::Config(
                "Incomplete examples cannot be selected".into(),
            ));
        }
        if reason.trim().is_empty() {
            return Err(DustError::Config("Review reason is required".into()));
        }
        self.append(
            manifest,
            Event::Review {
                id: id.into(),
                reuse,
                reason: truncate(reason),
            },
        )
    }

    pub fn select(
        &self,
        manifest: &AppManifest,
        input: &str,
        limit: usize,
    ) -> Result<Vec<Experience>> {
        let query = words(input);
        let mut scored: Vec<_> = self
            .list(manifest)?
            .into_iter()
            .filter(|e| e.approved && e.completed && !e.output.trim().is_empty())
            .filter_map(|e| {
                let score = words(&e.input).intersection(&query).count();
                (score > 0).then_some((score, e))
            })
            .collect();
        scored.sort_by(|(sa, a), (sb, b)| {
            sb.cmp(sa)
                .then(b.timestamp_ms.cmp(&a.timestamp_ms))
                .then(a.id.cmp(&b.id))
        });
        Ok(scored
            .into_iter()
            .take(limit.min(3))
            .map(|(_, e)| e)
            .collect())
    }
}

fn truncate(text: &str) -> String {
    let mut end = text.len().min(MAX_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_replay_relevance_and_manifest_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExperienceStore::new(dir.path());
        let app = AppManifest::new().with_name("../unsafe");
        let id = store
            .record(&app, "Rust 컴파일 오류", "cargo check", true)
            .unwrap();
        assert!(store.select(&app, "Rust", 3).unwrap().is_empty());
        store.approve(&app, &id, true).unwrap();
        assert_eq!(store.select(&app, "컴파일", 3).unwrap()[0].id, id);
        assert!(store.select(&app, "weather", 3).unwrap().is_empty());
        assert!(
            store
                .list(&app.clone().with_system_prompt("different"))
                .unwrap()
                .is_empty()
        );
        store.approve(&app, &id, false).unwrap();
        assert!(!store.list(&app).unwrap()[0].approved);
    }

    #[test]
    fn failed_empty_unknown_examples_cannot_be_approved() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExperienceStore::new(dir.path());
        let app = AppManifest::new();
        for (output, completed) in [("failure", false), ("   ", true)] {
            let id = store.record(&app, "input", output, completed).unwrap();
            assert!(store.approve(&app, &id, true).is_err());
        }
        assert!(store.approve(&app, "unknown", true).is_err());
        assert!(store.select(&app, "input", 3).unwrap().is_empty());
    }

    #[test]
    fn unicode_bounds_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExperienceStore::new(dir.path());
        let app = AppManifest::new();
        let id = store
            .record(&app, &"한".repeat(MAX_TEXT), "output", true)
            .unwrap();
        assert!(store.list(&app).unwrap()[0].input.len() <= MAX_TEXT);
        assert!(!store.list(&app).unwrap()[0].completed);
        assert!(store.approve(&app, &id, true).is_err());
        let oversized_output = store
            .record(&app, "input", &"x".repeat(MAX_TEXT + 1), true)
            .unwrap();
        assert!(store.approve(&app, &oversized_output, true).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(store.path(&app).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}

#[cfg(test)]
mod policy_scope_tests {
    use super::*;
    #[test]
    fn new_checker_does_not_inherit_old_selections() {
        let dir = tempfile::tempdir().unwrap();
        let store = ExperienceStore::new(dir.path());
        let original = AppManifest::new().with_name("same-app");
        let id = store.record(&original, "request", "result", true).unwrap();
        store
            .review(&original, &id, true, "prior model review")
            .unwrap();
        let mut with_checker = original.clone();
        with_checker.validation = Some(crate::application::validation::ValidationConfig {
            mode: crate::application::validation::ValidationMode::Legacy,
            command: "checker".into(),
            args: vec![],
            timeout_ms: 5000,
        });
        assert!(store.list(&with_checker).unwrap().is_empty());
        assert!(store.list(&original).unwrap()[0].approved);
    }
}
