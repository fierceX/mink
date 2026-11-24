use super::*;

pub fn goal_now() -> u64 {
    time::OffsetDateTime::now_utc().unix_timestamp().max(0) as u64
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Draft,
    Active,
    Paused,
    Blocked,
    BudgetExhausted,
    Completed,
    Cancelled,
    Failed,
}
impl GoalStatus {
    /// The run has ended. Only an explicit host resume may reopen Failed.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
    pub fn can_resume(&self) -> bool {
        matches!(
            self,
            Self::Active | Self::Paused | Self::Blocked | Self::BudgetExhausted | Self::Failed
        )
    }
    pub fn can_adjust_limits(&self) -> bool {
        !matches!(self, Self::Completed | Self::Cancelled)
    }
}

/// Stable lowercase label for display and model-facing text. Never `Debug`,
/// so no Rust enum or struct syntax can leak into a prompt.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum GoalAcceptance {
    AgentReviewed,
    ModelReported,
    Verifier { id: String },
    Human,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCompletionBasis {
    AgentReviewed,
    ModelReported,
    Verified,
    HumanApproved,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalLimits {
    pub max_turns: u32,
    pub deadline_unix_secs: u64,
    pub soft_token_limit: Option<u64>,
    /// Consecutive model-reported blockers required before a `blocked` report
    /// actually ends the goal. One unlucky turn must not discard the remaining
    /// budget, and a model that keeps re-reporting the same condition still
    /// reaches `blocked`.
    #[serde(default = "default_blocked_after_rounds")]
    pub blocked_after_consecutive_rounds: u32,
}

fn default_blocked_after_rounds() -> u32 {
    3
}
impl Default for GoalLimits {
    fn default() -> Self {
        Self {
            max_turns: 20,
            deadline_unix_secs: goal_now().saturating_add(3600),
            soft_token_limit: None,
            blocked_after_consecutive_rounds: default_blocked_after_rounds(),
        }
    }
}
impl GoalLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.max_turns > 0, "goal max_turns must be positive");
        ensure!(
            self.deadline_unix_secs > 0,
            "goal deadline must be positive"
        );
        ensure!(
            self.soft_token_limit != Some(0),
            "goal token soft limit must be positive"
        );
        ensure!(
            self.blocked_after_consecutive_rounds > 0,
            "goal blocked_after_consecutive_rounds must be positive"
        );
        Ok(())
    }
}
/// Persisted format version for the goal contract, snapshot, state container
/// and journal. The first release ships version 1; any other value is rejected
/// so an unknown format never reads as executable state.
pub(crate) const GOAL_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSpec {
    pub version: u32,
    #[serde(default)]
    pub original_request: String,
    #[serde(default)]
    pub subgoals: Vec<GoalSubgoal>,
    #[serde(default)]
    pub criteria: Vec<GoalCriterion>,
    #[serde(default)]
    pub suggested_actions: Vec<String>,
    #[serde(default)]
    pub checker_id: Option<String>,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub scope: String,
    pub constraints: Vec<String>,
    pub acceptance: GoalAcceptance,
    pub limits: GoalLimits,
}
impl GoalSpec {
    pub fn new(objective: impl Into<String>) -> Self {
        let objective = objective.into();
        Self {
            version: GOAL_FORMAT_VERSION,
            original_request: objective.clone(),
            subgoals: vec![GoalSubgoal { id: "subgoal-1".into(), objective: objective.clone() }],
            criteria: vec![GoalCriterion { id: "criterion-1".into(), subgoal_id: "subgoal-1".into(), outcome: objective.clone(), required_evidence: "Concrete files, artifacts or recorded verification results demonstrating the outcome".into() }],
            suggested_actions: Vec::new(),
            checker_id: None,
            acceptance_criteria: vec![objective.clone()],
            objective,
            scope: String::new(),
            constraints: Vec::new(),
            acceptance: GoalAcceptance::AgentReviewed,
            limits: GoalLimits::default(),
        }
    }
    /// Set simple acceptance outcomes while preserving stable criterion identities.
    pub fn set_acceptance_criteria(&mut self, outcomes: Vec<String>) {
        self.criteria = outcomes
            .iter()
            .enumerate()
            .map(|(i, outcome)| GoalCriterion {
                id: self
                    .criteria
                    .get(i)
                    .map(|c| c.id.clone())
                    .unwrap_or_else(|| format!("criterion-{}", i + 1)),
                subgoal_id: self
                    .subgoals
                    .first()
                    .map(|g| g.id.clone())
                    .unwrap_or_default(),
                outcome: outcome.clone(),
                required_evidence: "Concrete files, artifacts or recorded verification results"
                    .into(),
            })
            .collect();
        self.acceptance_criteria = outcomes;
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == GOAL_FORMAT_VERSION,
            "unsupported goal contract version"
        );
        ensure!(
            self.acceptance != GoalAcceptance::ModelReported,
            "model_reported acceptance is retired; use agent_reviewed"
        );
        validate_contract(self)?;
        ensure!(!self.objective.trim().is_empty(), "goal objective is empty");
        ensure!(
            !self.acceptance_criteria.is_empty()
                && self
                    .acceptance_criteria
                    .iter()
                    .all(|s| !s.trim().is_empty()),
            "goal requires nonempty acceptance criteria"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 64 * 1024,
            "goal contract too large"
        );
        if let GoalAcceptance::Verifier { id } = &self.acceptance {
            ensure!(!id.trim().is_empty(), "verifier ID is empty");
        }
        self.limits.validate()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalReportKind {
    Continue,
    Complete,
    Blocked,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalReport {
    pub goal_id: String,
    pub iteration: u32,
    pub kind: GoalReportKind,
    pub summary: String,
    pub next_step: String,
    pub criteria: Vec<String>,
    pub evidence: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    Idle,
    Executing,
    Verifying,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalIteration {
    pub iteration: u32,
    pub turn_id: String,
    pub billing_turn_id: Option<String>,
    pub finished: bool,
    #[serde(default)]
    pub conversation_start: usize,
    #[serde(default)]
    pub outcome_status: Option<crate::runtime::TurnStatus>,
    #[serde(default)]
    pub outcome_error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSnapshot {
    pub version: u32,
    pub id: String,
    pub revision: u64,
    pub contract_revision: u64,
    pub spec: GoalSpec,
    pub status: GoalStatus,
    pub phase: GoalPhase,
    pub reason: Option<String>,
    /// Infrastructure failure while auditing the current review candidate.
    /// Kept separate from goal evidence failures so a resume can retry the audit
    /// without admitting another execution turn.
    #[serde(default)]
    pub review_failure_reason: Option<String>,
    #[serde(default)]
    pub review_stage: Option<GoalReviewStage>,
    #[serde(default)]
    pub review_failure_stage: Option<GoalReviewStage>,
    #[serde(default)]
    pub review_requires_full: bool,
    #[serde(default)]
    pub review_escalation_reason: Option<String>,
    #[serde(default)]
    pub review_report: Option<GoalReviewReport>,
    #[serde(default)]
    pub continuations: Vec<GoalContinueRecord>,
    #[serde(default)]
    pub continuation_fingerprint: Option<String>,
    #[serde(default)]
    pub continuation_rounds: u32,
    pub completion_basis: Option<GoalCompletionBasis>,
    pub iterations: Vec<GoalIteration>,
    pub report: Option<GoalReport>,
    pub report_repair_used: bool,
    pub usage_records: Vec<crate::session::usage::UsageRecord>,
    pub usage: crate::session::usage::UsageSummary,
    pub pending_effects: Vec<String>,
    pub failure_fingerprint: Option<String>,
    #[serde(default)]
    pub repeated_failures: u32,
    /// Consecutive rounds whose report was `blocked`, whatever the condition.
    /// Reset by any other report kind; the block *gate* uses
    /// [`Self::blocker_rounds`], not this counter.
    #[serde(default)]
    pub blocked_rounds: u32,
    /// Consecutive rounds reporting the *same* blocker.
    ///
    /// [`Self::blocker_fingerprint`] records which condition the streak counts;
    /// a report with a different identity restarts this counter at 1, so a
    /// sequence of different blockers cannot end the goal. Continue or Complete
    /// clears the identity and both counters.
    #[serde(default)]
    pub blocker_rounds: u32,
    /// Identity of the blocker reported last round.
    #[serde(default)]
    pub blocker_fingerprint: Option<String>,
    #[serde(default)]
    pub evidence_versions: Vec<(String, String)>,
    #[serde(default)]
    pub acceptance_conversation_seq: usize,
    pub environment: String,
    pub hidden: bool,
    #[serde(default)]
    pub draft_duration_secs: Option<u64>,
    #[serde(default)]
    pub needs_confirmation: bool,
    #[serde(default)]
    pub review_candidate: Option<GoalReviewCandidate>,
    #[serde(default)]
    pub review_generation: u64,
    #[serde(default)]
    pub reviews: Vec<GoalReview>,
    #[serde(default)]
    pub checker_passed_candidate: Option<GoalReviewCandidate>,
    /// Versions observed immediately after a known passing checker, including
    /// an explicit absence when the checker removed an inspected file.
    #[serde(default)]
    pub checker_evidence_versions: Vec<(String, Option<String>)>,
    #[serde(default)]
    pub checker_records: Vec<GoalCheckerRecord>,
    #[serde(default)]
    pub confirmed_after_iteration: usize,
    #[serde(default)]
    pub draft_ready: bool,
}
/// Derived stopping budget for one goal.
///
/// The limits themselves are host-owned contract fields; this view is the
/// single place that turns them into the numbers every surface renders, so a
/// terminal, a web workbench and a future embedder cannot drift into showing
/// different budgets for the same goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalBudget {
    /// Admitted goal turns in total.
    pub max_turns: u32,
    /// Turns already consumed.
    pub turns_used: u32,
    pub remaining_turns: u32,
    /// Absolute deadline; the budget is spent when the clock passes it.
    pub deadline_unix_secs: u64,
    /// Seconds left before the deadline, saturating at zero.
    pub remaining_secs: u64,
    /// Consecutive model-reported blockers required to end the goal.
    pub blocked_after_consecutive_rounds: u32,
    /// Consecutive blocked reports (all blockers).
    pub blocked_rounds: u32,
    /// Consecutive rounds of the *same* blocker — the gate progress, not the
    /// count of blocked reports (see [`GoalSnapshot::blocker_rounds`]).
    pub blocker_rounds: u32,
    pub soft_token_limit: Option<u64>,
}

/// Default contract a host should offer when creating a goal: 20 turns and a
/// 60 minute absolute deadline. Exposed so no surface hard-codes its own copy.
pub fn default_limits() -> GoalLimits {
    GoalLimits::default()
}

impl GoalSnapshot {
    pub fn requirement_ids(&self) -> Vec<String> {
        std::iter::once("request:original".into())
            .chain(self.spec.criteria.iter().map(|c| c.id.clone()))
            .chain((0..self.spec.constraints.len()).map(|i| format!("constraint:{}", i + 1)))
            .collect()
    }
    pub fn latest_round_decision(&self) -> Option<GoalRoundDecision> {
        let full = self.reviews.iter().rev().find(|r| {
            r.candidate.goal_id == self.id
                && r.candidate.contract_revision == self.contract_revision
        });
        let partial = self.continuations.iter().rev().find(|r| {
            r.proposal.candidate.goal_id == self.id
                && r.proposal.candidate.contract_revision == self.contract_revision
        });
        match (full, partial) {
            (Some(f), Some(c))
                if (
                    c.proposal.candidate.iteration,
                    c.proposal.candidate.generation,
                ) > (f.candidate.iteration, f.candidate.generation) =>
            {
                Some(GoalRoundDecision::Continue(c.clone()))
            }
            (Some(f), _) => Some(GoalRoundDecision::Full(f.clone())),
            (_, Some(c)) => Some(GoalRoundDecision::Continue(c.clone())),
            _ => None,
        }
    }
    /// Current stopping budget, derived from the authoritative snapshot.
    pub fn budget(&self) -> GoalBudget {
        let turns_used = self.iterations.len().min(u32::MAX as usize) as u32;
        GoalBudget {
            max_turns: self.spec.limits.max_turns,
            turns_used,
            remaining_turns: self.remaining_turns(),
            deadline_unix_secs: self.spec.limits.deadline_unix_secs,
            remaining_secs: self
                .spec
                .limits
                .deadline_unix_secs
                .saturating_sub(goal_now()),
            blocked_after_consecutive_rounds: self.spec.limits.blocked_after_consecutive_rounds,
            blocked_rounds: self.blocked_rounds,
            blocker_rounds: self.blocker_rounds,
            soft_token_limit: self.spec.limits.soft_token_limit,
        }
    }

    /// A cancelled/completed goal cannot stay in an execution/verification
    /// phase. Paused, blocked, budget-exhausted and failed goals preserve it:
    /// resume uses `Verifying` to retry the interrupted review.
    pub(crate) fn normalize_phase(&mut self) {
        if matches!(self.status, GoalStatus::Cancelled | GoalStatus::Completed) {
            self.phase = GoalPhase::Idle;
            self.review_stage = None;
        }
    }

    pub fn remaining_turns(&self) -> u32 {
        self.spec
            .limits
            .max_turns
            .saturating_sub(self.iterations.len() as u32)
    }
    pub fn budget_reason(&self) -> Option<&'static str> {
        if goal_now() >= self.spec.limits.deadline_unix_secs {
            return Some("goal deadline reached");
        }
        if self.remaining_turns() == 0 {
            return Some("goal turn budget exhausted");
        }
        self.review_budget_reason()
    }
    /// Audits spend time and tokens, but never an additional execution turn.
    pub(crate) fn review_budget_reason(&self) -> Option<&'static str> {
        if goal_now() >= self.spec.limits.deadline_unix_secs {
            return Some("goal deadline reached");
        }
        let t = &self.usage.tokens;
        let used = t
            .input_tokens
            .saturating_add(t.cache_read_tokens)
            .saturating_add(t.cache_creation_tokens)
            .saturating_add(t.output_tokens);
        if self
            .spec
            .limits
            .soft_token_limit
            .is_some_and(|limit| used >= limit)
        {
            return Some("goal token soft limit reached");
        }
        None
    }
}
pub type GoalOutcome = GoalSnapshot;

/// Read the persisted goal without opening the store or taking a lease.
///
/// UI surfaces use this for a read-only mirror on session open. `GoalStore`
/// remains the only writer; a malformed or missing file reads as "no goal"
/// rather than failing the surface that is merely displaying it.
pub fn read_snapshot(path: &Path) -> Option<GoalSnapshot> {
    let bytes = std::fs::read(path).ok()?;
    let state: State = serde_json::from_slice(&bytes).ok()?;
    if state.version != GOAL_FORMAT_VERSION
        || state.current.as_ref().is_some_and(|g| {
            g.version != GOAL_FORMAT_VERSION || g.spec.version != GOAL_FORMAT_VERSION
        })
    {
        return None;
    }
    state.current
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct State {
    pub(super) version: u32,
    pub(super) current: Option<GoalSnapshot>,
    pub(super) history: Vec<GoalSnapshot>,
}
