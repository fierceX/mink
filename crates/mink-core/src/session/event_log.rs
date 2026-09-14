use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

const EVENT_LOG_QUEUE_CAPACITY: usize = 1024;

/// How long `flush` may retry a full queue before reporting a stalled writer.
#[cfg(not(test))]
const FLUSH_ENQUEUE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const FLUSH_ENQUEUE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long `flush` may wait for the writer acknowledgement. The deadline is
/// measured from `flush()` entry: enqueue retries (`FLUSH_ENQUEUE_DEADLINE`)
/// and the ack wait share the same start instant.
#[cfg(not(test))]
const FLUSH_ACK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const FLUSH_ACK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Deadline for committing contract-critical events.
#[cfg(not(test))]
pub(crate) const CRITICAL_ENQUEUE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
pub(crate) const CRITICAL_ENQUEUE_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(200);

/// Grace period where an in-flight acknowledgement may still land before a
/// pending cancel/interrupt aborts the wait (a healthy writer acks far sooner;
/// a stalled writer does not).
#[cfg(not(test))]
const CRITICAL_ACK_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
#[cfg(test)]
const CRITICAL_ACK_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

enum EventLogCmd {
    Append(String),
    /// Critical event with a per-event write acknowledgement: the sender
    /// learns whether the writer actually opened/wrote the line.
    AppendCritical {
        line: String,
        done: oneshot::Sender<io::Result<()>>,
    },
    Flush {
        done: oneshot::Sender<FlushAck>,
    },
}

/// Result of one write barrier, computed by the writer thread.
struct FlushAck {
    /// Current writability (flush of the open handle / last open failure).
    result: io::Result<()>,
    /// Losses the writer observed while processing commands (monotonic).
    processed_lost: usize,
    /// Last open/write failure that actually dropped an event, for the loss
    /// report (the current `failure` may already be cleared by a reopen).
    last_loss: Option<String>,
    /// Last open/write failure text, for the loss report.
    last_failure: Option<String>,
}

/// Shared state between the send side and the writer thread.
///
/// Ownership rules (see docs/DESIGN.md / Q1):
/// - the send side counts enqueue failures (`send_lost`): writer init failure,
///   channel disconnect, queue rejection — a writer that never started or has
///   already exited cannot be responsible for them;
/// - everything the writer thread owns exclusively lives in [`WriterState`]
///   (file handle, current failure, processed losses) with no locks/atomics;
/// - flush callers are serialized by `EventLogWriter.reported`; the caller
///   that receives a barrier result advances the watermark, so a cancelled
///   flush does not consume an unreported loss.
struct EventLogState {
    /// Writer thread creation failure, surfaced on flush instead of silently
    /// turning into a disconnected queue.
    init_error: Mutex<Option<String>>,
    send_lost: AtomicUsize,
    #[cfg(test)]
    processed_commands: AtomicUsize,
    #[cfg(test)]
    critical_queued: AtomicUsize,
}

impl Default for EventLogState {
    fn default() -> Self {
        Self {
            init_error: Mutex::new(None),
            send_lost: AtomicUsize::new(0),
            #[cfg(test)]
            processed_commands: AtomicUsize::new(0),
            #[cfg(test)]
            critical_queued: AtomicUsize::new(0),
        }
    }
}

/// Writer-thread-exclusive state: no Mutex/Atomic, owned by `run_writer`.
#[derive(Default)]
struct WriterState {
    file: Option<std::fs::File>,
    /// Current open/write failure, cleared once the file is writable again.
    failure: Option<String>,
    /// Cause of the most recent lost event (kept independently of `failure`:
    /// a successful reopen clears writability but not the loss reason).
    last_loss: Option<String>,
    /// Events the writer actually failed to persist.
    processed_lost: usize,
}

/// Serializes event-log writes for one session.
///
/// `log_event()` stays synchronous, but instead of opening the append file on
/// every event it enqueues onto a bounded channel drained by a dedicated OS
/// thread. When the queue is full, `send()` intentionally blocks: this is the
/// backpressure path and bounds memory, matching the old synchronous writer's
/// never-silently-drop behavior while keeping the common case off the tokio
/// worker.

