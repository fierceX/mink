//! Optional cross-turn scheduling. A turn remains the unit of execution/recovery.
use crate::store::*;
use crate::tools::GoalToolBundle;
use anyhow::{Result, ensure};
use async_trait::async_trait;
use mink::runtime::{
    AgentOptions, AgentRuntimeHandle, EventSink, ExtensionEmitter, RuntimeError, RuntimeResult,
    SessionReader,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

mod control;
mod driver;
#[cfg(test)]
mod lifecycle_tests;
pub(crate) mod review;
#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod test_helpers;
#[cfg(test)]
mod tests;
mod verification;

/// Configuration bundle installed into [`AgentOptions`] before starting an
/// [`AgentRuntime`](mink::runtime::AgentRuntime), then bound once attached to its
/// live [`AgentRuntimeHandle`] via [`GoalEngine::attach`].
#[derive(Clone, Default)]
pub struct GoalPlugin {
    pub(crate) tools: Arc<GoalToolBundle>,
    pub(crate) verifiers: Vec<Arc<dyn GoalVerifier>>,
    pub(crate) event_sink: Option<Arc<dyn EventSink>>,
}

impl GoalPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_verifier(mut self, verifier: Arc<dyn GoalVerifier>) -> Self {
        self.verifiers.push(verifier);
        self
    }

    pub fn with_event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Register Goal tools, turn policies, and compaction checkpoint hooks
    /// onto `options`. Does not open `goal.json` (that waits until
    /// [`GoalEngine::attach`] after `SessionLease` is held).
    pub fn install_into(&self, options: &mut AgentOptions) {
        for tool in self.tools.tools() {
            options.add_tool(tool);
        }
        options.add_turn_policy(self.tools.turn_policy());
        options.add_compaction_checkpoint_provider(self.tools.checkpoint_provider());
    }
}

/// Goal lifecycle, verification, and cross-turn scheduling engine attached to an
/// [`AgentRuntimeHandle`].
#[derive(Clone)]
pub struct GoalEngine {
    pub(crate) handle: AgentRuntimeHandle,
    pub(crate) goal_driver: Arc<GoalDriver>,
    pub(crate) goal_store: Arc<GoalStore>,
    pub(crate) goal_tools: Arc<GoalToolBundle>,
}

impl GoalEngine {
    /// Load and recover the session's durable goal state (under the runtime's
    /// active `SessionLease`) and wire Goal scheduling into `handle`.
    pub fn attach(handle: AgentRuntimeHandle, plugin: GoalPlugin) -> RuntimeResult<Arc<Self>> {
        let session_dir = handle.session_dir();
        let goal_store = Arc::new(
            GoalStore::load(&session_dir, handle.persistence_fault())
                .map_err(|e| RuntimeError::Configuration(format!("{e:#}")))?,
        );
        if goal_store.snapshot().is_some() {
            let records = handle
                .usage_records()
                .map_err(|e| RuntimeError::Configuration(format!("{e:#}")))?;
            goal_store
                .reconcile_usage(&records)
                .map_err(|e| RuntimeError::Configuration(format!("{e:#}")))?;
        }
        if goal_store
            .snapshot()
            .is_some_and(|g| !g.status.is_terminal())
        {
            let (history, committed_sequence) = SessionReader::new(session_dir)
                .conversation_with_sequence()
                .map_err(|e| RuntimeError::Configuration(format!("{e:#}")))?;
            let env = goal_environment_from_parts(
                &handle.session_info().cwd,
                &handle.tool_surface_fingerprint(),
            );
            goal_store
                .recover(&env, &history, committed_sequence)
                .map_err(|e| RuntimeError::Configuration(format!("{e:#}")))?;
        }
        plugin
            .tools
            .bind_store(goal_store.clone(), handle.artifact_reader());
        let goal_driver = Arc::new(GoalDriver::new(plugin.verifiers, plugin.event_sink));
        let controller = Arc::new(GoalExternalController::new(
            goal_driver.clone(),
            goal_store.clone(),
            handle.extension_emitter(),
        ));
        handle.attach_external_controller(controller)?;
        Ok(Arc::new(Self {
            handle,
            goal_driver,
            goal_store,
            goal_tools: plugin.tools,
        }))
    }

