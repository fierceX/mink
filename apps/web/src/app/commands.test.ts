import { afterEach, describe, expect, it, vi } from 'vitest'
import { runCommand } from './commands.ts'
import type { CommandContext } from './commands.ts'
import type { SessionSummary } from '../data/api.ts'
import { emptySession } from '../data/types.ts'

afterEach(() => {
  vi.unstubAllGlobals()
})

const SESSION: SessionSummary = {
  project_key: 'demo',
  corrupt: false,
  id: 'session-1',
  alias: null,
  title: '工作台',
  cwd: '/tmp/demo',
  created_at: '',
  updated_at: '',
  modified_secs: null,
  status: 'free',
  path: '/tmp/demo/session-1',
}

function context(overrides: Partial<CommandContext> = {}): CommandContext {
  const state = emptySession('session-1', '工作台')
  return {
    session: SESSION,
    state,
    createGoal: async () => ({ message: '', ok: true }),
    ...overrides,
  }
}

function stubApi(payload: unknown, code = 200, message = '') {
  const fetchMock = vi.fn(async () => new Response(JSON.stringify({ code, message, data: payload }), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  }))
  vi.stubGlobal('fetch', fetchMock)
  return fetchMock
}

describe('slash command runner', () => {
  it('opens the elected panel for a simple command', async () => {
    const result = await runCommand('/plan', context())
    expect(result.ok).toBe(true)
    expect(result.effects?.openDetails).toBe('task')
  })

  it('opens the diagnostics tab for /status', async () => {
    const result = await runCommand('/status', context())
    expect(result.ok).toBe(true)
    expect(result.effects?.openDetails).toBe('diagnostics')
  })

  it('stays quiet when a goal was created, because the bar shows it', async () => {
    // The runner hands creation to its owner, which reports no message: the goal
    // bar's own appearance is the feedback.
    const created: unknown[] = []
    const result = await runCommand('/goal 把工作台收尾', context({
      createGoal: async (body) => {
        created.push(body)
        return { message: '', ok: true }
      },
    }))
    expect(result.ok).toBe(true)
    expect(result.message).toBe('')
    expect(created).toEqual([{
      protocol_version: 2,
      objective: '把工作台收尾',
      acceptance: 'agent_reviewed',
    }])
  })

  it('asks the transcript to follow for /latest', async () => {
    const result = await runCommand('/latest', context())
    expect(result.ok).toBe(true)
    expect(result.effects?.follow).toBe(true)
  })

  it('switches the model through the server', async () => {
    const fetchMock = stubApi({ model: 'deepseek-pro', label: 'Pro' })
    const result = await runCommand('/pro', context())
    expect(result.ok).toBe(true)
    expect(result.message).toContain('Pro')
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toContain('/api/sessions/session-1/model')
    expect(JSON.parse(String(init.body))).toEqual({ model: 'pro' })
  })

  it('refuses a goal control with no goal instead of pretending it ran', async () => {
    const result = await runCommand('/goal pause', context())
    expect(result.ok).toBe(false)
    expect(result.message).toBe('本会话没有目标')
  })

  it('submits a goal control as a revision-checked call', async () => {
    const fetchMock = stubApi({ goal: { id: 'goal-1', revision: 4 } })
    const state = emptySession('session-1', '工作台')
    state.goal = { id: 'goal-1', revision: 4, status: 'active', phase: 'executing' }
    const result = await runCommand('/goal pause', context({ state }))
    expect(result.ok).toBe(true)
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toContain('/goal/pause')
    expect(JSON.parse(String(init.body))).toEqual({ goal_id: 'goal-1', revision: 4 })
  })

  it('confirms the exact draft revision through the host API', async () => {
    const fetchMock = stubApi({ goal: { id: 'draft-1', revision: 4 } })
    const state = emptySession('session-1', '工作台')
    state.goal = { id: 'draft-1', revision: 3, status: 'draft', phase: 'idle' }
    const result = await runCommand('/goal confirm draft-1 4', context({ state }))
    expect(result.ok).toBe(true)
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toContain('/goal/confirm')
    expect(JSON.parse(String(init.body))).toEqual({ goal_id: 'draft-1', revision: 4 })
    expect(result.effects?.refreshGoal).toBe(true)
  })

  it('submits draft refinement against the currently observed revision', async () => {
    const fetchMock = stubApi({ goal: { id: 'draft-1', revision: 5 } })
    const state = emptySession('session-1', '工作台')
    state.goal = { id: 'draft-1', revision: 4, status: 'draft', phase: 'idle' }
    const result = await runCommand('/goal refine 补充错误路径和证据', context({ state }))
    expect(result.ok).toBe(true)
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toContain('/goal/refine')
    expect(JSON.parse(String(init.body))).toEqual({
      goal_id: 'draft-1',
      revision: 4,
      feedback: '补充错误路径和证据',
    })
  })

  it('forwards the optional soft token limit when changing the goal budget', async () => {
    const fetchMock = stubApi({ goal: { id: 'goal-1', revision: 5 } })
    const state = emptySession('session-1', '工作台')
    state.goal = { id: 'goal-1', revision: 4, status: 'active', phase: 'executing' }
    const result = await runCommand('/goal budget 30 90 200000', context({ state }))
    expect(result.ok).toBe(true)
    const [, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(JSON.parse(String(init.body))).toEqual({
      goal_id: 'goal-1',
      revision: 4,
      max_turns: 30,
      max_minutes: 90,
      soft_token_limit: 200000,
    })
  })

  it('keeps the parser’s usage hint when the arguments are wrong', async () => {
    const result = await runCommand('/goal budget 3', context())
    expect(result.ok).toBe(false)
    expect(result.message).toContain('用法：/goal budget')
  })

  it('refuses an unknown command and one that is terminal-only', async () => {
    expect((await runCommand('/nope', context())).ok).toBe(false)
    expect((await runCommand('/quit', context())).message).toContain('只在终端可用')
  })

  it('refuses /resume for an input the queue does not hold', async () => {
    const result = await runCommand('/resume input-9', context())
    expect(result.ok).toBe(false)
    expect(result.message).toBe('未找到待处理输入 input-9')
  })
})
