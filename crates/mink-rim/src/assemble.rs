//! Declarative startup assembly (`RimProfile -> RimBundle -> Patch`) and
//! scoped runtime hook binding (`RimSession`).

use crate::scope::{Disposer, Scope, ScopeSegmentError};
use crate::seat::{SeatError, SeatHub};
use anyhow::{Context, bail};
use mink::runtime::{
    AgentOptions, AgentRuntime, AgentRuntimeHandle, ModelRequestHook, SessionEventSink,
    ToolExecutionGate, ToolOutputFilter,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

fn default_true() -> bool {
    true
}

/// Declarative activation profile assembling bundles and seat overrides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RimProfile {
    pub name: String,
    #[serde(default)]
    pub bundles: Vec<BundleActivation>,
    #[serde(default)]
    pub seat_overrides: BTreeMap<String, String>,
}

/// One bundle entry inside a [`RimProfile`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleActivation {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub patch: serde_json::Value,
}

/// Context handed to [`RimBundle::install`] during static profile assembly.
pub struct RimInstallContext<'a> {
    options: &'a mut AgentOptions,
    scope: Arc<Scope>,
    seats: &'a Arc<SeatHub>,
    seat_overrides: &'a BTreeMap<String, String>,
}

impl<'a> RimInstallContext<'a> {
    pub fn options(&mut self) -> &mut AgentOptions {
        self.options
    }

    pub fn scope(&self) -> &Arc<Scope> {
        &self.scope
    }

    pub fn seats(&self) -> &Arc<SeatHub> {
        self.seats
    }

    pub fn seat_override(&self, seat: &str) -> Option<&str> {
        self.seat_overrides.get(seat).map(String::as_str)
    }

    pub fn push_disposer(&self, label: impl Into<String>, disposer: Disposer) {
        self.scope.push_labeled(label, disposer);
    }

    /// Register a typed capability provider on `seat` bound to this bundle's
    /// lifecycle [`Scope`].
    pub fn register_seat_provider<P: ?Sized + Send + Sync + 'static>(
        &self,
        seat: &'static str,
        id: impl Into<String>,
        provider: Arc<P>,
        available: impl Fn(&P) -> bool + Send + Sync + 'static,
    ) -> Result<(), SeatError> {
        self.seats
            .registry::<P>(seat)
            .register_in(&self.scope, id, provider, available)
    }
}

/// Composable domain or cross-cutting bundle installed during static profile
/// assembly.
pub trait RimBundle: Send + Sync {
    fn id(&self) -> &'static str;

    fn install(
        &self,
        ctx: &mut RimInstallContext<'_>,
        patch: &serde_json::Value,
    ) -> anyhow::Result<()>;
}

/// Static startup assembler for [`RimProfile`]s with atomic LIFO rollback on
/// installation failure.
#[derive(Default)]
pub struct RuntimeAssembler {
    bundles: HashMap<String, Arc<dyn RimBundle>>,
}

