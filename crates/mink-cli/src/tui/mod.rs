//! TUI module for mink using ratatui.

mod attachments;
mod clipboard;
mod command;
mod display;
mod editor;
mod file_picker;
mod height_index;
mod inbox;
#[cfg(test)]
mod inbox_tests;
mod input;
mod markdown;
mod notify;
mod panels;
#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod regression_tests;
mod render;
mod replay;
mod sanitize;
mod signal;
mod state;
mod stream_boundary;
mod theme;

pub use display::{TuiDisplay, TuiSubAgentStreamSink};
pub use signal::TuiSignal;

#[derive(Clone)]
pub struct TuiRuntime {
    pub handle: crate::runtime::AgentRuntimeHandle,
    pub signals: std::sync::mpsc::Sender<TuiSignal>,
}

use crate::cli::RuntimeCmd;
use crate::config::{SandboxConfig, TuiMode};
use file_picker::FilePickerPolicy;
use input::handle_event_for_mode;
use notify::send_task_notification;
use render::render;
#[cfg(test)]
use replay::load_session;
#[cfg(test)]
use signal::drain_signals;
use state::{TuiState, short_cwd_label};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

pub async fn run_tui(
    mode: TuiMode,
    sig_rx: mpsc::Receiver<TuiSignal>,
    orch_tx: tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    session: &crate::runtime::SessionInfo,
    initial_model: &str,
    sandbox: &SandboxConfig,
    runtime: TuiRuntime,
) -> anyhow::Result<()> {
    match mode {
        TuiMode::Full => {
            run_full_tui(sig_rx, orch_tx, session, initial_model, sandbox, runtime).await
        }
        TuiMode::Inline => {
            run_inline_tui(sig_rx, orch_tx, session, initial_model, sandbox, runtime).await
        }
        TuiMode::Off => anyhow::bail!("TUI mode is disabled"),
    }
}

async fn run_full_tui(
    sig_rx: mpsc::Receiver<TuiSignal>,
    orch_tx: tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    session: &crate::runtime::SessionInfo,
    initial_model: &str,
    sandbox: &SandboxConfig,
    runtime: TuiRuntime,
) -> anyhow::Result<()> {
    struct FullRestoreGuard;
    impl Drop for FullRestoreGuard {
        fn drop(&mut self) {
            restore_terminal_on_exit();
        }
    }
    let _guard = FullRestoreGuard;
    let mut terminal = ratatui::try_init()?;
    TUI_RAW_ACTIVE.store(true, Ordering::SeqCst);
    TUI_ALT_SCREEN_ACTIVE.store(true, Ordering::SeqCst);
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )?;
    TUI_MOUSE_CAPTURE_ACTIVE.store(true, Ordering::SeqCst);
    TUI_BRACKETED_PASTE_ACTIVE.store(true, Ordering::SeqCst);
    enable_keyboard_enhancement();
    tui_main_loop(
        &mut terminal,
        sig_rx,
        (orch_tx, runtime),
        TuiMode::Full,
        session,
        initial_model,
        sandbox,
    )
    .await
}

async fn run_inline_tui(
    sig_rx: mpsc::Receiver<TuiSignal>,
    orch_tx: tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    session: &crate::runtime::SessionInfo,
    initial_model: &str,
    sandbox: &SandboxConfig,
    runtime: TuiRuntime,
) -> anyhow::Result<()> {
    struct RestoreGuard;
    impl Drop for RestoreGuard {
        fn drop(&mut self) {
            restore_terminal_on_exit();
        }
    }
    let _guard = RestoreGuard;
    let inline_height = preferred_inline_height();
    // ratatui 的 Inline viewport 初始化依赖光标位置查询（DSR `\x1b[6n`）。
    // 部分终端（dumb terminal、某些 SSH/multiplexer 环境）不响应该查询，
    // 直接 `init_with_options` 会在 `try_init_with_options` 失败时 panic。
    // 这里改用可失败的初始化，失败时降级为全屏 TUI，保证交互可用。
    let (mut terminal, effective_mode) =
        match ratatui::try_init_with_options(ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(inline_height),
        }) {
            Ok(terminal) => {
                TUI_RAW_ACTIVE.store(true, Ordering::SeqCst);
                (terminal, TuiMode::Inline)
            }
            Err(error) => {
                eprintln!("Inline TUI unavailable ({error}); falling back to fullscreen TUI.");
                let full_terminal = ratatui::try_init()?;
                TUI_RAW_ACTIVE.store(true, Ordering::SeqCst);
                TUI_ALT_SCREEN_ACTIVE.store(true, Ordering::SeqCst);
                crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture,)?;
                TUI_MOUSE_CAPTURE_ACTIVE.store(true, Ordering::SeqCst);
                (full_terminal, TuiMode::Full)
            }
        };
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste,)?;
    TUI_BRACKETED_PASTE_ACTIVE.store(true, Ordering::SeqCst);
    enable_keyboard_enhancement();
    tui_main_loop(
        &mut terminal,
        sig_rx,
        (orch_tx, runtime),
        effective_mode,
        session,
        initial_model,
        sandbox,
    )
    .await
}

