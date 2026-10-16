use crate::context::ToolContext;
use crate::session::goal::*;
use crate::tools::metadata::{ApprovalTier, ToolMetadata, ToolResultKind};
use crate::tools::runner::{ToolExec, ToolOutcome};

pub struct GoalReadTool;
impl ToolExec for GoalReadTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("GoalRead", ApprovalTier::Read, ToolResultKind::Text)
    }
    fn execute(&self, input: &serde_json::Value, ctx: &ToolContext) -> anyhow::Result<ToolOutcome> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {}
        let _: Args = crate::tools::args::decode_args(input)?;
        let goal = ctx
            .goal_review
            .as_ref()
            .or(ctx.goal_adjudication.as_ref())
            .map(|r| r.snapshot_for_read())
            .or_else(|| ctx.goal_store.snapshot())
            .filter(|g| !g.hidden);
        let snapshot = goal
            .as_ref()
            .map(|g| -> anyhow::Result<serde_json::Value> {
                let mut value = serde_json::to_value(g)?;
                value["budget"] = serde_json::to_value(g.budget())?;
                value["requirement_ids"] = serde_json::to_value(g.requirement_ids())?;
                value["latest_round_decision"] = serde_json::to_value(g.latest_round_decision())?;
                Ok(value)
            })
            .transpose()?;
        let value = if goal.as_ref().is_some_and(|g| g.status == GoalStatus::Draft) {
            serde_json::json!({"goal":null,"draft":snapshot})
        } else {
            serde_json::json!({"goal":snapshot,"draft":null})
        };
        Ok(ToolOutcome::text(serde_json::to_string(&value)?))
    }
}

pub struct GoalDraftTool;
impl ToolExec for GoalDraftTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("GoalDraft", ApprovalTier::Write, ToolResultKind::Control)
            .mutating()
            .storm_exempt()
    }
    fn execute(&self, input: &serde_json::Value, ctx: &ToolContext) -> anyhow::Result<ToolOutcome> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            draft_id: String,
            revision: u64,
            spec: GoalDraftSpec,
        }
        let args: Args = crate::tools::args::decode_args(input)?;
        anyhow::ensure!(ctx.goal_review.is_none(), "reviewers cannot edit contracts");
        let draft = ctx
            .goal_store
            .replace_draft(&args.draft_id, args.revision, args.spec)?;
        Ok(ToolOutcome::text(serde_json::to_string(&draft)?))
    }
}

pub struct GoalReviewTool;
impl ToolExec for GoalReviewTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("GoalReview", ApprovalTier::Write, ToolResultKind::Control)
            .mutating()
            .storm_exempt()
    }
    fn execute(&self, input: &serde_json::Value, ctx: &ToolContext) -> anyhow::Result<ToolOutcome> {
        let submission: GoalReviewSubmission =
            crate::tools::args::decode_args(input).map_err(|error| {
                if let Some(session) = &ctx.goal_adjudication {
                    session.record_rejection(format!("{error:#}"));
                }
                error
            })?;
        let session = ctx.goal_adjudication.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "GoalReview is only available during the host agent's adjudication turn"
            )
        })?;
        let result = match submission {
            GoalReviewSubmission::Decision(conclusion) => {
                session.submit_conclusion(conclusion, ctx)
            }
            GoalReviewSubmission::Full(review) => session.submit(review, ctx),
            GoalReviewSubmission::Continue(proposal) => session.submit_continue(proposal),
        };
        // A decoded but rejected conclusion is protocol feedback, not damaged
        // model output. Keep the failure visible and let the host correct it
        // in the same turn without spending the format-recovery window.
        if let Err(error) = result {
            session.record_rejection(format!("{error:#}"));
            let mut outcome = ToolOutcome::text(format!(
                "GoalReview rejected: {error:#}. No conclusion was recorded. Correct the reported fields and call GoalReview again. Evidence references must come from the independent report's inspected evidence or a successful Read; missing evidence uses [] and unknown findings."
            ));
            outcome.success = false;
            outcome.no_mutation = true;
            return Ok(outcome);
        }
        Ok(ToolOutcome::text("Host goal conclusion recorded.".into()))
    }
}
