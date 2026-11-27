use crate::tui::file_picker::{FilePickerPolicy, FilePickerState};
use crate::tui::notify::{TaskNotification, TaskNotificationKind};
use crate::tui::sanitize::sanitize_tui_text;
use crate::ui::StatsSnapshot;
use crate::ui::{
    ArtifactDisplay, GoalDisplay, PlanDisplay, TodoChangeDisplay, TodoDisplay, TodoStatusDisplay,
    ToolPresentation, ToolResultKind,
};
use ratatui::text::Line;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{Debug, Display as FmtDisplay, Formatter};
use std::path::PathBuf;
use std::time::Instant;

mod events;
mod input_state;
mod transcript;

pub(crate) use events::{ClipboardReader, TuiUiEvent, heartbeat_status_label};
use events::{set_todo_status, upsert_todo};
#[allow(unused_imports)]
pub(crate) use input_state::{
    InputState, PendingImage, clamp_char_boundary, compact_user_input_for_display,
    display_user_input, format_bytes, submitted_user_input,
};
#[allow(unused_imports)]
pub(crate) use transcript::{
    ArtifactDetail, CollapsePolicy, InlineSurfaceState, SubAgentDetail, SubAgentState,
    TranscriptItem, TranscriptKind, WorkState, goal_card_text,
};

#[derive(Clone, Default)]
pub(crate) struct ViewportState {
    pub anchor: Option<ReadingAnchor>,
    pub height: usize,
    pub content_x: u16,
    pub width: u16,
    pub scroll: usize,
    pub auto_scroll: bool,
    pub max_scroll: usize,
    pub click_map: Vec<ClickTarget>,
    pub content_y: u16,
}

