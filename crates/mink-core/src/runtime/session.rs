//! Read-only session discovery and inspection for hosts embedding mink.
//!
//! Runtime construction remains the only supported way to mutate a session.
//! This module intentionally exposes records rather than the internal session
//! stores and path-building machinery.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use crate::session::artifacts::{ArtifactManager, ArtifactRecord};
pub use crate::session::metadata::{SessionMetadata, SessionReferenceError, SessionSeed};
pub use crate::session::paths::Paths as SessionPaths;
pub use crate::session::todo::{TodoSnapshot, TodoStatus};
pub use crate::session::usage::{TokenUsage, UsageKind, UsageRecord, UsageStatus, UsageSummary};

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub path: PathBuf,
    pub alias: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub cwd: String,
    pub parent: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub modified: SystemTime,
}

impl From<crate::session::metadata::SessionRecord> for SessionRecord {
    fn from(record: crate::session::metadata::SessionRecord) -> Self {
        let metadata = record.metadata;
        Self {
            id: record.id,
            path: record.path,
            alias: metadata.alias,
            title: metadata.title,
            summary: metadata.summary,
            cwd: metadata.cwd,
            parent: metadata.parent,
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
            modified: record.modified,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionCatalog {
    home: PathBuf,
    cwd: PathBuf,
    layout: super::SessionLayout,
}

impl SessionCatalog {
    pub fn new(home: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            cwd: cwd.into(),
            layout: super::SessionLayout::ProjectScoped,
        }
    }

    pub fn with_layout(mut self, layout: super::SessionLayout) -> Self {
        self.layout = layout;
        self
    }

    pub async fn list(&self) -> Result<Vec<SessionRecord>> {
        crate::session::metadata::list_sessions_with_layout(&self.home, &self.cwd, self.layout)
            .await
            .map(|records| records.into_iter().map(SessionRecord::from).collect())
    }

    pub async fn resolve(&self, reference: &str) -> Result<Option<SessionRecord>> {
        crate::session::metadata::resolve_session_record_with_layout(
            &self.home,
            &self.cwd,
            reference,
            self.layout,
        )
        .await
        .map(|record| record.map(SessionRecord::from))
        .map_err(anyhow::Error::from)
    }
}

#[derive(Debug, Clone)]
pub struct SessionReader {
    directory: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SessionUsage {
    pub summary: UsageSummary,
    pub last_context_tokens: u64,
}

impl SessionReader {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn usage(&self) -> Result<UsageSummary> {
        self.usage_snapshot().map(|usage| usage.summary)
    }

    pub fn usage_snapshot(&self) -> Result<SessionUsage> {
        let records =
            crate::session::usage::read_records_resilient(&self.directory.join("usage.jsonl"))?;
        let last_context_tokens = records
            .iter()
            .rev()
            .find_map(|record| record.tokens.as_ref())
            .map(|tokens| {
                tokens
                    .input_tokens
                    .saturating_add(tokens.cache_read_tokens)
                    .saturating_add(tokens.cache_creation_tokens)
            })
            .unwrap_or_default();
        Ok(SessionUsage {
            summary: UsageSummary::from_records(&records),
            last_context_tokens,
        })
    }

    pub fn conversation(&self) -> Result<Vec<serde_json::Value>> {
        let path = self.directory.join("conversation.jsonl");
        let data = std::fs::read_to_string(&path)?;
        let mut rows = Vec::new();
        for (index, line) in data.split_inclusive('\n').enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(mut value) => {
                    value["seq"] = (index + 1).into();
                    rows.push(value);
                }
                Err(_) if !line.ends_with('\n') => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(rows)
    }
    /// Whole user turns for workbench pagination, retaining internal messages,
    /// guidance, and complete tool exchanges inside their original turn.
    pub fn conversation_turns(
        &self,
        from: u64,
        limit: usize,
        tail: bool,
        before: Option<u64>,
    ) -> Result<Vec<serde_json::Value>> {
        let rows = self.conversation()?;
        let mut starts: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row["role"] == "user"
                    && row["content"].is_string()
                    && row["internal"] != true
                    && row.pointer("/_mink/guidance") != Some(&serde_json::Value::Bool(true))
            })
            .map(|(index, _)| index)
            .collect();
        if starts.first() != Some(&0) && !rows.is_empty() {
            starts.insert(0, 0);
        }
        let eligible: Vec<usize> = starts
            .iter()
            .copied()
            .filter(|index| {
                let seq = rows[*index]["seq"].as_u64().unwrap_or_default();
                seq >= from && before.is_none_or(|boundary| seq < boundary)
            })
            .collect();
        if eligible.is_empty() {
            return Ok(vec![]);
        }
        let chosen: Vec<usize> = if tail || before.is_some() {
            eligible
                .iter()
                .rev()
                .take(limit)
                .copied()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        } else {
            eligible.into_iter().take(limit).collect()
        };
        let begin = chosen[0];
        let last = *chosen.last().unwrap();
        let end = starts
            .iter()
            .find(|index| **index > last)
            .copied()
            .unwrap_or(rows.len());
        Ok(rows[begin..end].to_vec())
    }
    pub fn inputs(&self) -> Result<Vec<crate::runtime::InputReceipt>> {
        crate::session::input::InputInbox::load(
            self.directory.join("inputs.json"),
            crate::session::persistence::PersistenceFault::default(),
        )
        .map(|i| i.entries())
    }
    pub fn artifacts(&self) -> Result<Vec<serde_json::Value>> {
        let data = match std::fs::read_to_string(self.directory.join("artifacts/index.jsonl")) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        Ok(crate::session::jsonl::parse_lossy_lines(
            &self.directory.join("artifacts/index.jsonl"),
            &data,
            &mut |warning| eprintln!("{warning}"),
        ))
    }
    pub fn plan(&self) -> Result<serde_json::Value> {
        fn optional(path: &Path) -> Result<Option<String>> {
            match std::fs::read_to_string(path) {
                Ok(text) => Ok(Some(text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        Ok(
            serde_json::json!({"plan":optional(&self.directory.join("plan.md"))?,"draft":optional(&self.directory.join("plan.draft"))?}),
        )
    }
    pub fn todo(&self) -> Result<TodoSnapshot> {
        crate::session::todo::TodoStore::load(self.directory.join("todos.json"))
            .map(|store| store.snapshot())
    }
}

pub fn title_from_prompt(prompt: &str) -> Option<String> {
    crate::session::metadata::title_from_prompt(prompt)
}

pub fn first_line(text: &str) -> &str {
    crate::session::store::first_line(text)
}

pub fn build_tool_summary_from_json(name: &str, event: &serde_json::Value) -> String {
    crate::session::store::build_tool_summary_from_json(name, event)
}

pub fn sanitize_alias(raw: &str) -> Option<String> {
    crate::session::metadata::sanitize_alias(raw)
}

pub fn project_key(cwd: &Path) -> String {
    crate::session::paths::project_key(cwd)
}

pub fn new_session_id() -> String {
    crate::session::paths::chrono_session_id()
}

pub fn paths_for(home: &Path, cwd: &Path, session_id: &str) -> SessionPaths {
    crate::session::paths::paths_for(home, cwd, session_id)
}

pub async fn resolve_record(
    home: &Path,
    cwd: &Path,
    reference: &str,
    layout: super::SessionLayout,
) -> Result<Option<crate::session::metadata::SessionRecord>> {
    crate::session::metadata::resolve_session_record_with_layout(home, cwd, reference, layout)
        .await
        .map_err(anyhow::Error::from)
}

pub async fn ensure_metadata(paths: &SessionPaths, cwd: &Path, seed: SessionSeed) -> Result<()> {
    crate::session::metadata::ensure_metadata(paths, cwd, seed).await
}
