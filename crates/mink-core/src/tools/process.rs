//! Child-process supervision shared by execution tools.

use anyhow::{Result, ensure};
use std::io::Read;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PROCESS_OUTPUT_CAPTURE_LIMIT: usize = 1_000_000;
const MIN_DEFAULT_TOOL_TIMEOUT_SECS: u64 = 5;

/// Shared argument parsing for the host Python and CPython WASI sandbox tools.
/// Both tools accept the same `script` / `script_file` / `timeout` surface;
/// keeping the dispatch here prevents their validation and path resolution
/// rules from drifting apart.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PythonScriptArgs {
    pub script: Option<String>,
    #[serde(default)]
    pub script_file: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
}

impl PythonScriptArgs {
    pub(crate) fn parse(input: &serde_json::Value) -> anyhow::Result<Self> {
        serde_json::from_value(input.clone()).map_err(Into::into)
    }

    /// Resolve the script body from the mutually exclusive arguments.
    pub(crate) fn resolve_script(&self, cwd: &std::path::Path) -> anyhow::Result<String> {
        match (&self.script, &self.script_file) {
            (Some(script), None) => Ok(script.clone()),
            (None, Some(path)) => {
                let full_path = if std::path::Path::new(path).is_absolute() {
                    std::path::PathBuf::from(path)
                } else {
                    cwd.join(path)
                };
                std::fs::read_to_string(&full_path).map_err(|error| {
                    anyhow::anyhow!(
                        "Failed to read script file {}: {error}",
                        full_path.display()
                    )
                })
            }
            (Some(_), Some(_)) => {
                anyhow::bail!("Error: provide either 'script' or 'script_file', not both");
            }
            (None, None) => {
                anyhow::bail!("Error: provide either 'script' or 'script_file'");
            }
        }
    }
}

/// Resolve the effective timeout for Bash/Python/custom tool execution.
///
/// `explicit` is the model-provided per-call value: 0/absent falls back to
/// `configured_default` (the global `tool_timeout`), which is clamped to
/// `5..=configured_max`. Explicit values above `configured_max` fail closed.
/// When no positive default is configured, the configured maximum is used.
pub(crate) fn resolve_tool_timeout(
    explicit: Option<u64>,
    configured_default: i32,
    configured_max: i32,
) -> Result<Duration> {
    let max = u64::try_from(configured_max).map_err(|_| {
        anyhow::anyhow!(
            "Error: tool timeout limit must be at least {MIN_DEFAULT_TOOL_TIMEOUT_SECS} seconds; got {configured_max}"
        )
    })?;
    ensure!(
        max >= MIN_DEFAULT_TOOL_TIMEOUT_SECS,
        "Error: tool timeout limit must be at least {MIN_DEFAULT_TOOL_TIMEOUT_SECS} seconds; got {configured_max}"
    );
    match explicit {
        Some(t) if t > 0 => {
            ensure!(
                t <= max,
                "Error: timeout must not exceed {max} seconds; got {t}"
            );
            Ok(Duration::from_secs(t))
        }
        _ if configured_default > 0 => Ok(Duration::from_secs(
            (configured_default as u64).clamp(MIN_DEFAULT_TOOL_TIMEOUT_SECS, max),
        )),
        _ => Ok(Duration::from_secs(max)),
    }
}

#[derive(Clone, Default)]
pub(crate) struct ProcessOutputBuffer {
    inner: Arc<Mutex<ProcessOutputBufferInner>>,
}

#[derive(Default)]
struct ProcessOutputBufferInner {
    bytes: Vec<u8>,
    truncated: bool,
}

impl ProcessOutputBuffer {
    pub(crate) fn append(&self, data: &[u8]) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let remaining = PROCESS_OUTPUT_CAPTURE_LIMIT.saturating_sub(guard.bytes.len());
        if data.len() > remaining {
            guard.bytes.extend_from_slice(&data[..remaining]);
            guard.truncated = true;
        } else {
            guard.bytes.extend_from_slice(data);
        }
    }

    pub(crate) fn to_string_lossy(&self, stream_name: &str) -> String {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::from_utf8_lossy(&guard.bytes).to_string();
        if guard.truncated {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!(
                "[... truncated {stream_name} after {PROCESS_OUTPUT_CAPTURE_LIMIT} bytes ...]"
            ));
        }
        out
    }
}

pub(crate) fn spawn_output_reader<R>(mut pipe: R, buffer: ProcessOutputBuffer) -> JoinHandle<()>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buffer.append(&chunk[..n]),
            }
        }
    })
}

/// Join output readers with a bounded grace period. A background grandchild
/// that inherited the stdout/stderr pipe can keep `read()` blocked after the
/// child exits; joining without a deadline would hang the tool call and leak
/// a blocking-pool thread. Blocked readers are detached (dropping the
/// JoinHandle detaches the thread): they exit on their own when the pipe
/// finally closes (grandchild exit). The rare case of a daemon that never
/// exits leaks one plain OS thread; accepted tradeoff vs. hanging the turn.
pub(crate) fn join_output_readers_bounded(readers: Vec<JoinHandle<()>>) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while readers.iter().any(|r| !r.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Outcome of terminating a spawned process tree.
///
/// Supervision uses this to avoid implying that all side effects stopped
/// when the process group could not actually be confirmed empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessTreeCleanup {
    /// No termination sequence ran; the child exited on its own.
    NotAttempted,
    /// The process group was confirmed gone (`ESRCH`) after termination
    /// and reaping; no group members remain.
    Confirmed,
    /// The direct child was reaped but the process group could not be
    /// confirmed empty; descendants may still be running.
    Unconfirmed,
}

