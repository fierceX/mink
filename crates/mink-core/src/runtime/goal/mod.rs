//! Optional cross-turn scheduling. A turn remains the unit of execution/recovery.
use crate::runtime::{AgentRuntimeHandle, RuntimeError, RuntimeResult};
use crate::session::goal::*;
use anyhow::{Result, ensure};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod control;
mod driver;
pub(crate) mod review;
#[cfg(test)]
mod tests;
mod verification;

/// Maximum wall-clock time for one independent review attempt.
pub(crate) const GOAL_REVIEW_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalContinuation {
    pub goal_id: String,
    pub contract_revision: u64,
    pub iteration: u32,
}

/// Host-owned execution capacity for one goal run.
///
/// The host wraps its capacity permit in a lease; core only keeps the lease
/// alive for exactly as long as the run owns scheduling, and drops it when
/// the run ends. Core never inspects it, so the host's accounting stays its
/// own and a rejected activation destroys the lease instead of stranding it.
pub trait GoalRunLease: Send + Sync {}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalVerificationStatus {
    Pass,
    Fail,
    Inconclusive,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalVerification {
    pub status: GoalVerificationStatus,
    pub reason: String,
    pub evidence: Vec<String>,
}
#[derive(Clone)]
pub struct GoalVerificationContext {
    pub cwd: PathBuf,
    pub goal: GoalSnapshot,
    pub cancelled: Arc<AtomicBool>,
    pub(crate) tools: Arc<crate::context::ToolContext>,
}
#[async_trait]
pub trait GoalVerifier: Send + Sync {
    /// Stable identity including configuration. Changed identities cannot resume acceptance.
    fn id(&self) -> &str;
    async fn verify(&self, context: GoalVerificationContext) -> Result<GoalVerification>;
}
/// Lifecycle of the goal driver.
///
/// `Running`/`Stopping` mean a worker still owns scheduling. `Faulted` is the
/// stable terminal state of an unrecoverable run: it survives even when the
/// failed status could not be persisted, and blocks new activation until the
/// session is reopened. `Idle` is the only state a new run may start from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DriverState {
    Idle,
    Running {
        run_id: u64,
        goal_id: String,
    },
    Stopping {
        run_id: u64,
        goal_id: String,
        reason: String,
    },
    Faulted {
        run_id: u64,
        goal_id: String,
        error: String,
    },
    Closed,
}

pub(crate) struct GoalControlState {
    pub(crate) enabled: bool,
    next_run_id: u64,
    pub(crate) driver: DriverState,
}

pub(crate) struct GoalDriver {
    /// Test-only observation of how many drive attempts this driver made.
    /// A failed run must stop scheduling instead of restarting while the
    /// on-disk goal still reads active.
    #[cfg(test)]
    pub(crate) drive_attempts: std::sync::atomic::AtomicU64,
    pub(crate) admission: Mutex<()>,
    pub(crate) control: Mutex<GoalControlState>,
    /// Driver lifecycle so waiters cannot lose a wakeup: a watch keeps the
    /// current state instead of racing a check against a notification.
    changes: tokio::sync::watch::Sender<DriverState>,
    pub(crate) verifiers: Vec<Arc<dyn GoalVerifier>>,
    pub(crate) sink: Option<Arc<dyn crate::runtime::EventSink>>,
    /// Whether a goal turn is executing right now.
    ///
    /// Hosts that drive a session need this to report the session as running:
    /// a goal turn never passes through the ordinary input path, so it is
    /// invisible from the outside until core says so.
    pub(crate) turn_active: Arc<tokio::sync::watch::Sender<bool>>,
}
impl GoalDriver {
    pub(crate) fn new(
        verifiers: Vec<Arc<dyn GoalVerifier>>,
        sink: Option<Arc<dyn crate::runtime::EventSink>>,
    ) -> Self {
        let (changes, _) = tokio::sync::watch::channel(DriverState::Idle);
        Self {
            #[cfg(test)]
            drive_attempts: std::sync::atomic::AtomicU64::new(0),
            admission: Mutex::new(()),
            control: Mutex::new(GoalControlState {
                enabled: false,
                next_run_id: 1,
                driver: DriverState::Idle,
            }),
            changes,
            turn_active: Arc::new(tokio::sync::watch::Sender::new(false)),
            verifiers,
            sink,
        }
    }

