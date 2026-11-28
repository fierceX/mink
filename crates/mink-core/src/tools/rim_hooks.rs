//! Reversible extension registries, lifecycle teardown, and closed policy
//! hook traits (`ToolExecutionGate`, `ToolOutputFilter`, `ModelRequestHook`,
//! `RuntimePlugin`).

use crate::cancel::CancellationToken;
use crate::events::EventLog;
use crate::tools::metadata::ApprovalTier;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Segment-aware scope path relation:
/// - Root `""` relates to every scope.
/// - Equal paths relate.
/// - Strict `'/'` segment prefixes relate; sibling or partial-name prefixes
///   (e.g. `"primary/a"` vs `"primary/a2"`) do NOT relate.
pub fn scopes_related(a: &str, b: &str) -> bool {
    fn is_prefix(longer: &str, shorter: &str) -> bool {
        longer.len() > shorter.len()
            && longer.as_bytes()[shorter.len()] == b'/'
            && longer.starts_with(shorter)
    }
    a.is_empty() || b.is_empty() || a == b || is_prefix(a, b) || is_prefix(b, a)
}

pub(crate) fn scope_origin_matches(origin: Option<&str>, target: &str) -> bool {
    match origin {
        None | Some("") => true,
        Some(origin) => scopes_related(origin, target),
    }
}

pub(crate) fn normalize_scope_origin(origin: &str) -> Option<Arc<str>> {
    (!origin.is_empty()).then(|| Arc::from(origin))
}

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

/// Instance-scoped lifecycle state and LIFO closing/teardown stacks.
///
/// Owned by a single `AgentRuntime` instance (`AgentSharedContext`), ensuring
/// strict isolation across concurrent sessions in the same process. Sub-agent
/// lifecycles link to their parent's `closing` flag so shutdown closing state
/// propagates immediately while keeping teardown stacks independent.
type TeardownCallback = Box<dyn FnOnce() + Send>;

pub(crate) struct RuntimeLifecycle {
    closing: Arc<AtomicBool>,
    parent_closing: Option<Arc<AtomicBool>>,
    disposed: AtomicBool,
    on_closing: Mutex<Vec<(String, TeardownCallback)>>,
    teardowns: Mutex<Vec<(String, TeardownCallback)>>,
}

