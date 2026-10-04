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
        use std::collections::VecDeque;
        use std::io::BufRead;
        let file = std::fs::File::open(self.directory.join("conversation.jsonl"))?;
        let mut reader = std::io::BufReader::new(file);
        let mut turns = VecDeque::new();
        let mut current: Vec<serde_json::Value> = Vec::new();
        let mut line = String::new();
        let mut seq = 0u64;
        let keep_tail = tail || before.is_some();
        let retain = |turn: Vec<serde_json::Value>,
                      turns: &mut VecDeque<Vec<serde_json::Value>>| {
            let Some(start) = turn.first().and_then(|row| row["seq"].as_u64()) else {
                return;
            };
            if limit == 0 || start < from || before.is_some_and(|boundary| start >= boundary) {
                return;
            }
            if keep_tail || turns.len() < limit {
                turns.push_back(turn);
                if turns.len() > limit {
                    turns.pop_front();
                }
            }
        };
        while reader.read_line(&mut line)? != 0 {
            seq += 1;
            if !line.trim().is_empty() {
                let mut row: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(row) => row,
                    Err(_) if !line.ends_with('\n') => break,
                    Err(error) => return Err(error.into()),
                };
                let starts_turn = row["role"] == "user"
                    && row["content"].is_string()
                    && row["internal"] != true
                    && row.pointer("/_mink/guidance") != Some(&serde_json::Value::Bool(true));
                if starts_turn && !current.is_empty() {
                    retain(std::mem::take(&mut current), &mut turns);
                }
                row["seq"] = seq.into();
                current.push(row);
            }
            line.clear();
        }
        retain(current, &mut turns);
        Ok(turns.into_iter().flatten().collect())
    }

    /// Read a child session within this parent's isolated subagent collection.
    pub fn sub_agent(&self, id: &str) -> Result<SessionReader> {
        let mut components = Path::new(id).components();
        anyhow::ensure!(
            matches!(components.next(), Some(std::path::Component::Normal(_)))
                && components.next().is_none(),
            "invalid sub-agent session ID"
        );
        let root = self.directory.join("subagents").canonicalize()?;
        anyhow::ensure!(
            root.starts_with(self.directory.canonicalize()?),
            "sub-agent collection escapes session"
        );
        let child = root.join(id).canonicalize()?;
        anyhow::ensure!(child.starts_with(&root), "sub-agent path escapes session");
        Ok(SessionReader::new(child))
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

#[cfg(test)]
mod reader_tests {
    use super::*;
    #[test]
    fn streamed_turn_windows_keep_guidance_internal_and_tool_exchange() {
        let dir = std::env::temp_dir().join(format!(
            "mink-reader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conversation.jsonl");
        let data = concat!(
            "{\"role\":\"user\",\"content\":\"one\"}\n",
            "{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"x\"}]}\n",
            "{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"x\"}]}\n",
            "{\"role\":\"user\",\"content\":\"guide\",\"_mink\":{\"guidance\":true}}\n",
            "{\"role\":\"user\",\"content\":\"internal\",\"internal\":true}\n",
            "{\"role\":\"user\",\"content\":\"two\"}\n",
            "{\"role\":\"assistant\",\"content\":\"answer\"}\n",
            "{partial"
        );
        std::fs::write(&path, data).unwrap();
        let reader = SessionReader::new(&dir);
        let head = reader.conversation_turns(1, 1, false, None).unwrap();
        assert_eq!(head.len(), 5);
        assert_eq!(head[4]["seq"], 5);
        let tail = reader.conversation_turns(1, 1, true, None).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0]["seq"], 6);
        assert_eq!(
            reader.conversation_turns(1, 1, false, Some(6)).unwrap(),
            head
        );
        assert!(
            reader
                .conversation_turns(1, 0, true, None)
                .unwrap()
                .is_empty()
        );
        std::fs::write(&path, "{broken}\n").unwrap();
        assert!(reader.conversation_turns(1, 1, true, None).is_err());
        assert!(reader.sub_agent("../escape").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
