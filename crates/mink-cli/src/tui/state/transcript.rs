use super::*;

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
    /// A structured goal boundary card (`_mink.goal_message`).
    Goal,
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
    /// Structured tool arguments, kept verbatim for card rendering/verification.
    pub tool_input: Option<serde_json::Value>,
    pub tool_status: Option<crate::runtime::ToolStatus>,
    pub tool_exit_code: Option<i32>,
    pub tool_result_kind: Option<ToolResultKind>,
    pub presentation: Option<ToolPresentation>,
    pub artifacts: Vec<ArtifactDisplay>,
    /// Structured goal boundary payload; only `TranscriptKind::Goal` carries it.
    pub goal_message: Option<crate::runtime::GoalMessage>,
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
            TranscriptKind::Tool | TranscriptKind::Goal => CollapsePolicy::Auto {
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
            tool_input: None,
            tool_status: None,
            tool_exit_code: None,
            tool_result_kind: None,
            presentation: None,
            artifacts: Vec::new(),
            goal_message: None,
            sealed: true,
            cached_lines: None,
            cached_width: 0,
            cached_collapsed: collapsed,
            cached_interactive: false,
            sub_detail: None,
        }
    }

    /// A sealed goal boundary card rendered from the structured payload alone.
    ///
    /// The plain fallback text keeps Inline output self-contained; the Full
    /// renderer colours the same card through `TranscriptKind::Goal`.
    pub(crate) fn goal_card(message: crate::runtime::GoalMessage) -> Self {
        let mut item = Self::new(goal_card_text(&message), TranscriptKind::Goal);
        item.goal_message = Some(message);
        item
    }

    pub(crate) fn new_tool_result(tool_name: String, text: String) -> Self {
        let mut line = Self::new(text, TranscriptKind::Tool);
        line.tool_name = Some(sanitize_tui_text(&tool_name));
        line
    }

    pub(crate) fn new_tool_call(
        tool_use_id: Option<String>,
        tool_name: String,
        summary: String,
        input: Option<serde_json::Value>,
    ) -> Self {
        // `tool_name`/`summary` are model-controlled and rendered outside the
        // sanitized body text; clean them at the single ingestion point too.
        let tool_name = sanitize_tui_text(&tool_name);
        let summary = sanitize_tui_text(&summary);
        let text = if summary.is_empty() {
            format!("[tool] {tool_name}")
        } else {
            format!("[tool] {summary}")
        };
        let mut item = Self::new(text, TranscriptKind::Tool);
        item.tool_use_id = tool_use_id;
        item.tool_name = Some(tool_name);
        item.tool_summary = Some(summary);
        item.tool_input = input;
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

/**
 * Plain-text rendering of a goal boundary card.
 *
 * Every field comes from the structured `GoalMessage`; nothing is parsed back
 * out of prose. The Inline projection and the collapsed Full summary both use
 * this text, while the Full renderer colours the same fields.
 */
pub(crate) fn goal_card_text(message: &crate::runtime::GoalMessage) -> String {
    use crate::runtime::GoalMessageKind;
    let label = match message.kind {
        GoalMessageKind::Draft => "目标草案讨论",
        GoalMessageKind::Start => "开始目标",
        GoalMessageKind::Continuation => "继续执行目标",
    };
    let mut text = format!("{label} · {}", message.objective);
    match message.kind {
        GoalMessageKind::Draft => {
            text.push_str(&format!("\n契约 r{}", message.contract_revision));
            if let Some(feedback) = message
                .feedback
                .as_deref()
                .filter(|feedback| !feedback.trim().is_empty())
            {
                text.push_str(&format!(" · 修改意见：{feedback}"));
            }
        }
        GoalMessageKind::Start | GoalMessageKind::Continuation => {
            text.push_str(&format!(
                "\n第 {} 轮 · 契约 r{}",
                message.iteration, message.contract_revision
            ));
        }
    }
    if let Some(decision) = message
        .prior_decision
        .as_deref()
        .filter(|decision| !decision.trim().is_empty())
    {
        text.push_str(&format!("\n上一轮结论：{decision}"));
    }
    if !message.acceptance_criteria.is_empty() {
        text.push_str("\n验收项：");
        for criterion in &message.acceptance_criteria {
            text.push_str(&format!("\n  · {criterion}"));
        }
    }
    if !message.constraints.is_empty() {
        text.push_str("\n约束：");
        for constraint in &message.constraints {
            text.push_str(&format!("\n  · {constraint}"));
        }
    }
    text
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
            tool_input: None,
            tool_status: None,
            tool_exit_code: None,
            tool_result_kind: None,
            presentation: None,
            artifacts: Vec::new(),
            goal_message: None,
            sealed: true,
            cached_lines: None,
            cached_width: 0,
            cached_collapsed: false,
            cached_interactive: false,
            sub_detail: None,
        }
    }
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

fn next_item_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