#[derive(Clone)]
pub(crate) struct EventLogWriter {
    tx: SyncSender<EventLogCmd>,
    warned: Arc<AtomicBool>,
    state: Arc<EventLogState>,
    /// Reported-loss watermark; the guard also serializes flush callers.
    reported: Arc<tokio::sync::Mutex<usize>>,
}

impl EventLogWriter {
    pub(crate) fn start(path: PathBuf) -> Self {
        #[cfg(test)]
        {
            Self::start_impl(path, None)
        }
        #[cfg(not(test))]
        {
            Self::start_impl(path)
        }
    }

    #[cfg(test)]
    pub(crate) fn start_paused(path: PathBuf, gate: Arc<EventLogWriterGate>) -> Self {
        Self::start_impl(path, Some(gate))
    }

    fn start_impl(path: PathBuf, #[cfg(test)] gate: Option<Arc<EventLogWriterGate>>) -> Self {
        let (tx, rx) = sync_channel::<EventLogCmd>(EVENT_LOG_QUEUE_CAPACITY);
        let warned = Arc::new(AtomicBool::new(false));
        let state = Arc::new(EventLogState::default());
        let thread_warned = warned.clone();
        #[cfg(test)]
        let thread_state = state.clone();

        #[cfg(test)]
        let thread_gate = gate.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("mink-event-log".to_string())
            .spawn(move || {
                run_writer(
                    path,
                    rx,
                    &thread_warned,
                    #[cfg(test)]
                    &thread_state,
                    #[cfg(test)]
                    thread_gate,
                )
            })
        {
            let message = format!("failed to start event log writer thread: {error}");
            warn_once(&warned, &message);
            *state
                .init_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
        }

        Self {
            tx,
            warned,
            state,
            reported: Arc::new(tokio::sync::Mutex::new(0)),
        }
    }

    /// Enqueue one JSON line. Blocks only when the bounded queue is full.
    ///
    /// Enqueue failures are counted here (the writer may not exist), and
    /// surface on the next flush as unreported loss.
    pub(crate) fn send(&self, line: String) -> bool {
        match self.tx.send(EventLogCmd::Append(line)) {
            Ok(()) => true,
            Err(_) => {
                self.state.send_lost.fetch_add(1, Ordering::SeqCst);
                warn_once(&self.warned, "event log writer is closed");
                false
            }
        }
    }

    fn enqueue_failure(&self, detail: &str) -> io::Error {
        let init = self
            .state
            .init_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let lost = self.state.send_lost.load(Ordering::SeqCst);
        io::Error::other(match init {
            Some(init) => format!("{init} ({detail}; lost {lost} event(s) before this barrier)"),
            None => format!("{detail} (lost {lost} event(s) before this barrier)"),
        })
    }

    #[cfg(test)]
    pub(crate) fn critical_queued(&self) -> u64 {
        self.state.critical_queued.load(Ordering::SeqCst) as u64
    }

