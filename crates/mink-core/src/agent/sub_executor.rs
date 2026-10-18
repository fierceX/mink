use crate::agent::turn::{TurnDecision, TurnExecutor};
use crate::config::ResolvedConfig as Config;
use crate::context::AgentSharedContext;
use crate::runtime::context_build::{AgentContextBuild, build_agent_context};
use crate::session::metadata::SessionSeed;
use crate::session::paths::SessionLayout;
use crate::session::stats::Stats;
use crate::session::store::ConversationStore;
use crate::ui::{Display, SubAgentStreamKind, SubAgentStreamSink};
use anyhow::{Result, bail};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Why a sub-agent execution ended, independent of the tool-status vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubAgentStopReason {
    Completed,
    Aborted,
    Error,
    MaxTurns,
    Timeout,
}

impl SubAgentStopReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Error => "error",
            Self::MaxTurns => "max_turns",
            Self::Timeout => "timeout",
        }
    }
}

/// Why this child session exists. Internal reviewers and replanners are not
/// model-visible delegations and must not enter the delegation catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubAgentOrigin {
    SubAgent,
    GoalReview,
    Replan,
}

impl SubAgentOrigin {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SubAgent => "sub_agent",
            Self::GoalReview => "goal_review",
            Self::Replan => "replan",
        }
    }
}

/// Terminal status of one sub-agent execution.
///
/// String conversion only happens at output boundaries (Display events,
/// EventLog, tool status); internal decisions never compare strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubAgentStatus {
    Succeeded,
    Failed,
    Interrupted,
    TimedOut,
}

impl SubAgentStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "ok",
            Self::Failed => "failed",
            Self::Interrupted => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    pub(crate) fn to_tool_status(self) -> crate::tools::metadata::ToolStatus {
        use crate::tools::metadata::{ToolFailureKind, ToolStatus};
        match self {
            Self::Succeeded => ToolStatus::Succeeded,
            Self::Failed => ToolStatus::Failed(ToolFailureKind::Unknown),
            Self::Interrupted => ToolStatus::Interrupted,
            Self::TimedOut => ToolStatus::Failed(ToolFailureKind::Timeout),
        }
    }
}

/// Result of a sub-agent execution.
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    pub status: SubAgentStatus,
    pub stop_reason: SubAgentStopReason,
    pub thinking: String,
    pub text: String,
    /// Bounded, model-safe terminal detail (never reasoning or tool payloads).
    pub diagnostic: Option<String>,
    pub usage: Stats,
    pub elapsed_ms: u64,
    /// Whether usage came from a real child StatsTracker (false for
    /// pre-permit cancellation / an unavailable child context).
    pub usage_known: bool,
    /// Whether elapsed_ms is an observation rather than a placeholder zero.
    pub elapsed_known: bool,
}

impl SubAgentResult {
    pub(crate) fn terminal(
        status: SubAgentStatus,
        stop_reason: SubAgentStopReason,
        text: String,
        diagnostic: Option<String>,
    ) -> Self {
        Self {
            status,
            stop_reason,
            thinking: String::new(),
            text,
            diagnostic: diagnostic.map(|detail| bounded_diagnostic(&detail)),
            usage: Stats::default(),
            elapsed_ms: 0,
            usage_known: true,
            elapsed_known: true,
        }
    }

    /// Terminal result where no child measurement exists (pre-permit cancel,
    /// unavailable child stats, or an unexpected host error).
    pub(crate) fn terminal_unmeasured(
        status: SubAgentStatus,
        stop_reason: SubAgentStopReason,
        diagnostic: Option<String>,
    ) -> Self {
        let mut result = Self::terminal(status, stop_reason, String::new(), diagnostic);
        result.usage_known = false;
        result.elapsed_known = false;
        result
    }
}