    pub fn handle(&self) -> &AgentRuntimeHandle {
        &self.handle
    }
}

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
    pub handle: AgentRuntimeHandle,
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
    pub(crate) control: Mutex<GoalControlState>,
    /// Driver lifecycle so waiters cannot lose a wakeup: a watch keeps the
    /// current state instead of racing a check against a notification.
    changes: tokio::sync::watch::Sender<DriverState>,
    pub(crate) verifiers: Vec<Arc<dyn GoalVerifier>>,
    pub(crate) sink: Option<Arc<dyn EventSink>>,
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
        sink: Option<Arc<dyn EventSink>>,
    ) -> Self {
        let (changes, _) = tokio::sync::watch::channel(DriverState::Idle);
        Self {
            #[cfg(test)]
            drive_attempts: std::sync::atomic::AtomicU64::new(0),
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
    /// Wait until any active driver run reaches an idle, closed, or faulted state.
    pub(crate) async fn wait_for_exit(&self) {
        let mut changes = self.changes.subscribe();
        loop {
            match changes.borrow_and_update().clone() {
                DriverState::Running { .. } | DriverState::Stopping { .. } => {}
                DriverState::Idle | DriverState::Closed | DriverState::Faulted { .. } => return,
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }
}

pub(crate) struct GoalExternalController {
    goal_driver: Arc<GoalDriver>,
    goal_store: Arc<GoalStore>,
    emitter: ExtensionEmitter,
}

impl GoalExternalController {
    pub(crate) fn new(
        goal_driver: Arc<GoalDriver>,
        goal_store: Arc<GoalStore>,
        emitter: ExtensionEmitter,
    ) -> Self {
        Self {
            goal_driver,
            goal_store,
            emitter,
        }
    }
}

impl mink::runtime::ExternalController for GoalExternalController {
    fn blocks_ordinary_control(&self) -> Option<&'static str> {
        let blocked = self.goal_store.snapshot().is_some_and(|g| {
            g.status == GoalStatus::Active
                || (g.status == GoalStatus::Blocked && g.phase == GoalPhase::Verifying)
        });
        blocked.then_some("goal")
    }

    fn on_user_interrupt(&self) {
        if self
            .goal_store
            .snapshot()
            .is_some_and(|g| g.status == GoalStatus::Active)
        {
            match self.goal_store.pause_if_active("paused by user interrupt") {
                Ok(Some(goal)) => {
                    let payload = serde_json::json!({ "goal": goal });
                    self.emitter
                        .emit_extension_event(crate::events::GOAL_UPDATED_EVENT, payload.clone());
                    if goal.status.is_terminal() {
                        self.emitter
                            .emit_live_extension_event(crate::events::GOAL_FINISHED_EVENT, payload);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    control::latch_stopping_after_failed_stop_intent(
                        &self.goal_driver,
                        &format!("{error:#}"),
                    );
                    self.emitter.render_error(&format!("{error:#}"));
                }
            }
        }
    }

    fn on_shutdown_disable(&self) {
        self.goal_driver.disable();
    }

    fn is_active(&self) -> bool {
        self.goal_driver.is_active()
    }

    fn wait_exit(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.goal_driver.wait_for_exit())
    }

    fn on_shutdown_close(&self) {
        self.goal_driver.close();
    }
}

pub(crate) fn goal_environment_from_parts(cwd: &Path, surface_fingerprint: &str) -> String {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    format!("{}:{}", cwd.display(), surface_fingerprint)
}
fn command_error(error: anyhow::Error) -> RuntimeError {
    RuntimeError::Command(format!("{error:#}"))
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
            context.handle.has_reachable_tool("Bash"),
            "checker requires Bash in the resolved tool surface"
        );
        let result = context
            .handle
            .execute_scoped_tool(
                "Bash",
                serde_json::json!({"command": self.command, "timeout": self.timeout_secs}),
            )
            .await?;
        Ok(GoalVerification {
            status: if result.succeeded {
                GoalVerificationStatus::Pass
            } else if result.exit_code.is_some() {
                GoalVerificationStatus::Fail
            } else {
                GoalVerificationStatus::Inconclusive
            },
            reason: result.content,
            evidence: result.artifacts,
        })
    }
}

/// Owns one goal run. `finish` settles the lifecycle explicitly with identity;
/// `Drop` is the panic/abort fallback and never claims the run succeeded. The
/// host lease lives here so capacity is released exactly when the run ends
/// *and* no execution it left behind still owns the turn gate.
struct RunGuard {
    engine: GoalEngine,
    run_id: u64,
    goal_id: String,
    lease: Option<Box<dyn GoalRunLease>>,
    finished: bool,
}
impl RunGuard {
    fn finish(mut self, fault: Option<String>) {
        self.finished = true;
        self.engine
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
        if self.engine.handle.active_turn_id().is_none() {
            drop(lease);
            return;
        }
        let handle = self.engine.handle.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    handle.wait_turn_idle().await;
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
            self.engine.finish_goal_run(
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