    /// Deadline- and cancel-bounded reliable commit for critical events.
    ///
    /// Returns the writer's **actual write result** (open/write), not just the
    /// enqueue result, so callers never advance on a phantom success. Waits
    /// are async: a full queue or a stalled writer cannot block a tokio worker
    /// and the cancel token releases the wait immediately.
    pub(crate) async fn send_critical_async(
        &self,
        line: String,
        cancel: &crate::cancel::CancellationToken,
        interrupt: &AtomicBool,
        deadline: std::time::Duration,
    ) -> io::Result<()> {
        let started = std::time::Instant::now();
        let aborted = || cancel.is_cancelled() || interrupt.load(Ordering::SeqCst);
        let (done_tx, mut done_rx) = oneshot::channel();
        let mut pending = Some(EventLogCmd::AppendCritical {
            line,
            done: done_tx,
        });

        // Fast path: a healthy writer commits without any waiting, so a
        // pre-set interrupt does not discard instantly completable evidence.
        // Enqueue without blocking the async worker.
        loop {
            let command = pending.take().expect("pending critical command");
            match self.tx.try_send(command) {
                Ok(()) => {
                    #[cfg(test)]
                    self.state.critical_queued.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                    if aborted() {
                        return Err(io::Error::other(
                            "critical event wait aborted (cancel/interrupt) before the writer accepted it",
                        ));
                    }
                    let remaining = deadline.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(io::Error::other(format!(
                            "event log queue full; critical event not enqueued within {deadline:?}"
                        )));
                    }
                    pending = Some(returned);
                    tokio::select! {
                        biased;
                        _ = stop_requested(cancel, interrupt) => {
                            return Err(io::Error::other(
                                "critical event wait aborted (cancel/interrupt) before the writer accepted it",
                            ));
                        }
                        _ = tokio::time::sleep(remaining.min(std::time::Duration::from_millis(2))) => {}
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Err(io::Error::other(
                        "event log writer is closed; critical event not persisted",
                    ));
                }
            }
        }

        // Wait for the writer's actual result.
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(io::Error::other(
                "critical event enqueued but not acknowledged before the deadline",
            ));
        }
        // Fast path: the acknowledgement may already be available.
        match done_rx.try_recv() {
            Ok(write_result) => return write_result,
            Err(oneshot::error::TryRecvError::Closed) => {
                return Err(io::Error::other(
                    "event log writer dropped the critical acknowledgement",
                ));
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
        }
        // Let a healthy writer finish its in-flight acknowledgement before a
        // pending cancel/interrupt aborts; only a genuinely stalled writer
        // reaches the stop-aware wait below.
        if aborted() {
            match tokio::time::timeout(CRITICAL_ACK_GRACE.min(remaining), &mut done_rx).await {
                Ok(Ok(write_result)) => return write_result,
                Ok(Err(_)) => {
                    return Err(io::Error::other(
                        "event log writer dropped the critical acknowledgement",
                    ));
                }
                Err(_) => {
                    return Err(io::Error::other(
                        "critical event wait aborted (cancel/interrupt) while awaiting the writer acknowledgement",
                    ));
                }
            }
        }
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(io::Error::other(
                "event log writer did not acknowledge the critical event before the deadline",
            ));
        }
        tokio::select! {
            biased;
            _ = stop_requested(cancel, interrupt) => Err(io::Error::other(
                "critical event wait aborted (cancel/interrupt) while awaiting the writer acknowledgement",
            )),
            result = &mut done_rx => match result {
                Ok(write_result) => write_result,
                Err(_) => Err(io::Error::other(
                    "event log writer dropped the critical acknowledgement",
                )),
            },
            _ = tokio::time::sleep(remaining) => Err(io::Error::other(
                "event log writer did not acknowledge the critical event before the deadline",
            )),
        }
    }

    /// Non-blocking enqueue for async callers (turn loop, runtime events).
    ///
    /// A full queue means the writer is stalled; blocking the async worker is
    /// worse than a visible diagnostic drop, so the loss is counted and
    /// reported by the next flush like any other unreported loss.
    pub(crate) fn send_best_effort(&self, line: String) -> bool {
        match self.tx.try_send(EventLogCmd::Append(line)) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                let lost = self.state.send_lost.fetch_add(1, Ordering::SeqCst) + 1;
                warn_once(
                    &self.warned,
                    &format!(
                        "event log queue is full; dropping diagnostic event ({lost} lost so far)"
                    ),
                );
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.state.send_lost.fetch_add(1, Ordering::SeqCst);
                warn_once(&self.warned, "event log writer is closed");
                false
            }
        }
    }

    /// Write barrier: report losses that happened at or before this barrier.
    ///
    /// The writer returns a snapshot without waiting for any confirmation;
    /// the reported watermark is advanced only after the snapshot arrives, so
    /// a cancelled flush leaves its loss unreported for the next flush.
    pub(crate) async fn flush(&self) -> io::Result<()> {
        let mut reported = self.reported.lock().await;
        let started = std::time::Instant::now();
        let send_lost_before = self.state.send_lost.load(Ordering::SeqCst);
        let (done, done_rx) = oneshot::channel();
        // Never block the async worker on the bounded queue: retry with async
        // sleeps up to an internal deadline, then report a stalled writer so
        // the caller (e.g. shutdown) can apply its own budget.
        let enqueue_deadline = started + FLUSH_ENQUEUE_DEADLINE;
        let mut command = Some(EventLogCmd::Flush { done });
        loop {
            let next = command.take().expect("flush command enqueued once");
            match self.tx.try_send(next) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                    if std::time::Instant::now() >= enqueue_deadline {
                        return Err(self.enqueue_failure(
                            "event log queue is full and the writer is not draining",
                        ));
                    }
                    command = Some(returned);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Err(self.enqueue_failure("event log writer is closed"));
                }
            }
        }
        let remaining = FLUSH_ACK_DEADLINE.saturating_sub(started.elapsed());
        let ack = tokio::select! {
            ack = done_rx => {
                ack.map_err(|_| io::Error::other("event log writer dropped flush ack"))?
            }
            _ = tokio::time::sleep(remaining) => {
                return Err(io::Error::other(format!(
                    "event log flush was not acknowledged within {:?}; writer stalled",
                    FLUSH_ACK_DEADLINE
                )));
            }
        };

        let total_lost = send_lost_before.saturating_add(ack.processed_lost);
        if total_lost > *reported {
            *reported = total_lost;
            let detail = ack
                .last_loss
                .or(ack.last_failure)
                .unwrap_or_else(|| "event log write failed".to_string());
            return Err(io::Error::other(format!(
                "event log lost {total_lost} event(s) that were never reported: {detail}"
            )));
        }
        ack.result
    }
}