/// CaptureDisplay silently buffers sub-agent output instead of forwarding to the parent TUI.
/// This prevents sub-agent thinking/text from polluting the main conversation view.
struct CaptureDisplay {
    stream_sink: Option<Arc<dyn SubAgentStreamSink>>,
    session_id: String,
    thinking: Mutex<String>,
    text: Mutex<String>,
}

impl CaptureDisplay {
    fn new(stream_sink: Option<Arc<dyn SubAgentStreamSink>>, session_id: &str) -> Self {
        Self {
            stream_sink,
            session_id: session_id.to_string(),
            thinking: Mutex::new(String::new()),
            text: Mutex::new(String::new()),
        }
    }
    fn take_thinking(&self) -> String {
        std::mem::take(
            &mut *self
                .thinking
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }
    fn take_text(&self) -> String {
        std::mem::take(&mut *self.text.lock().unwrap_or_else(|error| error.into_inner()))
    }
}

impl Display for CaptureDisplay {
    fn render_thinking(&self, c: &str) {
        self.thinking
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_str(c);
        if let Some(sink) = &self.stream_sink {
            sink.render_sub_agent_stream(&self.session_id, SubAgentStreamKind::Thinking, c);
        }
    }
    fn render_text(&self, c: &str) {
        self.text
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_str(c);
        if let Some(sink) = &self.stream_sink {
            sink.render_sub_agent_stream(&self.session_id, SubAgentStreamKind::Text, c);
        }
    }
    fn render_tool_call(&self, _call: &crate::ui::ToolCallDisplay<'_>) {}
    fn render_tool_result(&self, _result: &crate::ui::PresentedToolResultDisplay<'_>) {}
    fn render_stop(&self, _reason: &str) {}
    fn render_signal(&self, _kind: &str, _severity: f64, _message: &str) {}
    fn render_error(&self, _m: &str) {}
    fn render_retry(&self) {}
    fn render_info(&self, _m: &str) {}
    fn render_title_update(&self, _m: &str, _s: &crate::ui::StatsSnapshot) {}
    fn render_sub_agent_status(&self, _sid: &str, _st: &str, _it: u64, _ot: u64) {}
    fn render_sub_agent_output(
        &self,
        _sid: &str,
        _status: &str,
        _thinking: &str,
        _text: &str,
        _in_tokens: u64,
        _out_tokens: u64,
    ) {
    }
    fn render_prompt(&self) {}
    fn render_clear_line(&self) {}
}

/// SubAgentExecutor runs a child agent in an isolated context.
pub struct SubAgentExecutor {
    child_store: Arc<ConversationStore>,
    child_ctx: Arc<AgentSharedContext>,
    capture: Arc<CaptureDisplay>,
    parent_display: Arc<dyn Display>,
    session_id: String,
    label: Option<String>,
}

impl SubAgentExecutor {
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn new(
        parent_ctx: Arc<AgentSharedContext>,
        session_id: String,
        fork: bool,
        config: Config,
    ) -> Result<Self> {
        let cancel = parent_ctx.cancel.linked_child_token();
        Self::new_child(
            parent_ctx,
            session_id,
            fork,
            config,
            cancel,
            None,
            None,
            SubAgentOrigin::SubAgent,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_child(
        parent_ctx: Arc<AgentSharedContext>,
        session_id: String,
        fork: bool,
        config: Config,
        cancel: crate::cancel::CancellationToken,
        label: Option<String>,
        model: Option<String>,
        origin: SubAgentOrigin,
    ) -> Result<Self> {
        Self::new_inner(
            parent_ctx, session_id, fork, config, cancel, origin, label, model, None,
        )
        .await
    }

    pub(crate) async fn new_with_review(
        parent_ctx: Arc<AgentSharedContext>,
        session_id: String,
        cancel: crate::cancel::CancellationToken,
        review: Arc<crate::runtime::goal::review::GoalReviewSession>,
    ) -> Result<Self> {
        let mut config = parent_ctx.config.clone();
        let active = parent_ctx
            .active_model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        config.model = active.alias.unwrap_or(active.actual);
        config.enabled_tools = Some(
            ["Read", "Glob", "Grep", "GoalRead"]
                .into_iter()
                .filter(|name| parent_ctx.tool_surface.has(name))
                .map(str::to_string)
                .collect(),
        );
        config
            .tool_approval
            .retain(|name, _| config.enabled_tools.as_ref().unwrap().contains(name));
        config.sub_agent_timeout_secs = crate::runtime::goal::GOAL_REVIEW_TIMEOUT_SECS as i32;
        Self::new_inner(
            parent_ctx,
            session_id,
            true,
            config,
            cancel,
            SubAgentOrigin::GoalReview,
            Some("goal review".into()),
            None,
            Some(review),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn new_inner(
        parent_ctx: Arc<AgentSharedContext>,
        session_id: String,
        fork: bool,
        mut config: Config,
        cancel: crate::cancel::CancellationToken,
        origin: SubAgentOrigin,
        label: Option<String>,
        model: Option<String>,
        review: Option<Arc<crate::runtime::goal::review::GoalReviewSession>>,
    ) -> Result<Self> {
        if let Some(model) = model {
            config.model = model;
        }
        let child_depth = parent_ctx.sub_agent_depth.saturating_add(1);
        let max_depth = parent_ctx.tool_config.sub_agent_max_depth;
        if origin == SubAgentOrigin::SubAgent && child_depth > max_depth {
            bail!("sub-agent depth {child_depth} exceeds sub_agent_max_depth {max_depth}");
        }
        let mut components = Path::new(&session_id).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            bail!("invalid sub-agent session id: {session_id}");
        }
        let capture = Arc::new(CaptureDisplay::new(
            parent_ctx.sub_stream_tx.clone(),
            &session_id,
        ));
        let parent_display = parent_ctx.display.clone();
        let parent_session_dir = parent_ctx
            .store
            .path()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("parent conversation has no session directory"))?;
        let child_home = parent_session_dir.join("subagents").join(&session_id);
        let home_created = Arc::new(AtomicBool::new(false));
        let clone_running = Arc::new(AtomicBool::new(false));
        let abandoned = Arc::new(AtomicBool::new(false));
        let mut home_guard = ChildHomeGuard {
            path: child_home.clone(),
            created: home_created.clone(),
            clone_running: clone_running.clone(),
            abandoned: abandoned.clone(),
            armed: true,
        };
        prepare_child_home_with_cancel(
            parent_session_dir,
            &child_home,
            fork,
            Some(cancel.clone()),
            Some(&home_created),
            Some(clone_running.clone()),
            Some(abandoned),
        )
        .await?;
        let parent_lease = parent_ctx
            .session_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let child_lease = Arc::new(
            crate::session::lease::SessionLease::acquire_directory(&child_home)?
                .with_parent(parent_lease),
        );

        let built = build_agent_context(AgentContextBuild {
            goal_review: review.clone(),
            session_lease: Some(child_lease),
            config,
            home: child_home,
            // Mirror the parent session's `<environment>` heading: the child's
            // own home would otherwise split the request head and stop a forked
            // child from reusing the parent's cached prefix.
            environment_home: Some(parent_ctx.environment_home.clone()),
            cwd: parent_ctx.cwd.clone(),
            session_id: session_id.clone(),
            session_layout: SessionLayout::Isolated,
            resolved_paths: None,
            api_url: parent_ctx.api_url.clone(),
            display: capture.clone(),
            sub_stream_tx: None,
            cancel,
            interrupt: parent_ctx.interrupt.clone(),
            is_sub_agent: true,
            sub_agent_depth: child_depth,
            usage_journal: Some(parent_ctx.usage.clone()),
            read_only_fs: parent_ctx.read_only_fs.clone(),
            resource_session_id: parent_ctx.vfs_scope.resource_session_id.clone(),
            resource_handlers: Vec::new(),
            skill_providers: Vec::new(),
            runtime_skills: Vec::new(),
            skill_discovery_policy: crate::capabilities::SkillDiscoveryPolicy::Defaults,
            llm_backend: parent_ctx.llm_backend.clone(),
            resource_router: if review.is_some() {
                None
            } else {
                Some(parent_ctx.resource_router.clone())
            },
            capability_snapshot: Some(parent_ctx.capability_snapshot.clone()),
            custom_tools: if review.is_some() {
                Vec::new()
            } else {
                parent_ctx.custom_tools.as_ref().clone()
            },
            prefix_source: if review.is_some() {
                None
            } else {
                parent_ctx.prefix_source.clone()
            },
        })
        .await?;
        let child_ctx = built.ctx;
        let label = label
            .map(|label| crate::session::subagent_catalog::sanitize_label(&label, &session_id));
        let title = label
            .as_deref()
            .map(|label| format!("Sub-agent: {label}"))
            .unwrap_or_else(|| format!("Sub-agent: {session_id}"));
        crate::session::metadata::ensure_metadata(
            &built.paths,
            &parent_ctx.cwd,
            SessionSeed {
                alias: None,
                title: Some(title),
                first_prompt: None,
                parent: Some(parent_ctx.config.session_id.clone()),
                subagent: Some(crate::session::metadata::SubAgentMetadata {
                    version: 1,
                    origin: origin.as_str().to_string(),
                    mode: if fork { "fork" } else { "fresh" }.to_string(),
                    label: label.clone(),
                    depth: child_depth,
                }),
            },
        )
        .await?;
        home_guard.disarm();
        let child_store = child_ctx.store.clone();

        Ok(Self {
            child_store,
            child_ctx,
            capture,
            parent_display,
            session_id,
            label,
        })
    }

    /// Shared child context, exposed to crate tests and internal callers that
    /// need to inspect the built surface/provenance.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn context(&self) -> &Arc<AgentSharedContext> {
        &self.child_ctx
    }

    /// Execute the sub-agent with the given prompt.
    pub async fn execute(self, prompt: String) -> SubAgentResult {
        self.execute_turn(prompt).await
    }

    async fn execute_turn(&self, prompt: String) -> SubAgentResult {
        // 在 self 被 move 进 run_impl 之前，提取所有权字段
        let started = Instant::now();
        let capture = self.capture.clone();
        let parent_display = self.parent_display.clone();
        let session_id = self.session_id.clone();
        let child_ctx = self.child_ctx.clone();

        let timeout_secs = child_ctx.tool_config.sub_agent_timeout_secs.max(1) as u64;
        let cancel = child_ctx.cancel.clone();
        enum RunOutcome {
            Done(Result<(TurnDecision, String, String)>),
            TimedOut,
        }
        let outcome = tokio::select! {
            result = self.run_impl(&prompt) => RunOutcome::Done(result),
            _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
                cancel.cancel();
                RunOutcome::TimedOut
            }
        };

        let captured_thinking = capture.take_thinking();
        let captured_text = capture.take_text();
        let stats = child_ctx.stats.snapshot().await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let (status, stop_reason, thinking, text, diagnostic) = match outcome {
            RunOutcome::Done(Ok((TurnDecision::Stop, thinking, text))) => {
                let text = resolve_success_text(text, captured_text.clone());
                (
                    SubAgentStatus::Succeeded,
                    SubAgentStopReason::Completed,
                    if thinking.is_empty() {
                        captured_thinking.clone()
                    } else {
                        thinking
                    },
                    text,
                    None,
                )
            }
            RunOutcome::Done(Ok((TurnDecision::Interrupted, _, _))) => (
                SubAgentStatus::Interrupted,
                SubAgentStopReason::Aborted,
                captured_thinking.clone(),
                captured_text.clone(),
                Some("sub_agent_interrupted".to_string()),
            ),
            RunOutcome::Done(Ok((decision, _, _))) => {
                let (reason, diagnostic) = match decision {
                    TurnDecision::MaxTurnsExceeded => (
                        SubAgentStopReason::MaxTurns,
                        "sub_agent_max_turns: child exhausted max_turns before end_turn"
                            .to_string(),
                    ),
                    TurnDecision::Failed(message) => (
                        SubAgentStopReason::Error,
                        format!("sub_agent_error: {message}"),
                    ),
                    TurnDecision::Stop | TurnDecision::Interrupted => unreachable!(),
                };
                (
                    SubAgentStatus::Failed,
                    reason,
                    captured_thinking.clone(),
                    captured_text.clone(),
                    Some(diagnostic),
                )
            }
            RunOutcome::Done(Err(error)) => (
                SubAgentStatus::Failed,
                SubAgentStopReason::Error,
                captured_thinking.clone(),
                captured_text.clone(),
                Some(format!("sub_agent_error: {error:#}")),
            ),
            RunOutcome::TimedOut => (
                SubAgentStatus::TimedOut,
                SubAgentStopReason::Timeout,
                captured_thinking.clone(),
                captured_text.clone(),
                Some(format!("sub_agent_timeout phase=run after {timeout_secs}s")),
            ),
        };

        // 向父 Display 发送展示侧有界的输出（完整内容仍保留在子会话）
        let display_thinking = bounded_display(&captured_thinking);
        let display_text = bounded_display(&captured_text);
        parent_display.render_sub_agent_output_meta(
            &session_id,
            status.as_str(),
            self.label.as_deref(),
            Some(stop_reason.as_str()),
            &display_thinking,
            &display_text,
            stats.total_input_tokens,
            stats.total_output_tokens,
        );

        SubAgentResult {
            status,
            stop_reason,
            thinking,
            text,
            diagnostic: diagnostic.map(|detail| bounded_diagnostic(&detail)),
            usage: stats,
            elapsed_ms,
            usage_known: true,
            elapsed_known: true,
        }
    }

    /// Run one child turn without consuming the executor, so callers can
    /// classify the decision into a terminal status themselves.
    async fn run_impl(&self, prompt: &str) -> Result<(TurnDecision, String, String)> {
        let mut executor = TurnExecutor::new(self.child_ctx.clone());
        let (decision, _effects) = Box::pin(executor.execute(prompt, None)).await?;

        match decision {
            TurnDecision::Stop => {
                // Read back the last assistant message from the CHILD store.
                // The whole text of that message is the result; a child may
                // split its final answer across multiple text blocks.
                let mut result_thinking = String::new();
                let mut result_text = String::new();
                if let Some(line) = self.child_store.last_assistant_message_with_text().await?
                    && let Some(content) = line.get("content")
                {
                    (result_thinking, result_text) = extract_assistant_content(content);
                }
                Ok((TurnDecision::Stop, result_thinking, result_text))
            }
            other => Ok((other, String::new(), String::new())),
        }
    }
}

/// Reuses only a successful read-only reviewer, never an ordinary execution agent.
pub(crate) struct ReviewAgentSession {
    executor: SubAgentExecutor,
    usable: bool,
}
impl Drop for ReviewAgentSession {
    fn drop(&mut self) {
        self.executor.child_ctx.cancel.cancel();
    }
}
impl ReviewAgentSession {
    pub(crate) fn new(executor: SubAgentExecutor) -> Self {
        Self {
            executor,
            usable: true,
        }
    }
    pub(crate) async fn execute(&mut self, prompt: String) -> SubAgentResult {
        assert!(
            self.usable,
            "a failed reviewer cannot be restarted as a new turn"
        );
        let result = self.executor.execute_turn(prompt).await;
        self.usable = result.status == SubAgentStatus::Succeeded;
        result
    }
}

/// Concatenate every non-empty text block from the last assistant message and
/// keep the first thinking block for TUI display only. String content is the
/// legacy assistant shape and is treated as plain text.
fn extract_assistant_content(content: &serde_json::Value) -> (String, String) {
    match content {
        serde_json::Value::Array(blocks) => extract_assistant_text(blocks),
        serde_json::Value::String(text) => (String::new(), text.clone()),
        _ => (String::new(), String::new()),
    }
}

fn extract_assistant_text(content: &[serde_json::Value]) -> (String, String) {
    let mut thinking = String::new();
    let mut text = String::new();
    for block in content {
        match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "thinking" => {
                if thinking.is_empty()
                    && let Some(t) = block.get("thinking").and_then(|v| v.as_str())
                {
                    thinking = t.to_string();
                }
            }
            "text" => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str())
                    && !t.is_empty()
                {
                    text.push_str(t);
                }
            }
            _ => {}
        }
    }
    (thinking, text)
}

