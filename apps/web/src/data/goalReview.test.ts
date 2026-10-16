import { describe, expect, it } from 'vitest'
import type { GoalSnapshot } from './types.ts'
import { goalCanAdjustBudget, goalCanResume, goalResumeBudgetExhausted } from './goalReview.ts'

const goal: GoalSnapshot = {
  id: 'failed-goal', revision: 7, status: 'failed', phase: 'idle',
  spec: { limits: { max_turns: 2, deadline_unix_secs: 2_000, blocked_after_consecutive_rounds: 3 } },
  reason: 'execution failed', iterations: [{ finished: true, outcome_status: 'failed' }],
}

describe('goal control state matrix', () => {
  it.each([
    ['draft', false, true], ['active', true, true], ['paused', true, true],
    ['blocked', true, true], ['budget_exhausted', true, true],
    ['completed', false, false], ['cancelled', false, false], ['failed', true, true],
  ] as const)('%s has resume=%s and budget=%s', (status, resume, budget) => {
    expect(goalCanResume({ ...goal, status })).toBe(resume)
    expect(goalCanAdjustBudget({ ...goal, status })).toBe(budget)
  })

  it.each(['failed', 'paused', 'blocked', 'active', 'budget_exhausted'])('checks execution and review budgets separately for %s', status => {
    const lastRound = { ...goal, status, iterations: [{}, {}] }
    expect(goalResumeBudgetExhausted({ ...lastRound, phase: 'idle' }, 1_000)).toBe(true)
    expect(goalResumeBudgetExhausted({ ...lastRound, phase: 'verifying' }, 1_000)).toBe(false)
    expect(goalResumeBudgetExhausted({ ...lastRound, phase: 'verifying' }, 2_001)).toBe(true)
  })

  it('treats the token soft limit as an exhausted budget for the retry control', () => {
    const deadline = Math.floor(Date.now() / 1000) + 600
    const spent: GoalSnapshot = {
      ...goal,
      status: 'paused',
      phase: 'idle',
      iterations: [{}],
      spec: { limits: { max_turns: 5, deadline_unix_secs: deadline, blocked_after_consecutive_rounds: 3, soft_token_limit: 1_000 } },
      usage: { tokens: { input_tokens: 600, cache_read_tokens: 200, output_tokens: 200 } },
    }
    expect(goalResumeBudgetExhausted(spent)).toBe(true)
    expect(goalResumeBudgetExhausted({ ...spent, usage: { tokens: { input_tokens: 500 } } })).toBe(false)
    // Without a configured limit the same spend is not a refusal.
    expect(goalResumeBudgetExhausted({
      ...spent,
      spec: { limits: { ...spent.spec!.limits!, soft_token_limit: null } },
    })).toBe(false)
  })
})
