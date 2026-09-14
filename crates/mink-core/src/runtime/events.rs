use crate::runtime::{TurnId, TurnOutcome};
use crate::tools::metadata::{ToolResultKind, ToolStatus};
use crate::ui::{
    ArtifactDisplay, Display, PresentedToolResultDisplay, StatsSnapshot, ToolCallDisplay,
    ToolPresentation,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub turn_id: Option<TurnId>,
    pub sequence: u64,
    #[serde(flatten)]
    pub kind: AgentEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEventKind {
    TurnStarted,
    Thinking {
        content: String,
    },
    Text {
        content: String,
    },
    ToolCall {
        id: String,
        name: String,
        summary: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: Option<String>,
        tool_name: String,
        content_preview: String,
        content: String,
        status: ToolStatus,
        exit_code: Option<i32>,
        result_kind: ToolResultKind,
        presentation: Option<ToolPresentation>,
        artifacts: Vec<ArtifactDisplay>,
    },
    Signal {
        signal_kind: String,
        severity: f64,
        message: String,
    },
    Stop {
        reason: String,
    },
    Retry,
    Error {
        message: String,
    },
    Info {
        message: String,
    },
    TitleUpdate {
        model: String,
        stats: StatsSnapshot,
    },
    SubAgentStatus {
        session_id: String,
        status: String,
        in_tokens: u64,
        out_tokens: u64,
    },
    SubAgentOutput {
        session_id: String,
        status: String,
        thinking: String,
        text: String,
        in_tokens: u64,
        out_tokens: u64,
    },
    Prompt,
    ClearLine,
    Final {
        outcome: Box<TurnOutcome>,
    },
}

#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    async fn on_event(&self, event: AgentEvent) -> Result<(), String>;
}

pub(crate) struct EventDispatcher {
    tx: Mutex<Option<tokio::sync::mpsc::Sender<AgentEvent>>>,
    task: Mutex<Option<tokio::task::JoinHandle<Result<(), String>>>>,
    failure: Arc<Mutex<Option<String>>>,
    /// Number of observer events dropped because the bounded queue was full.
    /// The observer is best-effort telemetry: overflow drops the newest event
    /// and keeps the sink alive instead of permanently stopping it.
    dropped_events: AtomicU64,
    /// Ensures the first overflow is visible in production via warn_once.
    overflow_warned: AtomicBool,
}

impl EventDispatcher {
    pub(crate) fn new(sink: Arc<dyn EventSink>) -> Arc<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let failure = Arc::new(Mutex::new(None));
        let task_failure = failure.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let Err(error) = sink.on_event(event).await {
                    *task_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
                    return Err(error);
                }
            }
            Ok(())
        });
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            task: Mutex::new(Some(task)),
            failure,
            dropped_events: AtomicU64::new(0),
            overflow_warned: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    pub(crate) fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    fn dispatch(&self, event: AgentEvent) {
        let failure = {
            let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            tx.as_ref().and_then(|tx| match tx.try_send(event) {
                Ok(()) => None,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.dropped_events.fetch_add(1, Ordering::Relaxed);
                    crate::session::event_log::warn_once(
                        &self.overflow_warned,
                        "event observer queue overflowed; dropping newest event",
                    );
                    None
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    Some("event observer stopped before runtime shutdown")
                }
            })
        };
        if let Some(message) = failure {
            self.stop_with_failure(message);
        }
    }

    fn stop_with_failure(&self, message: impl Into<String>) {
        let message = message.into();
        let mut failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if failure.is_some() {
            return;
        }
        *failure = Some(message);
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(task) = self.task.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            task.abort();
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.shutdown_with_timeout(std::time::Duration::from_secs(5))
            .await
    }

    async fn shutdown_with_timeout(&self, timeout: std::time::Duration) -> Result<(), String> {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut task = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = task.as_mut() {
            match tokio::time::timeout(timeout, &mut *handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => self.stop_with_failure(error),
                Ok(Err(error)) if error.is_cancelled() => {}
                Ok(Err(error)) => {
                    self.stop_with_failure(format!("event observer task failed: {error}"))
                }
                Err(_) => {
                    handle.abort();
                    let _ = handle.await;
                    self.stop_with_failure(format!(
                        "event observer shutdown timed out after {:.3}s",
                        timeout.as_secs_f64()
                    ));
                }
            }
        }
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .map_or(Ok(()), Err)
    }
}

/// Default pending-progress budget for a live turn stream (1 MiB).
pub(crate) const PROGRESS_PENDING_BYTES_LIMIT: usize = 1 << 20;

