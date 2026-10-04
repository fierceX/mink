use crate::tui::file_picker::{FilePickerPolicy, FilePickerState};
use crate::tui::notify::{TaskNotification, TaskNotificationKind};
use crate::tui::sanitize::sanitize_tui_text;
use crate::ui::StatsSnapshot;
use crate::ui::{
    ArtifactDisplay, PlanDisplay, TodoChangeDisplay, TodoDisplay, TodoStatusDisplay,
    ToolPresentation, ToolResultKind,
};
use ratatui::text::Line;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{Debug, Display as FmtDisplay, Formatter};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum TranscriptKind {
    #[default]
    Text,
    Tool,
    Error,
    Info,
    SubAgent,
    StreamThinking,
    StreamText,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum WorkState {
    #[default]
    Idle,
    WaitingModel,
    StreamingThinking,
    StreamingText,
    RunningTool,
    RunningSubAgent,
    Compacting,
    Error,
}

impl WorkState {
    pub(crate) fn is_working(self) -> bool {
        !matches!(self, WorkState::Idle | WorkState::Error)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            WorkState::Idle => "idle",
            WorkState::WaitingModel => "waiting",
            WorkState::StreamingThinking => "thinking",
            WorkState::StreamingText => "generating",
            WorkState::RunningTool => "tool",
            WorkState::RunningSubAgent => "sub-agent",
            WorkState::Compacting => "compacting",
            WorkState::Error => "error",
        }
    }
}

#[derive(Clone)]
pub(crate) struct SubAgentDetail {
    pub thinking: String,
    pub text: String,
}

#[derive(Clone)]
pub(crate) struct ArtifactDetail {
    pub id: String,
    pub content: String,
    pub truncated: bool,
}

#[derive(Clone)]
pub(crate) struct TranscriptItem {
    pub id: u64,
    pub revision: u64,
    pub cached_offsets: Vec<usize>,
    pub text: String,
    pub kind: TranscriptKind,
    pub collapsed: bool,
    pub collapse_policy: CollapsePolicy,
    pub collapse_overridden: bool,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_summary: Option<String>,
    pub tool_status: Option<crate::runtime::ToolStatus>,
    pub tool_exit_code: Option<i32>,
    pub tool_result_kind: Option<ToolResultKind>,
    pub presentation: Option<ToolPresentation>,
    pub artifacts: Vec<ArtifactDisplay>,
    pub sealed: bool,
    pub cached_lines: Option<Vec<Line<'static>>>,
    pub cached_width: u16,
    pub cached_collapsed: bool,
    pub cached_interactive: bool,
    pub sub_detail: Option<SubAgentDetail>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CollapsePolicy {
    Never,
    Always,
    Auto { threshold_lines: usize },
}

impl CollapsePolicy {
    const LARGE_AUTO_COLLAPSE_BYTES: usize = 4096;
    const LARGE_AUTO_COLLAPSE_LINE_WIDTH: usize = 240;

    fn for_kind(kind: TranscriptKind) -> Self {
        match kind {
            TranscriptKind::StreamThinking => CollapsePolicy::Always,
            TranscriptKind::Tool => CollapsePolicy::Auto {
                threshold_lines: 20,
            },
            _ => CollapsePolicy::Never,
        }
    }

    fn initial_collapsed(self, text: &str) -> bool {
        match self {
            CollapsePolicy::Never => false,
            CollapsePolicy::Always => true,
            CollapsePolicy::Auto { threshold_lines } => {
                text.lines().count() > threshold_lines
                    || text.len() > Self::LARGE_AUTO_COLLAPSE_BYTES
                    || text.lines().any(|line| {
                        unicode_width::UnicodeWidthStr::width(line)
                            > Self::LARGE_AUTO_COLLAPSE_LINE_WIDTH
                    })
            }
        }
    }

