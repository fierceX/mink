use crate::protocol::ToolCallEvent;
use crate::session::artifacts::ArtifactManager;
use crate::session::goal::{GoalContinueProposal, GoalHostConclusion, GoalReview, GoalSnapshot};
use std::path::PathBuf;

/// The filesystem and artifact access needed to validate review evidence.
/// Keeping this smaller than `ToolContext` lets the runtime review session stay
/// independent of unrelated tool capabilities.
pub(crate) struct GoalReviewToolContext<'a> {
    pub(crate) cwd: &'a std::path::Path,
    pub(crate) artifact_dir: PathBuf,
    pub(crate) artifacts: &'a ArtifactManager,
}

/// Narrow bridge between goal-review tools and the runtime-owned review session.
/// The runtime keeps scheduling and persistence details behind this contract.
pub(crate) trait GoalReviewToolSession: Send + Sync {
    fn billing_turn_id(&self) -> String;
    fn snapshot_for_read(&self) -> GoalSnapshot;
    fn mark_inspecting(&self) -> anyhow::Result<()>;
    fn check_budget(&self) -> anyhow::Result<()>;
    fn record_rejection(&self, reason: String);
    fn submit_continue(&self, proposal: GoalContinueProposal) -> anyhow::Result<()>;
    fn submit(&self, review: GoalReview, ctx: &GoalReviewToolContext<'_>) -> anyhow::Result<()>;
    fn submit_conclusion(
        &self,
        conclusion: GoalHostConclusion,
        ctx: &GoalReviewToolContext<'_>,
    ) -> anyhow::Result<()>;
    fn capture_read(
        &self,
        call: &ToolCallEvent,
        ctx: &GoalReviewToolContext<'_>,
    ) -> anyhow::Result<Option<(String, PathBuf, String)>>;
    fn finish_read(&self, captured: (String, PathBuf, String)) -> anyhow::Result<()>;
}
