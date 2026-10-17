import { describe, expect, it } from 'vitest'
import type { GoalSnapshot } from './types.ts'
import { goalBudget } from './goalBudget.ts'

describe('goal budget projection', () => {
  it('derives current limits and progress from an un-enriched wire snapshot', () => {
    const goal: GoalSnapshot = {
      id: 'goal-1',
      revision: 8,
      status: 'active',
      phase: 'executing',
      spec: {
        limits: {
          max_turns: 12,
          deadline_unix_secs: 2_000,
          soft_token_limit: 900,
          blocked_after_consecutive_rounds: 4,
        },
      },
      iterations: [{}, {}, {}],
      blocker_rounds: 2,
    }

    expect(goalBudget(goal, 1_250)).toEqual({
      max_turns: 12,
      turns_used: 3,
      remaining_turns: 9,
      deadline_unix_secs: 2_000,
      remaining_secs: 750,
      blocked_after_consecutive_rounds: 4,
      blocked_rounds: 0,
      blocker_rounds: 2,
      soft_token_limit: 900,
    })
  })

  it('prefers current contract fields over a stale attached budget', () => {
    const goal: GoalSnapshot = {
      id: 'goal-1',
      revision: 9,
      status: 'active',
      phase: 'executing',
      spec: {
        limits: {
          max_turns: 6,
          deadline_unix_secs: 1_800,
          soft_token_limit: null,
          blocked_after_consecutive_rounds: 3,
        },
      },
      iterations: [{}, {}],
      blocker_rounds: 1,
      budget: {
        max_turns: 20,
        turns_used: 0,
        remaining_turns: 20,
        deadline_unix_secs: 4_000,
        remaining_secs: 3_000,
        blocked_after_consecutive_rounds: 3,
        blocked_rounds: 0,
        soft_token_limit: 500,
      },
    }

    expect(goalBudget(goal, 1_000)).toEqual({
      max_turns: 6,
      turns_used: 2,
      remaining_turns: 4,
      deadline_unix_secs: 1_800,
      remaining_secs: 800,
      blocked_after_consecutive_rounds: 3,
      blocked_rounds: 0,
      blocker_rounds: 1,
      soft_token_limit: null,
    })
  })
})