    pub(crate) fn should_collapse_rendered(self, rendered_lines: usize) -> bool {
        match self {
            CollapsePolicy::Auto { threshold_lines } => rendered_lines > threshold_lines,
            CollapsePolicy::Always => true,
            CollapsePolicy::Never => false,
        }
    }
}

impl TranscriptItem {
    pub(crate) fn cache_valid(&self, interactive: bool) -> bool {
        self.cached_lines.is_some()
            && self.cached_collapsed == self.collapsed
            && self.cached_interactive == interactive
    }

    pub(crate) fn new(text: String, kind: TranscriptKind) -> Self {
        let text = sanitize_tui_text(&text);
        let collapse_policy = CollapsePolicy::for_kind(kind);
        let collapsed = collapse_policy.initial_collapsed(&text);
        TranscriptItem {
            id: next_item_id(),
            revision: 0,
            cached_offsets: Vec::new(),
            text,
            kind,
            collapsed,
            collapse_policy,
            collapse_overridden: false,
            tool_name: None,
            tool_use_id: None,
            tool_summary: None,
            tool_status: None,
            tool_exit_code: None,
            tool_result_kind: None,
            presentation: None,
            artifacts: Vec::new(),
            sealed: true,
            cached_lines: None,
            cached_width: 0,
            cached_collapsed: collapsed,
            cached_interactive: false,
            sub_detail: None,
        }
    }

    pub(crate) fn new_tool_result(tool_name: String, text: String) -> Self {
        let mut line = Self::new(text, TranscriptKind::Tool);
        line.tool_name = Some(tool_name);
        line
    }

    pub(crate) fn new_tool_call(
        tool_use_id: Option<String>,
        tool_name: String,
        summary: String,
    ) -> Self {
        let text = if summary.is_empty() {
            format!("[tool] {tool_name}")
        } else {
            format!("[tool] {summary}")
        };
        let mut item = Self::new(text, TranscriptKind::Tool);
        item.tool_use_id = tool_use_id;
        item.tool_name = Some(tool_name);
        item.tool_summary = Some(summary);
        item.sealed = false;
        item
    }

    pub(crate) fn is_collapsible(&self) -> bool {
        self.collapse_policy != CollapsePolicy::Never
    }

    pub(crate) fn with_sub_detail(mut self, sub_detail: Option<SubAgentDetail>) -> Self {
        self.sub_detail = sub_detail;
        self
    }

    pub(crate) fn invalidate_cache(&mut self) {
        self.cached_lines = None;
        self.cached_offsets.clear();
        self.revision = self.revision.wrapping_add(1);
    }

    pub(crate) fn toggle_collapsed(&mut self) {
        self.collapsed = !self.collapsed;
        self.collapse_overridden = true;
        self.invalidate_cache();
    }
}

impl Default for TranscriptItem {
    fn default() -> Self {
        TranscriptItem {
            id: next_item_id(),
            revision: 0,
            cached_offsets: Vec::new(),
            text: String::new(),
            kind: TranscriptKind::Text,
            collapsed: false,
            collapse_policy: CollapsePolicy::Never,
            collapse_overridden: false,
            tool_name: None,
            tool_use_id: None,
            tool_summary: None,
            tool_status: None,
            tool_exit_code: None,
            tool_result_kind: None,
            presentation: None,
            artifacts: Vec::new(),
            sealed: true,
            cached_lines: None,
            cached_width: 0,
            cached_collapsed: false,
            cached_interactive: false,
            sub_detail: None,
        }
    }
}

/// One clipboard image staged for the next user message. The bytes live in
/// `<session_dir>/attachments/<sha256>.png`; the message only carries the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingImage {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

impl PendingImage {
    /// Stable marker appended to the submitted user text. The model is asked
    /// to `Read` the absolute path; the capture itself still runs through the
    /// v7 image pipeline.
    pub(crate) fn marker(&self) -> String {
        format!(
            "[Attached image: \"{}\" - Read it to view.]",
            self.path.display()
        )
    }

    pub(crate) fn chip(&self, index: usize) -> String {
        format!(
            "[image #{index} {}x{} {}]",
            self.width,
            self.height,
            format_bytes(self.bytes)
        )
    }
}

pub(crate) fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        scaled(value / MB, "MB")
    } else if value >= KB {
        scaled(value / KB, "KB")
    } else {
        format!("{bytes}B")
    }
}