async fn tui_main_loop(
    terminal: &mut ratatui::DefaultTerminal,
    sig_rx: mpsc::Receiver<TuiSignal>,
    runtime: (tokio::sync::mpsc::UnboundedSender<RuntimeCmd>, TuiRuntime),
    mode: TuiMode,
    session: &crate::runtime::SessionInfo,
    initial_model: &str,
    sandbox: &SandboxConfig,
) -> anyhow::Result<()> {
    let (orch_tx, runtime) = runtime;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (ui_tx, ui_rx) = mpsc::channel::<state::TuiUiEvent>();
    let mut state = TuiState {
        async_load: state::AsyncLoadState {
            loading_history: true,
            ui_tx: Some(ui_tx.clone()),
            ..Default::default()
        },
        mirrors: state::SessionMirrors {
            session_dir: session
                .events_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf(),
            cwd_label: short_cwd_label(),
            artifacts_dir: session.artifacts_dir.clone(),
            attachments_dir: attachments_dir(session),
            image_input: load_image_limits(session),
            model: initial_model.to_string(),
            ..Default::default()
        },
        file_picker_policy: FilePickerPolicy::from_sandbox(cwd, sandbox),
        admission: state::AdmissionState {
            runtime: Some(runtime),
            ..Default::default()
        },
        ..Default::default()
    };
    let snapshot = session.clone();
    std::thread::spawn(move || {
        let loaded = replay::load_session_history(&snapshot.events_path);
        let mut history = TuiState {
            transcript: state::TranscriptState {
                lines: loaded.lines,
                ..Default::default()
            },
            ..Default::default()
        };
        history.transcript.sub_agents.line_by_session = loaded.sub_agent_lines;
        load_persisted_state(&snapshot, &mut history);
        let _ = ui_tx.send(state::TuiUiEvent::HistoryLoaded {
            generation: 0,
            state: Box::new(history),
        });
    });
    state.refresh_inputs();
    let mut sig_rx = async_receiver(sig_rx);
    let mut ui_rx = async_receiver(ui_rx);
    let mut events = crossterm::event::EventStream::new();
    use futures::StreamExt;
    let mut saved_inline_terminal = None;
    let mut draw_at = tokio::time::Instant::now();
    let mut signal_closed = false;
    let mut pending_signals = std::collections::VecDeque::new();
    let mut goal_driver_started = false;
    loop {
        if !state.async_load.loading_history && !goal_driver_started {
            if let Some(runtime) = &state.admission.runtime {
                // UI and input wiring are ready: enable scheduling and pick up
                // a goal the previous session left active.
                runtime.handle.start_goal_driver_and_run()?;
            }
            goal_driver_started = true;
        }
        if mode == TuiMode::Inline {
            sync_inline_terminal_mode(terminal, &state, &mut saved_inline_terminal)?;
            if !state.async_load.loading_history
                && matches!(state.view, state::View::Main)
                && commit_ready(terminal, &mut state)?
            {
                state.dirty = true;
            }
        }
        if let Some(notification) = state.take_task_notification() {
            send_task_notification(&notification);
        }
        if state.quit {
            break;
        }
        if state.dirty && tokio::time::Instant::now() >= draw_at {
            terminal.draw(|f| render(f, &mut state, mode))?;
            state.dirty = matches!(state.view, state::View::Main)
                && (state.cache.rebuild_next.is_some() || !state.cache.pending.is_empty());
            draw_at = tokio::time::Instant::now() + Duration::from_millis(33);
        }
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(event)) => {
                        if handle_event_for_mode(event, &mut state, &orch_tx, mode) { break; }
                        state.dirty = true;
                        draw_at = tokio::time::Instant::now();
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => break,
                }
            }
            first = sig_rx.recv(), if !signal_closed && pending_signals.is_empty() => {
                if let Some(first) = first {
                    let start = std::time::Instant::now();
                    pending_signals.push_back(first);
                    while pending_signals.len() < 512 && start.elapsed() < Duration::from_millis(4) {
                        match sig_rx.try_recv() { Ok(signal) => pending_signals.push_back(signal), Err(_) => break }
                    }
                } else { signal_closed = true; }
            }
            _ = tokio::task::yield_now(), if !pending_signals.is_empty() => {
                let (changed, immediate) = process_signal_batch(&mut pending_signals, &mut state, mode);
                state.dirty |= changed;
                if immediate { draw_at = tokio::time::Instant::now(); }
            }
            Some(event) = ui_rx.recv() => {
                state.apply_ui_event(event);
                state.dirty = true;
                draw_at = tokio::time::Instant::now();
            }
            _ = async {
                if let Some(subscription) = state.admission.input_subscription.as_mut() { let _ = subscription.changed().await; }
                else { std::future::pending::<()>().await; }
            } => {
                if let Some(subscription) = state.admission.input_subscription.as_mut() {
                    state.admission.inputs = subscription.borrow_and_update().as_ref().clone();
                    state.invalidate_detail_resource("panel:Inputs");
                    state.invalidate_detail_resource("panel:Status");
                    state.dirty = true;
                }
            }
            _ = tokio::time::sleep_until(draw_at), if state.dirty => {}
            _ = tokio::task::yield_now(), if mode == TuiMode::Inline && !state.async_load.loading_history && matches!(state.view, state::View::Main)
                && state.transcript.lines.get(state.transcript.inline.committed).is_some_and(|item| item.sealed)
                && (state.stream.work_state.is_working() || state.transcript.inline.committed + 1 < state.transcript.lines.len()) => {}
        }
    }
    Ok(())
}