impl Default for RuntimeLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            closing: Arc::new(AtomicBool::new(false)),
            parent_closing: None,
            disposed: AtomicBool::new(false),
            on_closing: Mutex::new(Vec::new()),
            teardowns: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn child_of(parent: &RuntimeLifecycle) -> Self {
        Self {
            closing: Arc::new(AtomicBool::new(parent.is_closing())),
            parent_closing: Some(Arc::clone(&parent.closing)),
            disposed: AtomicBool::new(false),
            on_closing: Mutex::new(Vec::new()),
            teardowns: Mutex::new(Vec::new()),
        }
    }

    /// Transition into `closing` state and invoke all `on_closing` callbacks in
    /// strict LIFO order outside locks (idempotent).
    pub(crate) fn mark_closing(&self) {
        if self.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        let entries = {
            let mut guard = self
                .on_closing
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *guard)
        };
        for (_label, callback) in entries.into_iter().rev() {
            callback();
        }
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
            || self
                .parent_closing
                .as_ref()
                .is_some_and(|parent| parent.load(Ordering::SeqCst))
    }

    pub(crate) fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    /// Register a callback executed once when the runtime enters `closing`
    /// state (or immediately if already closing).
    pub(crate) fn register_on_closing(
        &self,
        label: impl Into<String>,
        f: impl FnOnce() + Send + 'static,
    ) {
        let label = label.into();
        let boxed: Box<dyn FnOnce() + Send> = Box::new(f);
        let run_now = {
            let mut guard = self
                .on_closing
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.is_closing() {
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
        self.mark_closing();
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

type ScopedSinkEntry = (
    u64,
    Option<Arc<str>>,
    Arc<dyn crate::runtime::SessionEventSink>,
);

/// Multi-subscriber, lock-free-dispatch registry for `SessionEventSink`.
#[derive(Default)]
pub(crate) struct SessionEventSinkRegistry {
    sinks: RwLock<Vec<ScopedSinkEntry>>,
    next_token: AtomicU64,
}

impl SessionEventSinkRegistry {
    pub(crate) fn new(initial: Vec<Arc<dyn crate::runtime::SessionEventSink>>) -> Self {
        let mut entries = Vec::with_capacity(initial.len());
        let mut next = 1u64;
        for sink in initial {
            entries.push((next, None, sink));
            next += 1;
        }
        Self {
            sinks: RwLock::new(entries),
            next_token: AtomicU64::new(next),
        }
    }

    pub(crate) fn register_scoped(
        self: &Arc<Self>,
        scope_origin: Option<Arc<str>>,
        sink: Arc<dyn crate::runtime::SessionEventSink>,
    ) -> RegistrationGuard {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .sinks
                .write()
                .unwrap_or_else(|error| error.into_inner());
            guard.push((token, scope_origin, sink));
        }
        let weak = Arc::downgrade(self);
        RegistrationGuard::new(move || {
            if let Some(registry) = weak.upgrade() {
                let mut guard = registry
                    .sinks
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                guard.retain(|(entry_token, _, _)| *entry_token != token);
            }
        })
    }

    /// Snapshot registered sinks matching `target_scope` under a short read
    /// lock, drop the lock, and dispatch each sink outside any lock.
    pub(crate) fn notify(&self, session_id: &str, target_scope: &str, event: &EventLog) {
        let snapshot: Vec<Arc<dyn crate::runtime::SessionEventSink>> = {
            let guard = self.sinks.read().unwrap_or_else(|error| error.into_inner());
            if guard.is_empty() {
                return;
            }
            guard
                .iter()
                .filter(|(_, origin, _)| scope_origin_matches(origin.as_deref(), target_scope))
                .map(|(_, _, sink)| Arc::clone(sink))
                .collect()
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

/// Read-only view handed to [`ModelRequestHook::before_request`].
#[derive(Debug, Clone)]
pub struct ModelRequestHookContext<'a> {
    pub session_id: &'a str,
    pub scope_path: &'a str,
    pub round: u32,
    pub model_name: &'a str,
    pub is_sub_agent: bool,
    pub message_count: usize,
    pub estimated_prefix_tokens: usize,
}

/// One ephemeral user-tail context block injected for a single model request
/// without modifying `ImmutablePrefix` or persisting to `ConversationStore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralContextBlock {
    pub tag: String,
    pub content: String,
}

/// Per-request mutable patch populated by [`ModelRequestHook`].
#[derive(Debug, Clone, Default)]
pub struct ModelRequestPatch {
    blocks: Vec<EphemeralContextBlock>,
    mutated: bool,
    pub code: Option<String>,
    pub reason: Option<String>,
}

impl ModelRequestPatch {
    pub fn push_ephemeral_block(&mut self, tag: impl Into<String>, content: impl Into<String>) {
        self.blocks.push(EphemeralContextBlock {
            tag: tag.into(),
            content: content.into(),
        });
        self.mutated = true;
    }

    pub fn blocks(&self) -> &[EphemeralContextBlock] {
        &self.blocks
    }

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

/// Asynchronous hook evaluated before each LLM request builds its projection.
///
/// Hooks can append single-request ephemeral context blocks into the projected
/// request tail while leaving `ImmutablePrefix` (`system_prompt`, `tools_json`,
/// prefix fingerprints) and durable `ConversationStore` untouched. Hook errors
/// and timeouts fail closed.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait ModelRequestHook: Send + Sync {
    /// Stable hook identifier recorded in critical `EventLog::PolicyEffect`
    /// audit records (`anchor = "before_model_request"`).
    fn id(&self) -> &str;

    /// Per-hook execution timeout budget (default: 10 seconds).
    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    async fn before_request(
        &self,
        ctx: &ModelRequestHookContext<'_>,
        patch: &mut ModelRequestPatch,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()>;
}

/// Counts of currently registered runtime policy hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolHookStats {
    pub gates: usize,
    pub filters: usize,
    pub model_request_hooks: usize,
}

type ScopedEntry<T> = (u64, Option<Arc<str>>, Arc<T>);

/// Lock-free-snapshot registry for `ToolExecutionGate`, `ToolOutputFilter`,
/// and `ModelRequestHook` with segment-aware scope filtering.
#[derive(Default)]
pub(crate) struct ToolHookRegistry {
    gates: RwLock<Vec<ScopedEntry<dyn ToolExecutionGate>>>,
    filters: RwLock<Vec<ScopedEntry<dyn ToolOutputFilter>>>,
    model_request_hooks: RwLock<Vec<ScopedEntry<dyn ModelRequestHook>>>,
    next_token: AtomicU64,
}

impl ToolHookRegistry {
    pub(crate) fn new(
        initial_gates: Vec<Arc<dyn ToolExecutionGate>>,
        initial_filters: Vec<Arc<dyn ToolOutputFilter>>,
        initial_model_request_hooks: Vec<Arc<dyn ModelRequestHook>>,
    ) -> Self {
        let mut next = 1u64;
        let mut gate_entries = Vec::with_capacity(initial_gates.len());
        for gate in initial_gates {
            gate_entries.push((next, None, gate));
            next += 1;
        }
        let mut filter_entries = Vec::with_capacity(initial_filters.len());
        for filter in initial_filters {
            filter_entries.push((next, None, filter));
            next += 1;
        }
        let mut model_entries = Vec::with_capacity(initial_model_request_hooks.len());
        for hook in initial_model_request_hooks {
            model_entries.push((next, None, hook));
            next += 1;
        }
        Self {
            gates: RwLock::new(gate_entries),
            filters: RwLock::new(filter_entries),
            model_request_hooks: RwLock::new(model_entries),
            next_token: AtomicU64::new(next),
        }
    }

    pub(crate) fn register_gate_scoped(
        self: &Arc<Self>,
        scope_origin: Option<Arc<str>>,
        gate: Arc<dyn ToolExecutionGate>,
    ) -> RegistrationGuard {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .gates
                .write()
                .unwrap_or_else(|error| error.into_inner());
            guard.push((token, scope_origin, gate));
        }
        let weak = Arc::downgrade(self);
        RegistrationGuard::new(move || {
            if let Some(registry) = weak.upgrade() {
                let mut guard = registry
                    .gates
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                guard.retain(|(entry_token, _, _)| *entry_token != token);
            }
        })
    }

    pub(crate) fn register_filter_scoped(
        self: &Arc<Self>,
        scope_origin: Option<Arc<str>>,
        filter: Arc<dyn ToolOutputFilter>,
    ) -> RegistrationGuard {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .filters
                .write()
                .unwrap_or_else(|error| error.into_inner());
            guard.push((token, scope_origin, filter));
        }
        let weak = Arc::downgrade(self);
        RegistrationGuard::new(move || {
            if let Some(registry) = weak.upgrade() {
                let mut guard = registry
                    .filters
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                guard.retain(|(entry_token, _, _)| *entry_token != token);
            }
        })
    }

    pub(crate) fn register_model_request_hook_scoped(
        self: &Arc<Self>,
        scope_origin: Option<Arc<str>>,
        hook: Arc<dyn ModelRequestHook>,
    ) -> RegistrationGuard {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .model_request_hooks
                .write()
                .unwrap_or_else(|error| error.into_inner());
            guard.push((token, scope_origin, hook));
        }
        let weak = Arc::downgrade(self);
        RegistrationGuard::new(move || {
            if let Some(registry) = weak.upgrade() {
                let mut guard = registry
                    .model_request_hooks
                    .write()
                    .unwrap_or_else(|error| error.into_inner());
                guard.retain(|(entry_token, _, _)| *entry_token != token);
            }
        })
    }

    pub(crate) fn snapshot_gates_for(&self, target_scope: &str) -> Vec<Arc<dyn ToolExecutionGate>> {
        let guard = self.gates.read().unwrap_or_else(|error| error.into_inner());
        if guard.is_empty() {
            return Vec::new();
        }
        guard
            .iter()
            .filter(|(_, origin, _)| scope_origin_matches(origin.as_deref(), target_scope))
            .map(|(_, _, gate)| Arc::clone(gate))
            .collect()
    }

    pub(crate) fn snapshot_filters_for(
        &self,
        target_scope: &str,
    ) -> Vec<Arc<dyn ToolOutputFilter>> {
        let guard = self
            .filters
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if guard.is_empty() {
            return Vec::new();
        }
        guard
            .iter()
            .filter(|(_, origin, _)| scope_origin_matches(origin.as_deref(), target_scope))
            .map(|(_, _, filter)| Arc::clone(filter))
            .collect()
    }

    pub(crate) fn snapshot_model_request_hooks_for(
        &self,
        target_scope: &str,
    ) -> Vec<Arc<dyn ModelRequestHook>> {
        let guard = self
            .model_request_hooks
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if guard.is_empty() {
            return Vec::new();
        }
        guard
            .iter()
            .filter(|(_, origin, _)| scope_origin_matches(origin.as_deref(), target_scope))
            .map(|(_, _, hook)| Arc::clone(hook))
            .collect()
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
        let model_request_hooks = self
            .model_request_hooks
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .len();
        ToolHookStats {
            gates,
            filters,
            model_request_hooks,
        }
    }
}

/// Zero-config pure-Rust startup plugin helper in `mink-core` that installs
/// tools, policies, hooks, and sinks directly into
/// [`crate::runtime::AgentOptions`].
///
/// Hosts that need declarative JSON patch profiles, scoped lifecycle rollback,
/// or typed capability slot arbitration should layer external bundle/profile
/// assemblers over `AgentOptions` and `AgentRuntimeHandle::ext()`.
pub trait RuntimePlugin: Send + Sync {
    fn install_into(&self, options: &mut crate::runtime::AgentOptions) -> anyhow::Result<()>;
}
