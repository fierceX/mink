//! Supervision tests for process-tree termination.
//!
//! Every test that spawns descendants installs [`ProcessTreeGuard`] so a
//! failed assertion never leaves a stray grandchild behind.

use super::*;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mink-process-tree-{}-{name}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// `true` while `pid` exists (including a zombie that is not yet reaped).
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
fn wait_for_pid_file(path: &std::path::Path, timeout: Duration) -> Option<i32> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Fallback cleanup: kills the grandchild and the whole group on drop.
#[cfg(unix)]
struct ProcessTreeGuard {
    pgid: i32,
    grandchild: Option<i32>,
}

#[cfg(unix)]
impl ProcessTreeGuard {
    fn new(child_pid: u32) -> Self {
        Self {
            pgid: child_pid as i32,
            grandchild: None,
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.grandchild {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        unsafe {
            libc::kill(-self.pgid, libc::SIGKILL);
        }
    }
}

/// Spawn a direct child that exits on SIGTERM while a grandchild in the same
/// process group ignores SIGTERM and keeps running.
#[cfg(unix)]
fn spawn_term_ignoring_grandchild(dir: &std::path::Path) -> (Child, ProcessTreeGuard, i32) {
    let marker = dir.join("grandchild.pid");
    let command = format!(
        "sh -c 'echo $$ > \"{}\"; trap \"\" TERM; while :; do sleep 1; done' & exec sleep 30",
        marker.display()
    );
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_child_process_group(&mut cmd);
    let child = cmd.spawn().expect("spawn test child");
    let mut guard = ProcessTreeGuard::new(child.id());
    let grandchild = wait_for_pid_file(&marker, Duration::from_secs(5))
        .expect("grandchild pid marker was not written");
    guard.grandchild = Some(grandchild);
    (child, guard, grandchild)
}

#[cfg(unix)]
#[test]
fn parent_exit_does_not_skip_group_kill_for_term_ignoring_grandchild() {
    let dir = temp_dir("term-ignoring");
    let (mut child, _guard, grandchild) = spawn_term_ignoring_grandchild(&dir);

    let cleanup = terminate_child_process_tree_with_grace(&mut child, Duration::from_millis(500));

    assert!(
        !pid_alive(grandchild),
        "grandchild that ignored SIGTERM must be killed before termination returns"
    );
    assert_eq!(cleanup, ProcessTreeCleanup::Confirmed);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn group_exit_on_sigterm_confirms_cleanup_without_full_grace() {
    let dir = temp_dir("clean-exit");
    let mut cmd = Command::new("sleep");
    cmd.arg("30").stdout(Stdio::null()).stderr(Stdio::null());
    configure_child_process_group(&mut cmd);
    let mut child = cmd.spawn().expect("spawn sleep");
    let _guard = ProcessTreeGuard::new(child.id());

    let start = Instant::now();
    let cleanup = terminate_child_process_tree_with_grace(&mut child, Duration::from_millis(500));

    assert_eq!(cleanup, ProcessTreeCleanup::Confirmed);
    assert!(
        start.elapsed() < Duration::from_millis(450),
        "an empty group must end the grace window early, took {:?}",
        start.elapsed()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn timeout_path_kills_term_ignoring_grandchild() {
    let dir = temp_dir("timeout");
    let (mut child, _guard, grandchild) = spawn_term_ignoring_grandchild(&dir);

    let completion = wait_child_with_output(
        &mut child,
        Vec::new(),
        Duration::from_millis(400),
        None,
        "test wait",
    )
    .expect("wait failed");

    assert!(completion.timed_out);
    assert_eq!(completion.exit_code, None);
    assert!(!pid_alive(grandchild), "timeout must stop the whole group");
    assert_eq!(completion.tree_cleanup, ProcessTreeCleanup::Confirmed);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn interrupt_path_kills_term_ignoring_grandchild() {
    let dir = temp_dir("interrupt");
    let (mut child, _guard, grandchild) = spawn_term_ignoring_grandchild(&dir);
    let interrupt = Arc::new(AtomicBool::new(false));
    let setter = {
        let interrupt = interrupt.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            interrupt.store(true, Ordering::SeqCst);
        })
    };

    let completion = wait_child_with_output(
        &mut child,
        Vec::new(),
        Duration::from_secs(30),
        Some(interrupt.as_ref()),
        "test wait",
    )
    .expect("wait failed");
    setter.join().expect("interrupt setter panicked");

    assert!(completion.interrupted);
    assert_eq!(completion.exit_code, None);
    assert!(
        !pid_alive(grandchild),
        "interrupt must stop the whole group"
    );
    assert_eq!(completion.tree_cleanup, ProcessTreeCleanup::Confirmed);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn natural_exit_reports_not_attempted() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("exit 7")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_child_process_group(&mut cmd);
    let mut child = cmd.spawn().expect("spawn sh");

    let completion = wait_child_with_output(
        &mut child,
        Vec::new(),
        Duration::from_secs(5),
        None,
        "test wait",
    )
    .expect("wait failed");

    assert_eq!(completion.exit_code, Some(7));
    assert!(!completion.timed_out);
    assert!(!completion.interrupted);
    assert_eq!(completion.tree_cleanup, ProcessTreeCleanup::NotAttempted);
}

#[test]
fn child_completion_termination_prefers_supervision_facts() {
    let completion = |exit_code, signal, timed_out, interrupted| ChildCompletion {
        exit_code,
        signal,
        timed_out,
        interrupted,
        tree_cleanup: ProcessTreeCleanup::NotAttempted,
    };
    assert_eq!(
        completion(Some(130), None, false, true).termination(),
        ProcessTermination::Interrupted
    );
    assert_eq!(
        completion(Some(124), None, true, false).termination(),
        ProcessTermination::TimedOut
    );
    assert_eq!(
        completion(None, Some(15), false, false).termination(),
        ProcessTermination::Signaled(15)
    );
    assert_eq!(
        completion(Some(2), None, false, false).termination(),
        ProcessTermination::Exited(2)
    );
    // A signal that arrived with the timeout flag still reports the timeout:
    // the supervision fact that mink enforced the deadline wins.
    assert_eq!(
        completion(None, Some(9), true, false).termination(),
        ProcessTermination::TimedOut
    );
}
