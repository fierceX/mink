import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import type { GoalMessageItem } from '../../data/types.ts'
import { GoalNotice } from './GoalNotice.tsx'

/**
 * Core tags every goal boundary with a typed `_mink.goal_message`: a draft
 * discussion, a goal start or a continuation. Each kind must render its own
 * label — a draft shown as "继续执行目标" told the reader the goal was running
 * while it was still waiting for confirmation.
 */

afterEach(cleanup)

function item(kind: GoalMessageItem['goal']['kind'], feedback?: string | null): GoalMessageItem {
  return {
    kind: 'goal_message',
    goal: {
      kind,
      goal_id: 'goal-1',
      contract_revision: 2,
      iteration: kind === 'draft' ? 0 : 3,
      objective: '修复审计问题',
      feedback: feedback ?? null,
    },
  }
}

describe('goal boundary labels', () => {
  it('labels a draft discussion as a draft, without a round number', () => {
    const { container } = render(<GoalNotice item={item('draft', '补充证据')} />)

    expect(screen.getByText('目标草案讨论')).toBeDefined()
    expect(container.textContent).toContain('契约 r2')
    expect(container.textContent).toContain('修改意见')
    expect(container.textContent).not.toContain('第 0 轮')
  })

  it('labels the first round as a start and later rounds as continuations', () => {
    const start = render(<GoalNotice item={item('start')} />)
    expect(start.container.textContent).toContain('开始目标')
    expect(start.container.textContent).toContain('第 3 轮')
    start.unmount()
    const continuation = render(<GoalNotice item={item('continuation')} />)
    expect(continuation.container.textContent).toContain('继续执行目标')
    expect(continuation.container.textContent).toContain('第 3 轮')
  })

  it('marks the card with the kind the runtime published', () => {
    const { container } = render(<GoalNotice item={item('draft')} />)
    expect(container.querySelector('[data-goal-notice]')?.getAttribute('data-goal-notice')).toBe('draft')
  })
})