/// Check the processing budget between reducer operations, retaining the rest
/// in order so the next select can service keys, stop, and background results.
fn process_signal_batch(
    pending: &mut std::collections::VecDeque<TuiSignal>,
    state: &mut TuiState,
    mode: TuiMode,
) -> (bool, bool) {
    let began = std::time::Instant::now();
    let mut count = 0;
    let mut changed = false;
    let mut immediate = false;
    while count < 512 {
        let Some(mut signal) = pending.pop_front() else {
            break;
        };
        count += 1;
        while count < 512 {
            match (&mut signal, pending.front()) {
                (TuiSignal::Text(text), Some(TuiSignal::Text(next)))
                | (TuiSignal::Thinking(text), Some(TuiSignal::Thinking(next))) => {
                    text.push_str(next);
                    pending.pop_front();
                    count += 1;
                }
                _ => break,
            }
        }
        immediate |= !matches!(
            signal,
            TuiSignal::Text(_) | TuiSignal::Thinking(_) | TuiSignal::SubAgentStream { .. }
        );
        changed |= signal::apply_signals(vec![signal], state, mode);
        if began.elapsed() >= Duration::from_millis(4) {
            break;
        }
    }
    (changed, immediate)
}

/// Preserve reliable ordering while waking the async loop from Display's sync API.
/// Bounded forwarding prevents a second unbounded progress queue.
fn async_receiver<T: Send + 'static>(rx: mpsc::Receiver<T>) -> tokio::sync::mpsc::Receiver<T> {
    let (tx, receiver) = tokio::sync::mpsc::channel(1024);
    std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            if tx.blocking_send(event).is_err() {
                break;
            }
        }
    });
    receiver
}

/// Whether this process pushed the progressive keyboard enhancement flags.
/// The panic hook and the restore guards share it so neither emits a stray pop
/// on a terminal that never enabled the protocol.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
static TUI_RAW_ACTIVE: AtomicBool = AtomicBool::new(false);
static TUI_ALT_SCREEN_ACTIVE: AtomicBool = AtomicBool::new(false);
static TUI_MOUSE_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
static TUI_BRACKETED_PASTE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Idempotently restore every terminal mode activated by the TUI.
///
/// Safe to invoke from both normal Drop guards and the global panic hook
/// (including release builds with `panic = "abort"` where Drop guards never
/// execute).
pub(crate) fn restore_terminal_on_exit() {
    disable_keyboard_enhancement();
    if TUI_BRACKETED_PASTE_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    }
    if TUI_MOUSE_CAPTURE_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    }
    if TUI_ALT_SCREEN_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    }
    if TUI_RAW_ACTIVE.swap(false, Ordering::SeqCst) {
        ratatui::restore();
    }
}