impl RuntimeAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_bundle(mut self, bundle: Arc<dyn RimBundle>) -> Self {
        self.add_bundle(bundle);
        self
    }

    pub fn add_bundle(&mut self, bundle: Arc<dyn RimBundle>) {
        self.bundles.insert(bundle.id().to_string(), bundle);
    }

    /// Apply `profile` onto `options`, creating a root [`Scope`] bound to
    /// runtime closing and teardown. If any bundle fails, already-installed
    /// bundle disposers are rolled back in LIFO order before returning `Err`;
    /// callers should discard `options` on failure (prefer
    /// [`RuntimeAssembler::start_runtime`]).
    pub fn apply_profile(
        &self,
        profile: &RimProfile,
        options: &mut AgentOptions,
    ) -> anyhow::Result<Arc<Scope>> {
        let seats = Arc::new(SeatHub::new());
        self.apply_profile_with_seats(profile, options, &seats)
    }

    /// Apply `profile` onto `options` using an explicit [`SeatHub`].
    pub fn apply_profile_with_seats(
        &self,
        profile: &RimProfile,
        options: &mut AgentOptions,
        seats: &Arc<SeatHub>,
    ) -> anyhow::Result<Arc<Scope>> {
        let root_scope = Scope::root();
        let bundles_root = root_scope.fork("bundle");
        let mut seen_enabled = HashSet::new();
        for activation in &profile.bundles {
            if !activation.enabled {
                continue;
            }
            if !seen_enabled.insert(activation.id.as_str()) {
                root_scope.dispose();
                bail!(
                    "rim profile {:?} enables duplicate bundle {:?}",
                    profile.name,
                    activation.id
                );
            }
            let Some(bundle) = self.bundles.get(&activation.id) else {
                root_scope.dispose();
                bail!(
                    "rim profile {:?} references unknown bundle {:?}",
                    profile.name,
                    activation.id
                );
            };
            let bundle_scope = match bundles_root.try_fork(bundle.id()) {
                Ok(scope) => scope,
                Err(error) => {
                    root_scope.dispose();
                    return Err(error.into());
                }
            };
            let mut ctx = RimInstallContext {
                options,
                scope: bundle_scope,
                seats,
                seat_overrides: &profile.seat_overrides,
            };
            if let Err(error) = bundle
                .install(&mut ctx, &activation.patch)
                .with_context(|| {
                    format!(
                        "failed to install bundle {:?} in rim profile {:?}",
                        activation.id, profile.name
                    )
                })
            {
                root_scope.dispose();
                return Err(error);
            }
        }
        let scope_for_close = Arc::clone(&root_scope);
        options.add_on_closing("mink-rim:close-root-scope", move || {
            scope_for_close.close();
        });
        let scope_for_teardown = Arc::clone(&root_scope);
        options.add_teardown("mink-rim:root-scope", move || {
            scope_for_teardown.dispose();
        });
        Ok(root_scope)
    }

    /// Assemble `profile` into `options` and start [`AgentRuntime`], returning
    /// both the runtime and a [`RimSession`] for scoped dynamic extensions and
    /// seat resolution.
    pub async fn start_runtime(
        &self,
        profile: &RimProfile,
        mut options: AgentOptions,
    ) -> anyhow::Result<(AgentRuntime, RimSession)> {
        let seats = Arc::new(SeatHub::new());
        let root_scope = self.apply_profile_with_seats(profile, &mut options, &seats)?;
        let runtime = match AgentRuntime::start(options).await {
            Ok(runtime) => runtime,
            Err(error) => {
                root_scope.dispose();
                return Err(error.into());
            }
        };
        let session = RimSession::new(runtime.handle(), root_scope)
            .with_seats(seats, profile.seat_overrides.clone());
        Ok((runtime, session))
    }
}

/// Live session facade pairing an [`AgentRuntimeHandle`] with a rim [`Scope`]
/// tree and [`SeatHub`] for zero-residual, scope-isolated dynamic extensions.
#[derive(Clone)]
pub struct RimSession {
    handle: AgentRuntimeHandle,
    root_scope: Arc<Scope>,
    primary_scope: Arc<Scope>,
    subagents_scope: Arc<Scope>,
    seats: Arc<SeatHub>,
    seat_overrides: BTreeMap<String, String>,
}

impl RimSession {
    pub fn new(handle: AgentRuntimeHandle, root_scope: Arc<Scope>) -> Self {
        let primary_scope = root_scope.fork("primary");
        let subagents_scope = root_scope.fork("subagent");
        Self {
            handle,
            root_scope,
            primary_scope,
            subagents_scope,
            seats: Arc::new(SeatHub::new()),
            seat_overrides: BTreeMap::new(),
        }
    }

    pub fn with_seats(
        mut self,
        seats: Arc<SeatHub>,
        seat_overrides: BTreeMap<String, String>,
    ) -> Self {
        self.seats = seats;
        self.seat_overrides = seat_overrides;
        self
    }