fn run_writer(
    path: PathBuf,
    rx: Receiver<EventLogCmd>,
    warned: &AtomicBool,
    #[cfg(test)] state: &EventLogState,
    #[cfg(test)] gate: Option<Arc<EventLogWriterGate>>,
) {
    let mut writer = WriterState::default();

    loop {
        // Paused before receiving, so a stalled writer leaves the queue
        // untouched (deterministic queue-full tests).
        #[cfg(test)]
        if let Some(gate) = &gate {
            gate.wait_if_paused();
        }
        let Ok(command) = rx.recv() else {
            break;
        };
        #[cfg(test)]
        state.processed_commands.fetch_add(1, Ordering::SeqCst);
        match command {
            EventLogCmd::Append(line) => {
                if let Err(message) = write_event(&mut writer, &path, warned, &line) {
                    // Diagnostic event: count it as a loss the writer observed.
                    writer.processed_lost += 1;
                    writer.last_loss = Some(message);
                }
            }
            EventLogCmd::AppendCritical { line, done } => {
                // Critical event: report the real open/write result back to
                // the sender; the caller decides whether to retry, so this is
                // not counted as a diagnostic loss.
                let result =
                    write_event(&mut writer, &path, warned, &line).map_err(io::Error::other);
                let _ = done.send(result);
            }
            EventLogCmd::Flush { done } => {
                let result = match writer.file.as_mut() {
                    Some(file) => file.flush(),
                    None => match writer.failure.clone() {
                        Some(message) => Err(io::Error::other(message)),
                        // No events have been written yet: flushing an empty
                        // session is a no-op, not an error.
                        None => Ok(()),
                    },
                };
                let _ = done.send(FlushAck {
                    result,
                    processed_lost: writer.processed_lost,
                    last_loss: writer.last_loss.clone(),
                    last_failure: writer.failure.clone(),
                });
            }
        }
    }

    if let Some(file) = writer.file.as_mut() {
        let _ = file.flush();
    }
}

/// Attempt one append; returns a diagnosable message on open/write failure.
///
/// The caller decides whether the failure is a counted loss (diagnostics) or
/// reported back through an acknowledgement (critical events).
fn write_event(
    writer: &mut WriterState,
    path: &std::path::Path,
    warned: &AtomicBool,
    line: &str,
) -> std::result::Result<(), String> {
    if writer.file.is_none() {
        match open_append(path) {
            Ok(file) => {
                writer.file = Some(file);
                writer.failure = None;
            }
            Err(message) => {
                warn_once(warned, &message);
                writer.failure = Some(message.clone());
                return Err(message);
            }
        }
    }
    let handle = writer.file.as_mut().expect("file opened above");
    if let Err(error) = writeln!(handle, "{line}") {
        let message = format!("failed to write event log {}: {error}", path.display());
        warn_once(warned, &message);
        writer.failure = Some(message.clone());
        // Drop the handle and retry opening on the next event instead of
        // treating one failed write as permanent.
        writer.file = None;
        return Err(message);
    }
    Ok(())
}