    pub(crate) fn lock_control(&self) -> std::sync::MutexGuard<'_, GoalControlState> {
        self.control.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn state(&self) -> DriverState {
        self.lock_control().driver.clone()
    }

    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self.state(),
            DriverState::Running { .. } | DriverState::Stopping { .. }
        )
    }

    /// Stop accepting new runs. The current run exits through cancellation.
    pub(crate) fn disable(&self) {
        self.lock_control().enabled = false;
    }

    /// Terminal lifecycle state: no run may start and waiters observe exit.
    pub(crate) fn close(&self) {
        let mut control = self.lock_control();
        if !matches!(control.driver, DriverState::Closed) {
            control.driver = DriverState::Closed;
            let _ = self.changes.send_replace(DriverState::Closed);
        }
    }

    fn set_driver_locked(&self, control: &mut GoalControlState, state: DriverState) {
        control.driver = state.clone();
        let _ = self.changes.send_replace(state);
    }
}
pub(crate) fn goal_environment(
    cwd: &Path,
    surface: &crate::tools::surface::ModelToolSurface,
) -> String {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    format!("{}:{}", cwd.display(), surface.fingerprint())
}
fn command_error(error: anyhow::Error) -> RuntimeError {
    RuntimeError::Command(format!("{error:#}"))
}
pub(crate) fn goal_projection_chars(window: usize) -> usize {
    if window == 0 {
        8192
    } else {
        (window / 8).clamp(128, 8192)
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn hash_file(path: &Path) -> Result<String> {
    use std::io::Read;
    ensure!(
        std::fs::metadata(path)?.is_file(),
        "evidence must be a regular file"
    );
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// A user-declared shell checker. Dispatch uses the session's Bash surface,
/// approval policy, cancellation, timeout and artifact output protection.
pub struct CommandGoalVerifier {
    id: String,
    command: String,
    timeout_secs: u64,
}
impl CommandGoalVerifier {
    pub fn new(command: impl Into<String>, timeout_secs: u64) -> Result<Self> {
        let command = command.into();
        ensure!(
            !command.trim().is_empty() && timeout_secs > 0,
            "checker command and timeout must be nonempty/positive"
        );
        let id = format!(
            "command-{}",
            hash_bytes(&serde_json::to_vec(&(command.as_str(), timeout_secs))?)
        );
        Ok(Self {
            id,
            command,
            timeout_secs,
        })
    }
}
#[async_trait]
impl GoalVerifier for CommandGoalVerifier {
    fn id(&self) -> &str {
        &self.id
    }
    async fn verify(&self, context: GoalVerificationContext) -> Result<GoalVerification> {
        ensure!(
            context.tools.tool_surface.has("Bash"),
            "checker requires Bash in the resolved tool surface"
        );
        let call = crate::protocol::ToolCallEvent {
            id: "goal-check".into(),
            name: "Bash".into(),
            input_json: serde_json::json!({"command":self.command,"timeout":self.timeout_secs}),
            fields: std::collections::BTreeMap::from([
                ("command".into(), self.command.clone()),
                ("timeout".into(), self.timeout_secs.to_string()),
            ]),
            parse_error: None,
            raw_arguments_digest: None,
        };
        let results = crate::tools::runner::ToolRunner::new(context.tools)
            .execute_all(vec![call])
            .await?;
        let result = &results[0];
        Ok(GoalVerification {
            status: if result.succeeded() {
                GoalVerificationStatus::Pass
            } else if result.exit_code.is_some() {
                GoalVerificationStatus::Fail
            } else {
                GoalVerificationStatus::Inconclusive
            },
            reason: result.content.clone(),
            evidence: result
                .artifacts
                .iter()
                .map(|a| format!("artifact://{}", a.id))
                .collect(),
        })
    }
}

/// Owns one goal run. `finish` settles the lifecycle explicitly with identity;
/// `Drop` is the panic/abort fallback and never claims the run succeeded. The
/// host lease lives here so capacity is released exactly when the run ends
/// *and* no execution it left behind still owns the turn gate.
struct RunGuard {
    handle: AgentRuntimeHandle,
    run_id: u64,
    goal_id: String,
    lease: Option<Box<dyn GoalRunLease>>,
    finished: bool,
}
impl RunGuard {
    fn finish(mut self, fault: Option<String>) {
        self.finished = true;
        self.handle
            .finish_goal_run(self.run_id, &self.goal_id, fault);
        self.hand_off_lease();
    }

    /// Release the host lease only once nothing owns the turn gate anymore.
    ///
    /// A paused or cancelled run leaves its drive loop while a checker (or an
    /// interrupted tool) can still be exiting and holding its `TurnPermit`.
    /// Host capacity must outlive that execution, so the lease is handed to a
    /// holder task that drops it when the gate goes idle. The holder blocks no
    /// caller (`wait_goal` returns as soon as the driver settles), and the
    /// execution it waits for is already stopping: pause/cancel set the turn
    /// interrupt and the acceptance/execution deadlines bound it, so the wait
    /// ends with the existing ownership signals rather than outliving them.
    fn hand_off_lease(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        if self.handle.active_turn_id().is_none() {
            drop(lease);
            return;
        }
        let gate = self.handle.turn_gate.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    gate.wait_idle().await;
                    drop(lease);
                });
            }
            // No async context to wait in: execution cannot make progress
            // without one, so the lease has nothing left to outlive.
            Err(_) => drop(lease),
        }
    }
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        // The host lease is never inspected, only kept alive for exactly as
        // long as the run owns execution: release it here. Unlike `finish`,
        // a drop can run while the async runtime is tearing down (task
        // cancellation), where spawning a holder is not possible; the run
        // aborted with no scheduled exit left to outlive.
        drop(self.lease.take());
        if !self.finished {
            self.handle.finish_goal_run(
                self.run_id,
                &self.goal_id,
                Some("goal runner exited unexpectedly".into()),
            );
        }
    }
}

/// Publishes `false` for goal-turn activity when the driving scope ends,
/// including on error and cancellation paths.
struct TurnActivityGuard {
    active: Arc<tokio::sync::watch::Sender<bool>>,
}
impl Drop for TurnActivityGuard {
    fn drop(&mut self) {
        self.active.send_replace(false);
    }
}
