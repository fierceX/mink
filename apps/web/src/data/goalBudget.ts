import type { GoalSnapshot } from './types.ts'

export type GoalBudgetView = NonNullable<GoalSnapshot['budget']>

/**
 * Resolve the goal budget from the wire snapshot's authoritative contract and
 * progress fields. `GoalSnapshot.budget()` is a core method, not a serialized
 * field, so REST and live `goal_updated` snapshots normally do not carry it.
 */
export function goalBudget(
  goal: GoalSnapshot,
  nowUnixSecs = Math.floor(Date.now() / 1000),
): GoalBudgetView | undefined {
  const limits = goal.spec?.limits
  const previous = goal.budget
  const maxTurns = limits?.max_turns ?? previous?.max_turns
  const deadline = limits?.deadline_unix_secs ?? previous?.deadline_unix_secs
  const blockedThreshold = limits?.blocked_after_consecutive_rounds
    ?? previous?.blocked_after_consecutive_rounds

  if (maxTurns === undefined || deadline === undefined || blockedThreshold === undefined) {
    return previous
  }

  const turnsUsed = goal.iterations?.length ?? previous?.turns_used ?? 0
  const blockedRounds = goal.blocker_rounds ?? previous?.blocked_rounds ?? 0
  const softTokenLimit = limits !== undefined
    && Object.prototype.hasOwnProperty.call(limits, 'soft_token_limit')
    ? limits.soft_token_limit ?? null
    : previous?.soft_token_limit ?? null

  return {
    max_turns: maxTurns,
    turns_used: turnsUsed,
    remaining_turns: Math.max(0, maxTurns - turnsUsed),
    deadline_unix_secs: deadline,
    remaining_secs: Math.max(0, deadline - nowUnixSecs),
    blocked_after_consecutive_rounds: blockedThreshold,
    blocked_rounds: blockedRounds,
    soft_token_limit: softTokenLimit,
  }
}
