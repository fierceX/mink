use crate::store::{
    GoalBudget, GoalContinueRecord, GoalCriterion, GoalReview, GoalReviewCandidate,
    GoalReviewStage, GoalSnapshot, GoalSubgoal,
};
use serde::{Deserialize, Serialize};

/// Read-only projection of the authoritative goal snapshot for UI surfaces.
///
/// The contract, budget and lifecycle stay owned by the goal engine; a UI only
/// renders this. `hidden` goals were explicitly cleared by the user and must
/// not be surfaced as an active marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalDisplay {
    #[serde(default)]
    pub draft_duration_secs: Option<u64>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub original_request: String,
    #[serde(default)]
    pub subgoals: Vec<GoalSubgoal>,
    #[serde(default)]
    pub criteria: Vec<GoalCriterion>,
    #[serde(default)]
    pub contract_revision: u64,
    #[serde(default)]
    pub reviews: Vec<GoalReview>,
    #[serde(default)]
    pub review_stage: Option<GoalReviewStage>,
    #[serde(default)]
    pub continuations: Vec<GoalContinueRecord>,
    #[serde(default)]
    pub review_candidate: Option<GoalReviewCandidate>,
    pub id: String,
    /// Derived stopping contract; the same numbers every surface renders.
    pub budget: GoalBudget,
    pub status: String,
    pub phase: String,
    pub objective: String,
    pub acceptance: String,
    pub acceptance_criteria: Vec<String>,
    pub scope: String,
    pub constraints: Vec<String>,
    pub completion_basis: Option<String>,
    pub remaining_turns: u32,
    pub max_turns: u32,
    pub deadline_unix_secs: u64,
    pub soft_token_limit: Option<u64>,
    pub iterations: usize,
    pub reason: Option<String>,
    pub hidden: bool,
}

impl From<&GoalSnapshot> for GoalDisplay {
    fn from(goal: &GoalSnapshot) -> Self {
        let budget = goal.budget();
        Self {
            draft_duration_secs: goal.draft_duration_secs,
            revision: goal.revision,
            original_request: goal.spec.original_request.clone(),
            subgoals: goal.spec.subgoals.clone(),
            criteria: goal.spec.criteria.clone(),
            contract_revision: goal.contract_revision,
            reviews: goal.reviews.clone(),
            review_stage: goal.review_stage,
            continuations: goal.continuations.clone(),
            review_candidate: goal.review_candidate.clone(),
            id: goal.id.clone(),
            budget: goal.budget(),
            status: format!("{:?}", goal.status).to_lowercase(),
            phase: format!("{:?}", goal.phase).to_lowercase(),
            objective: goal.spec.objective.clone(),
            acceptance: serde_json::to_value(&goal.spec.acceptance)
                .ok()
                .and_then(|v| v["mode"].as_str().map(str::to_string))
                .unwrap_or_default(),
            acceptance_criteria: goal.spec.acceptance_criteria.clone(),
            scope: goal.spec.scope.clone(),
            constraints: goal.spec.constraints.clone(),
            completion_basis: goal.completion_basis.as_ref().map(|basis| {
                serde_json::to_value(basis)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            }),
            remaining_turns: budget.remaining_turns,
            max_turns: budget.max_turns,
            deadline_unix_secs: budget.deadline_unix_secs,
            soft_token_limit: budget.soft_token_limit,
            iterations: goal.iterations.len(),
            reason: goal.reason.clone(),
            hidden: goal.hidden,
        }
    }
}
