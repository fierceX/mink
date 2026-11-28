//! Read-only session discovery and inspection for hosts embedding mink.
//!
//! Runtime construction remains the only supported way to mutate a session.
//! This module intentionally exposes records rather than the internal session
//! stores and path-building machinery.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::events::{EventLog as SessionEvent, SessionEventBatch};

pub use crate::session::artifacts::{ArtifactManager, ArtifactRecord};
pub use crate::session::metadata::{SessionMetadata, SessionReferenceError, SessionSeed};
pub use crate::session::paths::Paths as SessionPaths;
pub use crate::session::subagent_catalog::{
    SubAgentCatalogEntry, read_catalog as read_subagent_catalog,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedGuidanceEvent {
    pub input_id: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    User {
        text: String,
        guidance: bool,
    },
    Thinking(String),
    Text(String),
    ToolCall {
        id: String,
        name: String,
        summary: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: Option<String>,
        tool_name: String,
        content: String,
        status: Option<crate::tools::metadata::ToolStatus>,
        exit_code: Option<i32>,
        result_kind: crate::tools::metadata::ToolResultKind,
        presentation: Option<crate::ui::ToolPresentation>,
        artifacts: Vec<crate::ui::ArtifactDisplay>,
    },
    WebSource(String),
}

#[derive(Debug, Clone, Default)]
pub struct SessionUiSnapshot {
    pub image_input: Option<crate::capabilities::model_capabilities::OpenAiChatImageUrlLimits>,
    pub plan_draft: Option<String>,
    pub plan_active: Option<String>,
    pub todos: Option<TodoSnapshot>,
}

fn is_internal_conversation_row(row: &serde_json::Value) -> bool {
    if row["internal"] == true {
        return true;
    }
    let metadata = &row["_mink"];
    if metadata["internal"] == true {
        return true;
    }
    metadata.get("todo_state_kind").is_some()
        || metadata.get("plan_transaction_id").is_some()
        || metadata.get("source").is_some()
}

impl SessionReader {
    /// Remove provider replay metadata before publishing a conversation row.
    pub fn redact_provider_replay(message: &mut serde_json::Value) {
        crate::llm::deepseek_web::remove_replay_fields(message);
    }

    /// Derived delegation catalog recorded in this session's event log.
    pub fn subagent_catalog(&self) -> Result<Vec<SubAgentCatalogEntry>> {
        crate::session::subagent_catalog::read_catalog(&self.directory)
    }
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

    /// Read the whole durable event log (`events.jsonl`) as typed records.
    ///
    /// Unknown event types and malformed complete lines are skipped; use
    /// [`Self::events_since`] when the skipped count or incremental reads
    /// matter.
    pub fn events(&self) -> Result<Vec<SessionEvent>> {
        Ok(self.events_since(0, usize::MAX)?.events)
    }

    /// Read at most `max_events` typed records starting at a byte offset.
    ///
    /// Only complete lines are consumed. A trailing partial line stays at
    /// `next_offset` until the writer finishes it. An offset past the current
    /// end returns an empty batch with `truncated = true` and `next_offset = 0`.
    pub fn events_since(&self, offset: u64, max_events: usize) -> Result<SessionEventBatch> {
        crate::session::event_reader::events_since(
            &self.directory.join("events.jsonl"),
            offset,
            max_events,
        )
    }

    pub fn conversation(&self) -> Result<Vec<serde_json::Value>> {
        self.conversation_with_sequence().map(|(mut rows, _)| {
            rows.iter_mut().for_each(Self::redact_provider_replay);
            rows
        })
    }

    /// Read committed conversation rows with their physical JSONL sequence.
    /// The returned boundary includes blank physical lines, which still occupy
    /// sequence positions for persisted extension and reader cursors.
    pub fn conversation_with_sequence(&self) -> Result<(Vec<serde_json::Value>, usize)> {
        let path = self.directory.join("conversation.jsonl");
        let data = std::fs::read_to_string(&path)?;
        let mut rows = Vec::new();
        let mut committed_sequence = 0;
        for (index, line) in data.split_inclusive('\n').enumerate() {
            let sequence = index + 1;
            if line.trim().is_empty() {
                committed_sequence = sequence;
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(mut value) => {
                    value["seq"] = sequence.into();
                    rows.push(value);
                    committed_sequence = sequence;
                }
                Err(_) if !line.ends_with('\n') => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok((rows, committed_sequence))
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
        self.conversation_turns_bounded(from, limit, tail, before, None)
    }

    /// Whole user turns with an optional serialized-byte ceiling. The newest
    /// turn is always returned even when it alone exceeds the ceiling, so a
    /// paging request can never split a tool call from its result.
    pub fn conversation_turns_bounded(
        &self,
        from: u64,
        limit: usize,
        tail: bool,
        before: Option<u64>,
        max_bytes: Option<usize>,
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
                Self::redact_provider_replay(&mut row);
                if starts_turn && !current.is_empty() {
                    retain(std::mem::take(&mut current), &mut turns);
                }
                row["seq"] = seq.into();
                current.push(row);
            }
            line.clear();
        }
        retain(current, &mut turns);
        let turns: Vec<Vec<serde_json::Value>> = turns.into_iter().collect();
        let mut start = 0usize;
        if let Some(max_bytes) = max_bytes.filter(|max| *max > 0) {
            let turn_bytes = |turn: &Vec<serde_json::Value>| {
                serde_json::to_vec(turn)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0)
            };
            if keep_tail {
                let mut total: usize = turns.iter().map(turn_bytes).sum();
                while start + 1 < turns.len() && total > max_bytes {
                    total = total.saturating_sub(turn_bytes(&turns[start]));
                    start += 1;
                }
            } else {
                // A forward page keeps its requested beginning; it must not
                // silently turn into the newest subset of that page.
                let mut total = 0usize;
                let mut end = 0usize;
                for turn in &turns {
                    let bytes = turn_bytes(turn);
                    if end > 0 && total + bytes > max_bytes {
                        break;
                    }
                    total += bytes;
                    end += 1;
                }
                return Ok(turns.into_iter().take(end.max(1)).flatten().collect());
            }
        }
        Ok(turns.into_iter().skip(start).flatten().collect())
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

    /// Whether a committed conversation row contains structures that only
    /// `conversation.jsonl` carries (guidance, extension replay markers, native web calls).
    pub fn row_needs_conversation_replay(row: &serde_json::Value) -> bool {
        let metadata = &row["_mink"];
        metadata["guidance"] == true
            || metadata["replay"] == true
            || row["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "deepseek_web"))
    }

    /// Decode an applied human guidance row from a committed conversation message.
    pub fn decode_applied_guidance(row: &serde_json::Value) -> Option<AppliedGuidanceEvent> {
        if row["role"] != "user" || row["_mink"]["guidance"] != true {
            return None;
        }
        let input_id = row["_mink"]["input_id"].as_str()?.to_string();
        let text = row["content"].as_str()?.to_string();
        Some(AppliedGuidanceEvent { input_id, text })
    }

    /// Decode one committed conversation row into typed transcript entries.
    pub fn decode_conversation_row(row: &serde_json::Value) -> Vec<TranscriptEntry> {
        let mut entries = Vec::new();
        if row["role"] == "user"
            && let Some(text) = row["content"].as_str()
            && !is_internal_conversation_row(row)
        {
            entries.push(TranscriptEntry::User {
                text: text.to_string(),
                guidance: row["_mink"]["guidance"] == true,
            });
        }
        if let Some(blocks) = row["content"].as_array() {
            let mut citations: Option<&Vec<serde_json::Value>> = None;
            for block in blocks {
                match block["type"].as_str() {
                    Some("thinking") if row["role"] == "assistant" => {
                        entries.push(TranscriptEntry::Thinking(
                            block["thinking"].as_str().unwrap_or_default().to_string(),
                        ));
                    }
                    Some("text") if row["role"] == "assistant" => {
                        entries.push(TranscriptEntry::Text(
                            block["text"].as_str().unwrap_or_default().to_string(),
                        ));
                    }
                    Some("tool_use") if row["role"] == "assistant" => {
                        let name = block["name"].as_str().unwrap_or_default().to_string();
                        let id = block["id"].as_str().unwrap_or_default().to_string();
                        let input = block["input"].clone();
                        let summary = crate::session::store::build_tool_summary_from_json(
                            &name,
                            &serde_json::json!({ "input": &input }),
                        );
                        entries.push(TranscriptEntry::ToolCall {
                            id,
                            name,
                            summary,
                            input,
                        });
                    }
                    Some("deepseek_web") if row["role"] == "assistant" => {
                        for call in block["calls"].as_array().into_iter().flatten() {
                            let (Some(id), Some(name)) =
                                (call["id"].as_str(), call["tool"].as_str())
                            else {
                                continue;
                            };
                            let action = call["action"].clone();
                            let summary = crate::session::store::build_tool_summary_from_json(
                                name,
                                &serde_json::json!({ "input": &action }),
                            );
                            entries.push(TranscriptEntry::ToolCall {
                                id: id.to_string(),
                                name: name.to_string(),
                                summary,
                                input: action,
                            });
                            let status = match call["status"].as_str() {
                                Some("completed") => {
                                    Some(crate::tools::metadata::ToolStatus::Succeeded)
                                }
                                Some("failed") => Some(crate::tools::metadata::ToolStatus::Failed(
                                    crate::tools::metadata::ToolFailureKind::Unknown,
                                )),
                                _ => None,
                            };
                            entries.push(TranscriptEntry::ToolResult {
                                tool_use_id: Some(id.to_string()),
                                tool_name: name.to_string(),
                                content: serde_json::to_string_pretty(call).unwrap_or_default(),
                                status,
                                exit_code: None,
                                result_kind: crate::tools::metadata::ToolResultKind::Search,
                                presentation: None,
                                artifacts: Vec::new(),
                            });
                        }
                        citations = block["citations"].as_array();
                    }
                    Some("tool_result") => {
                        let metadata = &block["_mink"];
                        let tool_name = metadata["tool_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        let tool_use_id = block["tool_use_id"]
                            .as_str()
                            .or_else(|| metadata["tool_use_id"].as_str())
                            .map(str::to_string);
                        let content = block["content"].as_str().unwrap_or_default().to_string();
                        let status = metadata
                            .get("status")
                            .cloned()
                            .and_then(|v| serde_json::from_value(v).ok());
                        let exit_code = metadata
                            .get("exit_code")
                            .and_then(serde_json::Value::as_i64)
                            .and_then(|v| i32::try_from(v).ok());
                        let result_kind = metadata
                            .get("result_kind")
                            .cloned()
                            .and_then(|v| serde_json::from_value(v).ok())
                            .unwrap_or(crate::tools::metadata::ToolResultKind::Text);
                        let presentation = metadata
                            .get("presentation")
                            .cloned()
                            .and_then(|v| serde_json::from_value(v).ok());
                        let artifacts = metadata
                            .get("artifacts")
                            .cloned()
                            .and_then(|v| serde_json::from_value(v).ok())
                            .unwrap_or_default();
                        entries.push(TranscriptEntry::ToolResult {
                            tool_use_id,
                            tool_name,
                            content,
                            status,
                            exit_code,
                            result_kind,
                            presentation,
                            artifacts,
                        });
                    }
                    _ => {}
                }
            }
            for citation in citations.into_iter().flatten() {
                if let Some(url) = citation["url"].as_str() {
                    entries.push(TranscriptEntry::WebSource(format!("[Web source] {url}")));
                }
            }
        }
        entries
    }

    /// Extract `(thinking, text)` from a child sub-agent assistant conversation row.
    pub fn decode_subagent_assistant_output(row: &serde_json::Value) -> (String, String) {
        if row["role"] != "assistant" {
            return (String::new(), String::new());
        }
        let mut thinking = String::new();
        let mut text = String::new();
        if let Some(blocks) = row["content"].as_array() {
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(chunk) = block["text"].as_str() {
                            text.push_str(chunk);
                        }
                    }
                    Some("thinking") => {
                        if let Some(chunk) = block["thinking"].as_str() {
                            thinking.push_str(chunk);
                        }
                    }
                    _ => {}
                }
            }
        }
        (thinking, text)
    }

    /// Read the frozen image input limits from `model-capabilities.json`, if present.
    pub fn read_image_limits(
        &self,
    ) -> Option<crate::capabilities::model_capabilities::OpenAiChatImageUrlLimits> {
        #[derive(serde::Deserialize)]
        struct Snapshot {
            image_input: crate::capabilities::model_capabilities::ImageInputCapability,
        }
        let raw = std::fs::read_to_string(self.directory.join("model-capabilities.json")).ok()?;
        let snapshot: Snapshot = serde_json::from_str(&raw).ok()?;
        snapshot.image_input.limits().cloned()
    }

    /// Read persisted UI mirrors (image limits, plan draft/active, todos)
    /// without exposing internal session filenames to consumers.
    pub fn read_ui_snapshot(&self) -> SessionUiSnapshot {
        let image_input = self.read_image_limits();
        let plan_draft = std::fs::read_to_string(self.directory.join("plan.draft"))
            .ok()
            .filter(|content| !content.trim().is_empty());
        let plan_active = std::fs::read_to_string(self.directory.join("plan.md"))
            .ok()
            .filter(|content| !content.trim().is_empty());
        let todos = self.todo().ok();
        SessionUiSnapshot {
            image_input,
            plan_draft,
            plan_active,
            todos,
        }
    }

    /// Lazily derive and persist a session title in `session.json` from the first
    /// non-internal user prompt in `conversation.jsonl` if no title exists yet.
    pub async fn ensure_title(&self) -> Result<Option<String>> {
        use tokio::io::AsyncBufReadExt as _;

        let metadata_path = self.directory.join("session.json");
        let conversation_path = self.directory.join("conversation.jsonl");
        let Ok(metadata_text) = tokio::fs::read_to_string(&metadata_path).await else {
            return Ok(None);
        };
        let Ok(mut metadata) = serde_json::from_str::<serde_json::Value>(&metadata_text) else {
            return Ok(None);
        };
        if metadata
            .get("title")
            .and_then(|v| v.as_str())
            .is_some_and(|t| !t.is_empty())
        {
            return Ok(None);
        }
        let Ok(file) = tokio::fs::File::open(&conversation_path).await else {
            return Ok(None);
        };
        let mut reader = tokio::io::BufReader::new(file);
        let mut line = String::new();
        let title = loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            if msg.get("role").and_then(|v| v.as_str()) != Some("user")
                || is_internal_conversation_row(&msg)
            {
                continue;
            }
            let Some(content) = msg.get("content").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(title) = crate::session::metadata::title_from_prompt(content) {
                break title;
            }
        };
        metadata["title"] = serde_json::Value::String(title.clone());
        let now = time::OffsetDateTime::now_utc();
        let fmt = time::format_description::well_known::Rfc3339;
        metadata["updated_at"] = serde_json::Value::String(now.format(&fmt).unwrap_or_default());
        if let Ok(text) = serde_json::to_string_pretty(&metadata) {
            let payload = format!("{text}\n");
            let path = metadata_path;
            let _ = tokio::task::spawn_blocking(move || {
                let _ = crate::session::atomic_file::atomic_replace(&path, payload.as_bytes());
            })
            .await;
        }
        Ok(Some(title))
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

#[derive(Debug, Clone)]
pub struct ResolvedSessionRecord {
    pub id: String,
    pub path: PathBuf,
    pub metadata: SessionMetadata,
    pub modified: SystemTime,
}

impl From<crate::session::metadata::SessionRecord> for ResolvedSessionRecord {
    fn from(record: crate::session::metadata::SessionRecord) -> Self {
        Self {
            id: record.id,
            path: record.path,
            metadata: record.metadata,
            modified: record.modified,
        }
    }
}

pub async fn resolve_record(
    home: &Path,
    cwd: &Path,
    reference: &str,
    layout: super::SessionLayout,
) -> Result<Option<ResolvedSessionRecord>> {
    crate::session::metadata::resolve_session_record_with_layout(home, cwd, reference, layout)
        .await
        .map(|opt| opt.map(ResolvedSessionRecord::from))
        .map_err(anyhow::Error::from)
}

pub async fn ensure_metadata(paths: &SessionPaths, cwd: &Path, seed: SessionSeed) -> Result<()> {
    crate::session::metadata::ensure_metadata(paths, cwd, seed).await
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    #[test]
    fn conversation_reader_keeps_physical_sequence_across_blank_lines() {
        let dir = std::env::temp_dir().join(format!(
            "mink-reader-sequence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("conversation.jsonl"),
            "{\"role\":\"user\",\"content\":\"one\"}\n\n{\"role\":\"assistant\",\"content\":\"answer\"}\n",
        )
        .unwrap();

        let (rows, committed_sequence) = SessionReader::new(&dir)
            .conversation_with_sequence()
            .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["seq"], 1);
        assert_eq!(rows[1]["seq"], 3);
        assert_eq!(committed_sequence, 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subagent_catalog_is_a_public_reader_api_and_tolerates_legacy_events() {
        let dir = std::env::temp_dir().join(format!(
            "mink-subagent-catalog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            concat!(
                "{\"type\":\"sub_agent\",\"session_id\":\"sub_1\",\"status\":\"launched\",\"label\":\"review\",\"depth\":1}\n",
                "{\"type\":\"sub_agent\",\"session_id\":\"sub_1\",\"status\":\"ok\",\"stop_reason\":\"completed\",\"input_tokens\":4}\n",
                "{\"type\":\"sub_agent\",\"session_id\":\"sub_legacy\",\"status\":\"unknown\"}\n",
                "not json\n"
            ),
        )
        .unwrap();
        let catalog = SessionReader::new(&dir).subagent_catalog().unwrap();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].session_id, "sub_1");
        assert_eq!(catalog[0].label.as_deref(), Some("review"));
        assert_eq!(catalog[0].status, "ok");
        assert_eq!(catalog[0].stop_reason.as_deref(), Some("completed"));
        assert_eq!(catalog[1].session_id, "sub_legacy");
        assert_eq!(catalog[1].status, "unknown");
        assert!(
            SessionReader::new(dir.join("missing"))
                .subagent_catalog()
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn bounded_turn_pages_keep_whole_turns_and_the_newest_one() {
        let dir = std::env::temp_dir().join(format!(
            "mink-reader-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let body = "x".repeat(800);
        let data = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({"role":"user","content":"one"}),
            serde_json::json!({"role":"assistant","content":&body}),
            serde_json::json!({"role":"user","content":"two"}),
        ) + &format!(
            "{}\n",
            serde_json::json!({"role":"assistant","content":&body}),
        );
        std::fs::write(dir.join("conversation.jsonl"), data).unwrap();
        let reader = SessionReader::new(&dir);
        let page = reader
            .conversation_turns_bounded(0, 2, true, None, Some(900))
            .unwrap();
        assert_eq!(
            page.first().unwrap()["seq"],
            3,
            "the newest turn stays whole"
        );
        assert_eq!(page.last().unwrap()["role"], "assistant");
        assert_eq!(page.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bounded_forward_pages_keep_their_beginning() {
        let dir = std::env::temp_dir().join(format!(
            "mink-reader-forward-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let body = "x".repeat(800);
        let mut data = String::new();
        for turn in 1..=3 {
            data.push_str(&format!(
                "{}\n{}\n",
                serde_json::json!({"role":"user","content":format!("prompt {turn}")}),
                serde_json::json!({"role":"assistant","content":&body}),
            ));
        }
        std::fs::write(dir.join("conversation.jsonl"), data).unwrap();
        let page = SessionReader::new(&dir)
            .conversation_turns_bounded(0, 3, false, None, Some(900))
            .unwrap();
        assert_eq!(page.first().unwrap()["seq"], 1, "forward keeps the start");
        assert_eq!(page.last().unwrap()["seq"], 2);
        let _ = std::fs::remove_dir_all(dir);
    }

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

    #[tokio::test]
    async fn typed_projection_helpers_decode_ui_snapshot_and_ensure_title() {
        let dir = std::env::temp_dir().join(format!(
            "mink-reader-typed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session.json"),
            r#"{"id":"s1","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","cwd":"/tmp"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("conversation.jsonl"),
            concat!(
                "{\"role\":\"user\",\"content\":\"internal checkpoint\",\"internal\":true}\n",
                "{\"role\":\"user\",\"content\":\"ship typed session reader\"}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("plan.draft"), "1. design\n2. verify\n").unwrap();

        let reader = SessionReader::new(&dir);
        let snapshot = reader.read_ui_snapshot();
        assert_eq!(
            snapshot.plan_draft.as_deref(),
            Some("1. design\n2. verify\n")
        );
        assert!(snapshot.plan_active.is_none());

        let derived = reader.ensure_title().await.unwrap();
        assert_eq!(derived.as_deref(), Some("ship typed session reader"));
        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("session.json")).unwrap())
                .unwrap();
        assert_eq!(updated["title"], "ship typed session reader");

        let guidance_row = serde_json::json!({
            "role": "user",
            "content": "adjust scope",
            "_mink": { "guidance": true, "input_id": "in-1" }
        });
        assert_eq!(
            SessionReader::decode_applied_guidance(&guidance_row),
            Some(AppliedGuidanceEvent {
                input_id: "in-1".into(),
                text: "adjust scope".into(),
            })
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
#[path = "session_events_tests.rs"]
mod session_events_tests;
