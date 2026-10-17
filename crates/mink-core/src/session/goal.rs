//! Durable explicit goals; model reports never grant lifecycle authority.
pub use super::goal_protocol::*;
use crate::session::persistence::{PersistenceFault, publish_state};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
fn status_label(status: &GoalStatus) -> &'static str {
    match status {
        GoalStatus::Draft => "draft",
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::BudgetExhausted => "budget_exhausted",
        GoalStatus::Completed => "completed",
        GoalStatus::Cancelled => "cancelled",
        GoalStatus::Failed => "failed",
    }
}

fn phase_label(phase: &GoalPhase) -> &'static str {
    match phase {
        GoalPhase::Idle => "idle",
        GoalPhase::Executing => "executing",
        GoalPhase::Verifying => "verifying",
    }
}

fn acceptance_label(acceptance: &GoalAcceptance) -> &'static str {
    match acceptance {
        GoalAcceptance::AgentReviewed => "agent_reviewed",
        GoalAcceptance::ModelReported => "model_reported",
        GoalAcceptance::Verifier { .. } => "verifier",
        GoalAcceptance::Human => "human",
    }
}

fn basis_label(basis: &GoalCompletionBasis) -> &'static str {
    match basis {
        GoalCompletionBasis::AgentReviewed => "agent_reviewed",
        GoalCompletionBasis::ModelReported => "model_reported",
        GoalCompletionBasis::Verified => "verified",
        GoalCompletionBasis::HumanApproved => "human_approved",
    }
}
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
struct State {
    version: u32,
    current: Option<GoalSnapshot>,
    history: Vec<GoalSnapshot>,
}
/// Identity of one admitted goal iteration.
///
/// Settlement and late callbacks name the goal, iteration and turn they belong
/// to instead of "the latest iteration of whatever is current now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IterationKey {
    pub(crate) goal_id: String,
    pub(crate) iteration: u32,
    pub(crate) turn_id: String,
}

/// Outcome of an identity-bound settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyResult {
    Applied,
    AlreadyApplied,
}

/// Facts the orchestrator prepared at the persistence boundary.
#[derive(Debug, Clone, Default)]
pub(crate) struct SettlementFacts {
    /// An interrupted turn with no session-wide stop in flight pauses the goal.
    pub(crate) interrupted_pause: bool,
}

pub struct GoalStore {
    path: PathBuf,
    journal: PathBuf,
    state: Mutex<State>,
    fault: PersistenceFault,
    changes: tokio::sync::watch::Sender<Option<GoalSnapshot>>,
}
/// Why a goal change was refused.
///
/// A rejected command (stale revision, invalid transition) says nothing about
/// the run: it must not be treated as a stop-intent failure. Only a `Publish`
/// failure means the intent could not be stored, which is what stop commands
/// have to act on.
pub(crate) enum GoalChangeError {
    /// The command itself was rejected; the run keeps its lifecycle.
    Rejected(anyhow::Error),
    /// The new state could not be published; the authoritative snapshot is
    /// unchanged.
    Publish(anyhow::Error),
}

impl GoalChangeError {
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Rejected(error) | Self::Publish(error) => error,
        }
    }
}

