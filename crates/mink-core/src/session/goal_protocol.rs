//! Versioned goal contracts and independent, evidence-bound review results.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSubgoal {
    pub id: String,
    pub objective: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCriterion {
    pub id: String,
    pub subgoal_id: String,
    pub outcome: String,
    pub required_evidence: String,
}

/// Model-owned proposal fields. Limits and acceptance policy remain host-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalDraftSpec {
    pub objective: String,
    pub scope: String,
    pub constraints: Vec<String>,
    pub subgoals: Vec<GoalSubgoal>,
    pub criteria: Vec<GoalCriterion>,
    #[serde(default)]
    pub suggested_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalCommand {
    CreateDraft {
        spec: Box<super::goal::GoalSpec>,
        environment: String,
        duration_secs: u64,
        ready: bool,
    },
    Draft {
        id: String,
        revision: u64,
        spec: GoalDraftSpec,
    },
    Confirm {
        id: String,
        revision: u64,
        environment: String,
    },
    CancelDraft {
        id: String,
        revision: u64,
    },
}

impl GoalCommand {
    pub(crate) fn identity(&self) -> Option<(&str, u64)> {
        match self {
            Self::CreateDraft { .. } => None,
            Self::Draft { id, revision, .. }
            | Self::Confirm { id, revision, .. }
            | Self::CancelDraft { id, revision } => Some((id, *revision)),
        }
    }
    pub(crate) fn apply_to(self, goal: &mut super::goal::GoalSnapshot) -> Result<()> {
        use super::goal::{GoalPhase, GoalStatus};
        let (id, revision) = self
            .identity()
            .ok_or_else(|| anyhow::anyhow!("creation requires GoalStore"))?;
        ensure!(
            goal.id == id && goal.revision == revision,
            "stale draft command"
        );
        ensure!(goal.status == GoalStatus::Draft, "goal is not a draft");
        match self {
            Self::CreateDraft { .. } => unreachable!(),
            Self::Draft { spec, .. } => {
                goal.spec.objective = spec.objective;
                goal.spec.scope = spec.scope;
                goal.spec.constraints = spec.constraints;
                goal.spec.subgoals = spec.subgoals;
                goal.spec.criteria = spec.criteria;
                goal.spec.acceptance_criteria = goal
                    .spec
                    .criteria
                    .iter()
                    .map(|c| c.outcome.clone())
                    .collect();
                goal.spec.suggested_actions = spec.suggested_actions;
                goal.spec.validate()?;
                goal.draft_ready = true;
                goal.contract_revision = goal
                    .contract_revision
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("goal contract revision overflow"))?;
            }
            Self::Confirm { environment, .. } => {
                ensure!(
                    goal.draft_ready,
                    "goal proposal is not ready; refine the draft before confirmation"
                );
                ensure!(
                    goal.pending_effects.is_empty(),
                    "unknown effects must be reconciled before confirmation"
                );
                goal.spec.validate()?;
                if let Some(duration) = goal.draft_duration_secs.take() {
                    goal.spec.limits.deadline_unix_secs =
                        super::goal::goal_now().saturating_add(duration);
                }
                ensure!(
                    goal.budget_reason().is_none(),
                    "adjust exhausted goal limits before confirmation"
                );
                goal.needs_confirmation = false;
                goal.confirmed_after_iteration = goal.iterations.len();
                goal.status = GoalStatus::Active;
                goal.phase = GoalPhase::Idle;
                goal.continuation_rounds = 0;
                goal.continuation_fingerprint = None;
                goal.review_report = None;
                goal.review_requires_full = false;
                goal.review_stage = None;
                goal.report = None;
                goal.review_candidate = None;
                goal.checker_passed_candidate = None;
                goal.checker_evidence_versions.clear();
                goal.evidence_versions.clear();
                goal.environment = environment;
                goal.reason = Some("contract confirmed by host".into());
            }
            Self::CancelDraft { .. } => goal.status = GoalStatus::Cancelled,
        }
        Ok(())
    }
}

/// A draft uses the same durable identity and revision as its confirmed goal.
pub type GoalDraftSnapshot = super::goal::GoalSnapshot;

/// Host-admitted draft discussion, distinct from a human inbox request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalDraftDiscussion {
    pub goal_id: String,
    pub revision: u64,
    pub feedback: String,
}

