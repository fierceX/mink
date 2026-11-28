//! Declarative startup assembly (`RimProfile -> RimBundle -> Patch`) and
//! scoped runtime hook binding (`RimSession`).

use crate::scope::{Disposer, Scope};
use anyhow::{Context, bail};
use mink::runtime::{
    AgentOptions, AgentRuntime, AgentRuntimeHandle, SessionEventSink, ToolExecutionGate,
    ToolOutputFilter,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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
    seat_overrides: &'a BTreeMap<String, String>,
}

impl<'a> RimInstallContext<'a> {
    pub fn options(&mut self) -> &mut AgentOptions {
        self.options
    }

    pub fn scope(&self) -> &Arc<Scope> {
        &self.scope
    }

    pub fn seat_override(&self, seat: &str) -> Option<&str> {
        self.seat_overrides.get(seat).map(String::as_str)
    }

    pub fn push_disposer(&self, label: impl Into<String>, disposer: Disposer) {
        self.scope.push_labeled(label, disposer);
    }
}

/// Composable domain or cross-cutting bundle installed during startup assembly.
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
    /// runtime teardown. If any bundle fails, already-installed bundles are
    /// rolled back in LIFO order before returning `Err`.
    pub fn apply_profile(
        &self,
        profile: &RimProfile,
        options: &mut AgentOptions,
    ) -> anyhow::Result<Arc<Scope>> {
        let root_scope = Scope::root();
        let bundles_root = root_scope.fork("bundle");
        for activation in &profile.bundles {
            if !activation.enabled {
                continue;
            }
            let Some(bundle) = self.bundles.get(&activation.id) else {
                root_scope.dispose();
                bail!(
                    "rim profile {:?} references unknown bundle {:?}",
                    profile.name,
                    activation.id
                );
            };
            let bundle_scope = bundles_root.fork(bundle.id());
            let mut ctx = RimInstallContext {
                options,
                scope: bundle_scope,
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
        let scope_for_teardown = Arc::clone(&root_scope);
        options.add_teardown("mink-rim:root-scope", move || {
            scope_for_teardown.dispose();
        });
        Ok(root_scope)
    }

    /// Assemble `profile` into `options` and start [`AgentRuntime`], returning
    /// both the runtime and a [`RimSession`] for scoped dynamic extensions.
    pub async fn start_runtime(
        &self,
        profile: &RimProfile,
        mut options: AgentOptions,
    ) -> anyhow::Result<(AgentRuntime, RimSession)> {
        let root_scope = self.apply_profile(profile, &mut options)?;
        let runtime = match AgentRuntime::start(options).await {
            Ok(runtime) => runtime,
            Err(error) => {
                root_scope.dispose();
                return Err(error.into());
            }
        };
        let session = RimSession::new(runtime.handle(), root_scope);
        Ok((runtime, session))
    }
}

/// Live session facade pairing an [`AgentRuntimeHandle`] with a rim [`Scope`]
/// tree for zero-residual runtime hook attachment.
#[derive(Clone)]
pub struct RimSession {
    handle: AgentRuntimeHandle,
    root_scope: Arc<Scope>,
}

impl RimSession {
    pub fn new(handle: AgentRuntimeHandle, root_scope: Arc<Scope>) -> Self {
        Self { handle, root_scope }
    }

    pub fn handle(&self) -> &AgentRuntimeHandle {
        &self.handle
    }

    pub fn root_scope(&self) -> &Arc<Scope> {
        &self.root_scope
    }

    pub fn fork_scope(&self, segment: &str) -> Arc<Scope> {
        self.root_scope.fork(segment)
    }

    pub fn attach_session_event_sink(
        &self,
        scope: &Scope,
        label: impl Into<String>,
        sink: Arc<dyn SessionEventSink>,
    ) {
        let guard = self.handle.ext().register_session_event_sink(sink);
        if scope.is_closing() || scope.is_disposed() {
            guard.unregister();
            return;
        }
        scope.push_labeled(label, Disposer::from(guard));
    }

    pub fn attach_tool_execution_gate(
        &self,
        scope: &Scope,
        label: impl Into<String>,
        gate: Arc<dyn ToolExecutionGate>,
    ) {
        let guard = self.handle.ext().register_tool_execution_gate(gate);
        if scope.is_closing() || scope.is_disposed() {
            guard.unregister();
            return;
        }
        scope.push_labeled(label, Disposer::from(guard));
    }

    pub fn attach_tool_output_filter(
        &self,
        scope: &Scope,
        label: impl Into<String>,
        filter: Arc<dyn ToolOutputFilter>,
    ) {
        let guard = self.handle.ext().register_tool_output_filter(filter);
        if scope.is_closing() || scope.is_disposed() {
            guard.unregister();
            return;
        }
        scope.push_labeled(label, Disposer::from(guard));
    }
}
