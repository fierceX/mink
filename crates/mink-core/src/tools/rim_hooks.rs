//! Reversible extension registries, lifecycle teardown, and closed policy
//! hook traits (`ToolExecutionGate`, `ToolOutputFilter`, `RuntimePlugin`).

use crate::cancel::CancellationToken;
use crate::events::EventLog;
use crate::tools::metadata::ApprovalTier;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// RAII handle that unregisters a runtime-attached hook or event sink on drop.
pub struct RegistrationGuard {
    unregister: Option<Box<dyn FnOnce() + Send>>,
}

impl RegistrationGuard {
    pub(crate) fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self {
            unregister: Some(Box::new(f)),
        }
    }

    /// Construct a no-op guard that performs no action on drop.
    pub fn noop() -> Self {
        Self { unregister: None }
    }

    /// Explicitly run the unregistration callback immediately.
    pub fn unregister(mut self) {
        if let Some(f) = self.unregister.take() {
            f();
        }
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        if let Some(f) = self.unregister.take() {
            f();
        }
    }
}

/// Instance-scoped lifecycle state and LIFO teardown stack.
///
/// Owned by a single `AgentRuntime` instance (`AgentSharedContext`), ensuring
/// strict isolation across concurrent sessions in the same process.
type TeardownCallback = Box<dyn FnOnce() + Send>;

#[derive(Default)]
pub(crate) struct RuntimeLifecycle {
    closing: AtomicBool,
    disposed: AtomicBool,
    teardowns: Mutex<Vec<(String, TeardownCallback)>>,
}

impl RuntimeLifecycle {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn mark_closing(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    pub(crate) fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    /// Register a teardown callback. If the lifecycle is already disposed, the
    /// callback runs immediately outside the lock so late registrations never leak.
    pub(crate) fn register_teardown(
        &self,
        label: impl Into<String>,
        f: impl FnOnce() + Send + 'static,
    ) {
        let label = label.into();
        let boxed: Box<dyn FnOnce() + Send> = Box::new(f);
        let run_now = {
            let mut guard = self
                .teardowns
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.disposed.load(Ordering::SeqCst) {
                Some(boxed)
            } else {
                guard.push((label, boxed));
                None
            }
        };
        if let Some(callback) = run_now {
            callback();
        }
    }

    /// Idempotently execute all registered teardown callbacks in strict LIFO order
    /// outside the mutex.
    pub(crate) fn dispose(&self) {
        self.closing.store(true, Ordering::SeqCst);
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let entries = {
            let mut guard = self
                .teardowns
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *guard)
        };
        for (_label, callback) in entries.into_iter().rev() {
            callback();
        }
    }