/// Prefer the last assistant text; fall back to streamed text when compaction or
/// cancellation removed it, and never return a blank success payload.
fn resolve_success_text(text: String, captured: String) -> String {
    let text = if text.is_empty() { captured } else { text };
    if text.trim().is_empty() {
        "(no output)".to_string()
    } else {
        text
    }
}

/// Strip control characters (except newline/tab) and truncate to 4096 bytes on
/// a UTF-8 boundary. The diagnostic is model-visible, so it must stay bounded
/// and must never carry raw protocol payloads.
fn bounded_diagnostic(detail: &str) -> String {
    const LIMIT: usize = 4096;
    const MARKER: &str = "\n… [truncated]";
    let mut cleaned = String::with_capacity(detail.len().min(LIMIT));
    for ch in detail.chars() {
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            cleaned.push(ch);
        }
    }
    if cleaned.len() <= LIMIT {
        return cleaned;
    }
    // Reserve room for the marker so the whole diagnostic (text + marker)
    // stays within the model-visible 4096-byte contract.
    let mut end = LIMIT - MARKER.len();
    while end > 0 && !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = cleaned[..end].to_string();
    out.push_str(MARKER);
    debug_assert!(out.len() <= LIMIT);
    out
}

fn bounded_display(text: &str) -> String {
    const LIMIT: usize = 32 * 1024;
    const MARKER: &str = "\n… [truncated]\n";
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let budget = LIMIT - MARKER.len();
    let mut head = budget / 2;
    while head > 0 && !text.is_char_boundary(head) {
        head -= 1;
    }
    let desired_tail = text.len() - (budget - head);
    let mut tail = desired_tail;
    while tail < text.len() && !text.is_char_boundary(tail) {
        tail += 1;
    }
    // The ceil-boundary may add up to 3 bytes; trim the head by the same
    // amount so the result still fits the limit.
    let overflow = tail - desired_tail;
    head = head.saturating_sub(overflow);
    while head > 0 && !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut out = String::with_capacity(LIMIT);
    out.push_str(&text[..head]);
    out.push_str(MARKER);
    out.push_str(&text[tail..]);
    debug_assert!(out.len() <= LIMIT);
    out
}