pub(crate) fn validate_contract(spec: &super::goal::GoalSpec) -> Result<()> {
    ensure!(
        !spec.original_request.trim().is_empty(),
        "original request is empty"
    );
    ensure!(!spec.subgoals.is_empty(), "goal requires subgoals");
    let mut ids = BTreeSet::new();
    for goal in &spec.subgoals {
        ensure!(
            !goal.id.trim().is_empty() && !goal.objective.trim().is_empty(),
            "empty subgoal"
        );
        ensure!(ids.insert(goal.id.as_str()), "duplicate subgoal ID");
    }
    let mut criteria = BTreeSet::new();
    ensure!(!spec.criteria.is_empty(), "goal requires criteria");
    for criterion in &spec.criteria {
        ensure!(
            !criterion.id.trim().is_empty() && criteria.insert(criterion.id.as_str()),
            "duplicate or empty criterion ID"
        );
        ensure!(
            ids.contains(criterion.subgoal_id.as_str()),
            "unknown criterion subgoal"
        );
        ensure!(
            !criterion.outcome.trim().is_empty() && !criterion.required_evidence.trim().is_empty(),
            "criterion requires outcome and evidence"
        );
    }
    for goal in &spec.subgoals {
        ensure!(
            spec.criteria.iter().any(|c| c.subgoal_id == goal.id),
            "subgoal has no acceptance criterion"
        );
    }
    ensure!(
        spec.acceptance_criteria
            == spec
                .criteria
                .iter()
                .map(|c| c.outcome.clone())
                .collect::<Vec<_>>(),
        "criterion projection differs from contract"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalReviewCandidate {
    /// Audit identity within one execution iteration (legacy snapshots use 0).
    #[serde(default)]
    pub generation: u64,
    pub goal_id: String,
    pub contract_revision: u64,
    pub iteration: u32,
    pub conversation_seq: usize,
    pub reviewer_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalReviewStage {
    Screening,
    Inspecting,
    Adjudicating,
}

/// A negative-only scheduling proposal; it grants no acceptance authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalContinueProposal {
    pub candidate: GoalReviewCandidate,
    pub summary: String,
    pub remaining_requirements: Vec<String>,
    pub next_step: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalContinueRecord {
    pub proposal: GoalContinueProposal,
    pub report: String,
    pub fingerprint: String,
    pub consecutive_rounds: u32,
    pub conversation_seq: usize,
}

/// Durable successful descriptive report, never a completion certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalReviewReport {
    pub candidate: GoalReviewCandidate,
    pub artifact: String,
    pub content_hash: String,
    pub reference_mapping: Vec<(String, String)>,
    pub evidence_versions: Vec<(String, String)>,
    pub conversation_seq: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "record", rename_all = "snake_case")]
pub enum GoalRoundDecision {
    Full(GoalReview),
    Continue(GoalContinueRecord),
}

/// Small model-facing decision; candidate, report and evidence are host-bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalHostConclusion {
    pub decision: GoalHostDecision,
    pub summary: String,
    #[serde(default)]
    pub next_step: String,
    #[serde(default)]
    pub remaining_requirements: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalHostDecision {
    Continue,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum GoalReviewSubmission {
    Full(GoalReview),
    Continue(GoalContinueProposal),
    #[serde(untagged)]
    Decision(GoalHostConclusion),
}
impl<'de> Deserialize<'de> for GoalReviewSubmission {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        if value.get("decision").is_some()
            || !["candidate", "mode", "verdict"]
                .iter()
                .any(|key| value.get(key).is_some())
        {
            return serde_json::from_value(value)
                .map(Self::Decision)
                .map_err(serde::de::Error::custom);
        }
        let mode = value.as_object_mut().and_then(|o| o.remove("mode"));
        match mode.as_ref().and_then(|v| v.as_str()) {
            None if mode.is_none() => serde_json::from_value(value)
                .map(Self::Full)
                .map_err(serde::de::Error::custom),
            Some("full") => serde_json::from_value(value)
                .map(Self::Full)
                .map_err(serde::de::Error::custom),
            Some("continue") => serde_json::from_value(value)
                .map(Self::Continue)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom("mode must be full or continue")),
        }
    }
}