/// Push progressive keyboard enhancement flags so Shift+Enter (and Super+V)
/// arrive distinctly; most terminals send the same CR for Enter and
/// Shift+Enter without them.
fn enable_keyboard_enhancement() {
    let env_value = std::env::var("MINK_KEYBOARD_ENHANCEMENT").ok();
    let term = std::env::var("TERM").ok();
    if !keyboard_enhancement_allowed(env_value.as_deref(), term.as_deref()) {
        return;
    }
    if !crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        return;
    }
    let pushed = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    )
    .is_ok();
    if pushed {
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    }
}

/// Env/TERM gate evaluated *before* the terminal probe: the probe blocks up to
/// two seconds when the terminal does not answer, so dumb or opted-out sessions
/// must not pay for it.
fn keyboard_enhancement_allowed(env_value: Option<&str>, term: Option<&str>) -> bool {
    if matches!(
        env_value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("off" | "0" | "false" | "no")
    ) {
        return false;
    }
    !matches!(term, Some("") | Some("dumb"))
}

/// Pop the flags once, if this process pushed them. Safe to call from both the
/// restore guards and the panic hook.
pub(crate) fn disable_keyboard_enhancement() {
    if !KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags
    );
}

/// Session paste staging directory (`<session_dir>/attachments`).
fn attachments_dir(session: &crate::runtime::SessionInfo) -> std::path::PathBuf {
    session
        .events_path
        .parent()
        .map(|dir| dir.join("attachments"))
        .unwrap_or_default()
}

/// Read the frozen image capability from the session snapshot. A missing or
/// text-only snapshot disables clipboard paste (fail closed).
fn load_image_limits(
    session: &crate::runtime::SessionInfo,
) -> Option<crate::runtime::OpenAiChatImageUrlLimits> {
    #[derive(serde::Deserialize)]
    struct Snapshot {
        image_input: crate::runtime::ImageInputCapability,
    }
    let dir = session.events_path.parent()?;
    let raw = std::fs::read_to_string(dir.join("model-capabilities.json")).ok()?;
    let snapshot: Snapshot = serde_json::from_str(&raw).ok()?;
    snapshot.image_input.limits().cloned()
}

fn preferred_inline_height() -> u16 {
    crossterm::terminal::size()
        .map(|(_, height)| height.saturating_sub(4).clamp(8, 12))
        .unwrap_or(10)
}

fn sync_inline_terminal_mode(
    terminal: &mut ratatui::DefaultTerminal,
    state: &TuiState,
    saved_inline_terminal: &mut Option<ratatui::DefaultTerminal>,
) -> anyhow::Result<()> {
    use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
    use ratatui::backend::CrosstermBackend;

    let wants_detail = !matches!(state.view, state::View::Main);
    if wants_detail == saved_inline_terminal.is_some() {
        return Ok(());
    }
    if wants_detail {
        crossterm::execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
        )?;
        TUI_ALT_SCREEN_ACTIVE.store(true, Ordering::SeqCst);
        TUI_MOUSE_CAPTURE_ACTIVE.store(true, Ordering::SeqCst);
        let detail_terminal = ratatui::Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
        *saved_inline_terminal = Some(std::mem::replace(terminal, detail_terminal));
    } else {
        crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            LeaveAlternateScreen,
        )?;
        TUI_MOUSE_CAPTURE_ACTIVE.store(false, Ordering::SeqCst);
        TUI_ALT_SCREEN_ACTIVE.store(false, Ordering::SeqCst);
        let inline_terminal = saved_inline_terminal
            .take()
            .ok_or_else(|| anyhow::anyhow!("inline terminal state is unavailable"))?;
        *terminal = inline_terminal;
    }
    terminal.clear()?;
    Ok(())
}