impl GoalStore {
    pub(crate) fn load(directory: &Path, fault: PersistenceFault) -> Result<Self> {
        let path = directory.join("goal.json");
        let journal = directory.join("goal-transaction.json");
        let read = |p: &Path| -> Result<Option<State>> {
            match std::fs::read(p) {
                Ok(bytes) => {
                    let state: State = serde_json::from_slice(&bytes)?;
                    ensure!(
                        state.version == GOAL_FORMAT_VERSION,
                        "unsupported goal state version"
                    );
                    for goal in state.current.iter().chain(state.history.iter()) {
                        ensure!(
                            goal.version == GOAL_FORMAT_VERSION
                                && goal.spec.version == GOAL_FORMAT_VERSION,
                            "unsupported goal contract version"
                        );
                    }
                    Ok(Some(state))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        };
        let state = read(&path)?.unwrap_or(State {
            version: GOAL_FORMAT_VERSION,
            ..State::default()
        });
        let (changes, _) = tokio::sync::watch::channel(state.current.clone());
        let store = Self {
            path,
            journal,
            state: Mutex::new(state),
            fault,
            changes,
        };
        if let Some(pending) = read(&store.journal)? {
            ensure!(
                pending.version == GOAL_FORMAT_VERSION,
                "unsupported goal journal"
            );
            publish_state(&store.path, &serde_json::to_vec(&pending)?, &store.fault)?;
            *store.state.lock().unwrap() = pending.clone();
            store.changes.send_replace(pending.current);
        }
        ensure!(
            store.state.lock().unwrap().version == GOAL_FORMAT_VERSION,
            "unsupported goal state"
        );
        Ok(store)
    }
    pub fn snapshot(&self) -> Option<GoalSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .clone()
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<GoalSnapshot>> {
        self.changes.subscribe()
    }
    fn publish(&self, state: &mut State, mut next: State) -> Result<()> {
        self.fault.check()?;
        next.version = GOAL_FORMAT_VERSION;
        for goal in next.current.iter_mut().chain(next.history.iter_mut()) {
            goal.version = GOAL_FORMAT_VERSION;
        }
        let bytes = serde_json::to_vec(&next)?;
        publish_state(&self.journal, &bytes, &self.fault)?;
        if let Err(error) = publish_state(&self.path, &bytes, &self.fault) {
            return Err(self.fault.raise(
                &self.path,
                format!("goal journal committed, snapshot publication failed: {error:#}"),
            ));
        }
        *state = next;
        self.changes.send_replace(state.current.clone());
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn create(&self, spec: GoalSpec, environment: String) -> Result<GoalSnapshot> {
        self.create_snapshot(spec, environment, None, GoalStatus::Active, true)
    }
    pub(crate) fn draft(
        &self,
        spec: GoalSpec,
        environment: String,
        duration_secs: u64,
    ) -> Result<GoalDraftSnapshot> {
        self.command(GoalCommand::CreateDraft {
            spec: Box::new(spec),
            environment,
            duration_secs,
            ready: true,
        })
    }
    pub(crate) fn draft_pending(
        &self,
        spec: GoalSpec,
        environment: String,
        duration_secs: u64,
    ) -> Result<GoalDraftSnapshot> {
        self.command(GoalCommand::CreateDraft {
            spec: Box::new(spec),
            environment,
            duration_secs,
            ready: false,
        })
    }
    pub(crate) fn command(&self, command: GoalCommand) -> Result<GoalSnapshot> {
        if let GoalCommand::CreateDraft {
            spec,
            environment,
            duration_secs,
            ready,
        } = command
        {
            ensure!(duration_secs > 0, "draft duration must be positive");
            return self.create_snapshot(
                *spec,
                environment,
                Some(duration_secs),
                GoalStatus::Draft,
                ready,
            );
        }
        let (id, revision) = command.identity().expect("existing goal command");
        let id = id.to_string();
        self.change(&id, revision, |g| command.apply_to(g))
    }

    fn create_snapshot(
        &self,
        spec: GoalSpec,
        environment: String,
        duration: Option<u64>,
        status: GoalStatus,
        draft_ready: bool,
    ) -> Result<GoalSnapshot> {
        spec.validate()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state
                .current
                .as_ref()
                .is_none_or(|g| g.status.is_terminal()),
            "session already has an unfinished goal"
        );
        let mut next = state.clone();
        if let Some(old) = next.current.take() {
            next.history.push(old);
        }
        let goal = GoalSnapshot {
            version: GOAL_FORMAT_VERSION,
            id: format!(
                "goal-{}",
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            ),
            revision: 1,
            contract_revision: 1,
            spec,
            status,
            phase: GoalPhase::Idle,
            reason: None,
            review_failure_reason: None,
            review_stage: None,
            review_failure_stage: None,
            review_requires_full: false,
            review_escalation_reason: None,
            review_report: None,
            continuations: Vec::new(),
            continuation_fingerprint: None,
            continuation_rounds: 0,
            completion_basis: None,
            iterations: Vec::new(),
            report: None,
            report_repair_used: false,
            usage_records: Vec::new(),
            usage: Default::default(),
            pending_effects: Vec::new(),
            failure_fingerprint: None,
            repeated_failures: 0,
            blocked_rounds: 0,
            blocker_rounds: 0,
            blocker_fingerprint: None,
            evidence_versions: Vec::new(),
            acceptance_conversation_seq: 0,
            environment,
            hidden: false,
            draft_duration_secs: duration,
            needs_confirmation: false,
            review_candidate: None,
            review_generation: 0,
            reviews: Vec::new(),
            checker_passed_candidate: None,
            checker_evidence_versions: Vec::new(),
            checker_records: Vec::new(),
            confirmed_after_iteration: 0,
            draft_ready,
        };
        next.current = Some(goal.clone());
        self.publish(&mut state, next)?;
        Ok(goal)
    }
    pub(crate) fn replace_draft(
        &self,
        id: &str,
        revision: u64,
        draft: GoalDraftSpec,
    ) -> Result<GoalDraftSnapshot> {
        self.command(GoalCommand::Draft {
            id: id.into(),
            revision,
            spec: draft,
        })
    }
    pub(crate) fn confirm(
        &self,
        id: &str,
        revision: u64,
        environment: String,
    ) -> Result<GoalSnapshot> {
        self.command(GoalCommand::Confirm {
            id: id.into(),
            revision,
            environment,
        })
    }

    pub(crate) fn change(
        &self,
        id: &str,
        revision: u64,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> Result<GoalSnapshot> {
        self.change_classified(id, revision, f)
            .map_err(GoalChangeError::into_anyhow)
    }

    /// [`Self::change`] with the refusal classified as command or publication.
    ///
    /// Stop commands must stop the run when their intent cannot be stored, but
    /// must not touch the run for a stale revision or an invalid transition:
    /// only this split makes that distinction.
    pub(crate) fn change_classified(
        &self,
        id: &str,
        revision: u64,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> std::result::Result<GoalSnapshot, GoalChangeError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = state.clone();
        let g = next
            .current
            .as_mut()
            .ok_or_else(|| GoalChangeError::Rejected(anyhow::anyhow!("no goal")))?;
        if g.id != id || g.revision != revision {
            return Err(GoalChangeError::Rejected(anyhow::anyhow!(
                "stale goal ID or revision; reread goal_snapshot"
            )));
        }
        f(g).map_err(GoalChangeError::Rejected)?;
        g.revision = g
            .revision
            .checked_add(1)
            .ok_or_else(|| GoalChangeError::Rejected(anyhow::anyhow!("goal revision overflow")))?;
        g.version = GOAL_FORMAT_VERSION;
        let result = g.clone();
        self.publish(&mut state, next)
            .map_err(GoalChangeError::Publish)?;
        Ok(result)
    }
    /// Maintenance update for the whole snapshot (recovery, reconciliation,
    /// projection helpers). Round decisions must use [`Self::update_if_current`]
    /// or a typed entry point so a replaced goal can never be modified.
    pub(crate) fn update(
        &self,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> Result<GoalSnapshot> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = state.clone();
        let g = next
            .current
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no goal"))?;
        f(g)?;
        g.revision = g
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("goal revision overflow"))?;
        g.version = GOAL_FORMAT_VERSION;
        let result = g.clone();
        self.publish(&mut state, next)?;
        Ok(result)
    }
    /// Update the current goal only while `id` is still the current one.
    ///
    /// A driver can still be settling a round after the goal was cancelled and
    /// replaced. Revisions restart at 1 for every goal, so an unguarded update
    /// would land the old round's usage and decisions on the replacement.
    /// Returns `None` when the goal was replaced, so callers can skip.
    pub(crate) fn update_if_current(
        &self,
        id: &str,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> Result<Option<GoalSnapshot>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .current
            .as_ref()
            .is_none_or(|current| current.id != id)
        {
            return Ok(None);
        }
        let mut next = state.clone();
        let goal = next
            .current
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no goal"))?;
        f(goal)?;
        goal.revision = goal
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("goal revision overflow"))?;
        goal.version = GOAL_FORMAT_VERSION;
        let result = goal.clone();
        self.publish(&mut state, next)?;
        Ok(Some(result))
    }

    pub(crate) fn mark_effect(&self, call_id: &str) -> Result<()> {
        if self
            .snapshot()
            .is_none_or(|g| g.phase != GoalPhase::Executing)
        {
            return Ok(());
        }
        self.update(|g| {
            ensure!(
                g.status == GoalStatus::Active && g.phase == GoalPhase::Executing,
                "goal stopped before tool dispatch"
            );
            if !g.pending_effects.iter().any(|id| id == call_id) {
                g.pending_effects.push(call_id.into());
            }
            Ok(())
        })?;
        Ok(())
    }
    pub(crate) fn close_effects(&self, ids: &[String]) -> Result<()> {
        if self
            .snapshot()
            .is_none_or(|g| !g.pending_effects.iter().any(|id| ids.contains(id)))
        {
            return Ok(());
        }
        self.update(|g| {
            g.pending_effects.retain(|id| !ids.contains(id));
            Ok(())
        })?;
        Ok(())
    }
    /// Append the terminal goal facts once, at the tail of the conversation.
    ///
    /// Informational by contract: the lifecycle has already settled, so a failed
    /// append is reported by the caller instead of changing the goal state or
    /// latching the session.
    pub(crate) async fn append_ended_row(
        &self,
        store: &crate::session::store::ConversationStore,
        max_chars: usize,
    ) -> Result<bool> {
        let Some(g) = self
            .snapshot()
            .filter(|g| g.status.is_terminal() && !g.hidden)
        else {
            return Ok(false);
        };
        if store
            .lines()
            .await?
            .iter()
            .any(|m| m["_mink"]["goal_id"] == g.id && m["_mink"]["goal_state_kind"] == "ended")
        {
            return Ok(false);
        }
        let message = serde_json::json!({
            "role": "user",
            "content": state_text(&g, GoalStateKind::Ended, max_chars),
            "_mink": {
                "internal": true,
                "goal_id": g.id,
                "goal_revision": g.revision,
                "goal_state_kind": "ended",
            },
        });
        store.append_runtime_message_durable(message).await?;
        Ok(true)
    }
    /// Contract checkpoint mounted inside the active window after a compaction.
    ///
    /// The rendering carries no per-round facts, so the checkpoint stays
    /// byte-stable between rounds and the request prefix survives the very
    /// compaction that introduced it.
    pub(crate) fn checkpoint(&self, max_chars: usize) -> Result<Option<serde_json::Value>> {
        let Some(g) = self
            .snapshot()
            .filter(|g| !g.status.is_terminal() && !g.hidden)
        else {
            return Ok(None);
        };
        Ok(Some(serde_json::json!({
            "role": "user",
            "content": state_text(&g, GoalStateKind::Contract, max_chars),
            "_mink": {
                "internal": true,
                "goal_id": g.id,
                "goal_revision": g.revision,
                "goal_state_kind": "contract",
            },
        })))
    }
    /// Begin an iteration for the current schedulable goal.
    ///
    /// Schedulability, phase reset and the iteration row land in one atomic
    /// commit; the caller maps the error to its own surface.
    pub(crate) fn begin_iteration(
        &self,
        turn_id: &str,
        conversation_start: usize,
    ) -> Result<GoalSnapshot> {
        self.update(|g| {
            anyhow::ensure!(
                g.status == GoalStatus::Active && g.phase == GoalPhase::Idle,
                "goal is not schedulable"
            );
            anyhow::ensure!(g.budget_reason().is_none(), "goal budget exhausted");
            g.phase = GoalPhase::Executing;
            g.review_stage = None;
            g.review_requires_full = false;
            g.review_escalation_reason = None;
            g.review_report = None;
            g.report = None;
            g.review_candidate = None;
            g.review_failure_stage = None;
            g.checker_passed_candidate = None;
            g.checker_evidence_versions.clear();
            g.evidence_versions.clear();
            g.iterations.push(GoalIteration {
                iteration: g.iterations.len() as u32 + 1,
                turn_id: turn_id.to_string(),
                billing_turn_id: None,
                finished: false,
                conversation_start,
                outcome_status: None,
                outcome_error: None,
            });
            Ok(())
        })
    }

    /// Associate the running iteration with the turn that bills it.
    ///
    /// Identity-bound to the iteration that is currently executing: a
    /// finished or missing iteration is left untouched.
    pub(crate) fn associate_iteration_billing(&self, billing_turn_id: &str) -> Result<()> {
        self.update(|g| {
            if g.phase == GoalPhase::Executing
                && let Some(iteration) = g.iterations.last_mut()
                && !iteration.finished
            {
                iteration.billing_turn_id = Some(billing_turn_id.to_string());
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Pause the current goal when it is still active.
    ///
    /// The user interrupt targets "the goal that is running now"; a replaced
    /// or already paused goal must not be touched.
    pub(crate) fn pause_if_active(&self, reason: &str) -> Result<Option<GoalSnapshot>> {
        if self
            .snapshot()
            .is_none_or(|goal| goal.status != GoalStatus::Active)
        {
            return Ok(None);
        }
        let goal = self.update(|g| {
            if g.status == GoalStatus::Active {
                g.status = GoalStatus::Paused;
                g.reason = Some(reason.to_string());
            }
            Ok(())
        })?;
        Ok(Some(goal))
    }

    /// Settle one iteration by identity, exactly once.
    ///
    /// A repeat with identical facts is idempotent; a repeat with different
    /// facts is an error instead of silently overwriting. The status decision
    /// of the round (failed/interrupted) lands in the same atomic commit as
    /// the finished/billing/usage facts, so a crash cannot leave them split.
    pub(crate) fn settle_iteration(
        &self,
        key: &IterationKey,
        result: &crate::agent::orchestrator::TurnRunResult,
        facts: &SettlementFacts,
    ) -> Result<ApplyResult> {
        use crate::runtime::TurnStatus;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = state.clone();
        let is_current = next
            .current
            .as_ref()
            .is_some_and(|goal| goal.id == key.goal_id);
        let goal = if is_current {
            next.current.as_mut().expect("checked current goal")
        } else {
            next.history
                .iter_mut()
                .find(|goal| goal.id == key.goal_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "goal iteration settlement names an unknown goal: {}",
                        key.goal_id
                    )
                })?
        };
        let iteration = goal
            .iterations
            .iter_mut()
            .find(|iteration| {
                iteration.iteration == key.iteration && iteration.turn_id == key.turn_id
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "goal {} has no iteration {} for turn {}",
                    key.goal_id,
                    key.iteration,
                    key.turn_id
                )
            })?;
        if iteration.finished {
            let same = iteration.billing_turn_id.as_deref()
                == Some(result.billing_turn_id.as_str())
                && iteration.outcome_status == Some(result.status);
            return if same {
                Ok(ApplyResult::AlreadyApplied)
            } else {
                anyhow::bail!(
                    "goal iteration {} was already settled with different facts",
                    key.iteration
                );
            };
        }
        iteration.finished = true;
        iteration.billing_turn_id = Some(result.billing_turn_id.clone());
        iteration.outcome_status = Some(result.status);
        iteration.outcome_error = result.error.clone();
        for record in &result.usage_records {
            if !goal
                .usage_records
                .iter()
                .any(|old| old.request_id == record.request_id)
            {
                goal.usage_records.push(record.clone());
            }
        }
        goal.usage = crate::session::usage::UsageSummary::from_records(&goal.usage_records);
        if is_current {
            let goal = next.current.as_mut().expect("checked current goal");
            goal.phase = GoalPhase::Idle;
            if goal.status == GoalStatus::Active {
                match result.status {
                    TurnStatus::Failed => {
                        goal.status = GoalStatus::Failed;
                        goal.reason = result.error.clone();
                    }
                    TurnStatus::Interrupted if facts.interrupted_pause => {
                        goal.status = GoalStatus::Paused;
                        goal.reason = Some("execution stopped".into());
                    }
                    _ => {}
                }
            }
            // Natural completion only creates an audit phase. The execution model
            // cannot decide the goal's completion or consume a repair round.
            if goal.status == GoalStatus::Active && result.status == TurnStatus::Ok {
                goal.phase = GoalPhase::Verifying;
                goal.review_candidate = None;
                goal.review_stage = Some(GoalReviewStage::Screening);
            }
        }
        self.publish(&mut state, next)?;
        Ok(ApplyResult::Applied)
    }
    pub(crate) fn reconcile_usage(
        &self,
        records: &[crate::session::usage::UsageRecord],
    ) -> Result<()> {
        let Some(goal) = self.snapshot() else {
            return Ok(());
        };
        let additional: Vec<_> = records
            .iter()
            .filter(|r| {
                (goal
                    .iterations
                    .iter()
                    .any(|i| i.billing_turn_id.as_deref() == Some(r.billing_turn_id.as_str()))
                    || r.billing_turn_id
                        .starts_with(&format!("goal-review:{}:", goal.id)))
                    && !goal
                        .usage_records
                        .iter()
                        .any(|old| old.request_id == r.request_id)
            })
            .cloned()
            .collect();
        if additional.is_empty() {
            return Ok(());
        }
        self.update(|g| {
            for record in additional {
                if !g
                    .usage_records
                    .iter()
                    .any(|old| old.request_id == record.request_id)
                {
                    g.usage_records.push(record);
                }
            }
            g.usage = crate::session::usage::UsageSummary::from_records(&g.usage_records);
            Ok(())
        })?;
        Ok(())
    }
    pub(crate) fn recover(&self, environment: &str, history: &[serde_json::Value]) -> Result<()> {
        let Some(g) = self.snapshot().filter(|g| !g.status.is_terminal()) else {
            return Ok(());
        };
        self.change(&g.id, g.revision, |g| {
            let start = g.iterations.last().map_or(0, |i| i.conversation_start);
            let resolved: Vec<_> = history
                .iter()
                .skip(start)
                .filter_map(|m| m["content"].as_array())
                .flatten()
                .filter(|b| {
                    b["type"] == "tool_result"
                        && serde_json::from_value::<crate::tools::metadata::ToolStatus>(
                            b["_mink"]["status"].clone(),
                        )
                        .is_ok()
                })
                .filter_map(|b| b["tool_use_id"].as_str())
                .collect();
            g.pending_effects
                .retain(|id| !resolved.contains(&id.as_str()));
            ensure!(
                g.spec.version == GOAL_FORMAT_VERSION,
                "unsupported goal contract version"
            );
            g.spec.validate()?;
            if g.status != GoalStatus::Active {
                if !g.pending_effects.is_empty() {
                    if g.status != GoalStatus::Draft {
                        g.status = GoalStatus::Blocked;
                    }
                    g.reason = Some("side effect result unknown; reconcile before resume".into());
                }
                return Ok(());
            }
            if g.environment != environment {
                g.status = GoalStatus::Blocked;
                g.reason = Some(
                    "goal permissions or working directory changed; reconcile before resume".into(),
                );
            } else if !g.pending_effects.is_empty() {
                g.status = GoalStatus::Blocked;
                g.reason = Some("side effect result unknown; reconcile before resume".into());
            } else if g.iterations.last().is_some_and(|i| i.finished) {
                let settled_review = g.reviews.last().filter(|review| {
                    review.candidate.goal_id == g.id
                        && review.candidate.contract_revision == g.contract_revision
                        && review.candidate.iteration as usize == g.iterations.len()
                        && g.acceptance_conversation_seq == history.len()
                        && review.verdict != GoalReviewVerdict::Satisfied
                });
                match g.iterations.last().and_then(|i| i.outcome_status) {
                    Some(crate::runtime::TurnStatus::Failed) => {
                        g.status = GoalStatus::Failed;
                        g.reason = g.iterations.last().and_then(|i| i.outcome_error.clone());
                        g.phase = GoalPhase::Idle;
                    }
                    Some(crate::runtime::TurnStatus::Ok)
                        if settled_review.is_some()
                            || g.continuations.last().is_some_and(|c| {
                                c.proposal.candidate.goal_id == g.id
                                    && c.proposal.candidate.contract_revision == g.contract_revision
                                    && c.proposal.candidate.iteration as usize == g.iterations.len()
                                    && c.conversation_seq == history.len()
                            }) =>
                    {
                        // This boundary already includes an atomic review/decision;
                        // resume the next execution rather than replaying its audit.
                        g.phase = GoalPhase::Idle;
                        g.review_candidate =
                            settled_review.map(|r| r.candidate.clone()).or_else(|| {
                                g.continuations.last().map(|c| c.proposal.candidate.clone())
                            });
                    }
                    Some(crate::runtime::TurnStatus::Ok)
                        if matches!(g.phase, GoalPhase::Idle | GoalPhase::Executing) =>
                    {
                        g.phase = GoalPhase::Verifying;
                        g.review_candidate = None;
                        g.reason = Some(
                            "recovered normal execution boundary; independent audit required"
                                .into(),
                        );
                    }
                    _ => {}
                }
            } else if g.phase == GoalPhase::Executing {
                g.phase = GoalPhase::Idle;
                g.reason = Some("interrupted iteration; recovery iteration required".into());
            }
            Ok(())
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_contract_requires_independent_review_protocol() {
        let spec = serde_json::to_value(GoalSpec::new("produce proof")).unwrap();
        assert_eq!(spec["version"], GOAL_FORMAT_VERSION);
        assert_eq!(spec["acceptance"]["mode"], "agent_reviewed");
        assert_eq!(spec["original_request"], "produce proof");
        assert_eq!(spec["criteria"][0]["id"], "criterion-1");
    }
    fn store() -> (PathBuf, GoalStore) {
        let dir = std::env::temp_dir().join(format!(
            "mink-goal-{}-{:?}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
        (dir, store)
    }
    #[test]
    fn draft_confirmation_is_cas_and_starts_the_deadline_only_once() {
        let (dir, store) = store();
        let mut spec = GoalSpec::new("ship");
        spec.limits.deadline_unix_secs = 1;
        let draft = store.draft(spec, "env".into(), 120).unwrap();
        assert_eq!(draft.status, GoalStatus::Draft);
        assert!(store.begin_iteration("turn", 0).is_err());
        assert!(
            store
                .confirm(&draft.id, draft.revision + 1, "env".into())
                .is_err()
        );
        let confirmed = store
            .confirm(&draft.id, draft.revision, "env".into())
            .unwrap();
        assert_eq!(confirmed.status, GoalStatus::Active);
        assert!(confirmed.spec.limits.deadline_unix_secs >= goal_now() + 119);
        assert!(
            store
                .confirm(&draft.id, draft.revision, "env".into())
                .is_err()
        );
        let restored = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
        assert_eq!(restored.snapshot(), Some(confirmed));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pending_request_is_never_confirmable_before_a_proposal_is_saved() {
        let (dir, store) = store();
        let pending = store
            .draft_pending(GoalSpec::new("ship"), "env".into(), 120)
            .unwrap();
        assert!(!pending.draft_ready);
        assert!(
            store
                .confirm(&pending.id, pending.revision, "env".into())
                .is_err()
        );
        let spec = &pending.spec;
        let ready = store
            .replace_draft(
                &pending.id,
                pending.revision,
                GoalDraftSpec {
                    objective: spec.objective.clone(),
                    scope: spec.scope.clone(),
                    constraints: spec.constraints.clone(),
                    subgoals: spec.subgoals.clone(),
                    criteria: spec.criteria.clone(),
                    suggested_actions: Vec::new(),
                },
            )
            .unwrap();
        assert!(ready.draft_ready);
        assert!(
            store
                .confirm(&pending.id, pending.revision, "env".into())
                .is_err()
        );
        assert!(
            store
                .confirm(&ready.id, ready.revision, "env".into())
                .is_ok()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Hosts render the budget from this one derivation instead of each
    /// recomputing remaining turns, deadline and blocked threshold themselves.
    #[test]
    fn budget_view_derives_the_stopping_contract() {
        let (dir, store) = store();
        let created = store
            .create(GoalSpec::new("bounded"), "env".into())
            .unwrap();
        let budget = created.budget();
        assert_eq!(budget.max_turns, 20);
        assert_eq!(budget.turns_used, 0);
        assert_eq!(budget.remaining_turns, 20);
        assert_eq!(budget.blocked_after_consecutive_rounds, 3);
        assert_eq!(budget.blocked_rounds, 0);
        assert_eq!(budget.blocker_rounds, 0);
        assert_eq!(budget.soft_token_limit, None);
        assert!(budget.remaining_secs > 0 && budget.remaining_secs <= 3600);
        assert_eq!(
            budget.deadline_unix_secs,
            created.spec.limits.deadline_unix_secs
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn budget_defaults_match_the_documented_contract() {
        let limits = default_limits();
        assert_eq!(limits.max_turns, 20);
        assert_eq!(limits.blocked_after_consecutive_rounds, 3);
        assert!(limits.deadline_unix_secs >= goal_now() + 3600 - 5);
    }

    /// The model-visible goal state is the only goal state the model ever reads,
    /// so every rendering must be readable text — and the post-compaction
    /// checkpoint must not carry per-round facts, or it would invalidate the
    /// provider prefix cache on every round.
    #[test]
    fn contract_checkpoint_is_readable_and_free_of_round_facts() {
        let (dir, store) = store();
        let mut spec = GoalSpec::new("ship the parser");
        spec.set_acceptance_criteria(vec!["parses fixtures".into()]);
        spec.constraints = vec!["no network".into()];
        spec.acceptance = GoalAcceptance::Verifier { id: "v1".into() };
        let created = store.create(spec, "env".into()).unwrap();
        let text = state_text(&created, GoalStateKind::Contract, 4096);

        assert!(text.starts_with("<current-goal>"), "{text}");
        assert!(text.ends_with("</current-goal>"), "{text}");
        assert!(text.contains("Objective: ship the parser"), "{text}");
        assert!(text.contains("parses fixtures"), "{text}");
        assert!(text.contains("no network"), "{text}");
        assert!(text.contains("verifier"), "{text}");
        assert!(text.contains("Deadline (unix):"), "{text}");
        assert!(
            text.contains("Stop for the same blocker only after 3 consecutive reviewed rounds"),
            "{text}"
        );
        for forbidden in [
            "Round:",
            "Remaining turns",
            "GoalReport iteration",
            "Phase:",
            "Decision:",
            "Completed rounds",
        ] {
            assert!(
                !text.contains(forbidden),
                "the contract checkpoint must stay byte-stable across rounds, found {forbidden:?}:\n{text}"
            );
        }
        // Only per-round state moves; the checkpoint bytes must not.
        let mut progressed = created.clone();
        progressed.iterations.push(GoalIteration {
            iteration: 1,
            turn_id: "turn-1".into(),
            billing_turn_id: None,
            finished: true,
            conversation_start: 0,
            outcome_status: None,
            outcome_error: None,
        });
        progressed.phase = GoalPhase::Executing;
        progressed.reason = Some("round one finished".into());
        progressed.blocked_rounds = 2;
        progressed.revision += 1;
        assert_eq!(
            state_text(&progressed, GoalStateKind::Contract, 4096),
            text,
            "the contract checkpoint changed with per-round state"
        );
        // No Rust syntax may leak into the model's context.
        for leak in [
            "Some(",
            "None",
            "Ok(",
            "GoalSpec",
            "GoalLimits",
            "{:?",
            "[\"",
        ] {
            assert!(!text.contains(leak), "projection leaked {leak:?}:\n{text}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The per-round facts travel with the appended round prompt: exactly one
    /// number may be read as the report iteration, and it is the admitted one.
    #[test]
    fn round_prompt_states_one_round_number_and_the_host_limits() {
        let (dir, store) = store();
        let mut spec = GoalSpec::new("ship the parser");
        spec.set_acceptance_criteria(vec!["parses fixtures".into()]);
        spec.constraints = vec!["no network".into()];
        let created = store.create(spec, "env".into()).unwrap();
        let mut admitted = created.clone();
        admitted.iterations.push(GoalIteration {
            iteration: 1,
            turn_id: "turn-1".into(),
            billing_turn_id: None,
            finished: false,
            conversation_start: 0,
            outcome_status: None,
            outcome_error: None,
        });
        admitted.phase = GoalPhase::Executing;
        admitted.reason = Some("initial iteration".into());

        let text = round_prompt(&admitted, 4096);

        assert!(text.starts_with("<current-goal>"), "{text}");
        assert!(text.contains("Round: 1/20"), "{text}");
        assert!(text.contains("Remaining turns: 19"), "{text}");
        assert!(!text.contains("GoalReport iteration:"), "{text}");
        assert!(text.contains("Phase: executing"), "{text}");
        assert!(text.contains("Decision: initial iteration"), "{text}");
        assert!(text.contains("Deadline (unix):"), "{text}");
        assert!(
            text.contains("Stop for the same blocker only after 3 consecutive reviewed rounds"),
            "{text}"
        );
        // The mandate stays outside the facts block and never advertises a
        // neighbouring round.
        assert!(text.contains("</current-goal>"), "{text}");
        assert!(text.contains("end this turn naturally"), "{text}");
        assert!(!text.contains("Round: 0"), "{text}");
        assert!(!text.contains("iteration: 0"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Terminal facts replace the removed projection as the model-visible end
    /// signal.
    #[test]
    fn ended_facts_report_the_terminal_status_and_basis() {
        let (dir, store) = store();
        let goal = store
            .create(GoalSpec::new("ship the parser"), "env".into())
            .unwrap();
        let mut finished = goal.clone();
        finished.status = GoalStatus::Completed;
        finished.phase = GoalPhase::Idle;
        finished.completion_basis = Some(GoalCompletionBasis::Verified);
        finished.reason = Some("checker passed".into());

        let text = state_text(&finished, GoalStateKind::Ended, 4096);
        assert!(text.contains("Status: completed"), "{text}");
        assert!(text.contains("(basis: verified)"), "{text}");
        assert!(text.contains("Decision: checker passed"), "{text}");
        assert!(text.contains("The goal has ended"), "{text}");
        assert!(!text.contains("Round:"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn goal_conflicts_stale_revision_and_limits_are_durable() {
        let (dir, store) = store();
        let g = store.create(GoalSpec::new("build"), "env".into()).unwrap();
        assert_eq!(g.remaining_turns(), 20);
        assert!(
            store
                .create(GoalSpec::new("replace"), "env".into())
                .is_err()
        );
        assert!(store.change(&g.id, 0, |_| Ok(())).is_err());
        store
            .change(&g.id, g.revision, |g| {
                g.status = GoalStatus::Paused;
                Ok(())
            })
            .unwrap();
        let restored = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
        assert_eq!(restored.snapshot().unwrap().status, GoalStatus::Paused);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn unknown_side_effect_blocks_recovery() {
        let (dir, store) = store();
        let g = store.create(GoalSpec::new("build"), "env".into()).unwrap();
        store
            .change(&g.id, g.revision, |g| {
                g.phase = GoalPhase::Executing;
                Ok(())
            })
            .unwrap();
        store.mark_effect("call").unwrap();
        store.recover("env", &[]).unwrap();
        assert_eq!(store.snapshot().unwrap().status, GoalStatus::Blocked);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn journal_recovers_snapshot_failure_and_fault_latches() {
        let (dir, store) = store();
        let goal = store.create(GoalSpec::new("build"), "env".into()).unwrap();
        std::fs::remove_file(dir.join("goal.json")).unwrap();
        std::fs::create_dir(dir.join("goal.json")).unwrap();
        assert!(
            store
                .change(&goal.id, goal.revision, |g| {
                    g.status = GoalStatus::Paused;
                    Ok(())
                })
                .is_err()
        );
        assert!(store.fault.check().is_err());
        assert_eq!(store.snapshot().unwrap().status, GoalStatus::Active);
        std::fs::remove_dir(dir.join("goal.json")).unwrap();
        let restored = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
        assert_eq!(restored.snapshot().unwrap().status, GoalStatus::Paused);
        assert_eq!(restored.snapshot().unwrap().revision, goal.revision + 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn formal_result_resolves_effect_but_synthetic_repair_does_not() {
        for (kind, paused) in (0..3).flat_map(|kind| [false, true].map(|paused| (kind, paused))) {
            let formal = kind == 1;
            let (dir, store) = store();
            let goal = store.create(GoalSpec::new("build"), "env".into()).unwrap();
            store
                .change(&goal.id, goal.revision, |g| {
                    g.phase = GoalPhase::Executing;
                    Ok(())
                })
                .unwrap();
            store.mark_effect("write").unwrap();
            if paused {
                store
                    .update(|g| {
                        g.status = GoalStatus::Paused;
                        Ok(())
                    })
                    .unwrap();
            }
            let mut block = serde_json::json!({"type":"tool_result","tool_use_id":"write","content":"result unavailable"});
            if formal {
                block["_mink"] = serde_json::json!({"status":{"state":"succeeded"}});
            }
            if kind == 2 {
                block["_mink"] = serde_json::json!({"status":{"state":"unknown"}});
            }
            store
                .recover(
                    "env",
                    &[serde_json::json!({"role":"user","content":[block]})],
                )
                .unwrap();
            let recovered = store.snapshot().unwrap();
            assert_eq!(
                recovered.status,
                if formal && paused {
                    GoalStatus::Paused
                } else if formal {
                    GoalStatus::Active
                } else {
                    GoalStatus::Blocked
                }
            );
            assert_eq!(recovered.pending_effects.is_empty(), formal);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
    #[tokio::test]
    async fn checkpoint_is_bounded_and_terminal_facts_are_published_once() {
        let (dir, store) = store();
        let mut spec = GoalSpec::new("x".repeat(4000));
        spec.set_acceptance_criteria(vec!["proof".into()]);
        let goal = store.create(spec, "env".into()).unwrap();
        let checkpoint = store.checkpoint(256).unwrap().unwrap();
        assert_eq!(checkpoint["_mink"]["goal_state_kind"], "contract");
        assert!(checkpoint["content"].as_str().unwrap().chars().count() < 400);
        assert_eq!(store.snapshot().unwrap().spec.objective.len(), 4000);

        let history = crate::session::store::ConversationStore::new(dir.join("conversation.jsonl"));
        history.ensure().await.unwrap();
        assert!(
            !store.append_ended_row(&history, 4096).await.unwrap(),
            "a live goal has no terminal row"
        );
        store
            .change(&goal.id, goal.revision, |g| {
                g.status = GoalStatus::Completed;
                g.completion_basis = Some(GoalCompletionBasis::ModelReported);
                g.reason = Some("model-reported completion".into());
                Ok(())
            })
            .unwrap();
        assert!(store.append_ended_row(&history, 4096).await.unwrap());
        assert!(
            !store.append_ended_row(&history, 4096).await.unwrap(),
            "the terminal row is published exactly once"
        );
        let lines = history.lines().await.unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["_mink"]["goal_state_kind"], "ended");
        assert_eq!(lines[0]["_mink"]["internal"], true);
        assert!(
            store.checkpoint(4096).unwrap().is_none(),
            "a terminal goal leaves the request prefix alone"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn crash_usage_reconciliation_counts_all_kinds_once_and_unknown_attempts() {
        use crate::session::usage::{TokenUsage, UsageKind, UsageRecord, UsageStatus};
        let (dir, store) = store();
        let goal = store.create(GoalSpec::new("work"), "env".into()).unwrap();
        store
            .change(&goal.id, goal.revision, |g| {
                g.iterations.push(GoalIteration {
                    iteration: 1,
                    turn_id: "goal-turn".into(),
                    billing_turn_id: Some("billing".into()),
                    finished: false,
                    conversation_start: 0,
                    outcome_status: None,
                    outcome_error: None,
                });
                Ok(())
            })
            .unwrap();
        let records: Vec<_> = [UsageKind::Agent, UsageKind::Compaction, UsageKind::SubAgent]
            .into_iter()
            .enumerate()
            .map(|(n, kind)| UsageRecord {
                version: 1,
                billing_turn_id: "billing".into(),
                request_id: format!("request-{n}"),
                kind,
                origin_session_id: "session".into(),
                model: "mock".into(),
                attempt_count: 2,
                status: if n == 2 {
                    UsageStatus::Unreported
                } else {
                    UsageStatus::Reported
                },
                tokens: if n == 2 {
                    None
                } else {
                    Some(TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    })
                },
                cost_nano_cny: None,
                reason: None,
                completed_at: "mock".into(),
            })
            .collect();
        store.reconcile_usage(&records).unwrap();
        let revision = store.snapshot().unwrap().revision;
        store.reconcile_usage(&records).unwrap();
        let goal = store.snapshot().unwrap();
        assert_eq!(goal.revision, revision);
        assert_eq!(goal.usage.request_count, 3);
        assert_eq!(goal.usage.attempt_count, 6);
        assert_eq!(goal.usage.unreported_request_count, 1);
        assert_eq!(goal.usage.tokens.input_tokens, 20);
        let restored = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
        assert_eq!(restored.snapshot().unwrap().usage, goal.usage);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn settled_failure_is_not_restarted_and_report_is_idempotent() {
        let (dir, store) = store();
        let goal = store.create(GoalSpec::new("work"), "env".into()).unwrap();
        store
            .change(&goal.id, goal.revision, |g| {
                g.phase = GoalPhase::Executing;
                Ok(())
            })
            .unwrap();
        let report = GoalReport {
            goal_id: goal.id.clone(),
            iteration: 0,
            kind: GoalReportKind::Continue,
            summary: "progress".into(),
            next_step: "next".into(),
            criteria: vec![],
            evidence: vec![],
        };
        store
            .update(|g| {
                g.report = Some(report.clone());
                Ok(())
            })
            .unwrap();
        let revision = store.snapshot().unwrap().revision;
        assert_eq!(store.snapshot().unwrap().report, Some(report));
        assert_eq!(store.snapshot().unwrap().revision, revision);
        store
            .update(|g| {
                g.iterations.push(GoalIteration {
                    iteration: 1,
                    turn_id: "turn".into(),
                    billing_turn_id: None,
                    finished: true,
                    conversation_start: 0,
                    outcome_status: Some(crate::runtime::TurnStatus::Failed),
                    outcome_error: Some("format_recovery_exhausted".into()),
                });
                Ok(())
            })
            .unwrap();
        store.recover("env", &[]).unwrap();
        let recovered = store.snapshot().unwrap();
        assert_eq!(recovered.status, GoalStatus::Failed);
        assert_eq!(
            recovered.reason.as_deref(),
            Some("format_recovery_exhausted")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A round belongs to the goal that admitted it. Late work that names the
    /// replaced goal must be refused (revision/identity) or a no-op (stale side
    /// effect), never written onto whatever is current now.
    #[test]
    fn late_work_for_a_replaced_goal_never_touches_the_replacement() {
        let (dir, store) = store();
        let first = store.create(GoalSpec::new("first"), "env".into()).unwrap();
        let running = store.begin_iteration("turn-a", 0).unwrap();
        assert_eq!(running.phase, GoalPhase::Executing);
        store.mark_effect("call-a").unwrap();
        assert_eq!(
            store.snapshot().unwrap().pending_effects,
            vec!["call-a".to_string()]
        );

        // The first goal is cancelled and replaced before its round settled.
        let executing = store.snapshot().unwrap();
        let cancelled = store
            .change(&first.id, executing.revision, |g| {
                g.status = GoalStatus::Cancelled;
                Ok(())
            })
            .unwrap();
        assert_eq!(cancelled.status, GoalStatus::Cancelled);
        let replacement = store.create(GoalSpec::new("second"), "env".into()).unwrap();

        // Every identity-bound entry point refuses the replaced goal instead of
        // applying its work to the replacement.
        assert!(
            store
                .update_if_current(&first.id, |_| Ok(()))
                .unwrap()
                .is_none()
        );
        assert_eq!(store.snapshot().unwrap().revision, replacement.revision);
        assert!(
            store
                .change(&replacement.id, cancelled.revision, |_| Ok(()))
                .is_err(),
            "a stale revision of the replacement must be refused by CAS"
        );
        assert_eq!(store.snapshot().unwrap().revision, replacement.revision);

        // A stale side-effect marker for the dead run is a no-op: the
        // replacement must not be blocked on effects it never dispatched, and
        // closing the old ids changes nothing either.
        store.mark_effect("late-call").unwrap();
        store.close_effects(&["call-a".into()]).unwrap();
        assert!(store.snapshot().unwrap().pending_effects.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

// ── Model-visible goal state ─────────────────────────────────────
//
// Goal facts live in two places and never anywhere else: the contract
// checkpoint mounted after a compaction, and the facts appended at the tail
// (the round prompt and the terminal row). Mounting per-round state in front of
// the history is what used to invalidate the provider prefix cache on every
// round, so `Contract` must stay byte-stable between rounds.

/// Which goal facts one model-visible state row carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoalStateKind {
    /// Stable contract checkpoint (post-compaction projection).
    Contract,
    /// Per-round facts carried by the appended round prompt.
    Round,
    /// Terminal facts appended once when the goal ends.
    Ended,
}

/// Render the model-visible goal state for one row kind.
///
/// `Contract` deliberately omits every fact that changes per round (round
/// position, remaining turns, phase, decision, blocked streak): it is mounted
/// inside the request prefix after a compaction, so byte stability is what
/// keeps prefix-cache reuse alive between rounds. The per-round facts travel
/// with the appended round prompt instead ([`round_prompt`]).
pub(crate) fn state_text(goal: &GoalSnapshot, kind: GoalStateKind, max_chars: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Goal ID: {}", goal.id));
    if kind == GoalStateKind::Round {
        lines.push(format!(
            "Round: {}/{}",
            goal.iterations.len(),
            goal.spec.limits.max_turns
        ));
        lines.push(format!("Remaining turns: {}", goal.remaining_turns()));
    }
    lines.push(format!("Status: {}", status_label(&goal.status)));
    if kind == GoalStateKind::Round {
        lines.push(format!("Phase: {}", phase_label(&goal.phase)));
    }
    lines.push(format!("Objective: {}", goal.spec.objective));
    if kind == GoalStateKind::Contract {
        lines.push(format!("Original request: {}", goal.spec.original_request));
        lines.push(format!(
            "Subgoals and criteria: {}",
            serde_json::to_string(&(&goal.spec.subgoals, &goal.spec.criteria)).unwrap_or_default()
        ));
    }
    lines.push("Acceptance criteria:".to_string());
    if goal.spec.acceptance_criteria.is_empty() {
        lines.push("  (none declared)".to_string());
    } else {
        for criterion in &goal.spec.acceptance_criteria {
            lines.push(format!("  · {criterion}"));
        }
    }
    if !goal.spec.scope.trim().is_empty() {
        lines.push(format!("Scope: {}", goal.spec.scope));
    }
    lines.push("Constraints:".to_string());
    if goal.spec.constraints.is_empty() {
        lines.push("  (none declared)".to_string());
    } else {
        for constraint in &goal.spec.constraints {
            lines.push(format!("  · {constraint}"));
        }
    }
    lines.push(format!(
        "Acceptance: {}{}",
        acceptance_label(&goal.spec.acceptance),
        match goal.completion_basis.as_ref() {
            Some(basis) => format!(" (basis: {})", basis_label(basis)),
            None => String::new(),
        }
    ));
    lines.push(format!(
        "Deadline (unix): {}",
        goal.spec.limits.deadline_unix_secs
    ));
    lines.push(format!(
        "Stop for the same blocker only after {} consecutive reviewed rounds",
        goal.spec.limits.blocked_after_consecutive_rounds
    ));
    if kind == GoalStateKind::Round {
        if goal.blocked_rounds > 0 {
            lines.push(format!(
                "Consecutive blocked rounds so far: {}",
                goal.blocked_rounds
            ));
        }
        lines.push(format!(
            "Decision: {}",
            goal.reason.as_deref().unwrap_or("none")
        ));
        lines.push(
            "GoalRead retrieves the full current contract. End naturally when this round is done; the host independently reviews completion."
                .to_string(),
        );
    }
    if kind == GoalStateKind::Ended {
        lines.push(format!(
            "Decision: {}",
            goal.reason.as_deref().unwrap_or("none")
        ));
        lines.push(
            "The goal has ended; the host owns its contract and its result. Continue with the user's next instruction."
                .to_string(),
        );
    }
    let text = lines.join("\n");
    let excerpt: String = text.chars().take(max_chars.max(128)).collect();
    format!(
        "<current-goal>\n{excerpt}{}\n</current-goal>",
        if excerpt.len() < text.len() {
            "\n[goal state omitted; use GoalRead for the full contract]"
        } else {
            ""
        }
    )
}

/// Mandate carried beside the round facts of a scheduled round.
const ROUND_INSTRUCTIONS: &str = "Continue working toward this explicit goal in the same session. Treat the current workspace, tool results and durable session state as authoritative; inspect them instead of assuming earlier narration is current. The goal contract and limits are host-owned; do not edit runtime state files. Read GoalRead when the contract is omitted or needs checking. Gather concrete evidence, then end this turn naturally. An independent read-only reviewer decides whether the confirmed objective is satisfied.";

/// Model-visible prompt that opens one admitted round.
///
/// The orchestrator appends it as the round's user message, so per-round goal
/// facts never enter the request prefix and the provider prefix cache survives
/// the next round.
pub(crate) fn round_prompt(goal: &GoalSnapshot, max_chars: usize) -> String {
    format!(
        "{}\n{}",
        if goal.iterations.len() == goal.confirmed_after_iteration + 1 {
            format!(
                "{}\n{}",
                state_text(goal, GoalStateKind::Contract, max_chars),
                state_text(goal, GoalStateKind::Round, max_chars)
            )
        } else {
            state_text(goal, GoalStateKind::Round, max_chars)
        },
        ROUND_INSTRUCTIONS
    )
}

// ── Goal lifecycle messages ──────────────────────────────────────
//
// The model-visible text of a goal turn is the appended round prompt; a UI
// cannot tell it apart from ordinary prose without parsing English. These
// entries give both goal boundaries a typed, structured form that travels in
// `_mink`, so hosts render a goal card instead of guessing.

/// The `_mink.source` tag that accompanies a goal turn message.
///
/// It stays `goal_continuation` for every round, including the first: existing
/// consumers already key off it, and start-versus-continuation is carried by
/// [`GoalMessage::kind`] rather than by renaming the tag.
pub const GOAL_MESSAGE_SOURCE: &str = "goal_continuation";

/// Which boundary of a goal's life a message marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalMessageKind {
    /// A read-only discussion of a not-yet-confirmed contract.
    Draft,
    /// The goal was accepted and its first round is scheduled.
    Start,
    /// A further round of an existing goal was admitted.
    Continuation,
}

/// Structured payload for a goal lifecycle message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalMessage {
    pub kind: GoalMessageKind,
    pub goal_id: String,
    pub contract_revision: u64,
    /// 1-based round this message admits.
    pub iteration: u32,
    pub objective: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub prior_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
}

impl GoalSnapshot {
    /// The boundary message that opens a round of this goal.
    ///
    /// The first round is the goal starting; later rounds continue it, which is
    /// the only distinction a host needs to pick a card.
    pub fn boundary_kind(iteration: u32) -> GoalMessageKind {
        if iteration <= 1 {
            GoalMessageKind::Start
        } else {
            GoalMessageKind::Continuation
        }
    }

    /// Build the structured payload for a lifecycle message about this goal.
    pub fn message(&self, kind: GoalMessageKind, iteration: u32) -> GoalMessage {
        GoalMessage {
            kind,
            goal_id: self.id.clone(),
            contract_revision: self.contract_revision,
            iteration,
            objective: self.spec.objective.clone(),
            acceptance_criteria: self.spec.acceptance_criteria.clone(),
            constraints: self.spec.constraints.clone(),
            prior_decision: self.reason.clone(),
            feedback: None,
        }
    }
}
