import { describe, expect, it } from 'vitest'
import type { ToolItem } from '../../../data/types.ts'
import { goalReportCard, planCard, subAgentCard, todoCard, todoStatusLabel } from './toolCards.ts'

/** A delegation item with the given argument JSON. */
function delegation(input: string): ToolItem {
  return {
    kind: 'tool',
    id: 't1',
    name: 'SubAgent',
    color: 'delegate',
    view: 'text',
    summary: '派发子代理',
    input,
    result: 'ok',
    success: true,
    key: 'message:1:0',
  }
}

describe('delegation card', () => {
  it('reads the current single-prompt schema as one row', () => {
    const card = subAgentCard(delegation(JSON.stringify({
      prompt: '逐项对照。',
      description: '核对工具卡家族',
      fork: false,
      model: 'pro',
    })))
    expect(card?.tasks).toEqual([
      { id: '核对工具卡家族', description: '核对工具卡家族', prompt: '逐项对照。' },
    ])
  })

  it('reads one row per dispatched task', () => {
    const card = subAgentCard(delegation(JSON.stringify({
      tasks: [
        { id: 'cards', description: '核对工具卡家族', prompt: '逐项对照。' },
        { description: '没有 id 的任务' },
      ],
    })))
    expect(card?.tasks).toEqual([
      { id: 'cards', description: '核对工具卡家族', prompt: '逐项对照。' },
      { id: 'task-2', description: '没有 id 的任务', prompt: '' },
    ])
  })

  it('leaves anything it cannot read to the generic card', () => {
    // Not a delegation, unparsable, or a task list with nothing to show.
    expect(subAgentCard({ ...delegation('{}'), name: 'Bash' })).toBeNull()
    expect(subAgentCard(delegation('not json'))).toBeNull()
    expect(subAgentCard(delegation(JSON.stringify({ tasks: [{}, 'x'] })))).toBeNull()
    expect(subAgentCard(delegation(JSON.stringify({ tasks: 'nope' })))).toBeNull()
  })
})

describe('todo and plan cards', () => {
  it('reads the todo list the tool answered with', () => {
    const item: ToolItem = {
      ...delegation('{}'),
      name: 'TodoWrite',
      result: JSON.stringify({
        revision: 3,
        todos: [
          { id: 'T0001', content: '重建外壳', status: 'completed' },
          { id: 'T0002', content: '收尾卡片', status: 'in_progress' },
        ],
      }),
    }
    expect(todoCard(item)).toEqual({
      revision: 3,
      // The head the legacy card drew: the tally comes from the rows when the
      // payload carries none.
      counts: { pending: 0, in_progress: 1, completed: 1 },
      entries: [
        { id: 'T0001', content: '重建外壳', status: 'completed' },
        { id: 'T0002', content: '收尾卡片', status: 'in_progress' },
      ],
    })
    expect(todoStatusLabel('in_progress')).toBe('进行中')
    expect(todoStatusLabel('unknown-state')).toBe('unknown-state')
  })

  it('leaves a non-answer to the generic card', () => {
    expect(todoCard({ ...delegation('{}'), name: 'Bash' })).toBeNull()
    expect(todoCard({ ...delegation('{}'), name: 'TodoRead', result: 'not json' })).toBeNull()
    expect(todoCard({ ...delegation('{}'), name: 'TodoRead', result: '{"todos":[]}' })).toBeNull()
  })

  it('shows a plan’s own text with its answer line', () => {
    const item: ToolItem = {
      ...delegation('{}'),
      name: 'PlanDraft',
      input: JSON.stringify({ content: '# 计划\n1. 对齐卡片' }),
      result: '计划草稿已写入 plan.md',
    }
    expect(planCard(item)).toEqual({ text: '# 计划\n1. 对齐卡片', answer: '计划草稿已写入 plan.md' })
    // Not a plan tool, or a call that carries neither text nor answer.
    expect(planCard({ ...item, name: 'Bash' })).toBeNull()
    expect(planCard({ ...item, input: '{}', result: '' })).toBeNull()
  })
})

describe('goal report card', () => {
  it('reads the report’s own parts', () => {
    const item: ToolItem = {
      ...delegation('{}'),
      name: 'GoalReport',
      input: JSON.stringify({
        iteration: 3,
        kind: 'complete',
        summary: '重建完成',
        next_step: '无',
        criteria: ['结构一致'],
        evidence: ['vitest 218 passed', 42],
      }),
    }
    expect(goalReportCard(item)).toEqual({
      kind: '报告完成',
      iteration: 3,
      summary: '重建完成',
      nextStep: '无',
      criteria: ['结构一致'],
      // A non-string entry is dropped rather than printed as "[object Object]".
      evidence: ['vitest 218 passed'],
    })
  })

  it('leaves anything else to the generic card', () => {
    expect(goalReportCard({ ...delegation('{}'), name: 'Bash' })).toBeNull()
    expect(goalReportCard({ ...delegation('{}'), name: 'GoalReport', input: 'not json' })).toBeNull()
    expect(goalReportCard({ ...delegation('{}'), name: 'GoalReport', input: JSON.stringify({ kind: 'continue' }) })).toBeNull()
  })
})