    pub fn handle(&self) -> &AgentRuntimeHandle {
        &self.handle
    }

    /// Session-wide root scope (`""`); hooks attached here apply across the
    /// primary agent and all inherited sub-agents.
    pub fn root_scope(&self) -> &Arc<Scope> {
        &self.root_scope
    }

    /// Primary-agent scope (`"primary"`); hooks attached here or in child
    /// scopes forked via [`RimSession::fork_scope`] apply to the primary agent
    /// without bleeding into delegated sub-agents.
    pub fn primary_scope(&self) -> &Arc<Scope> {
        &self.primary_scope
    }

    /// Fork a primary-agent child scope (`"primary/<segment>"`).
    ///
    /// Panics if `segment` is empty or contains `'/'`.
    pub fn fork_scope(&self, segment: &str) -> Arc<Scope> {
        self.primary_scope.fork(segment)
    }

    /// Fallibly fork a primary-agent child scope (`"primary/<segment>"`).
    pub fn try_fork_scope(&self, segment: &str) -> Result<Arc<Scope>, ScopeSegmentError> {
        self.primary_scope.try_fork(segment)
    }

    /// Fallibly fork a scope isolated to one delegated sub-agent
    /// (`"subagent/<sub_session_id>"`) and its nested descendants.
    pub fn try_fork_subagent_scope(
        &self,
        sub_session_id: &str,
    ) -> Result<Arc<Scope>, ScopeSegmentError> {
        self.subagents_scope.try_fork(sub_session_id)
    }

    pub fn seats(&self) -> &Arc<SeatHub> {
        &self.seats
    }

    /// Resolve a typed capability provider on `seat`, honoring
    /// [`RimProfile::seat_overrides`] for this session.
    pub fn resolve_seat<P: ?Sized + Send + Sync + 'static>(
        &self,
        seat: &'static str,
    ) -> Result<Arc<P>, SeatError> {
        let configured = self.seat_overrides.get(seat).map(String::as_str);
        self.seats.resolve::<P>(seat, configured)
    }

    pub fn attach_session_event_sink(
        &self,
        scope: &Scope,
        label: impl Into<String>,
        sink: Arc<dyn SessionEventSink>,
    ) -> bool {
        if self.handle.ext().is_closing() || scope.is_closing() || scope.is_disposed() {
            return false;
        }
        let guard = self
            .handle
            .ext()
            .register_session_event_sink_scoped(scope.path(), sink);
        scope.try_push_labeled(label, Disposer::from(guard))
    }

    pub fn attach_tool_execution_gate(
        &self,
        scope: &Scope,
        label: impl Into<String>,
        gate: Arc<dyn ToolExecutionGate>,
    ) -> bool {
        if self.handle.ext().is_closing() || scope.is_closing() || scope.is_disposed() {
            return false;
        }
        let guard = self
            .handle
            .ext()
            .register_tool_execution_gate_scoped(scope.path(), gate);
        scope.try_push_labeled(label, Disposer::from(guard))
    }

    pub fn attach_tool_output_filter(
        &self,
        scope: &Scope,
        label: impl Into<String>,
        filter: Arc<dyn ToolOutputFilter>,
    ) -> bool {
        if self.handle.ext().is_closing() || scope.is_closing() || scope.is_disposed() {
            return false;
        }
        let guard = self
            .handle
            .ext()
            .register_tool_output_filter_scoped(scope.path(), filter);
        scope.try_push_labeled(label, Disposer::from(guard))
    }

    pub fn attach_model_request_hook(
        &self,
        scope: &Scope,
        label: impl Into<String>,
        hook: Arc<dyn ModelRequestHook>,
    ) -> bool {
        if self.handle.ext().is_closing() || scope.is_closing() || scope.is_disposed() {
            return false;
        }
        let guard = self
            .handle
            .ext()
            .register_model_request_hook_scoped(scope.path(), hook);
        scope.try_push_labeled(label, Disposer::from(guard))
    }
}