#[cfg(test)]
async fn prepare_child_home(parent: &Path, child: &Path, fork: bool) -> Result<()> {
    prepare_child_home_with_cancel(parent, child, fork, None, None, None, None).await
}

async fn prepare_child_home_with_cancel(
    parent: &Path,
    child: &Path,
    fork: bool,
    cancel: Option<crate::cancel::CancellationToken>,
    created: Option<&AtomicBool>,
    clone_running: Option<Arc<AtomicBool>>,
    abandoned: Option<Arc<AtomicBool>>,
) -> Result<()> {
    if child.exists() {
        bail!("sub-agent home already exists: {}", child.display());
    }
    let child_parent = child
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sub-agent home has no parent"))?;
    ensure_real_directory(child_parent).await?;
    if fork {
        if let Some(created) = created {
            created.store(true, Ordering::SeqCst);
        }
        if let Some(running) = &clone_running {
            running.store(true, Ordering::SeqCst);
        }
        let parent = parent.to_path_buf();
        let destination = child.to_path_buf();
        let copy_destination = destination.clone();
        let copy_cancel = cancel.clone();
        let running_flag = clone_running.clone();
        let abandoned_flag = abandoned.clone();
        tokio::task::spawn_blocking(move || {
            let mut result = clone_session_dir(&parent, &copy_destination, copy_cancel.as_ref());
            if let Some(running) = running_flag {
                running.store(false, Ordering::SeqCst);
            }
            if result.is_ok()
                && abandoned_flag
                    .as_ref()
                    .is_some_and(|abandoned| abandoned.load(Ordering::SeqCst))
            {
                let _ = std::fs::remove_dir_all(&copy_destination);
                result = Err(anyhow::anyhow!("sub-agent fork setup abandoned"));
            }
            result
        })
        .await??;
        for name in ["events.jsonl", "stats.json", "usage.jsonl", "session.json"] {
            let path = destination.join(name);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    let _ = tokio::fs::remove_dir_all(&destination).await;
                    return Err(error.into());
                }
            }
        }
    } else {
        tokio::fs::create_dir(child).await?;
        if let Some(created) = created {
            created.store(true, Ordering::SeqCst);
        }
    }
    #[cfg(test)]
    if child
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("slow_setup_"))
    {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    Ok(())
}