/// At most one decimal: `220KB` instead of `220.0KB`, `1.5KB` keeps its digit.
fn scaled(value: f64, unit: &str) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if (rounded - rounded.trunc()).abs() < f64::EPSILON {
        format!("{rounded:.0}{unit}")
    } else {
        format!("{rounded:.1}{unit}")
    }
}

/// Result of a background clipboard read, delivered back to the TUI loop.
pub(crate) enum TuiUiEvent {
    ImageCaptured(PendingImage),
    ClipboardFailed(String),
    FilePickerReady {
        generation: u64,
        picker: FilePickerState,
    },
    ArtifactReady {
        generation: u64,
        id: String,
        result: Result<ArtifactDetail, String>,
    },
    HistoryLoaded {
        generation: u64,
        state: Box<TuiState>,
    },
    Admission(Box<crate::tui::inbox::AdmissionResult>),
    SubAgentReady {
        generation: u64,
        id: String,
        result: Result<SubAgentDetail, String>,
    },
}
impl Debug for TuiUiEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ImageCaptured(_) => "ImageCaptured",
            Self::ClipboardFailed(_) => "ClipboardFailed",
            Self::FilePickerReady { .. } => "FilePickerReady",
            Self::ArtifactReady { .. } => "ArtifactReady",
            Self::HistoryLoaded { .. } => "HistoryLoaded",
            Self::Admission(_) => "Admission",
            Self::SubAgentReady { .. } => "SubAgentReady",
        })
    }
}

/// Injectable clipboard reader (tests); production uses the platform impl.
pub(crate) type ClipboardReader = std::sync::Arc<
    dyn Fn(
            &std::path::Path,
            &crate::runtime::OpenAiChatImageUrlLimits,
        ) -> anyhow::Result<crate::tui::clipboard::ClipboardPng>
        + Send
        + Sync,
>;

/// Submitted text for the runtime: the typed text plus one marker per image.
pub(crate) fn submitted_user_input(typed: &str, images: &[PendingImage]) -> String {
    if images.is_empty() {
        return typed.to_string();
    }
    let markers = images
        .iter()
        .map(PendingImage::marker)
        .collect::<Vec<_>>()
        .join("\n");
    if typed.is_empty() {
        markers
    } else {
        format!("{typed}\n\n{markers}")
    }
}

/// Transcript echo for one user message: images collapse to `[image #N]`.
pub(crate) fn display_user_input(typed: &str, images: &[PendingImage]) -> String {
    let prefix = (1..=images.len())
        .map(|index| format!("[image #{index}]"))
        .collect::<Vec<_>>()
        .join(" ");
    match (typed.is_empty(), prefix.is_empty()) {
        (true, _) => prefix,
        (false, true) => typed.to_string(),
        (false, false) => format!("{prefix} {typed}"),
    }
}

