//! Generic turn-scoped tool execution masks, observers, journals, and policy providers.

use crate::session::artifacts::{ArtifactManager, ArtifactRecord};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Turn-scoped execution filter applied by [`crate::tools::runner::ToolRunner`]
/// and PTC dispatch without altering the wire `ModelToolSurface` schema.
#[derive(Debug, Clone)]
pub struct ToolExecutionMask {
    pub allowed_tools: BTreeSet<String>,
    pub allow_exclusive_ptc: bool,
    pub ptc_allowed_tools: BTreeSet<String>,
    pub block_custom_resource_schemes: bool,
    pub disable_native_web: bool,
    pub disable_replan: bool,
    pub block_reason: String,
}

impl ToolExecutionMask {
    /// Allow only the named tools on the wire surface for this turn.
    pub fn allow_only<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowed_tools: tools.into_iter().map(Into::into).collect(),
            allow_exclusive_ptc: false,
            ptc_allowed_tools: ["Read", "Glob", "Grep"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            block_custom_resource_schemes: false,
            disable_native_web: false,
            disable_replan: false,
            block_reason: "tool is blocked by the current turn execution policy".to_string(),
        }
    }

    pub fn with_exclusive_ptc(mut self, allow: bool) -> Self {
        self.allow_exclusive_ptc = allow;
        self
    }

    pub fn with_ptc_allowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ptc_allowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_block_custom_resource_schemes(mut self, block: bool) -> Self {
        self.block_custom_resource_schemes = block;
        self
    }

    pub fn with_disable_native_web(mut self, disable: bool) -> Self {
        self.disable_native_web = disable;
        self
    }

    pub fn with_disable_replan(mut self, disable: bool) -> Self {
        self.disable_replan = disable;
        self
    }

    pub fn with_block_reason(mut self, reason: impl Into<String>) -> Self {
        self.block_reason = reason.into();
        self
    }

    /// Combine two turn masks by taking the strictest intersection of allowed
    /// tools and the logical union of restrictions.
    pub fn intersect(&self, other: &Self) -> Self {
        let block_reason = if self.block_reason == other.block_reason {
            self.block_reason.clone()
        } else if self.block_reason.is_empty() {
            other.block_reason.clone()
        } else if other.block_reason.is_empty() {
            self.block_reason.clone()
        } else {
            format!("{}; {}", self.block_reason, other.block_reason)
        };
        Self {
            allowed_tools: self
                .allowed_tools
                .intersection(&other.allowed_tools)
                .cloned()
                .collect(),
            allow_exclusive_ptc: self.allow_exclusive_ptc && other.allow_exclusive_ptc,
            ptc_allowed_tools: self
                .ptc_allowed_tools
                .intersection(&other.ptc_allowed_tools)
                .cloned()
                .collect(),
            block_custom_resource_schemes: self.block_custom_resource_schemes
                || other.block_custom_resource_schemes,
            disable_native_web: self.disable_native_web || other.disable_native_web,
            disable_replan: self.disable_replan || other.disable_replan,
            block_reason,
        }
    }
}

/// Read-only session filesystem & artifact view passed to [`ToolCallObserver`]
/// read hooks.
pub struct ReadEvidenceContext<'a> {
    cwd: &'a Path,
    artifacts_dir: PathBuf,
    artifacts: &'a ArtifactManager,
}

impl<'a> ReadEvidenceContext<'a> {
    pub(crate) fn new(
        cwd: &'a Path,
        artifacts_dir: PathBuf,
        artifacts: &'a ArtifactManager,
    ) -> Self {
        Self {
            cwd,
            artifacts_dir,
            artifacts,
        }
    }

    pub fn cwd(&self) -> &Path {
        self.cwd
    }

    pub fn artifacts_dir(&self) -> &Path {
        &self.artifacts_dir
    }

    pub fn find_artifact(&self, id: &str) -> anyhow::Result<Option<ArtifactRecord>> {
        self.artifacts.find(id)
    }

    pub fn read_artifact_text(&self, id: &str) -> anyhow::Result<String> {
        self.artifacts.read_text(id)
    }
}

/// Durable side-effect journal for crash-recoverable external orchestrators.
pub trait ToolEffectJournal: Send + Sync {
    /// Whether this turn requires durable tool-result persistence and
    /// side-effect tracking.
    fn tracks_durable_effects(&self) -> bool {
        false
    }

