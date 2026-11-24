//! Session registry: scans the mink home, manages the active runtime map,
//! and owns the `session.lock` mutual-exclusion protocol.
//!
//! All writable runtimes and deletion share the core advisory lease. The stable lock file is
//! retained permanently; an open, exclusively locked file handle owns lease.

use crate::session::admission::{
    CapacityOwner, CapacityPool, PermitLease, ReserveError, SessionInstanceKey,
};
use crate::session::runtime::SessionRuntime;
use anyhow::{Result, anyhow};
use mink::runtime::SessionLease;
use mink::runtime::session::{self as runtime_session, SessionMetadata};
use mink::runtime::{AgentOptions, SessionPolicy};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

pub(super) const LOCK_FILE: &str = "session.lock";

mod discovery;
mod execution;
mod options;
#[cfg(test)]
mod tests;

use discovery::*;

#[derive(Debug)]
pub enum RegistryError {
    NotFound(String),
    Ambiguous(String),
    Locked(String),
    Busy(String),
    Capacity(String),
    Internal(anyhow::Error),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message)
            | Self::Ambiguous(message)
            | Self::Locked(message)
            | Self::Busy(message)
            | Self::Capacity(message) => f.write_str(message),
            Self::Internal(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<anyhow::Error> for RegistryError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl RegistryError {
    fn from_resolution(error: anyhow::Error) -> Self {
        match error.downcast_ref::<runtime_session::SessionReferenceError>() {
            Some(runtime_session::SessionReferenceError::Ambiguous(message)) => {
                Self::Ambiguous(message.clone())
            }
            Some(runtime_session::SessionReferenceError::Other(_)) | None => Self::Internal(error),
        }
    }

    fn from_lease(error: anyhow::Error) -> Self {
        let is_lock_contention = error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
        });
        if is_lock_contention {
            Self::Locked(error.to_string())
        } else {
            Self::Internal(error)
        }
    }
}

pub type RegistryResult<T> = std::result::Result<T, RegistryError>;

pub use crate::session::summary::SessionSummary;
pub(crate) use crate::session::summary::summarize_usage;