/// `true` while any member of the child's process group exists.
///
/// Only `ESRCH` means "no such process group"; `EPERM` and any unexpected
/// error are treated as "still present" so cleanup never claims a group is
/// gone without evidence. Zombies count as members until reaped, so
/// liveness is re-checked after `SIGKILL` instead of assumed.
#[cfg(unix)]
fn process_group_alive(child_pid: u32) -> bool {
    let Ok(pid) = i32::try_from(child_pid) else {
        return true;
    };
    let result = unsafe { libc::kill(-pid, 0) };
    if result == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error();
    errno != Some(libc::ESRCH)
}

/// Outcome of waiting for a child process with timeout/interrupt enforcement.
pub(crate) struct ChildCompletion {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub interrupted: bool,
    /// Cleanup status of the process group after a timeout/interrupt
    /// termination. `NotAttempted` when the child exited on its own.
    pub tree_cleanup: ProcessTreeCleanup,
}

/// Wait for the child to exit (killing the process tree on timeout or
/// interrupt), then join output readers with a bounded grace period.
/// Shared by the Bash and Python tools so their wait semantics stay
/// identical; the caller supplies a label for the wait-error message.
///
/// `tree_cleanup` reports whether the process group was confirmed empty
/// after termination; callers must not imply all descendants stopped when
/// it is [`ProcessTreeCleanup::Unconfirmed`].
pub(crate) fn wait_child_with_output(
    child: &mut Child,
    readers: Vec<JoinHandle<()>>,
    timeout: Duration,
    interrupt: Option<&std::sync::atomic::AtomicBool>,
    wait_error_label: &str,
) -> anyhow::Result<ChildCompletion> {
    let start = Instant::now();
    let mut timed_out = false;
    let mut interrupted = false;
    let mut tree_cleanup = ProcessTreeCleanup::NotAttempted;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if interrupt.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)) {
                    tree_cleanup = terminate_child_process_tree(child);
                    interrupted = true;
                    break Some(130);
                }
                if start.elapsed() >= timeout {
                    tree_cleanup = terminate_child_process_tree(child);
                    timed_out = true;
                    break Some(124);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(anyhow::anyhow!("Error: {wait_error_label}: {e}")),
        }
    };
    join_output_readers_bounded(readers);
    Ok(ChildCompletion {
        exit_code,
        timed_out,
        interrupted,
        tree_cleanup,
    })
}

/// Put spawned Unix children in their own process group so timeout/cancel can
/// clean up grandchildren that keep pipes or files open.
#[cfg(unix)]
pub(crate) fn configure_child_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn configure_child_process_group(_cmd: &mut Command) {}

pub(crate) fn terminate_child_process_tree(child: &mut Child) -> ProcessTreeCleanup {
    terminate_child_process_tree_with_grace(child, Duration::from_millis(250))
}

/// Terminate the spawned process group after `SIGTERM` did not finish it,
/// then confirm the group is actually gone.
///
/// The direct child exiting during the grace window is **not** sufficient
/// evidence: grandchildren share the group and may ignore `SIGTERM`. The
/// grace window therefore ends only when the group is confirmed empty
/// (`ESRCH`), otherwise the remaining members receive `SIGKILL`, followed
/// by a bounded settle-and-confirm window (reaping a `SIGKILL`ed
/// descendant can lag the signal). Processes that escape the group via
/// `setsid` are out of scope for group-based cleanup.
pub(crate) fn terminate_child_process_tree_with_grace(
    child: &mut Child,
    grace: Duration,
) -> ProcessTreeCleanup {
    #[cfg(unix)]
    {
        let pgid = -(child.id() as i32);
        // Ask the whole group to terminate; ESRCH (already gone) is fine.
        unsafe {
            libc::kill(pgid, libc::SIGTERM);
        }
        let start = Instant::now();
        let mut child_exited = false;
        loop {
            if !child_exited {
                match child.try_wait() {
                    Ok(Some(_)) => child_exited = true,
                    Ok(None) => {}
                    // Cannot wait on the child: escalate to the group kill
                    // (preserves the pre-existing fallback behavior).
                    Err(_) => break,
                }
            }
            // Only an empty process group ends the grace window.
            if !process_group_alive(child.id()) {
                if !child_exited {
                    let _ = child.wait();
                }
                return ProcessTreeCleanup::Confirmed;
            }
            if start.elapsed() >= grace {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(pgid, libc::SIGKILL);
        }
        if !child_exited {
            let _ = child.wait();
        }
        // Bounded confirmation: do not claim cleanup while group members
        // (including zombies pending reaping) still exist.
        let settle_start = Instant::now();
        while process_group_alive(child.id()) && settle_start.elapsed() < grace {
            std::thread::sleep(Duration::from_millis(10));
        }
        if process_group_alive(child.id()) {
            ProcessTreeCleanup::Unconfirmed
        } else {
            ProcessTreeCleanup::Confirmed
        }
    }

    #[cfg(not(unix))]
    {
        let _ = child.kill();
        let _ = child.wait();
        // Descendants beyond the direct child are not tracked on this
        // platform, so cleanup cannot be confirmed.
        ProcessTreeCleanup::Unconfirmed
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