    /// Invoked immediately before a tracked side-effecting tool is dispatched.
    fn before_side_effect(&self, _call_id: &str, _tool_name: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether `call_id` currently has an unresolved pending side effect.
    fn has_pending_effect(&self, _call_id: &str) -> bool {
        false
    }

    /// Invoked after durable tool-result persistence for all calls whose
    /// execution outcome is known.
    fn after_known_effects(&self, _known_call_ids: &[String]) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Per-read observation handle returned by [`ToolCallObserver::observe_read`].
pub trait ReadObservation: Send {
    /// Invoked for image reads before committing to verify the prepared bytes
    /// match the pre-read observation.
    fn verify_bytes(&self, _bytes: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    /// Invoked after a `Read` succeeds to finalize evidence recording.
    fn on_succeeded(self: Box<Self>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Turn-scoped tool call observer for parse diagnostics, pre-execution checks,
/// and read inspection tracking.
pub trait ToolCallObserver: Send + Sync {
    /// Invoked when a tool call fails argument JSON parsing before dispatch.
    fn on_parse_error(&self, _tool_name: &str, _error: &str) {}

    /// Invoked after surface/mask validation, just before tool execution begins.
    fn before_prepare_execute(&self, _tool_name: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Invoked before executing a `Read` call (text or image). Returning
    /// `Some(observation)` pairs with [`ReadObservation::verify_bytes`] and
    /// [`ReadObservation::on_succeeded`].
    fn observe_read(
        &self,
        _tool_name: &str,
        _call_id: &str,
        _input: &serde_json::Value,
        _ctx: &ReadEvidenceContext<'_>,
    ) -> anyhow::Result<Option<Box<dyn ReadObservation>>> {
        Ok(None)
    }
}

/// Provides turn-scoped masks, effect journals, and observers for ordinary session turns.
pub trait TurnPolicyProvider: Send + Sync {
    fn ordinary_turn_mask(&self) -> Option<ToolExecutionMask> {
        None
    }

    fn ordinary_effect_journal(&self) -> Option<Arc<dyn ToolEffectJournal>> {
        None
    }

    fn ordinary_tool_observer(&self) -> Option<Arc<dyn ToolCallObserver>> {
        None
    }
}

struct CompositeReadObservation(Vec<Box<dyn ReadObservation>>);

impl ReadObservation for CompositeReadObservation {
    fn verify_bytes(&self, bytes: &[u8]) -> anyhow::Result<()> {
        for observation in &self.0 {
            observation.verify_bytes(bytes)?;
        }
        Ok(())
    }

    fn on_succeeded(self: Box<Self>) -> anyhow::Result<()> {
        for observation in self.0 {
            observation.on_succeeded()?;
        }
        Ok(())
    }
}

/// Fans out [`ToolCallObserver`] callbacks across multiple registered observers
/// in registration order without erasing stateful [`ReadObservation`] handles.
pub(crate) struct CompositeToolCallObserver {
    observers: Vec<Arc<dyn ToolCallObserver>>,
}

impl CompositeToolCallObserver {
    pub(crate) fn new(observers: Vec<Arc<dyn ToolCallObserver>>) -> Self {
        Self { observers }
    }
}

impl ToolCallObserver for CompositeToolCallObserver {
    fn on_parse_error(&self, tool_name: &str, error: &str) {
        for observer in &self.observers {
            observer.on_parse_error(tool_name, error);
        }
    }

    fn before_prepare_execute(&self, tool_name: &str) -> anyhow::Result<()> {
        for observer in &self.observers {
            observer.before_prepare_execute(tool_name)?;
        }
        Ok(())
    }

    fn observe_read(
        &self,
        tool_name: &str,
        call_id: &str,
        input: &serde_json::Value,
        ctx: &ReadEvidenceContext<'_>,
    ) -> anyhow::Result<Option<Box<dyn ReadObservation>>> {
        let mut captures = Vec::new();
        for observer in &self.observers {
            if let Some(observation) = observer.observe_read(tool_name, call_id, input, ctx)? {
                captures.push(observation);
            }
        }
        match captures.len() {
            0 => Ok(None),
            1 => Ok(captures.into_iter().next()),
            _ => Ok(Some(Box::new(CompositeReadObservation(captures)))),
        }
    }
}

pub(crate) type ComposedTurnPolicies = (
    Option<ToolExecutionMask>,
    Option<Arc<dyn ToolEffectJournal>>,
    Option<Arc<dyn ToolCallObserver>>,
);

/// Compose multiple [`TurnPolicyProvider`]s and optional defaults without silent
/// shadowing: masks intersect, observers compose, and conflicting effect
/// journals fail closed.
pub(crate) fn compose_turn_policies(
    policies: &[Arc<dyn TurnPolicyProvider>],
    default_mask: Option<ToolExecutionMask>,
    default_journal: Option<Arc<dyn ToolEffectJournal>>,
    default_observer: Option<Arc<dyn ToolCallObserver>>,
) -> anyhow::Result<ComposedTurnPolicies> {
    let mut mask = default_mask;
    let mut journal = default_journal;
    let mut observers = Vec::new();
    if let Some(observer) = default_observer {
        observers.push(observer);
    }

    for policy in policies {
        if let Some(next_mask) = policy.ordinary_turn_mask() {
            mask = Some(match mask {
                Some(current) => current.intersect(&next_mask),
                None => next_mask,
            });
        }
        if let Some(next_journal) = policy.ordinary_effect_journal() {
            if journal.is_some() {
                anyhow::bail!(
                    "conflicting TurnPolicyProvider registrations: at most one ToolEffectJournal may be active per runtime"
                );
            }
            journal = Some(next_journal);
        }
        if let Some(observer) = policy.ordinary_tool_observer() {
            observers.push(observer);
        }
    }

    let observer = match observers.len() {
        0 => None,
        1 => observers.into_iter().next(),
        _ => Some(Arc::new(CompositeToolCallObserver::new(observers)) as Arc<dyn ToolCallObserver>),
    };

    Ok((mask, journal, observer))
}