#[derive(Clone, Debug)]
pub(crate) struct ReadingAnchor {
    pub id: u64,
    pub offset: usize,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ClickTarget {
    pub line_idx: usize,
    pub start_row: usize,
    pub end_row: usize,
    pub action: ClickAction,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ClickAction {
    ToggleCollapse,
    OpenPlan,
    OpenTodos,
    OpenGoal,
    OpenArtifact { id: String },
    OpenSubAgent { session_id: String },
}

#[derive(Clone, Default)]
pub(crate) struct RenderCache {
    pub width: u16,
    pub heights: crate::tui::height_index::HeightIndex,
    pub indices: HashMap<u64, usize>,
    pub pending: BTreeSet<usize>,
    pub rebuild_next: Option<usize>,
    pub start: usize,
    pub interactive: bool,
    pub visited: usize,
    pub parsed: usize,
    pub detail: Option<DetailLayout>,
    pub detail_parses: usize,
    pub stream_code_lines: Vec<Line<'static>>,
    pub stream_code_bytes: usize,
    pub stream_code_key: Option<(u8, usize, u16)>,
    pub stream_code_closed: bool,
    pub stream_parses: usize,
    pub stream_width: u16,
    pub stream_kind: TranscriptKind,
    pub stream_revision: u64,
    pub stream_lines: Option<Vec<Line<'static>>>,
}

#[derive(Clone)]
pub(crate) struct DetailLayout {
    pub identity: String,
    pub revision: u64,
    pub width: u16,
    pub lines: std::sync::Arc<Vec<Line<'static>>>,
}

#[derive(Clone, Default)]
pub(crate) struct TranscriptState {
    pub lines: Vec<TranscriptItem>,
    pub unsealed: HashSet<usize>,
    pub active_tools: HashMap<String, usize>,
    pub sub_agents: SubAgentState,
    pub inline: InlineSurfaceState,
    /// 流式期间收到的 Info 信号（如 llm_wait_heartbeat）。
    /// 不打断进行中的 markdown 流：先缓冲，待流结束时落为独立条目。
    pub pending_infos: Vec<String>,
}

#[derive(Clone, Default)]
pub(crate) struct StreamState {
    pub streaming: bool,
    pub active_turn_id: Option<String>,
    pub stopping: bool,
    pub work_state: WorkState,
    pub stream_boundary: crate::tui::stream_boundary::StreamBoundary,
    pub stream_filter: crate::ui::sanitize::ControlSequenceFilter,
    pub stream_line: String,
    pub stream_kind: TranscriptKind,
    pub stream_revision: u64,
    /// 流式期间的等待状态标签（心跳精简后，如 `·30s`），仅在状态栏瞬时展示。
    pub stream_status: Option<String>,
    pub last_interrupt: Option<Instant>,
    pub(crate) task_notification_armed: bool,
    pub(crate) pending_task_notification: Option<TaskNotification>,
}

#[derive(Clone)]
pub(crate) struct SessionMirrors {
    pub session_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    /// Paste staging directory (`<session_dir>/attachments`).
    pub attachments_dir: PathBuf,
    pub model: String,
    pub cwd_label: String,
    pub stats: StatsSnapshot,
    pub plan: Option<PlanDisplay>,
    pub todos: Option<TodoDisplay>,
    /// Read-only mirror of the authoritative goal snapshot. `None` when the
    /// session has no goal; `hidden` goals are dropped rather than shown.
    pub goal: Option<GoalDisplay>,
    /// Screen row of the status bar, and the column range its clickable goal
    /// marker occupies. Recorded during render so a click can open the goal.
    pub status_row: u16,
    pub status_goal_span: Option<(u16, u16)>,
    /// Frozen session image limits; `None` disables clipboard image paste.
    pub image_input: Option<crate::runtime::OpenAiChatImageUrlLimits>,
    pub artifact_detail: Option<ArtifactDetail>,
    pub restored_sub_detail: Option<(String, SubAgentDetail)>,
    pub detail_revision: u64,
    pub detail_height: usize,
    pub detail_max_scroll: usize,
}

impl Default for SessionMirrors {
    fn default() -> Self {
        Self {
            session_dir: PathBuf::new(),
            artifacts_dir: PathBuf::new(),
            attachments_dir: PathBuf::new(),
            model: "flash".into(),
            cwd_label: short_cwd_label(),
            stats: StatsSnapshot::default(),
            plan: None,
            todos: None,
            goal: None,
            status_row: 0,
            status_goal_span: None,
            image_input: None,
            artifact_detail: None,
            restored_sub_detail: None,
            detail_revision: 0,
            detail_height: 0,
            detail_max_scroll: 0,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct AsyncLoadState {
    pub background_generation: u64,
    pub loading_history: bool,
    pub picker_generation: u64,
    pub picker_worker: Option<std::sync::mpsc::Sender<crate::tui::file_picker::PickerRequest>>,
    pub artifact_generation: u64,
    pub sub_detail_generation: u64,
    /// When the in-flight clipboard read started. A read that never reports
    /// back (hung `osascript`) must not disable paste for the whole session.
    pub clipboard_started: Option<Instant>,
    /// Result channel of background clipboard and async UI work (absent in
    /// tests that do not exercise the async path).
    pub ui_tx: Option<tokio::sync::mpsc::UnboundedSender<TuiUiEvent>>,
    /// Injectable clipboard reader; `None` uses the platform implementation.
    pub clipboard_reader: Option<ClipboardReader>,
}

#[derive(Clone, Default)]
pub(crate) struct AdmissionState {
    pub runtime: Option<crate::tui::TuiRuntime>,
    pub input_subscription:
        Option<tokio::sync::watch::Receiver<std::sync::Arc<Vec<crate::runtime::InputReceipt>>>>,
    pub inbox_source: Option<std::sync::Arc<crate::runtime::InputInbox>>,
    pub edit_input: Option<(String, u64)>,
    pub admission_pending: bool,
    pub admission_cancelled: bool,
    pub failed_drafts: Vec<InputState>,
    pub inputs: Vec<crate::runtime::InputReceipt>,
    pub applied_inputs: HashSet<String>,
    pub input_notice: Option<String>,
}

#[derive(Clone)]
pub(crate) struct TuiState {
    pub transcript: TranscriptState,
    pub stream: StreamState,
    pub mirrors: SessionMirrors,
    pub async_load: AsyncLoadState,
    pub admission: AdmissionState,
    pub input: InputState,
    pub viewport: ViewportState,
    pub cache: RenderCache,
    pub view: View,
    pub overlay: Option<ActiveOverlay>,
    pub file_picker_policy: FilePickerPolicy,
    pub dirty: bool,
    pub quit: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) enum View {
    #[default]
    Main,
    SubAgentDetail {
        session_id: String,
        scroll: usize,
    },
    Plan {
        scroll: usize,
    },
    Goal {
        scroll: usize,
    },
    Todos {
        scroll: usize,
    },
    Artifact {
        scroll: usize,
    },
    Panel {
        panel: PanelKind,
        scroll: usize,
        selected: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelKind {
    Help,
    Status,
    Inputs,
    Details,
}

#[derive(Clone, Debug)]
pub(crate) enum ActiveOverlay {
    FilePicker(FilePickerState),
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            transcript: TranscriptState::default(),
            stream: StreamState::default(),
            mirrors: SessionMirrors::default(),
            async_load: AsyncLoadState::default(),
            admission: AdmissionState::default(),
            input: InputState::default(),
            viewport: ViewportState {
                auto_scroll: true,
                ..Default::default()
            },
            cache: RenderCache::default(),
            view: View::Main,
            overlay: None,
            file_picker_policy: FilePickerPolicy::default(),
            dirty: true,
            quit: false,
        }
    }
}

pub(crate) fn short_cwd_label() -> String {
    let Some(cwd) = std::env::current_dir().ok() else {
        return "?".into();
    };

    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from)
        && cwd == home
    {
        return "~".into();
    }

    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("/")
        .to_string()
}

impl Debug for TuiState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TuiState[lines={}, stream={}, input={}]",
            self.transcript.lines.len(),
            self.stream.stream_line.len(),
            self.input.buf.len()
        )
    }
}

impl FmtDisplay for TuiState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self, f)
    }
}