    #[allow(dead_code)]
    pub(crate) fn teardown_count(&self) -> usize {
        self.teardowns
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

/// Multi-subscriber, lock-free-dispatch registry for `SessionEventSink`.
#[derive(Default)]
pub(crate) struct SessionEventSinkRegistry {
    sinks: RwLock<Vec<(u64, Arc<dyn crate::runtime::SessionEventSink>)>>,
    next_token: AtomicU64,
}

impl SessionEventSinkRegistry {
    pub(crate) fn new(initial: Vec<Arc<dyn crate::runtime::SessionEventSink>>) -> Self {
        let mut entries = Vec::with_capacity(initial.len());
        let mut next = 1u64;
        for sink in initial {
            entries.push((next, sink));
            next += 1;
        }
        Self {
            sinks: RwLock::new(entries),
            next_token: AtomicU64::new(next),
        }
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        sink: Arc<dyn crate::runtime::SessionEventSink>,
    ) -> RegistrationGuard {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .sinks
                .write()
                .unwrap_or_else(|error| error.into_inner());
            guard.push((token, sink));
        }
        let weak = Arc::downgrade(self);
        RegistrationGuard::new(move || {
            if let Some(registry) = weak.upgrade() {
                let mut guard = registry
                    .sinks
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                guard.retain(|(entry_token, _)| *entry_token != token);
            }
        })
    }

    /// Snapshot registered sinks under a short read lock, drop the lock, and
    /// dispatch each sink outside any lock.
    pub(crate) fn notify(&self, session_id: &str, event: &EventLog) {
        let snapshot: Vec<Arc<dyn crate::runtime::SessionEventSink>> = {
            let guard = self.sinks.read().unwrap_or_else(|error| error.into_inner());
            if guard.is_empty() {
                return;
            }
            guard.iter().map(|(_, sink)| Arc::clone(sink)).collect()
        };
        for sink in snapshot {
            if sink.wants(session_id, event) {
                sink.on_session_event(session_id, event);
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.sinks
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

/// Request metadata handed to a [`ToolExecutionGate`].
#[derive(Debug, Clone)]
pub struct ToolGateRequest<'a> {
    pub session_id: &'a str,
    pub call_id: &'a str,
    pub tool_name: &'a str,
    pub input: &'a serde_json::Value,
    pub cwd: &'a Path,
    pub is_sub_agent: bool,
    pub is_ptc_nested: bool,
    pub approval_tier: ApprovalTier,
    pub mutating: bool,
}

/// Decision returned by a [`ToolExecutionGate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGateDecision {
    Allow,
    Deny { code: String, reason: String },
}

/// Asynchronous pre-execution policy gate for tool calls (covering direct
/// `ToolRunner::execute_all`, nested PTC `RunCode` sub-calls, and inherited
/// sub-agent sessions).
///
/// Gate failures, timeouts, and closing states fail closed.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait ToolExecutionGate: Send + Sync {
    /// Stable policy identifier recorded in `EventLog::PolicyEffect` and
    /// `_mink.policy_effect`.
    fn id(&self) -> &str;

    /// Per-gate execution timeout budget (default: 30 seconds).
    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    /// Evaluate a tool call before side-effect journaling or dispatch.
    async fn check(
        &self,
        req: &ToolGateRequest<'_>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<ToolGateDecision>;
}

/// Metadata passed to [`ToolOutputFilter`].
#[derive(Debug, Clone)]
pub struct ToolOutputFilterContext<'a> {
    pub session_id: &'a str,
    pub call_id: &'a str,
    pub tool_name: &'a str,
    pub is_sub_agent: bool,
    pub is_ptc_nested: bool,
    pub succeeded: bool,
}

/// Mutable tool output payload inspected by [`ToolOutputFilter`] before any
/// artifact spill or conversation persistence occurs.
#[derive(Debug, Clone)]
pub struct ToolOutputMutation {
    pub content: String,
    pub conv_content: String,
    pub structured_value: Option<serde_json::Value>,
    mutated: bool,
    pub code: Option<String>,
    pub reason: Option<String>,
}

impl ToolOutputMutation {
    pub fn new(
        content: String,
        conv_content: String,
        structured_value: Option<serde_json::Value>,
    ) -> Self {
        Self {
            content,
            conv_content,
            structured_value,
            mutated: false,
            code: None,
            reason: None,
        }
    }

    /// Explicitly mark the output as policy-modified with a structured code
    /// and optional reason for audit logs.
    pub fn mark_mutated(&mut self, code: impl Into<String>, reason: Option<String>) {
        self.mutated = true;
        self.code = Some(code.into());
        self.reason = reason;
    }

    pub fn is_mutated(&self) -> bool {
        self.mutated
    }

    pub(crate) fn clear_step_flag(&mut self) {
        self.mutated = false;
        self.code = None;
        self.reason = None;
    }
}

/// Synchronous post-execution output filter running before artifact spill and
/// conversation persistence.
///
/// Errors fail closed (raw unredacted output is replaced with a policy block
/// notice so unredacted text cannot leak to disk or the model).
pub trait ToolOutputFilter: Send + Sync {
    /// Stable filter identifier recorded in audit events.
    fn id(&self) -> &str;

    fn filter(
        &self,
        ctx: &ToolOutputFilterContext<'_>,
        output: &mut ToolOutputMutation,
    ) -> anyhow::Result<()>;
}

/// Counts of currently registered runtime policy hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolHookStats {
    pub gates: usize,
    pub filters: usize,
}

/// Lock-free-snapshot registry for `ToolExecutionGate` and `ToolOutputFilter`.
#[derive(Default)]
pub(crate) struct ToolHookRegistry {
    gates: RwLock<Vec<(u64, Arc<dyn ToolExecutionGate>)>>,
    filters: RwLock<Vec<(u64, Arc<dyn ToolOutputFilter>)>>,
    next_token: AtomicU64,
}

impl ToolHookRegistry {
    pub(crate) fn new(
        initial_gates: Vec<Arc<dyn ToolExecutionGate>>,
        initial_filters: Vec<Arc<dyn ToolOutputFilter>>,
    ) -> Self {
        let mut gate_entries = Vec::with_capacity(initial_gates.len());
        let mut next = 1u64;
        for gate in initial_gates {
            gate_entries.push((next, gate));
            next += 1;
        }
        let mut filter_entries = Vec::with_capacity(initial_filters.len());
        for filter in initial_filters {
            filter_entries.push((next, filter));
            next += 1;
        }
        Self {
            gates: RwLock::new(gate_entries),
            filters: RwLock::new(filter_entries),
            next_token: AtomicU64::new(next),
        }
    }

    pub(crate) fn register_gate(
        self: &Arc<Self>,
        gate: Arc<dyn ToolExecutionGate>,
    ) -> RegistrationGuard {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .gates
                .write()
                .unwrap_or_else(|error| error.into_inner());
            guard.push((token, gate));
        }
        let weak = Arc::downgrade(self);
        RegistrationGuard::new(move || {
            if let Some(registry) = weak.upgrade() {
                let mut guard = registry
                    .gates
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                guard.retain(|(entry_token, _)| *entry_token != token);
            }
        })
    }

    pub(crate) fn register_filter(
        self: &Arc<Self>,
        filter: Arc<dyn ToolOutputFilter>,
    ) -> RegistrationGuard {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .filters
                .write()
                .unwrap_or_else(|error| error.into_inner());
            guard.push((token, filter));
        }
        let weak = Arc::downgrade(self);
        RegistrationGuard::new(move || {
            if let Some(registry) = weak.upgrade() {
                let mut guard = registry
                    .filters
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                guard.retain(|(entry_token, _)| *entry_token != token);
            }
        })
    }

    pub(crate) fn snapshot_gates(&self) -> Vec<Arc<dyn ToolExecutionGate>> {
        let guard = self.gates.read().unwrap_or_else(|error| error.into_inner());
        if guard.is_empty() {
            return Vec::new();
        }
        guard.iter().map(|(_, gate)| Arc::clone(gate)).collect()
    }

    pub(crate) fn snapshot_filters(&self) -> Vec<Arc<dyn ToolOutputFilter>> {
        let guard = self
            .filters
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if guard.is_empty() {
            return Vec::new();
        }
        guard.iter().map(|(_, filter)| Arc::clone(filter)).collect()
    }

    pub(crate) fn stats(&self) -> ToolHookStats {
        let gates = self
            .gates
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .len();
        let filters = self
            .filters
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .len();
        ToolHookStats { gates, filters }
    }
}

/// Composable startup plugin that installs tools, policies, gates, filters,
/// and sinks into [`crate::runtime::AgentOptions`].
pub trait RuntimePlugin: Send + Sync {
    fn install_into(&self, options: &mut crate::runtime::AgentOptions) -> anyhow::Result<()>;
}