/// Minimum bytes charged per progress event: queue-slot and structural
/// overhead must count even for empty deltas (zero-length events would
/// otherwise consume unbounded slots for free).
pub(crate) const PROGRESS_EVENT_MIN_BYTES: usize = 128;

/// Producer-side byte budget for coalescible progress events.
///
/// Text/Thinking deltas are display progress: when the pending (sent but not
/// yet consumed) progress bytes exceed the budget, further deltas are dropped
/// and one reliable notice is emitted instead. Reliable events (tool results,
/// stop/error, control updates) are never dropped; the bound therefore covers
/// the unbounded-growth case without introducing producer blocking (a bounded
/// channel would deadlock `outcome()`-only consumers).
pub(crate) struct ProgressBudget {
    limit: usize,
    pending: AtomicUsize,
    dropped: AtomicU64,
    notice_sent: AtomicBool,
}

impl ProgressBudget {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            pending: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            notice_sent: AtomicBool::new(false),
        })
    }

    /// Reserve one progress delta (payload length + per-event minimum);
    /// `false` means the delta must be dropped (caller emits the notice).
    pub(crate) fn reserve(&self, payload_len: usize) -> bool {
        let bytes = payload_len.max(PROGRESS_EVENT_MIN_BYTES);
        let mut current = self.pending.load(Ordering::Acquire);
        loop {
            if current.saturating_add(bytes) > self.limit {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            match self.pending.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Release consumed progress bytes (same charge as `reserve`).
    pub(crate) fn release(&self, payload_len: usize) {
        let bytes = payload_len.max(PROGRESS_EVENT_MIN_BYTES);
        let _ = self
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                debug_assert!(
                    current >= bytes,
                    "progress budget released more than reserved: {current} < {bytes}"
                );
                Some(current.saturating_sub(bytes))
            });
    }

    fn should_notice(&self) -> bool {
        !self.notice_sent.swap(true, Ordering::AcqRel)
    }

    /// Pending (sent but not yet consumed) progress bytes, including the
    /// per-event structural minimum.
    pub(crate) fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// Number of progress deltas dropped for exceeding the budget.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl AgentEventKind {
    /// Progress deltas eligible for budget-based dropping.
    pub(crate) fn progress_len(&self) -> Option<usize> {
        match self {
            Self::Text { content } | Self::Thinking { content } => Some(content.len()),
            _ => None,
        }
    }
}

pub(crate) struct TurnEventEmitter {
    turn_id: TurnId,
    next_sequence: AtomicU64,
    tx: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
    dispatcher: Option<Arc<EventDispatcher>>,
    progress_budget: Option<Arc<ProgressBudget>>,
}

impl TurnEventEmitter {
    pub(crate) fn new(
        turn_id: TurnId,
        tx: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
        dispatcher: Option<Arc<EventDispatcher>>,
    ) -> Self {
        Self {
            turn_id,
            next_sequence: AtomicU64::new(1),
            tx,
            dispatcher,
            progress_budget: None,
        }
    }

    /// Attach a pending-progress byte budget (used by `stream_turn`).
    pub(crate) fn with_progress_budget(mut self, budget: Arc<ProgressBudget>) -> Self {
        self.progress_budget = Some(budget);
        self
    }

    pub(crate) fn emit(&self, kind: AgentEventKind) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let make_event = |kind| AgentEvent {
            turn_id: Some(self.turn_id.clone()),
            sequence,
            kind,
        };

        // Stream exit: bounded progress budget (only when a stream subscriber
        // exists). Reliable events always pass.
        if let Some(tx) = &self.tx {
            let stream_kind = match &self.progress_budget {
                Some(budget) => match kind.progress_len() {
                    Some(bytes) => {
                        if budget.reserve(bytes) {
                            Some(kind.clone())
                        } else if budget.should_notice() {
                            Some(AgentEventKind::Info {
                                message: format!(
                                    "live progress output exceeded the pending {} KiB budget; deltas were dropped for this stream (the final result is unaffected)",
                                    budget.limit / 1024
                                ),
                            })
                        } else {
                            None
                        }
                    }
                    None => Some(kind.clone()),
                },
                None => Some(kind.clone()),
            };
            if let Some(stream_kind) = stream_kind
                && let Err(error) = tx.send(make_event(stream_kind))
                && let Some(budget) = &self.progress_budget
                && let Some(bytes) = error.0.kind.progress_len()
            {
                // Only events that reserved budget reach this send; a failed
                // send must give the reservation back (the consumer never saw
                // it). Info/Stop etc. have no progress_len and are no-ops.
                budget.release(bytes);
            }
        }

        // Observer exit: independent delivery policy (EventDispatcher owns its
        // own bounded queue and overflow handling). A slow stream consumer must
        // not starve the observer.
        if let Some(dispatcher) = &self.dispatcher {
            dispatcher.dispatch(make_event(kind));
        }
    }
}