/// Removes a partially created child home when setup is cancelled or fails.
/// Disarmed only after the fully constructed `SubAgentExecutor` is returned.
struct ChildHomeGuard {
    path: std::path::PathBuf,
    created: Arc<AtomicBool>,
    /// Fork clone owns the directory while this is true; deleting it here would
    /// race with the blocking copy. The clone's own guard removes partials.
    clone_running: Arc<AtomicBool>,
    /// Set when setup is dropped while the blocker is still copying; the clone
    /// closure removes the destination before returning.
    abandoned: Arc<AtomicBool>,
    armed: bool,
}

impl ChildHomeGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildHomeGuard {
    fn drop(&mut self) {
        if self.armed && self.created.load(Ordering::SeqCst) {
            if self.clone_running.load(Ordering::SeqCst) {
                self.abandoned.store(true, Ordering::SeqCst);
            } else {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }
}

/// Owns a partially copied fork destination inside the blocking clone.
/// It only cleans up while the copy has not committed; once the copy succeeds,
/// the outer `ChildHomeGuard` owns the directory.
struct CloneDirGuard {
    path: std::path::PathBuf,
    armed: bool,
}

impl Drop for CloneDirGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

async fn ensure_real_directory(path: &Path) -> Result<()> {
    match tokio::fs::create_dir(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }

    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "sub-agent directory is not a real directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn clone_session_dir(
    source: &Path,
    destination: &Path,
    cancel: Option<&crate::cancel::CancellationToken>,
) -> Result<()> {
    std::fs::create_dir(destination)?;
    let mut guard = CloneDirGuard {
        path: destination.to_path_buf(),
        armed: true,
    };
    #[cfg(test)]
    if destination
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("slow_clone_"))
    {
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    copy_session_entries(source, destination, true, cancel)?;
    #[cfg(test)]
    if destination
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("slow_commit_"))
    {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if cancel.is_some_and(crate::cancel::CancellationToken::is_cancelled) {
        bail!("sub-agent fork cancelled");
    }
    guard.armed = false;
    Ok(())
}

fn copy_session_entries(
    source: &Path,
    destination: &Path,
    root: bool,
    cancel: Option<&crate::cancel::CancellationToken>,
) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        if cancel.is_some_and(crate::cancel::CancellationToken::is_cancelled) {
            bail!("sub-agent fork cancelled");
        }
        let entry = entry?;
        if root && entry.file_name().to_string_lossy() == "subagents" {
            continue;
        }
        // Exclude the image cache tree: child agents own an isolated cache
        // and must never inherit parent objects (v7 §5.1).
        if entry.file_name().to_string_lossy() == "cache" {
            continue;
        }
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir(&target)?;
            copy_session_entries(&entry.path(), &target, false, cancel)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            bail!(
                "unsupported entry in session fork: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "sub_executor_tests.rs"]
mod tests;