/// Compact paste markers in replayed user input back to `[image]` so a resumed
/// session does not echo absolute attachment paths.
pub(crate) fn compact_user_input_for_display(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            if line.starts_with("[Attached image: \"") && line.ends_with("\" - Read it to view.]") {
                "[image]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Default)]
pub(crate) struct InputState {
    pub revision: u64,
    pub layout: Option<std::sync::Arc<super::editor::InputLayout>>,
    pub preferred_column: Option<usize>,
    pub undo: Vec<super::editor::DraftEdit>,
    pub redo: Vec<super::editor::DraftEdit>,
    pub buf: String,
    pub cursor: usize,
    pub scroll_row: usize,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub draft_before_history: Option<String>,
    /// Clipboard images queued for the next submitted message.
    pub pending_images: Vec<PendingImage>,
}

impl InputState {
    pub(crate) fn clamped_cursor(&self) -> usize {
        if let Some(layout) = &self.layout
            && layout.revision == self.revision
            && layout.text == self.buf
        {
            layout.cursor(self.cursor.min(self.buf.len())).byte
        } else {
            super::editor::clamp(&self.buf, self.cursor)
        }
    }

    pub(crate) fn draft_snapshot(&self) -> Self {
        Self {
            revision: self.revision,
            buf: self.buf.clone(),
            cursor: self.cursor,
            pending_images: self.pending_images.clone(),
            ..Default::default()
        }
    }
    pub(crate) fn layout(&mut self, width: usize) -> std::sync::Arc<super::editor::InputLayout> {
        let valid = self.layout.as_ref().is_some_and(|layout| {
            layout.revision == self.revision && layout.width == width && layout.text == self.buf
        });
        if !valid {
            self.layout = Some(std::sync::Arc::new(super::editor::InputLayout::new(
                &self.buf,
                self.revision,
                width,
            )));
        }
        self.layout.as_ref().expect("input layout").clone()
    }
    pub(crate) fn undo_edit(&mut self, redo: bool) {
        let edit = if redo {
            self.redo.pop()
        } else {
            self.undo.pop()
        };
        if let Some(edit) = edit {
            let current = super::editor::DraftEdit {
                text: std::mem::replace(&mut self.buf, edit.text),
                cursor: self.cursor,
            };
            if redo {
                self.undo.push(current);
            } else {
                self.redo.push(current);
            }
            self.cursor = edit.cursor;
            self.preferred_column = None;
        }
    }
    pub(crate) fn clamp_cursor(&mut self) {
        self.cursor = self.clamped_cursor();
    }
}

pub(crate) fn clamp_char_boundary(s: &str, pos: usize) -> usize {
    let mut pos = pos.min(s.len());
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

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
    OpenArtifact { id: String },
    OpenSubAgent { session_id: String },
}

#[derive(Clone, Default)]
pub(crate) struct RenderCache {
    pub width: u16,
    pub heights: super::height_index::HeightIndex,
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
pub(crate) struct SubAgentState {
    pub active_sessions: HashSet<String>,
    pub line_by_session: HashMap<String, usize>,
}

impl SubAgentState {
    pub(crate) fn session_for_line(&self, line_idx: usize) -> Option<&str> {
        self.line_by_session
            .iter()
            .find_map(|(session_id, &idx)| (idx == line_idx).then_some(session_id.as_str()))
    }
}

#[derive(Clone, Default)]
pub(crate) struct InlineSurfaceState {
    pub committed: usize,
    pub row_offset: usize,
}

#[derive(Clone)]
pub(crate) struct TuiState {
    pub input_subscription:
        Option<tokio::sync::watch::Receiver<std::sync::Arc<Vec<crate::runtime::InputReceipt>>>>,
    pub inbox_source: Option<std::sync::Arc<crate::runtime::InputInbox>>,
    pub edit_input: Option<(String, u64)>,
    pub admission_pending: bool,
    pub failed_drafts: Vec<InputState>,
    pub session_dir: PathBuf,
    pub sub_detail_generation: u64,
    pub restored_sub_detail: Option<(String, SubAgentDetail)>,
    pub background_generation: u64,
    pub picker_generation: u64,
    pub picker_worker: Option<std::sync::mpsc::Sender<super::file_picker::PickerRequest>>,
    pub admission_cancelled: bool,
    pub artifact_generation: u64,
    pub loading_history: bool,
    pub runtime: Option<crate::tui::TuiRuntime>,
    pub active_tools: HashMap<String, usize>,
    pub unsealed: HashSet<usize>,
    pub active_turn_id: Option<String>,
    pub stopping: bool,
    pub inputs: Vec<crate::runtime::InputReceipt>,
    pub applied_inputs: HashSet<String>,
    pub input_notice: Option<String>,
    pub lines: Vec<TranscriptItem>,
    pub inline: InlineSurfaceState,
    pub stream_boundary: super::stream_boundary::StreamBoundary,
    pub stream_line: String,
    pub stream_kind: TranscriptKind,
    pub streaming: bool,
    pub input: InputState,
    pub model: String,
    pub cwd_label: String,
    pub stats: StatsSnapshot,
    pub viewport: ViewportState,
    pub dirty: bool,
    pub stream_revision: u64,
    pub detail_revision: u64,
    pub detail_height: usize,
    pub detail_max_scroll: usize,
    pub cache: RenderCache,
    pub quit: bool,
    pub last_interrupt: Option<Instant>,
    pub work_state: WorkState,
    pub sub_agents: SubAgentState,
    pub plan: Option<PlanDisplay>,
    pub todos: Option<TodoDisplay>,
    pub artifacts_dir: PathBuf,
    /// Paste staging directory (`<session_dir>/attachments`).
    pub attachments_dir: PathBuf,
    /// Frozen session image limits; `None` disables clipboard image paste.
    pub image_input: Option<crate::runtime::OpenAiChatImageUrlLimits>,
    /// Result channel of background clipboard reads (absent in tests that do
    /// not exercise the async path).
    pub ui_tx: Option<std::sync::mpsc::Sender<TuiUiEvent>>,
    /// When the in-flight clipboard read started. A read that never reports
    /// back (hung `osascript`) must not disable paste for the whole session.
    pub clipboard_started: Option<Instant>,
    /// Injectable clipboard reader; `None` uses the platform implementation.
    pub clipboard_reader: Option<ClipboardReader>,
    pub artifact_detail: Option<ArtifactDetail>,
    /// 流式期间收到的 Info 信号（如 llm_wait_heartbeat）。
    /// 不打断进行中的 markdown 流：先缓冲，待流结束时落为独立条目。
    pub pending_infos: Vec<String>,
    /// 流式期间的等待状态标签（心跳精简后，如 `·30s`），仅在状态栏瞬时展示。
    pub stream_status: Option<String>,
    /// 中断当前任务（由 Ctrl+C 触发），None 表示无中断能力
    pub view: View,
    pub overlay: Option<ActiveOverlay>,
    pub file_picker_policy: FilePickerPolicy,
    pub(crate) task_notification_armed: bool,
    pub(crate) pending_task_notification: Option<TaskNotification>,
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
            input_subscription: None,
            inbox_source: None,
            edit_input: None,
            admission_pending: false,
            failed_drafts: Vec::new(),
            session_dir: PathBuf::new(),
            sub_detail_generation: 0,
            restored_sub_detail: None,
            background_generation: 0,
            picker_generation: 0,
            picker_worker: None,
            admission_cancelled: false,
            artifact_generation: 0,
            loading_history: false,
            runtime: None,
            active_tools: HashMap::new(),
            unsealed: HashSet::new(),
            active_turn_id: None,
            stopping: false,
            inputs: Vec::new(),
            applied_inputs: HashSet::new(),
            input_notice: None,
            lines: Vec::new(),
            inline: InlineSurfaceState::default(),
            stream_boundary: super::stream_boundary::StreamBoundary::default(),
            stream_line: String::new(),
            stream_kind: TranscriptKind::default(),
            streaming: false,
            input: InputState::default(),
            model: "flash".into(),
            cwd_label: short_cwd_label(),
            stats: StatsSnapshot::default(),
            viewport: ViewportState {
                auto_scroll: true,
                ..Default::default()
            },
            dirty: true,
            stream_revision: 0,
            detail_revision: 0,
            detail_height: 0,
            detail_max_scroll: 0,
            cache: RenderCache::default(),
            quit: false,
            last_interrupt: None,
            work_state: WorkState::Idle,
            sub_agents: SubAgentState::default(),
            plan: None,
            todos: None,
            artifacts_dir: PathBuf::new(),
            attachments_dir: PathBuf::new(),
            image_input: None,
            ui_tx: None,
            clipboard_started: None,
            clipboard_reader: None,
            artifact_detail: None,
            pending_infos: Vec::new(),
            stream_status: None,
            view: View::Main,
            overlay: None,
            file_picker_policy: FilePickerPolicy::default(),
            task_notification_armed: false,
            pending_task_notification: None,
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
            self.lines.len(),
            self.stream_line.len(),
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
        self.cache.heights = super::height_index::HeightIndex::default();
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
        self.detail_revision = self.detail_revision.wrapping_add(1);
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
        let idx = self.lines.len();
        if !line.sealed {
            self.unsealed.insert(idx);
        }
        self.lines.push(line);
        self.cache.pending.insert(idx);
        idx
    }

    pub(crate) fn save_stream(&mut self) {
        self.stream_boundary = super::stream_boundary::StreamBoundary::default();
        let text = std::mem::take(&mut self.stream_line);
        if !text.is_empty() {
            self.push_line(TranscriptItem::new(text, self.stream_kind));
        }
        self.stream_revision = self.stream_revision.wrapping_add(1);
        self.invalidate_stream_cache();
    }

    pub(crate) fn finalize_stream(&mut self) {
        self.save_stream();
        self.streaming = false;
        // 流式期间缓冲的 Info（非心跳告警等）在流结束后落盘：此时文本已完整，
        // 不会被切成两段重新按 markdown 解析（围栏上下文不再丢失）。
        self.stream_status = None;
        let pending = std::mem::take(&mut self.pending_infos);
        for info in pending {
            self.push_line(TranscriptItem::new(info, TranscriptKind::Info));
        }
    }

    pub(crate) fn promote_stable_stream_prefix(&mut self) {
        if self.stream_kind != TranscriptKind::StreamText || self.stream_line.is_empty() {
            return;
        }
        let end = self.stream_boundary.scan(&self.stream_line);
        if end == 0 {
            return;
        }
        let remaining = self.stream_line.split_off(end);
        let stable = std::mem::replace(&mut self.stream_line, remaining);
        if !stable.is_empty() {
            self.push_line(TranscriptItem::new(stable, TranscriptKind::StreamText));
        }
        self.stream_revision = self.stream_revision.wrapping_add(1);
        self.invalidate_stream_cache();
    }

    pub(crate) fn seal_incomplete_transcript(&mut self, reason: &str) {
        let mut changed = false;
        for index in std::mem::take(&mut self.unsealed) {
            if index < self.inline.committed {
                continue;
            }
            let Some(item) = self.lines.get_mut(index).filter(|item| !item.sealed) else {
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
            self.sub_agents.active_sessions.clear();
            self.active_tools.clear();
        }
    }

    pub(crate) fn apply_todo_presentation(&mut self, update: &TodoDisplay) {
        self.invalidate_detail_resource("todos");
        self.invalidate_detail_resource("panel:Status");
        if update.changes.is_empty() {
            self.todos = Some(update.clone());
            return;
        }

        let mut current = self.todos.take().unwrap_or_else(|| TodoDisplay {
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
        self.todos = Some(current);
    }

    /// Apply one background clipboard result. Failed reads are reported once;
    /// a captured image only grows the pending list (the chip row is the
    /// visible feedback).
    pub(crate) fn apply_ui_event(&mut self, event: TuiUiEvent) {
        match event {
            TuiUiEvent::ImageCaptured(image) => {
                self.clipboard_started = None;
                // The same bytes stage to the same content-addressed path: a
                // duplicate paste would make the model receive one picture
                // twice (double vision tokens) for no benefit.
                if self
                    .input
                    .pending_images
                    .iter()
                    .any(|pending| pending.path == image.path)
                {
                    // No path in the notice: the transcript would leak the
                    // absolute session path and wrap over several rows.
                    self.push_line(TranscriptItem::new(
                        "Image already queued (same content).".into(),
                        TranscriptKind::Info,
                    ));
                } else {
                    self.input.pending_images.push(image);
                    self.input.revision = self.input.revision.wrapping_add(1);
                }
            }
            TuiUiEvent::ClipboardFailed(message) => {
                self.clipboard_started = None;
                self.push_line(TranscriptItem::new(
                    format!("Clipboard image unavailable: {message}"),
                    TranscriptKind::Error,
                ));
            }
            TuiUiEvent::FilePickerReady { generation, picker } => {
                if generation == self.picker_generation && self.overlay.is_some() {
                    self.overlay = Some(ActiveOverlay::FilePicker(picker));
                }
            }
            TuiUiEvent::ArtifactReady {
                generation,
                id,
                result,
            } => {
                if generation == self.artifact_generation
                    && matches!(self.view, View::Artifact { .. })
                {
                    self.invalidate_detail_resource("artifact:");
                    match result {
                        Ok(detail) => self.artifact_detail = Some(detail),
                        Err(error) => {
                            self.artifact_detail = Some(ArtifactDetail {
                                id,
                                content: error,
                                truncated: false,
                            });
                        }
                    }
                }
            }
            TuiUiEvent::HistoryLoaded { generation, state } => {
                if generation == self.background_generation && self.loading_history {
                    let mut history = state.lines;
                    let offset = history.len();
                    history.append(&mut self.lines);
                    self.lines = history;
                    for index in self.sub_agents.line_by_session.values_mut() {
                        *index += offset;
                    }
                    for index in self.active_tools.values_mut() {
                        *index += offset;
                    }
                    self.unsealed = self.unsealed.iter().map(|index| *index + offset).collect();
                    if self.plan.is_none() {
                        self.plan = state.plan;
                    }
                    if self.todos.is_none() {
                        self.todos = state.todos;
                    }
                    self.loading_history = false;
                    self.invalidate_all_cache();
                }
            }
            TuiUiEvent::Admission(result) => self.finish_admission(*result),
            TuiUiEvent::SubAgentReady {
                generation,
                id,
                result,
            } => {
                if generation == self.sub_detail_generation
                    && matches!(&self.view, View::SubAgentDetail { session_id, .. } if session_id == &id)
                {
                    self.invalidate_detail_resource(&format!("sub:{id}"));
                    self.restored_sub_detail = Some((
                        id,
                        result.unwrap_or_else(|error| SubAgentDetail {
                            thinking: String::new(),
                            text: sanitize_tui_text(&error),
                        }),
                    ));
                }
            }
        }
    }

    pub(crate) fn arm_task_notification(&mut self) {
        self.task_notification_armed = true;
        self.pending_task_notification = None;
    }

    pub(crate) fn finish_task_notification(&mut self, kind: TaskNotificationKind) {
        if !self.task_notification_armed {
            return;
        }
        self.task_notification_armed = false;
        self.pending_task_notification = Some(TaskNotification::new(kind, &self.model));
    }

    pub(crate) fn take_task_notification(&mut self) -> Option<TaskNotification> {
        self.pending_task_notification.take()
    }

    pub(crate) fn add_help(&mut self) {
        self.view = View::Panel {
            panel: PanelKind::Help,
            scroll: 0,
            selected: 0,
        };
    }
    pub(crate) fn show_skills(&mut self) {
        self.push_line(TranscriptItem::new(
            "=== Skills ===".into(),
            TranscriptKind::Info,
        ));
        match crate::local::discoverable_skill_lines() {
            Ok(lines) => {
                for line in lines {
                    self.push_line(TranscriptItem::new(line, TranscriptKind::Text));
                }
            }
            Err(error) => {
                self.push_line(TranscriptItem::new(
                    format!("Error loading skills: {error}"),
                    TranscriptKind::Error,
                ));
            }
        }
        self.push_line(TranscriptItem::new(
            crate::local::SKILL_LOAD_HINT.into(),
            TranscriptKind::Info,
        ));
    }

    pub(crate) fn open_sub_agent(&mut self, id: &str) {
        self.invalidate_detail_resource(&format!("sub:{id}"));
        self.view = View::SubAgentDetail {
            session_id: id.to_string(),
            scroll: 0,
        };
        self.sub_detail_generation = self.sub_detail_generation.wrapping_add(1);
        let available = self
            .sub_agents
            .line_by_session
            .get(id)
            .and_then(|index| self.lines.get(*index))
            .is_some_and(|item| item.sub_detail.is_some());
        if available {
            return;
        }
        let Some(tx) = self.ui_tx.clone() else { return };
        let reader = crate::runtime::session::SessionReader::new(&self.session_dir);
        let id = id.to_string();
        let generation = self.sub_detail_generation;
        self.restored_sub_detail = Some((
            id.clone(),
            SubAgentDetail {
                thinking: String::new(),
                text: "Loading…".into(),
            },
        ));
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<SubAgentDetail> {
                let rows = reader
                    .sub_agent(&id)?
                    .conversation_turns(1, 1, true, None)?;
                let mut detail = SubAgentDetail {
                    thinking: String::new(),
                    text: String::new(),
                };
                for row in rows {
                    if row["role"] != "assistant" {
                        continue;
                    }
                    if let Some(blocks) = row["content"].as_array() {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("text") => {
                                    if let Some(text) = block["text"].as_str() {
                                        detail.text.push_str(text);
                                    }
                                }
                                Some("thinking") => {
                                    if let Some(text) = block["thinking"].as_str() {
                                        detail.thinking.push_str(text);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                detail.text = sanitize_tui_text(&detail.text);
                detail.thinking = sanitize_tui_text(&detail.thinking);
                Ok(detail)
            })()
            .map_err(|error| format!("Cannot load sub-agent {id}: {error}"));
            let _ = tx.send(TuiUiEvent::SubAgentReady {
                generation,
                id,
                result,
            });
        });
    }

    pub(crate) fn open_artifact(&mut self, id: &str) {
        self.invalidate_detail_resource("artifact:");
        const MAX_ARTIFACT_DETAIL_BYTES: usize = 256 * 1024;
        let manager = crate::session::artifacts::ArtifactManager::new(self.artifacts_dir.clone());
        if let Some(tx) = self.ui_tx.clone() {
            self.artifact_generation = self.artifact_generation.wrapping_add(1);
            let generation = self.artifact_generation;
            let id = id.to_string();
            self.artifact_detail = Some(ArtifactDetail {
                id: id.clone(),
                content: "Loading…".into(),
                truncated: false,
            });
            self.view = View::Artifact { scroll: 0 };
            std::thread::spawn(move || {
                let result = manager
                    .read_text_prefix(&id, MAX_ARTIFACT_DETAIL_BYTES)
                    .map(|(content, truncated)| ArtifactDetail {
                        id: id.clone(),
                        content: sanitize_tui_text(&content),
                        truncated,
                    })
                    .map_err(|error| {
                        sanitize_tui_text(&format!("Cannot open artifact://{id}: {error}"))
                    });
                let _ = tx.send(TuiUiEvent::ArtifactReady {
                    generation,
                    id,
                    result,
                });
            });
            return;
        }
        match manager.read_text_prefix(id, MAX_ARTIFACT_DETAIL_BYTES) {
            Ok((content, truncated)) => {
                self.artifact_detail = Some(ArtifactDetail {
                    id: id.to_string(),
                    content,
                    truncated,
                });
                self.view = View::Artifact { scroll: 0 };
            }
            Err(error) => {
                self.push_line(TranscriptItem::new(
                    format!("Cannot open artifact://{id}: {error}"),
                    TranscriptKind::Error,
                ));
            }
        }
    }
}

/// 识别 LLM 等待心跳消息并提取精简状态标签（如 `·30s`）。
/// 消息格式由 mink-core 统一（`mink::runtime::llm_wait_heartbeat_message`）；
/// 非心跳消息返回 None（按普通告警处理）。
pub(crate) fn heartbeat_status_label(msg: &str) -> Option<String> {
    let elapsed = mink::runtime::parse_llm_wait_heartbeat_elapsed(msg)?;
    Some(format!("·{elapsed}s"))
}

fn upsert_todo(items: &mut Vec<crate::ui::TodoItemDisplay>, update: crate::ui::TodoItemDisplay) {
    if let Some(item) = items.iter_mut().find(|item| item.id == update.id) {
        *item = update;
    } else {
        items.push(update);
    }
}

fn set_todo_status(items: &mut [crate::ui::TodoItemDisplay], id: &str, status: TodoStatusDisplay) {
    if let Some(item) = items.iter_mut().find(|item| item.id == id) {
        item.status = status;
    }
}

fn next_item_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