impl GoalContinueProposal {
    pub fn validate(&self, goal: &super::goal::GoalSnapshot) -> Result<()> {
        ensure!(
            goal.review_candidate.as_ref() == Some(&self.candidate),
            "stale review candidate"
        );
        ensure!(
            !goal.review_requires_full,
            "this candidate requires full evidence review"
        );
        ensure!(
            !self.summary.trim().is_empty() && !self.next_step.trim().is_empty(),
            "continuation requires a summary and concrete next step"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 64 * 1024,
            "continuation too large"
        );
        let valid: BTreeSet<_> = goal.requirement_ids().into_iter().collect();
        let actual: BTreeSet<_> = self.remaining_requirements.iter().cloned().collect();
        ensure!(
            !actual.is_empty() && actual.len() == self.remaining_requirements.len(),
            "remaining requirements must be nonempty and unique"
        );
        ensure!(
            actual.is_subset(&valid),
            "unknown remaining requirement; read GoalRead requirement_ids"
        );
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut ids = self.remaining_requirements.clone();
        ids.sort();
        let bytes = serde_json::to_vec(&(self.candidate.contract_revision, ids))?;
        Ok(Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalReviewVerdict {
    Satisfied,
    NeedsWork,
    Blocked,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCriterionStatus {
    Pass,
    Fail,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCriterionReview {
    pub criterion_id: String,
    pub status: GoalCriterionStatus,
    pub reason: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalConstraintReview {
    pub constraint: String,
    pub status: GoalCriterionStatus,
    pub reason: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalBlocker {
    pub category: String,
    pub criterion_ids: Vec<String>,
    pub dependency: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalReview {
    pub candidate: GoalReviewCandidate,
    pub verdict: GoalReviewVerdict,
    /// Descriptive conclusions retain the report instead of manufacturing item findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<Box<GoalReviewReport>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request: Option<GoalRequestReview>,
    pub criteria: Vec<GoalCriterionReview>,
    pub constraints: Vec<GoalConstraintReview>,
    pub summary: String,
    pub next_step: String,
    pub blocker: Option<GoalBlocker>,
}

/// Per-requirement observations used to validate a host-submitted conclusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalReviewFeedback {
    pub candidate: GoalReviewCandidate,
    pub original_request: GoalRequestReview,
    pub criteria: Vec<GoalCriterionReview>,
    pub constraints: Vec<GoalConstraintReview>,
    pub summary: String,
    pub next_step: String,
    pub blocker: Option<GoalBlocker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalRequestReview {
    pub status: GoalCriterionStatus,
    pub reason: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCheckerRecord {
    pub candidate: GoalReviewCandidate,
    pub checker_id: String,
    pub status: GoalCriterionStatus,
    pub reason: String,
    pub evidence: Vec<String>,
}

impl GoalReview {
    pub(crate) fn feedback(&self) -> GoalReviewFeedback {
        GoalReviewFeedback {
            candidate: self.candidate.clone(),
            original_request: self.original_request.clone().expect("legacy item review"),
            criteria: self.criteria.clone(),
            constraints: self.constraints.clone(),
            summary: self.summary.clone(),
            next_step: self.next_step.clone(),
            blocker: self.blocker.clone(),
        }
    }

    pub fn validate(&self, goal: &super::goal::GoalSnapshot) -> Result<()> {
        ensure!(
            goal.review_candidate.as_ref() == Some(&self.candidate),
            "stale review candidate"
        );
        ensure!(!self.summary.trim().is_empty(), "review summary is empty");
        if let Some(report) = &self.report {
            ensure!(
                report.candidate == self.candidate
                    && !report.artifact.is_empty()
                    && !report.content_hash.is_empty(),
                "invalid bound review report"
            );
            ensure!(
                self.original_request.is_none()
                    && self.criteria.is_empty()
                    && self.constraints.is_empty(),
                "descriptive conclusions cannot manufacture item findings"
            );
            ensure!(
                serde_json::to_vec(self)?.len() <= 64 * 1024,
                "review too large"
            );
            if self.verdict == GoalReviewVerdict::Satisfied {
                ensure!(
                    !report.evidence_versions.is_empty(),
                    "completion requires independently inspected evidence"
                );
                ensure!(self.blocker.is_none(), "complete cannot contain a blocker");
            } else {
                ensure!(
                    !self.next_step.trim().is_empty(),
                    "continue or blocked needs a concrete next_step"
                );
            }
            if self.verdict == GoalReviewVerdict::Blocked {
                let blocker = self.blocker.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("blocked needs an objective external dependency")
                })?;
                ensure!(
                    !blocker.dependency.trim().is_empty() && !blocker.evidence.is_empty(),
                    "blocked needs an inspected external dependency"
                );
            } else {
                ensure!(self.blocker.is_none(), "only blocked may contain a blocker");
            }
            return Ok(());
        }
        let original_request = self
            .original_request
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("original request must be reviewed explicitly"))?;
        ensure!(
            !original_request.reason.trim().is_empty(),
            "original request must be reviewed explicitly"
        );
        ensure!(
            original_request.status != GoalCriterionStatus::Pass
                || !original_request.evidence.is_empty(),
            "passing original request requires evidence"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 64 * 1024,
            "review too large"
        );
        let expected: BTreeSet<_> = goal.spec.criteria.iter().map(|c| c.id.as_str()).collect();
        let actual: BTreeSet<_> = self
            .criteria
            .iter()
            .map(|c| c.criterion_id.as_str())
            .collect();
        ensure!(
            expected == actual && actual.len() == self.criteria.len(),
            "review must cover each criterion exactly once"
        );
        ensure!(
            self.constraints.len() == goal.spec.constraints.len(),
            "review must cover all constraints"
        );
        for (check, constraint) in self.constraints.iter().zip(&goal.spec.constraints) {
            ensure!(
                &check.constraint == constraint && !check.reason.trim().is_empty(),
                "invalid constraint review"
            );
            ensure!(
                check.status != GoalCriterionStatus::Pass || !check.evidence.is_empty(),
                "passing constraint requires evidence"
            );
        }
        for check in &self.criteria {
            ensure!(!check.reason.trim().is_empty(), "criterion reason is empty");
            ensure!(
                check.status != GoalCriterionStatus::Pass || !check.evidence.is_empty(),
                "passing criterion requires evidence"
            );
        }
        if self.verdict == GoalReviewVerdict::Satisfied {
            ensure!(
                original_request.status == GoalCriterionStatus::Pass
                    && !original_request.evidence.is_empty(),
                "original user requirements are not supported by evidence"
            );
            ensure!(
                self.criteria
                    .iter()
                    .all(|c| c.status == GoalCriterionStatus::Pass)
                    && self
                        .constraints
                        .iter()
                        .all(|c| c.status == GoalCriterionStatus::Pass),
                "satisfied review contains unmet conditions"
            );
            ensure!(self.blocker.is_none(), "satisfied review has a blocker");
        } else {
            ensure!(
                !self.next_step.trim().is_empty(),
                "non-complete review needs a next step"
            );
        }
        if self.verdict == GoalReviewVerdict::Blocked {
            let blocker = self.blocker.as_ref().ok_or_else(|| {
                anyhow::anyhow!("blocked review requires an objective dependency")
            })?;
            ensure!(
                !blocker.category.trim().is_empty() && !blocker.dependency.trim().is_empty(),
                "empty blocker identity"
            );
            ensure!(
                !blocker.evidence.is_empty(),
                "an external blocker requires inspected factual evidence; otherwise report needs_work"
            );
            ensure!(
                !blocker.criterion_ids.is_empty()
                    && blocker
                        .criterion_ids
                        .iter()
                        .all(|id| expected.contains(id.as_str())),
                "invalid blocker criteria"
            );
        }
        Ok(())
    }
    pub fn evidence(&self) -> Vec<String> {
        let mut refs: Vec<_> = self
            .criteria
            .iter()
            .flat_map(|c| c.evidence.clone())
            .chain(self.constraints.iter().flat_map(|c| c.evidence.clone()))
            .collect();
        if let Some(blocker) = &self.blocker {
            refs.extend(blocker.evidence.clone());
        }
        if let Some(original) = &self.original_request {
            refs.extend(original.evidence.clone());
        }
        if let Some(report) = &self.report {
            refs.extend(report.evidence_versions.iter().map(|(r, _)| r.clone()));
        }
        refs.sort();
        refs.dedup();
        refs
    }
}

impl GoalReviewFeedback {
    pub(crate) fn validate(&self, goal: &super::goal::GoalSnapshot) -> Result<()> {
        ensure!(
            goal.review_candidate.as_ref() == Some(&self.candidate),
            "stale review candidate"
        );
        ensure!(!self.summary.trim().is_empty(), "review summary is empty");
        ensure!(
            !self.original_request.reason.trim().is_empty(),
            "original request must be reviewed explicitly"
        );
        ensure!(
            self.original_request.status != GoalCriterionStatus::Pass
                || !self.original_request.evidence.is_empty(),
            "passing original request requires evidence"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 64 * 1024,
            "review feedback too large"
        );
        let expected: BTreeSet<_> = goal.spec.criteria.iter().map(|c| c.id.as_str()).collect();
        let actual: BTreeSet<_> = self
            .criteria
            .iter()
            .map(|c| c.criterion_id.as_str())
            .collect();
        ensure!(
            expected == actual && actual.len() == self.criteria.len(),
            "review must cover each criterion exactly once"
        );
        ensure!(
            self.constraints.len() == goal.spec.constraints.len(),
            "review must cover all constraints"
        );
        for (check, constraint) in self.constraints.iter().zip(&goal.spec.constraints) {
            ensure!(
                &check.constraint == constraint && !check.reason.trim().is_empty(),
                "invalid constraint review"
            );
            ensure!(
                check.status != GoalCriterionStatus::Pass || !check.evidence.is_empty(),
                "passing constraint requires evidence"
            );
        }
        for check in &self.criteria {
            ensure!(!check.reason.trim().is_empty(), "criterion reason is empty");
            ensure!(
                check.status != GoalCriterionStatus::Pass || !check.evidence.is_empty(),
                "passing criterion requires evidence"
            );
        }
        let all_requirements_pass = self.original_request.status == GoalCriterionStatus::Pass
            && self
                .criteria
                .iter()
                .all(|criterion| criterion.status == GoalCriterionStatus::Pass)
            && self
                .constraints
                .iter()
                .all(|constraint| constraint.status == GoalCriterionStatus::Pass);
        if !all_requirements_pass || self.blocker.is_some() {
            ensure!(
                !self.next_step.trim().is_empty(),
                "incomplete review feedback needs a next step"
            );
        }
        if let Some(blocker) = &self.blocker {
            ensure!(
                !blocker.category.trim().is_empty() && !blocker.dependency.trim().is_empty(),
                "empty blocker identity"
            );
            ensure!(
                !blocker.evidence.is_empty(),
                "an external blocker requires inspected factual evidence; otherwise report unknown findings"
            );
            ensure!(
                !blocker.criterion_ids.is_empty()
                    && blocker.criterion_ids.iter().all(|id| {
                        expected.contains(id.as_str())
                            && self.criteria.iter().any(|criterion| {
                                criterion.criterion_id == *id
                                    && criterion.status != GoalCriterionStatus::Pass
                            })
                    }),
                "blocker criteria must be unmet goal criteria"
            );
        }
        Ok(())
    }

    /// Derive the consistency expectation from submitted requirement statuses.
    /// This validates, rather than submits or commits, the host agent's verdict.
    pub(crate) fn into_review(self, goal: &super::goal::GoalSnapshot) -> Result<GoalReview> {
        self.validate(goal)?;
        let all_requirements_pass = self.original_request.status == GoalCriterionStatus::Pass
            && self
                .criteria
                .iter()
                .all(|criterion| criterion.status == GoalCriterionStatus::Pass)
            && self
                .constraints
                .iter()
                .all(|constraint| constraint.status == GoalCriterionStatus::Pass);

        let verdict = if self.blocker.is_some() {
            GoalReviewVerdict::Blocked
        } else if all_requirements_pass {
            GoalReviewVerdict::Satisfied
        } else {
            GoalReviewVerdict::NeedsWork
        };
        let review = GoalReview {
            candidate: self.candidate,
            verdict,
            report: None,
            original_request: Some(self.original_request),
            criteria: self.criteria,
            constraints: self.constraints,
            summary: self.summary,
            next_step: self.next_step,
            blocker: self.blocker,
        };
        review.validate(goal)?;
        Ok(review)
    }
}
