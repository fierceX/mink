import type { GoalSnapshot } from './types.ts'
import { goalBudget } from './goalBudget.ts'

export function goalCanAdjustBudget(goal: GoalSnapshot): boolean {
  return !['completed', 'cancelled'].includes(goal.status)
}

export function goalResumeBudgetExhausted(goal: GoalSnapshot, nowUnixSecs?: number): boolean {
  const budget = goalBudget(goal, nowUnixSecs)
  if (budget === undefined) return false
  // Core stops admitting requests once the soft token limit is reached, on the
  // execution and the review path alike, so the control must not offer a retry
  // the server is guaranteed to refuse.
  if (budget.soft_token_limit !== null && goalTokensUsed(goal) >= budget.soft_token_limit) return true
  const reviewing = goal.review_failure_reason != null || goal.phase === 'verifying'
  return budget.remaining_secs <= 0 || (!reviewing && budget.remaining_turns <= 0)
}

/** Known token spend, summed the way core's goal budget sums it. */
export function goalTokensUsed(goal: GoalSnapshot): number {
  const tokens = goal.usage?.tokens
  return (tokens?.input_tokens ?? 0)
    + (tokens?.cache_read_tokens ?? 0)
    + (tokens?.cache_creation_tokens ?? 0)
    + (tokens?.output_tokens ?? 0)
}

export function goalIsTerminal(goal: GoalSnapshot): boolean {
  return ['completed', 'cancelled', 'failed'].includes(goal.status)
}

export function goalCanResume(goal: GoalSnapshot): boolean {
  return ['paused', 'blocked', 'budget_exhausted', 'failed'].includes(goal.status)
    || (goal.status === 'active' && goal.phase !== 'executing')
}

export function goalReviewStageText(goal: GoalSnapshot): string {
  switch (goal.review_stage) {
    case 'screening': return '判断是否需要验收'
    case 'inspecting': return '检查完成证据'
    case 'adjudicating': return '宿主提交结论'
    default: return '独立审核中'
  }
}

export function latestGoalContinuation(goal: GoalSnapshot) {
  const record = goal.continuations?.at(-1)
  const candidate = record?.proposal.candidate
  if (candidate?.goal_id !== goal.id || candidate.contract_revision !== goal.contract_revision) return undefined
  const full = goal.reviews?.at(-1)?.candidate
  if (full !== undefined && (full.iteration ?? 0) >= (candidate.iteration ?? 0)) return undefined
  return record
}