impl TuiState {
    pub(crate) fn invalidate_all_cache(&mut self) {
        self.cache.heights = crate::tui::height_index::HeightIndex::default();
        self.cache.indices.clear();
        self.cache.pending.clear();
        self.cache.rebuild_next = Some(0);
        self.cache.detail = None;
    }

    pub(crate) fn invalidate_item(&mut self, index: usize) {
        self.invalidate_detail_resource("panel:Details");
        self.cache.pending.insert(index);
    }
    pub(crate) fn detail_changed(&mut self) {
        self.mirrors.detail_revision = self.mirrors.detail_revision.wrapping_add(1);
    }
    pub(crate) fn invalidate_detail_resource(&mut self, identity: &str) {
        if self.cache.detail.as_ref().is_some_and(|cache| {
            cache.identity == identity
                || cache
                    .identity
                    .strip_prefix(identity)
                    .is_some_and(|suffix| identity.ends_with(':') || suffix.starts_with(':'))
        }) {
            self.detail_changed();
        }
    }
    pub(crate) fn invalidate_stream_cache(&mut self) {
        self.cache.stream_lines = None;
        self.cache.stream_code_lines.clear();
        self.cache.stream_code_bytes = 0;
        self.cache.stream_code_key = None;
        self.cache.stream_code_closed = false;
    }

    pub(crate) fn push_line(&mut self, line: TranscriptItem) -> usize {
        self.invalidate_detail_resource("panel:Details");
        let idx = self.transcript.lines.len();
        if !line.sealed {
            self.transcript.unsealed.insert(idx);
        }
        self.transcript.lines.push(line);
        self.cache.pending.insert(idx);
        idx
    }

    pub(crate) fn save_stream(&mut self) {
        self.stream.stream_boundary = crate::tui::stream_boundary::StreamBoundary::default();
        self.stream.stream_filter.reset();
        let text = std::mem::take(&mut self.stream.stream_line);
        if !text.is_empty() {
            self.push_line(TranscriptItem::new(text, self.stream.stream_kind));
        }
        self.stream.stream_revision = self.stream.stream_revision.wrapping_add(1);
        self.invalidate_stream_cache();
    }

