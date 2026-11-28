use super::*;

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