fn load_persisted_state(session: &crate::runtime::SessionInfo, state: &mut TuiState) {
    use crate::session::todo::{TodoSnapshot, TodoStatus};
    use crate::ui::{
        PlanDisplay, PlanTransitionDisplay, TodoCountsDisplay, TodoDisplay, TodoItemDisplay,
        TodoStatusDisplay,
    };

    let plan = std::fs::read_to_string(&session.plan_draft_path)
        .ok()
        .filter(|content| !content.trim().is_empty())
        .map(|content| PlanDisplay {
            transition: PlanTransitionDisplay::DraftSaved,
            content: Some(content),
        })
        .or_else(|| {
            std::fs::read_to_string(&session.plan_path)
                .ok()
                .filter(|content| !content.trim().is_empty())
                .map(|content| PlanDisplay {
                    transition: PlanTransitionDisplay::Confirmed,
                    content: Some(content),
                })
        });
    state.mirrors.plan = plan;

    // Read-only goal mirror: a resumed session must show its unfinished goal
    // in the status bar before the driver emits anything.
    state.mirrors.goal = mink::runtime::session::read_goal_snapshot(&session.goal_path())
        .as_ref()
        .map(crate::ui::GoalDisplay::from)
        .filter(|goal| !goal.hidden);

    let Ok(bytes) = std::fs::read(&session.todos_path) else {
        state.mirrors.todos = None;
        return;
    };
    let Ok(snapshot) = serde_json::from_slice::<TodoSnapshot>(&bytes) else {
        state.mirrors.todos = None;
        return;
    };
    let (pending, in_progress, completed) =
        snapshot
            .items
            .iter()
            .fold((0, 0, 0), |counts, item| match item.status {
                TodoStatus::Pending => (counts.0 + 1, counts.1, counts.2),
                TodoStatus::InProgress => (counts.0, counts.1 + 1, counts.2),
                TodoStatus::Completed => (counts.0, counts.1, counts.2 + 1),
            });
    state.mirrors.todos = Some(TodoDisplay {
        revision: snapshot.revision,
        counts: TodoCountsDisplay {
            pending,
            in_progress,
            completed,
        },
        items: snapshot
            .items
            .into_iter()
            .map(|item| TodoItemDisplay {
                id: item.id,
                content: item.content,
                status: match item.status {
                    TodoStatus::Pending => TodoStatusDisplay::Pending,
                    TodoStatus::InProgress => TodoStatusDisplay::InProgress,
                    TodoStatus::Completed => TodoStatusDisplay::Completed,
                },
            })
            .collect(),
        changes: Vec::new(),
    });
}

fn commit_ready<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    state: &mut TuiState,
) -> Result<bool, B::Error> {
    use ratatui::text::Text;
    use ratatui::widgets::{Paragraph, Widget};

    let end = if !state.stream.work_state.is_working() {
        state.transcript.lines.len().saturating_sub(1)
    } else {
        state.transcript.lines.len()
    };
    let width = terminal.size()?.width.saturating_sub(2).max(1);
    let mut batch = Vec::new();
    let mut consumed = Vec::new();
    let mut index = state.transcript.inline.committed;
    let mut offset = state.transcript.inline.row_offset;
    while index < end && batch.len() < 256 {
        let item = &mut state.transcript.lines[index];
        if !item.sealed {
            break;
        }
        render::content::prepare_inline_item(item, width, &mut offset);
        let lines = item.cached_lines.as_deref().unwrap_or(&[]);
        let count = (256 - batch.len()).min(lines.len().saturating_sub(offset));
        batch.extend(lines.iter().skip(offset).take(count).cloned());
        offset += count;
        if offset >= lines.len() {
            consumed.push(index);
            index += 1;
            offset = 0;
        } else {
            break;
        }
    }
    if batch.is_empty() && consumed.is_empty() {
        return Ok(false);
    }
    if !batch.is_empty() {
        terminal.insert_before(batch.len() as u16, move |buf| {
            Paragraph::new(Text::from(batch)).render(buf.area, buf);
        })?;
    }
    // Advance only after terminal insertion succeeds; never retry partial writes.
    state.transcript.inline.committed = index;
    state.transcript.inline.row_offset = offset;
    if index < state.transcript.lines.len() {
        state.cache.pending.insert(index);
    }
    for index in consumed {
        let item = &mut state.transcript.lines[index];
        if item.kind != state::TranscriptKind::SubAgent {
            item.text.clear();
            item.text.shrink_to_fit();
        }
        item.cached_lines = None;
        item.cached_offsets.clear();
        item.cached_offsets.shrink_to_fit();
        item.tool_input = None;
        item.tool_summary = None;
        item.goal_message = None;
        item.artifacts.clear();
        item.artifacts.shrink_to_fit();
        item.presentation = None;
        item.sub_detail = None;
        state.cache.pending.remove(&index);
        if index < state.cache.heights.len() {
            state.cache.heights.set(index, 0);
        }
    }
    // Partial rows remain in the active viewport until the next bounded batch.
    Ok(true)
}

#[cfg(test)]
fn sealed_prefix_end(state: &TuiState) -> usize {
    let mut end = state.transcript.inline.committed;
    while state
        .transcript
        .lines
        .get(end)
        .is_some_and(|item| item.sealed)
    {
        end += 1;
    }
    end
}

#[cfg(test)]
fn committable_prefix_end(state: &TuiState) -> usize {
    let end = sealed_prefix_end(state);
    if !state.stream.work_state.is_working()
        && end == state.transcript.lines.len()
        && end > state.transcript.inline.committed
    {
        end - 1
    } else {
        end
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
