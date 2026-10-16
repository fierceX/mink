import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { GoalSnapshot } from '../../data/types.ts'
import { GoalBar } from './GoalBar.tsx'

afterEach(cleanup)

const GOAL: GoalSnapshot = {
  id: 'goal-1',
  revision: 4,
  status: 'active',
  phase: 'verifying',
  spec: {
    objective: '完成独立审核提示',
    limits: { max_turns: 4, deadline_unix_secs: Math.floor(Date.now() / 1000) + 3_600, blocked_after_consecutive_rounds: 3 },
  },
  iterations: [{}, {}],
}

describe('GoalBar review status', () => {
  it.each(['draft', 'completed', 'cancelled'])('does not offer resume for %s', status => {
    render(<GoalBar goal={{ ...GOAL, status }} onAction={async () => undefined} />)
    expect(screen.queryByRole('button', { name: '恢复目标' })).toBeNull()
  })

  it('shows remaining turns derived from the raw goal snapshot', () => {
    const { container } = render(<GoalBar goal={GOAL} onAction={async () => undefined} />)

    expect(container.querySelector('[title]')?.getAttribute('title')).toContain('剩余 2 轮')
  })

  it('shows that a separate read-only audit is underway', () => {
    render(<GoalBar goal={GOAL} onAction={async () => undefined} />)

    expect(screen.getByText('独立审核中')).toBeDefined()
    expect(screen.getByRole('status').textContent).toContain('只读核对目标、验收项与证据')
    expect(screen.getByRole('status').textContent).toContain('不会宣布目标完成')
  })

  it.each([
    ['screening', '判断是否需要验收'],
    ['inspecting', '检查完成证据'],
    ['adjudicating', '宿主提交结论'],
  ] as const)('renders review substage %s', (review_stage, label) => {
    render(<GoalBar goal={{ ...GOAL, ...{ review_stage } }} onAction={async () => undefined} />)
    expect(screen.getByText(label)).toBeDefined()
  })

  it('does not show a paused candidate as running review', () => {
    render(<GoalBar goal={{ ...GOAL, status: 'paused', review_stage: null }} onAction={async () => undefined} />)
    expect(screen.getByText('已暂停的目标')).toBeDefined()
    expect(screen.queryByRole('status')).toBeNull()
  })

  it('offers a review retry and explains that timeout did not fail the goal', async () => {
    const goal: GoalSnapshot = {
      ...GOAL,
      status: 'blocked',
      phase: 'idle',
      review_candidate: {
        goal_id: 'goal-1', contract_revision: 2, generation: 3, iteration: 5,
        conversation_seq: 42, reviewer_id: 'reviewer-v2',
      },
      review_failure_reason: 'review timed out',
    }
    const onAction = vi.fn(async () => undefined)
    render(<GoalBar goal={goal} onAction={onAction} />)

    expect(screen.getByText('审核待重试')).toBeDefined()
    expect(screen.getByRole('alert').textContent).toContain('目标工作尚未被判失败')
    const retry = screen.getByRole('button', { name: '重试独立审核' })
    fireEvent.click(retry)
    await waitFor(() => expect(onAction).toHaveBeenCalledWith('resume', goal))
  })

  it('keeps a satisfied human-review goal in the approval state', () => {
    const goal: GoalSnapshot = {
      ...GOAL,
      status: 'blocked',
      phase: 'verifying',
      spec: { objective: '等待人工批准', acceptance: { mode: 'human' } },
      reviews: [{
        verdict: 'satisfied', summary: '契约已满足', next_step: '',
        criteria: [], constraints: [],
      }],
    }
    render(<GoalBar goal={goal} onAction={async () => undefined} />)

    expect(screen.getByText('待人工验收')).toBeDefined()
    expect(screen.queryByRole('status')).toBeNull()
  })

  it('offers the review retry for an active goal whose audit failed', async () => {
    const goal: GoalSnapshot = {
      ...GOAL,
      status: 'active',
      phase: 'verifying',
      review_failure_reason: 'independent review failed',
    }
    const onAction = vi.fn(async () => undefined)
    render(<GoalBar goal={goal} onAction={onAction} />)

    const retry = screen.getByRole('button', { name: '重试独立审核' })
    fireEvent.click(retry)
    await waitFor(() => expect(onAction).toHaveBeenCalledWith('resume', goal))
  })

  it('disables the retry once the known spend reaches the token soft limit', () => {
    const goal: GoalSnapshot = {
      ...GOAL,
      status: 'paused',
      phase: 'idle',
      spec: {
        objective: '限额目标',
        limits: {
          max_turns: 5,
          deadline_unix_secs: Math.floor(Date.now() / 1000) + 3_600,
          blocked_after_consecutive_rounds: 3,
          soft_token_limit: 100,
        },
      },
      usage: { tokens: { input_tokens: 100 } },
    }
    render(<GoalBar goal={goal} onAction={async () => undefined} />)

    const retry = screen.getByRole('button', { name: '恢复目标' }) as HTMLButtonElement
    expect(retry.disabled).toBe(true)
  })

  it('shows the failure a control promise resolves instead of failing silently', async () => {
    const onAction = vi.fn(async () => 'Failed to fetch')
    const { container } = render(<GoalBar goal={GOAL} onAction={onAction} />)

    fireEvent.click(screen.getByRole('button', { name: '暂停目标' }))
    await waitFor(() => {
      expect(container.querySelector('[role="alert"]')?.textContent).toContain('Failed to fetch')
    })
  })
})
