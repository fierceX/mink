//! Generic turn-scoped tool execution masks, observers, and policy providers.

use crate::session::artifacts::{ArtifactManager, ArtifactRecord};
use std::any::Any;
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
}

/// Read-only session filesystem & artifact view passed to [`ToolExecutionObserver`]
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

/// Generic tool lifecycle observer for side-effect journaling, read evidence
/// verification, and parse/rejection tracking.
pub trait ToolExecutionObserver: Send + Sync {
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

    /// Invoked when a tool call fails argument JSON parsing before dispatch.
    fn on_parse_error(&self, _tool_name: &str, _error: &str) {}

    /// Invoked after surface/mask validation, just before tool execution begins.
    fn before_prepare_execute(&self, _tool_name: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Invoked before executing a `Read` call (text or image). Returning
    /// `Some(token)` pairs with [`Self::verify_read_bytes`] / [`Self::after_read_succeeded`].
    fn before_read(
        &self,
        _tool_name: &str,
        _call_id: &str,
        _input: &serde_json::Value,
        _ctx: &ReadEvidenceContext<'_>,
    ) -> anyhow::Result<Option<Box<dyn Any + Send>>> {
        Ok(None)
    }

    /// Invoked for image reads before committing to verify the prepared bytes
    /// match the pre-read observation.
    fn verify_read_bytes(&self, _token: &(dyn Any + Send), _bytes: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    /// Invoked after a `Read` succeeds to finalize evidence recording.
    fn after_read_succeeded(&self, _token: Box<dyn Any + Send>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Provides turn-scoped masks and observers for ordinary session turns.
pub trait TurnPolicyProvider: Send + Sync {
    fn ordinary_turn_mask(&self) -> Option<ToolExecutionMask> {
        None
    }

    fn ordinary_tool_observer(&self) -> Option<Arc<dyn ToolExecutionObserver>> {
        None
    }
}