struct ActiveSession {
    runtime: Arc<SessionRuntime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct SessionLocator {
    pub(super) project_key: String,
    pub(super) id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CreateLocator {
    pub(super) project_key: String,
    pub(super) alias: String,
}

pub(super) type ScannedSession = (PathBuf, Option<SessionMetadata>, Option<SystemTime>);

pub struct Registry {
    pub(super) home: PathBuf,
    pub(super) model: String,
    active: Mutex<HashMap<SessionLocator, ActiveSession>>,
    /// The single capacity ledger shared by ordinary turns, resumed inputs
    /// and goal drivers. Every execution that spends `max_running` holds a
    /// permit for exactly as long as it runs.
    pub(crate) capacity: Arc<CapacityPool>,
    /// Per-session instance generation: closing a session bumps it so a
    /// permit from the closed instance can never be released into the
    /// reopened one.
    pub(super) generations: Mutex<HashMap<SessionLocator, u64>>,
    operation_locks: Mutex<HashMap<SessionLocator, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    create_locks: Mutex<HashMap<CreateLocator, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    pub(super) llm_backend: Option<Arc<dyn mink::runtime::LlmBackend>>,
    /// User-level agent config from `~/.minkrc`（与 CLI 同格式同文件）。
    pub(super) agent_layer: crate::session::agent_config::AgentConfig,
}

impl Registry {
    pub fn new(home: PathBuf, model: String, max_running: usize) -> Self {
        Self {
            home,
            model,
            active: Mutex::new(HashMap::new()),
            capacity: Arc::new(CapacityPool::new(max_running)),
            generations: Mutex::new(HashMap::new()),
            operation_locks: Mutex::new(HashMap::new()),
            create_locks: Mutex::new(HashMap::new()),
            llm_backend: None,
            agent_layer: crate::session::agent_config::AgentConfig::default(),
        }
    }

    /// Attach the user-level `~/.minkrc` layer parsed once at startup.
    pub fn with_agent_layer(mut self, layer: crate::session::agent_config::AgentConfig) -> Self {
        self.agent_layer = layer;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_llm_backend(
        home: PathBuf,
        model: String,
        max_running: usize,
        llm_backend: Arc<dyn mink::runtime::LlmBackend>,
    ) -> Self {
        let mut registry = Self::new(home, model, max_running);
        registry.llm_backend = Some(llm_backend);
        registry
    }

    /// Scan all sessions under `~/.mink/projects/<project_key>/<id>` across
    /// every project (workspace), not just the current cwd's project — the
    /// three-pane UI groups sessions by workspace directory.
    pub async fn open(&self, id: &str, project: Option<&str>) -> RegistryResult<()> {
        self.open_inner(id, project).await
    }

    async fn open_inner(&self, id: &str, project: Option<&str>) -> RegistryResult<()> {
        let dir = self.session_dir(id, project).await?;
        let locator = locator_from_dir(&dir, id);
        let operation_lock = self.operation_lock(&locator);
        let _operation = operation_lock.lock().await;
        // 锁内只做“决定”，所有 .await 都在锁外（std MutexGuard 是 !Send，
        // 且词法层面“可能被后续分支使用”会让 future 非 Send）。
        {
            let mut active = self.lock_active_state()?;
            match active.get(&locator).map(|session| session.runtime.phase()) {
                Some(crate::session::runtime::RuntimePhase::Idle)
                | Some(crate::session::runtime::RuntimePhase::Running)
                | Some(crate::session::runtime::RuntimePhase::Cancelling) => return Ok(()),
                Some(crate::session::runtime::RuntimePhase::Closing) => {
                    return Err(RegistryError::Busy(format!(
                        "session {id} runtime is closing"
                    )));
                }
                Some(crate::session::runtime::RuntimePhase::Closed) => {
                    active.remove(&locator);
                }
                None => {}
            }
        }
        if !dir.is_dir() {
            return Err(RegistryError::NotFound(format!("session {id} not found")));
        }
        // 使用 session 自身的 cwd（跨工作区打开的关键：UseOrCreate 需要在
        // 正确的 project 布局下解析，而不是服务启动目录）。
        let session_cwd = session_cwd_from_dir(&dir).map_err(RegistryError::Internal)?;
        let runtime = SessionRuntime::open(
            self.build_options(id, &session_cwd)?
                .with_session(SessionPolicy::Resume(id.to_string())),
        )
        .await
        .map_err(RegistryError::from_lease)?;
        let actual_id = runtime.session_id();
        // 校验与插入顺序：先验证实际解析出的 id，再入 active——
        // 校验失败时不得留下不一致状态。
        if actual_id != id {
            let _ = runtime.shutdown().await;
            return Err(RegistryError::Internal(anyhow!(
                "opened session {actual_id} instead of {id}"
            )));
        }

        // 同一 locator 的 operation lock 覆盖最终 recheck、构造与 lease 转移。
        {
            let mut active = self.lock_active_state()?;
            active.insert(
                locator,
                ActiveSession {
                    runtime: Arc::new(runtime),
                },
            );
        }
        Ok(())
    }

    /// Submit a user input on an open session. The turn runs on its own task;
    /// events land in `events.jsonl` via the core (same file as the TUI).
    /// 获取活动 runtime 的事件 receiver（SSE 订阅；Arc 在取 receiver 后释放）。
    pub fn active_runtime(
        &self,
        id: &str,
        project: Option<&str>,
    ) -> RegistryResult<Option<Arc<SessionRuntime>>> {
        let locator = self.active_locator(id, project)?;
        let Some(locator) = locator else {
            return Ok(None);
        };
        let active = self.lock_active_state()?;
        Ok(active.get(&locator).map(|a| a.runtime.clone()))
    }

    pub(super) fn active_locator(
        &self,
        id: &str,
        project: Option<&str>,
    ) -> RegistryResult<Option<SessionLocator>> {
        let active = self.lock_active_state()?;
        let candidates = active
            .keys()
            .filter(|locator| {
                locator.id == id && project.is_none_or(|project| locator.project_key == project)
            })
            .cloned()
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [] => Ok(None),
            [locator] => Ok(Some(locator.clone())),
            many => Err(RegistryError::Ambiguous(format!(
                "session {id} is ambiguous across projects: {}",
                many.iter()
                    .map(|locator| locator.project_key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    pub(super) fn required_active_locator(
        &self,
        id: &str,
        project: Option<&str>,
    ) -> RegistryResult<SessionLocator> {
        self.active_locator(id, project)?
            .ok_or_else(|| RegistryError::NotFound(format!("session {id} is not open")))
    }

    pub async fn close(&self, id: &str, project: Option<&str>) -> RegistryResult<()> {
        self.close_inner(id, project).await
    }

    async fn close_inner(&self, id: &str, project: Option<&str>) -> RegistryResult<()> {
        // Web clients always provide the project key. Constructing the locator
        // before consulting `active` lets close queue behind an in-flight open
        // on the same per-session operation lock.
        let locator = match project {
            Some(project_key) => SessionLocator {
                project_key: project_key.to_string(),
                id: id.to_string(),
            },
            None => self.required_active_locator(id, None)?,
        };
        let operation_lock = self.operation_lock(&locator);
        let _operation = operation_lock.lock().await;
        let runtime = {
            let active = self.lock_active_state()?;
            let session = active
                .get(&locator)
                .ok_or_else(|| RegistryError::NotFound(format!("session {id} is not open")))?;
            session.runtime.clone()
        };
        let shutdown_result = runtime.shutdown().await;
        self.lock_active_state()?.remove(&locator);
        // A permit from the closed instance must never be released into the
        // reopened one: give the next open a new generation.
        if let Ok(mut generations) = self.generations.lock() {
            let entry = generations.entry(locator).or_insert(1);
            *entry = entry.saturating_add(1);
        }
        shutdown_result.map_err(RegistryError::Internal)
    }

    /// Fail-closed error for poisoned registry state locks.
    ///
    /// `active` / `operation_locks` / `create_locks` guard active runtimes and
    /// leases: after a panic mid-update their invariants cannot be trusted, so
    /// callers that can report an error must refuse the operation instead of
    /// continuing on unverified state.
    fn poisoned(&self, area: &str) -> RegistryError {
        RegistryError::Internal(anyhow::anyhow!(
            "{area} lock poisoned by a previous panic; restart the server to recover"
        ))
    }

    /// Fallible state lock used by operations that can report an error.
    fn lock_active_state(
        &self,
    ) -> RegistryResult<std::sync::MutexGuard<'_, HashMap<SessionLocator, ActiveSession>>> {
        self.active
            .lock()
            .map_err(|_| self.poisoned("session registry state"))
    }

    pub(super) fn operation_lock(&self, locator: &SessionLocator) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .operation_locks
            .lock()
            .unwrap_or_else(|_| panic!("operation lock table poisoned; restart the server"));
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(locator).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(locator.clone(), Arc::downgrade(&lock));
        lock
    }

    pub(super) fn create_lock(&self, locator: &CreateLocator) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .create_locks
            .lock()
            .unwrap_or_else(|_| panic!("create lock table poisoned; restart the server"));
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(locator).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(locator.clone(), Arc::downgrade(&lock));
        lock
    }

    pub(super) fn active_status(&self, locator: &SessionLocator) -> Option<&'static str> {
        // Infallible signature: a poisoned state lock is unrecoverable here,
        // so fail fast with an explicit message instead of continuing.
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|_| panic!("session registry state lock poisoned; restart the server"));
        match active.get(locator).map(|session| session.runtime.phase()) {
            Some(crate::session::runtime::RuntimePhase::Running)
            | Some(crate::session::runtime::RuntimePhase::Cancelling)
            | Some(crate::session::runtime::RuntimePhase::Closing) => Some("running"),
            Some(crate::session::runtime::RuntimePhase::Idle) => Some("active"),
            Some(crate::session::runtime::RuntimePhase::Closed) => {
                active.remove(locator);
                None
            }
            None => None,
        }
    }

    pub async fn delete(&self, id: &str, project: Option<&str>) -> RegistryResult<()> {
        self.delete_inner(id, project).await
    }

    async fn delete_inner(&self, id: &str, project: Option<&str>) -> RegistryResult<()> {
        let dir = self.session_dir(id, project).await?;
        let locator = locator_from_dir(&dir, id);
        let operation_lock = self.operation_lock(&locator);
        let _operation = operation_lock.lock().await;
        let active_session = self
            .lock_active_state()?
            .get(&locator)
            .map(|session| session.runtime.clone());
        if let Some(runtime) = active_session {
            runtime.shutdown().await.map_err(RegistryError::Internal)?;
            self.lock_active_state()?.remove(&locator);
        }
        if !dir.is_dir() {
            return Err(RegistryError::NotFound(format!("session {id} not found")));
        }

        // `active` only describes runtimes owned by this Registry instance. A
        // different server process (or another Registry in the same process)
        // may still have the session open and own the advisory file lock. Take
        // the lease here, after any locally owned runtime has shut down, so the
        // destructive operation is protected by the same cross-process gate as
        // open(). Keep the file handle alive through remove_dir_all: releasing
        // it before deletion would introduce a lock-to-delete TOCTOU window.
        let _delete_lease =
            SessionLease::acquire(dir.join(LOCK_FILE)).map_err(RegistryError::from_lease)?;
        tokio::task::spawn_blocking(move || fs::remove_dir_all(dir))
            .await
            .map_err(|error| RegistryError::Internal(error.into()))?
            .map_err(|error| RegistryError::Internal(error.into()))?;
        self.generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&locator);
        Ok(())
    }

    pub fn is_open(&self, id: &str, project: Option<&str>) -> RegistryResult<bool> {
        Ok(self.active_locator(id, project)?.is_some())
    }

    pub fn running(&self, id: &str, project: Option<&str>) -> RegistryResult<bool> {
        let locator = self.active_locator(id, project)?;
        Ok(locator
            .and_then(|locator| {
                // Pure-read status: recover a poisoned lock instead of
                // panicking the request path.
                let guard = self
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.get(&locator).map(|a| a.runtime.running())
            })
            .unwrap_or(false))
    }

    pub fn closed_session_ids(&self) -> Vec<(String, String)> {
        let guard = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .iter()
            .filter(|(_, session)| session.runtime.closed())
            .map(|(locator, _)| (locator.id.clone(), locator.project_key.clone()))
            .collect()
    }

    pub fn idle_session_ids(&self, minimum_idle: Duration) -> Vec<(String, String)> {
        let guard = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .iter()
            .filter(|(_, session)| {
                session
                    .runtime
                    .idle_for()
                    .is_some_and(|idle| idle >= minimum_idle)
            })
            .map(|(locator, _)| (locator.id.clone(), locator.project_key.clone()))
            .collect()
    }

    pub async fn shutdown_all(&self) -> Result<()> {
        let ids = self
            .lock_active_state()?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let results = futures::future::join_all(
            ids.iter()
                .map(|locator| self.close(&locator.id, Some(&locator.project_key))),
        )
        .await;
        let errors = ids
            .into_iter()
            .zip(results)
            .filter_map(|(locator, result)| {
                result
                    .err()
                    .map(|error| format!("{}: {error:#}", locator.id))
            })
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("failed to close sessions: {}", errors.join("; "))
        }
    }
}