/// Completes as soon as the runtime cancel token fires or the current turn
/// is interrupted (the public `interrupt_current_turn()` flag). Polling the
/// atomic keeps this dependency-free and prompt (<=5 ms).
async fn stop_requested(cancel: &crate::cancel::CancellationToken, interrupt: &AtomicBool) {
    loop {
        if interrupt.load(Ordering::SeqCst) {
            return;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
        }
    }
}

fn open_append(path: &std::path::Path) -> std::result::Result<std::fs::File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("failed to open event log {}: {error}", path.display()))
}

pub(crate) fn warn_once(warned: &AtomicBool, message: &str) {
    if !warned.swap(true, Ordering::SeqCst) {
        eprintln!("[mink] Warning: {message}");
    }
}

/// Test-only stall gate: pauses one writer thread at command boundaries so
/// queue-full / shutdown-deadline behavior can be tested deterministically
/// without affecting other writers.
#[cfg(test)]
pub(crate) struct EventLogWriterGate {
    paused: Mutex<bool>,
    cv: std::sync::Condvar,
}

#[cfg(test)]
impl EventLogWriterGate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            paused: Mutex::new(false),
            cv: std::sync::Condvar::new(),
        })
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        *self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = paused;
        self.cv.notify_all();
    }

    fn wait_if_paused(&self) {
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *paused {
            paused = self
                .cv
                .wait(paused)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writer_preserves_order_and_flushes() {
        let dir = std::env::temp_dir().join(format!(
            "mink-event-log-test-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("events.jsonl");
        let writer = EventLogWriter::start(path.clone());

        for index in 0..100 {
            assert!(writer.send(format!("{{\"index\":{index}}}")));
        }
        writer.flush().await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 100);
        assert_eq!(lines[0], r#"{"index":0}"#);
        assert_eq!(lines[99], r#"{"index":99}"#);

        drop(writer);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn bounded_queue_never_drops_events() {
        let dir = std::env::temp_dir().join(format!(
            "mink-event-log-bound-test-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("events.jsonl");
        let writer = EventLogWriter::start(path.clone());

        let event_count = EVENT_LOG_QUEUE_CAPACITY * 4;
        for index in 0..event_count {
            assert!(writer.send(format!("{{\"index\":{index}}}")));
        }
        writer.flush().await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents.lines().count(), event_count);
        drop(writer);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn flush_without_any_events_is_a_noop() {
        let path = std::env::temp_dir().join(format!(
            "mink-event-log-empty-{}-{}.jsonl",
            std::process::id(),
            uuid_like()
        ));
        let writer = EventLogWriter::start(path.clone());
        writer.flush().await.unwrap();
        drop(writer);
    }

    #[tokio::test]
    async fn open_failure_is_reported_and_recovers_on_retry() {
        let root = std::env::temp_dir().join(format!(
            "mink-event-log-open-test-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        let missing_dir = root.join("missing");
        let path = missing_dir.join("events.jsonl");
        let writer = EventLogWriter::start(path.clone());

        // The parent directory does not exist yet: these events cannot be
        // written, but the writer must keep retrying instead of closing.
        assert!(writer.send(r#"{"index":"before-open"}"#.to_string()));
        let error = writer.flush().await.unwrap_err();
        assert!(error.to_string().contains("failed to open"), "{error}");

        tokio::fs::create_dir_all(&missing_dir).await.unwrap();
        assert!(writer.send(r#"{"index":"after-open"}"#.to_string()));
        writer.flush().await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!contents.contains("before-open"), "{contents}");
        assert!(contents.contains("after-open"), "{contents}");

        drop(writer);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn unreported_loss_surfaces_on_first_flush() {
        let (root, missing_dir, path, writer) = loss_scenario();

        // Wait until the writer actually processed the unopenable event.
        wait_processed(&writer, 1).await;
        tokio::fs::create_dir_all(&missing_dir).await.unwrap();
        assert!(writer.send(r#"{"index":"kept"}"#.to_string()));
        wait_processed(&writer, 2).await;

        // Recovery must not hide the earlier loss: the first flush after it
        // still reports.
        let error = writer.flush().await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("lost 1 event(s)"), "{message}");
        assert!(message.contains("failed to open"), "{message}");

        // Reported once: the next flush is clean and the file has the kept line.
        writer.flush().await.unwrap();
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(contents.contains("kept"), "{contents}");
        assert!(!contents.contains("lost"), "{contents}");

        drop(writer);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn dropped_flush_ack_does_not_consume_loss() {
        let (root, missing_dir, path, writer) = loss_scenario();
        wait_processed(&writer, 1).await;
        tokio::fs::create_dir_all(&missing_dir).await.unwrap();
        assert!(writer.send(r#"{"index":"kept"}"#.to_string()));
        wait_processed(&writer, 2).await;

        // A barrier whose ack is dropped must not advance the watermark.
        let (done, done_rx) = oneshot::channel();
        writer
            .tx
            .send(EventLogCmd::Flush { done })
            .expect("writer alive");
        drop(done_rx);
        wait_processed(&writer, 3).await;

        // The writer kept consuming while no caller confirmed.
        assert!(writer.send(r#"{"index":"after-dropped"}"#.to_string()));
        wait_processed(&writer, 4).await;

        let error = writer.flush().await.unwrap_err();
        assert!(error.to_string().contains("lost 1 event(s)"), "{error}");
        writer.flush().await.unwrap();
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(contents.contains("after-dropped"), "{contents}");

        drop(writer);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn serialized_flush_reports_once() {
        let (root, missing_dir, _path, writer) = loss_scenario();
        wait_processed(&writer, 1).await;
        tokio::fs::create_dir_all(&missing_dir).await.unwrap();
        assert!(writer.send(r#"{"index":"kept"}"#.to_string()));
        wait_processed(&writer, 2).await;

        let first = writer.flush().await;
        let second = writer.flush().await;
        assert!(first.is_err(), "{first:?}");
        assert!(second.is_ok(), "{second:?}");

        drop(writer);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn enqueue_failure_is_counted_without_writer() {
        // Receiver already gone: there is no writer to account for the loss,
        // so the send side must do it and flush must report it directly.
        let (tx, rx) = sync_channel::<EventLogCmd>(1);
        drop(rx);
        let writer = EventLogWriter {
            tx,
            warned: Arc::new(AtomicBool::new(false)),
            state: Arc::new(EventLogState::default()),
            reported: Arc::new(tokio::sync::Mutex::new(0)),
        };

        assert!(!writer.send(r#"{"index":"lost"}"#.to_string()));
        let error = writer.flush().await.unwrap_err();
        assert!(error.to_string().contains("lost 1 event(s)"), "{error}");
    }

    #[tokio::test]
    async fn writer_init_failure_is_observable() {
        let (tx, rx) = sync_channel::<EventLogCmd>(1);
        drop(rx);
        let state = Arc::new(EventLogState::default());
        *state
            .init_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some("failed to start event log writer thread: injected".to_string());
        let writer = EventLogWriter {
            tx,
            warned: Arc::new(AtomicBool::new(false)),
            state,
            reported: Arc::new(tokio::sync::Mutex::new(0)),
        };

        let error = writer.flush().await.unwrap_err();
        assert!(
            error.to_string().contains("failed to start"),
            "init failure must be observable on flush: {error}"
        );
    }

    fn loss_scenario() -> (PathBuf, PathBuf, PathBuf, EventLogWriter) {
        let root = std::env::temp_dir().join(format!(
            "mink-event-log-loss-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        let missing_dir = root.join("missing");
        let path = missing_dir.join("events.jsonl");
        let writer = EventLogWriter::start(path.clone());
        assert!(writer.send(r#"{"index":"lost"}"#.to_string()));
        (root, missing_dir, path, writer)
    }

    /// Deterministic processing barrier: waits until the writer thread has
    /// consumed `count` commands (test-only counter), with a failure timeout.
    async fn wait_processed(writer: &EventLogWriter, count: usize) {
        for _ in 0..400 {
            if writer.state.processed_commands.load(Ordering::SeqCst) >= count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "writer did not process {count} commands; processed={}",
            writer.state.processed_commands.load(Ordering::SeqCst)
        );
    }

    fn uuid_like() -> String {
        format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
    #[tokio::test]
    // Cross-runtime progress is asserted as a soft check here; the strict
    // single-thread proof lives in
    // `critical_commit_does_not_block_async_worker_when_queue_full`
    // (`current_thread` + pre-await snapshot).
    async fn flush_does_not_block_when_queue_full_and_writer_is_paused() {
        let dir = std::env::temp_dir().join(format!(
            "mink-event-log-full-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("events.jsonl");
        let gate = EventLogWriterGate::new();
        gate.set_paused(true);
        let writer = EventLogWriter::start_paused(path.clone(), gate.clone());

        // Fill the bounded queue exactly (no blocking send), with the writer stalled.
        for index in 0..EVENT_LOG_QUEUE_CAPACITY {
            assert!(writer.send(format!("{{\"index\":{index}}}")));
        }

        // flush must report instead of blocking the async worker forever
        // (test-only enqueue deadline is 200 ms).
        let started = std::time::Instant::now();
        let error = writer.flush().await.unwrap_err();
        assert!(error.to_string().contains("not draining"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "flush must not block the async worker: {:?}",
            started.elapsed()
        );

        // An independent async task keeps running while the writer is stalled.
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_task = ticks.clone();
        let ticker = tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                ticks_task.fetch_add(1, Ordering::SeqCst);
            }
        });
        ticker.await.unwrap();
        assert!(
            ticks.load(Ordering::SeqCst) >= 2,
            "runtime must keep progressing while the event-log writer is stalled"
        );

        // Release the writer: everything drains exactly once and flush recovers.
        gate.set_paused(false);
        let mut recovered = false;
        for _ in 0..400 {
            if writer.flush().await.is_ok() {
                recovered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(recovered, "flush must recover after the writer resumes");

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), EVENT_LOG_QUEUE_CAPACITY);
        let unique = lines.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            unique.len(),
            EVENT_LOG_QUEUE_CAPACITY,
            "no duplicate or lost events after recovery"
        );

        drop(writer);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn best_effort_enqueue_degrades_visibly_when_writer_is_stalled() {
        let dir = std::env::temp_dir().join(format!(
            "mink-event-log-besteffort-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("events.jsonl");
        let gate = EventLogWriterGate::new();
        gate.set_paused(true);
        let writer = EventLogWriter::start_paused(path.clone(), gate.clone());

        for index in 0..EVENT_LOG_QUEUE_CAPACITY {
            assert!(writer.send_best_effort(format!("{{\"index\":{index}}}")));
        }
        assert!(
            !writer.send_best_effort("{\"index\":\"overflow\"}".to_string()),
            "a full queue must drop the diagnostic event instead of blocking"
        );

        let error = writer.flush().await.unwrap_err();
        assert!(error.to_string().contains("lost 1 event(s)"), "{error}");

        gate.set_paused(false);
        let mut recovered = false;
        for _ in 0..400 {
            if writer.flush().await.is_ok() {
                recovered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(recovered);
        drop(writer);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn critical_commit_reports_real_write_failure() {
        let dir = std::env::temp_dir().join(format!(
            "mink-event-log-critical-io-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        let path = dir.join("missing").join("events.jsonl");
        let writer = EventLogWriter::start(path.clone());
        let cancel = crate::cancel::CancellationToken::new();

        let error = writer
            .send_critical_async(
                "{\"type\":\"prefix_snapshot\"}".to_string(),
                &cancel,
                &AtomicBool::new(false),
                CRITICAL_ENQUEUE_DEADLINE,
            )
            .await
            .expect_err("an open failure must reach the critical sender");
        assert!(error.to_string().contains("failed to open"), "{error}");

        // Recovery: create the directory and retry; the writer reopens.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        writer
            .send_critical_async(
                "{\"type\":\"prefix_snapshot\"}".to_string(),
                &cancel,
                &AtomicBool::new(false),
                CRITICAL_ENQUEUE_DEADLINE,
            )
            .await
            .expect("critical commit must succeed after recovery");
        writer.flush().await.unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents
                .lines()
                .filter(|line| line.contains("prefix_snapshot"))
                .count(),
            1,
            "the failed attempt must not leave a phantom event"
        );
        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn critical_commit_does_not_block_async_worker_when_queue_full() {
        let dir = std::env::temp_dir().join(format!(
            "mink-event-log-critical-full-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let gate = EventLogWriterGate::new();
        gate.set_paused(true);
        let writer = EventLogWriter::start_paused(path.clone(), gate.clone());
        for index in 0..EVENT_LOG_QUEUE_CAPACITY {
            assert!(writer.send_best_effort(format!("{{\"index\":{index}}}")));
        }

        // An independent async task must keep progressing while the critical
        // commit waits for capacity.
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticker_ticks = ticks.clone();
        let ticker = tokio::spawn(async move {
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                ticker_ticks.fetch_add(1, Ordering::SeqCst);
            }
        });

        let cancel = crate::cancel::CancellationToken::new();
        let started = std::time::Instant::now();
        let error = writer
            .send_critical_async(
                "{\"type\":\"prefix_snapshot\"}".to_string(),
                &cancel,
                &AtomicBool::new(false),
                CRITICAL_ENQUEUE_DEADLINE,
            )
            .await
            .expect_err("a stalled full queue must hit the deadline");
        assert!(error.to_string().contains("not enqueued within"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "critical commit deadline: {:?}",
            started.elapsed()
        );
        // Snapshot before awaiting the ticker: the count must only contain
        // progress made while the commit was waiting on the full queue.
        let progressed = ticks.load(Ordering::SeqCst);
        ticker.await.unwrap();
        assert!(
            progressed >= 5,
            "the async worker must keep progressing while the commit waits"
        );

        // Release the writer: the critical commit retries successfully.
        gate.set_paused(false);
        writer
            .send_critical_async(
                "{\"type\":\"prefix_snapshot\"}".to_string(),
                &cancel,
                &AtomicBool::new(false),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("critical commit must succeed after drain");
        writer.flush().await.unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("prefix_snapshot")
        );
        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn critical_commit_is_released_by_cancel() {
        let dir = std::env::temp_dir().join(format!(
            "mink-event-log-critical-cancel-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let gate = EventLogWriterGate::new();
        gate.set_paused(true);
        let writer = EventLogWriter::start_paused(path.clone(), gate.clone());
        for index in 0..EVENT_LOG_QUEUE_CAPACITY {
            assert!(writer.send_best_effort(format!("{{\"index\":{index}}}")));
        }

        let cancel = crate::cancel::CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_writer = writer.clone();
        let handle = tokio::spawn(async move {
            task_writer
                .send_critical_async(
                    "{\"type\":\"prefix_snapshot\"}".to_string(),
                    &task_cancel,
                    &AtomicBool::new(false),
                    std::time::Duration::from_secs(5),
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        cancel.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("cancel must release the critical commit")
            .unwrap()
            .expect_err("cancelled commit must report an error");
        assert!(error.to_string().contains("aborted"), "{error}");

        gate.set_paused(false);
        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn critical_commit_is_released_by_turn_interrupt() {
        let dir = std::env::temp_dir().join(format!(
            "mink-event-log-critical-interrupt-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let gate = EventLogWriterGate::new();
        gate.set_paused(true);
        let writer = EventLogWriter::start_paused(path.clone(), gate.clone());
        for index in 0..EVENT_LOG_QUEUE_CAPACITY {
            assert!(writer.send_best_effort(format!("{{\"index\":{index}}}")));
        }

        let cancel = crate::cancel::CancellationToken::new();
        let interrupt = Arc::new(AtomicBool::new(false));
        let task_writer = writer.clone();
        let task_interrupt = interrupt.clone();
        let handle = tokio::spawn(async move {
            task_writer
                .send_critical_async(
                    "{\"type\":\"prefix_snapshot\"}".to_string(),
                    &cancel,
                    &task_interrupt,
                    std::time::Duration::from_secs(5),
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        interrupt.store(true, Ordering::SeqCst);
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("turn interrupt must release the critical commit")
            .unwrap()
            .expect_err("interrupted commit must report an error");
        assert!(error.to_string().contains("aborted"), "{error}");

        gate.set_paused(false);
        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }
}