    pub(crate) fn finalize_stream(&mut self) {
        self.save_stream();
        self.stream.streaming = false;
        // 流式期间缓冲的 Info（非心跳告警等）在流结束后落盘：此时文本已完整，
        // 不会被切成两段重新按 markdown 解析（围栏上下文不再丢失）。
        self.stream.stream_status = None;
        let pending = std::mem::take(&mut self.transcript.pending_infos);
        for info in pending {
            self.push_line(TranscriptItem::new(info, TranscriptKind::Info));
        }
    }

    pub(crate) fn promote_stable_stream_prefix(&mut self) {
        if self.stream.stream_kind != TranscriptKind::StreamText
            || self.stream.stream_line.is_empty()
        {
            return;
        }
        let end = self.stream.stream_boundary.scan(&self.stream.stream_line);
        if end == 0 {
            return;
        }
        let remaining = self.stream.stream_line.split_off(end);
        let stable = std::mem::replace(&mut self.stream.stream_line, remaining);
        if !stable.is_empty() {
            self.push_line(TranscriptItem::new(stable, TranscriptKind::StreamText));
        }
        self.stream.stream_revision = self.stream.stream_revision.wrapping_add(1);
        self.invalidate_stream_cache();
    }

    pub(crate) fn seal_incomplete_transcript(&mut self, reason: &str) {
        let mut changed = false;
        for index in std::mem::take(&mut self.transcript.unsealed) {
            if index < self.transcript.inline.committed {
                continue;
            }
            let Some(item) = self
                .transcript
                .lines
                .get_mut(index)
                .filter(|item| !item.sealed)
            else {
                continue;
            };
            item.sealed = true;
            if item.kind == TranscriptKind::Tool {
                if item.text.is_empty() || item.text.starts_with("[tool]") {
                    item.text = reason.to_string();
                }
            } else if !reason.is_empty() {
                item.text.push('\n');
                item.text.push_str(reason);
            }
            item.invalidate_cache();
            self.cache.pending.insert(index);
            changed = true;
        }
        if changed {
            self.transcript.sub_agents.active_sessions.clear();
            self.transcript.active_tools.clear();
        }
    }

    pub(crate) fn apply_todo_presentation(&mut self, update: &TodoDisplay) {
        self.invalidate_detail_resource("todos");
        self.invalidate_detail_resource("panel:Status");
        if update.changes.is_empty() {
            self.mirrors.todos = Some(update.clone());
            return;
        }

        let mut current = self.mirrors.todos.take().unwrap_or_else(|| TodoDisplay {
            revision: update.revision,
            counts: update.counts.clone(),
            items: Vec::new(),
            changes: Vec::new(),
        });
        for change in &update.changes {
            match change {
                TodoChangeDisplay::Added { item } => upsert_todo(&mut current.items, item.clone()),
                TodoChangeDisplay::Updated { id, content } => {
                    if let Some(item) = current.items.iter_mut().find(|item| item.id == *id) {
                        item.content.clone_from(content);
                    }
                }
                TodoChangeDisplay::Removed { id } => {
                    current.items.retain(|item| item.id != *id);
                }
                TodoChangeDisplay::Completed { id } => {
                    set_todo_status(&mut current.items, id, TodoStatusDisplay::Completed);
                }
                TodoChangeDisplay::Activated { id } => {
                    set_todo_status(&mut current.items, id, TodoStatusDisplay::InProgress);
                }
                TodoChangeDisplay::Paused { id } | TodoChangeDisplay::Reopened { id } => {
                    set_todo_status(&mut current.items, id, TodoStatusDisplay::Pending);
                }
            }
        }
        for item in &update.items {
            upsert_todo(&mut current.items, item.clone());
        }
        current.revision = update.revision;
        current.counts = update.counts.clone();
        current.changes.clone_from(&update.changes);
        self.mirrors.todos = Some(current);
    }
}