pub(crate) struct EventDisplay {
    dispatcher: Option<Arc<EventDispatcher>>,
    next_control_sequence: AtomicU64,
    current_turn: Mutex<Option<Arc<TurnEventEmitter>>>,
}

impl EventDisplay {
    pub(crate) fn new(dispatcher: Option<Arc<EventDispatcher>>) -> Self {
        Self {
            dispatcher,
            next_control_sequence: AtomicU64::new(1),
            current_turn: Mutex::new(None),
        }
    }

    pub(crate) fn begin_turn(&self, emitter: Arc<TurnEventEmitter>) {
        *self.current_turn.lock().unwrap_or_else(|e| e.into_inner()) = Some(emitter);
    }

    pub(crate) fn dispatcher(&self) -> Option<Arc<EventDispatcher>> {
        self.dispatcher.clone()
    }

    pub(crate) fn end_turn(&self, turn_id: &TurnId) {
        let mut current = self.current_turn.lock().unwrap_or_else(|e| e.into_inner());
        if current
            .as_ref()
            .is_some_and(|emitter| &emitter.turn_id == turn_id)
        {
            *current = None;
        }
    }

    fn emit(&self, kind: AgentEventKind) {
        let emitter = self
            .current_turn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(emitter) = emitter {
            emitter.emit(kind);
        } else if let Some(dispatcher) = &self.dispatcher {
            dispatcher.dispatch(AgentEvent {
                turn_id: None,
                sequence: self.next_control_sequence.fetch_add(1, Ordering::Relaxed),
                kind,
            });
        }
    }
}

impl Display for EventDisplay {
    fn render_thinking(&self, content: &str) {
        self.emit(AgentEventKind::Thinking {
            content: content.into(),
        });
    }
    fn render_text(&self, content: &str) {
        self.emit(AgentEventKind::Text {
            content: content.into(),
        });
    }
    fn render_tool_call(&self, call: &ToolCallDisplay<'_>) {
        self.emit(AgentEventKind::ToolCall {
            id: call.tool_use_id.into(),
            name: call.tool_name.into(),
            summary: call.summary.into(),
            input: call.input.cloned().unwrap_or(serde_json::Value::Null),
        });
    }
    fn render_tool_result(&self, result: &PresentedToolResultDisplay<'_>) {
        self.emit(AgentEventKind::ToolResult {
            tool_use_id: result.base.tool_use_id.map(Into::into),
            tool_name: result.base.tool_name.into(),
            content_preview: result.base.content_preview.into(),
            content: result.base.content.into(),
            status: result.status,
            exit_code: result.base.exit_code,
            result_kind: result.result_kind,
            presentation: result.presentation.cloned(),
            artifacts: result.artifacts.to_vec(),
        });
    }
    fn render_signal(&self, signal_kind: &str, severity: f64, message: &str) {
        self.emit(AgentEventKind::Signal {
            signal_kind: signal_kind.into(),
            severity,
            message: message.into(),
        });
    }
    fn render_stop(&self, reason: &str) {
        self.emit(AgentEventKind::Stop {
            reason: reason.into(),
        });
    }
    fn render_error(&self, message: &str) {
        self.emit(AgentEventKind::Error {
            message: message.into(),
        });
    }
    fn render_retry(&self) {
        self.emit(AgentEventKind::Retry);
    }
    fn render_info(&self, msg: &str) {
        self.emit(AgentEventKind::Info {
            message: msg.into(),
        });
    }
    fn render_title_update(&self, model: &str, stats: &StatsSnapshot) {
        self.emit(AgentEventKind::TitleUpdate {
            model: model.into(),
            stats: stats.clone(),
        });
    }
    fn render_sub_agent_status(
        &self,
        session_id: &str,
        status: &str,
        in_tokens: u64,
        out_tokens: u64,
    ) {
        self.emit(AgentEventKind::SubAgentStatus {
            session_id: session_id.into(),
            status: status.into(),
            in_tokens,
            out_tokens,
        });
    }
    fn render_sub_agent_output(
        &self,
        session_id: &str,
        status: &str,
        thinking: &str,
        text: &str,
        in_tokens: u64,
        out_tokens: u64,
    ) {
        self.emit(AgentEventKind::SubAgentOutput {
            session_id: session_id.into(),
            status: status.into(),
            thinking: thinking.into(),
            text: text.into(),
            in_tokens,
            out_tokens,
        });
    }
    fn render_prompt(&self) {
        self.emit(AgentEventKind::Prompt);
    }
    fn render_clear_line(&self) {
        self.emit(AgentEventKind::ClearLine);
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
