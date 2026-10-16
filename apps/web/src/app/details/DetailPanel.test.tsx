import { useState } from 'react'
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { GoalSnapshot, SessionSummary } from '../../data/api.ts'
import { emptySession } from '../../data/types.ts'
import { DetailPanel } from './DetailPanel.tsx'
import type { DetailTab } from './useDetails.ts'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

const SESSION: SessionSummary = {
  project_key: 'demo',
  corrupt: false,
  id: 'session-1',
  alias: null,
  title: '工作台',
  cwd: '/tmp/demo',
  created_at: '2026-10-10T00:00:00Z',
  updated_at: '2026-10-10T00:00:00Z',
  modified_secs: 0,
  status: 'free',
  path: '/tmp/demo/session-1',
}

const GOAL: GoalSnapshot = {
  id: 'goal-1',
  revision: 3,
  status: 'active',
  phase: 'executing',
  iterations: [{}, {}, {}, {}, {}],
  blocker_rounds: 0,
  spec: {
    objective: '把工作台重建起来',
    acceptance_criteria: ['照抄设计', '代码可直接拷贝'],
    constraints: ['不改运行时状态'],
    acceptance: { mode: 'human_approved' },
    limits: {
      max_turns: 20,
      deadline_unix_secs: Math.floor(Date.now() / 1000) + 3_600,
      soft_token_limit: null,
      blocked_after_consecutive_rounds: 3,
    },
  },
  completion_basis: null,
  reason: null,
}

const DRAFT_GOAL: GoalSnapshot = {
  id: 'draft-1',
  revision: 4,
  contract_revision: 2,
  status: 'draft',
  phase: 'idle',
  draft_ready: true,
  spec: {
    original_request: '修复上传失败后无法重试',
    objective: '失败上传可以安全重试',
    subgoals: [{ id: 'subgoal-1', objective: '保留失败附件并提供重试入口' }],
    criteria: [{
      id: 'criterion-1',
      subgoal_id: 'subgoal-1',
      outcome: '失败后附件状态显示失败原因',
      required_evidence: '组件测试与服务器响应',
    }],
    acceptance_criteria: ['失败后附件状态显示失败原因'],
    acceptance: { mode: 'agent_reviewed' },
  },
}

/** Answer the detail endpoints, recording every request path. */
/** Marker the file stub returns to make the client ask for the read form. */
const NOT_A_FILE = Symbol('not-a-file')

/** The files endpoint: a listing for a directory, a read once `raw=true` is asked. */
function fileAnswer(url: string): unknown {
  const asked = new URL(url, 'http://web.test')
  const rel = asked.searchParams.get('path') ?? ''
  const raw = asked.searchParams.get('raw') === 'true'
  if (raw) return { path: rel, content: '# README\n\n正文' }
  // The root, and every path a directory the panel descended into.
  if (rel === '' || rel.endsWith('/')) {
    return rel === ''
      ? { path: rel, dir: true, items: [{ name: 'src', dir: true }, { name: 'README.md', dir: false }] }
      : { path: rel, dir: true, items: [{ name: 'nested.ts', dir: false }] }
  }
  // Not a directory: the server answers 404 and the client retries as a read.
  return NOT_A_FILE
}

function stubApi(overrides: Record<string, unknown> = {}, goal: GoalSnapshot = GOAL) {
  const calls: string[] = []
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    calls.push(url)
    const body = url.includes('/goal')
      ? { goal, driven: true }
      : url.includes('/plan')
        ? { plan: '# 计划\n\n按来源重建外壳。', draft: null }
        : url.includes('/todo')
          ? { todos: { revision: 4, items: [{ id: 'T0001', status: 'completed', content: '重建外壳' }] } }
          : url.includes('/artifacts')
            ? { artifacts: [{ id: 'artifact-1', tool: 'Bash' }] }
            : url.includes('/files')
              ? fileAnswer(url)
              : overrides
    if (body === NOT_A_FILE) {
      return new Response(JSON.stringify({ code: 404, message: 'directory not found', data: null }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    }
    return new Response(JSON.stringify({ code: 200, message: '', data: body }), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    })
  })
  vi.stubGlobal('fetch', fetchMock)
  return calls
}

/**
 * Controlled host: the panel's tab and path live in the shell, so tests drive
 * them through this wrapper exactly as the app does.
 */
function Panel({ session = SESSION, state }: { session?: SessionSummary; state?: Parameters<typeof DetailPanel>[0]['state'] }) {
  const [tab, setTab] = useState<DetailTab>('task')
  const [path, setPath] = useState<string | undefined>(undefined)
  return (
    <DetailPanel
      session={session}
      width={360}
      state={state}
      tab={tab}
      path={path}
      onTabChange={setTab}
      onPathChange={setPath}
      onClose={() => {}}
    />
  )
}

describe('detail panel', () => {
  it('does not carry budget edits into a replacement goal', async () => {
    stubApi()
    const state = { ...emptySession(SESSION.id, 'work'), goal: GOAL }
    const { rerender } = render(<Panel state={state} />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    await screen.findByRole('spinbutton', { name: '总轮数上限' })
    fireEvent.change(screen.getByRole('spinbutton', { name: '总轮数上限' }), { target: { value: '30' } })
    rerender(<Panel state={{ ...state, goal: { ...GOAL, id: 'replacement', revision: 1 } }} />)
    await waitFor(() => expect((screen.getByRole('spinbutton', { name: '总轮数上限' }) as HTMLInputElement).value).toBe(''))
  })

  it('retries a failed goal only after an explicit budget adjustment', async () => {
    let current: GoalSnapshot = { ...GOAL, status: 'failed', phase: 'idle',
      spec: { ...GOAL.spec, limits: { ...GOAL.spec!.limits, deadline_unix_secs: 1 } },
      reason: 'unclassified execution failure',
      iterations: [{ finished: true, outcome_status: 'failed',
        outcome_error: 'unclassified execution failure' }],
    }
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (String(input).includes('/goal/budget')) {
        const body = JSON.parse(String(init?.body)) as { max_turns: number; max_minutes: number }
        current = { ...current, revision: 4, spec: { ...current.spec, limits: { ...current.spec!.limits,
          max_turns: body.max_turns, deadline_unix_secs: Math.floor(Date.now() / 1000) + body.max_minutes * 60 } } }
      }
      return new Response(JSON.stringify({ code: 200, message: '', data: { goal: current, driven: true } }),
        { status: 200, headers: { 'Content-Type': 'application/json' } })
    })
    vi.stubGlobal('fetch', fetchMock)
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    const retry = await screen.findByRole('button', { name: '重试目标' })
    expect((retry as HTMLButtonElement).disabled).toBe(true)
    fireEvent.change(screen.getByRole('spinbutton', { name: '总轮数上限' }), { target: { value: '30' } })
    fireEvent.change(screen.getByRole('spinbutton', { name: '从现在起允许分钟数' }), { target: { value: '120' } })
    fireEvent.click(screen.getByRole('button', { name: '调整预算' }))
    await waitFor(() => expect((screen.getByRole('button', { name: '重试目标' }) as HTMLButtonElement).disabled).toBe(false))
    const budget = fetchMock.mock.calls.find(([url]) => String(url).includes('/goal/budget'))
    expect(JSON.parse(String(budget?.[1]?.body))).toEqual({ goal_id: GOAL.id, revision: 3, max_turns: 30, max_minutes: 120 })
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/goal/resume'))).toBe(false)
    fireEvent.click(screen.getByRole('button', { name: '重试目标' }))
    await waitFor(() => expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/goal/resume'))).toBe(true))
    const resumed = fetchMock.mock.calls.find(([url]) => String(url).includes('/goal/resume'))
    expect(JSON.parse(String(resumed?.[1]?.body))).toEqual({ goal_id: GOAL.id, revision: 4 })
  })

  it('offers explicit failed execution retry without suggesting review retry', async () => {
    const calls = stubApi({}, { ...GOAL, status: 'failed', phase: 'idle', reason: 'execution failed' })
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    expect(await screen.findByText(/本次运行失败，已停止自动执行/)).toBeDefined()
    expect(screen.queryByRole('button', { name: '重试审核' })).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: '重试目标' }))
    await waitFor(() => expect(calls.some(url => url.includes('/goal/resume'))).toBe(true))
  })

  it.each(['completed', 'cancelled'])('does not offer recovery or live controls for a %s goal', async status => {
    stubApi({}, { ...GOAL, status, phase: 'idle', reason: 'unclassified execution failure' })
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    await screen.findByText('把工作台重建起来')
    for (const name of ['恢复', '暂停', '批准完成', '对账副作用', '取消']) {
      expect(screen.queryByRole('button', { name })).toBeNull()
    }
    expect(screen.getByRole('button', { name: '清除' })).toBeDefined()
  })

  it('shows continuation separately from an older full review and links its report', async () => {
    const candidate = { goal_id: GOAL.id, contract_revision: 1, iteration: 2, conversation_seq: 10, reviewer_id: 'v3' }
    stubApi({}, { ...GOAL, contract_revision: 1, phase: 'idle', review_candidate: candidate,
      continuations: [{ proposal: { candidate, summary: '本轮只实现了一部分', remaining_requirements: ['criterion-1'], next_step: '补齐测试' }, report: 'artifact://screening-report', fingerprint: 'stable', consecutive_rounds: 1, conversation_seq: 12 }],
      reviews: [{ candidate: { ...candidate, iteration: 1 }, verdict: 'needs_work', summary: '第一轮证据不足', next_step: '实现功能', criteria: [], constraints: [] }],
    })
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    expect(await screen.findByText(/第 2 轮：本轮部分完成/)).toBeDefined()
    expect(screen.getByText('剩余要求：criterion-1')).toBeDefined()
    expect(screen.getByText(/本轮未进行完整证据验收/)).toBeDefined()
    expect(screen.getByText(/最近审核：仍需工作（第 1 轮）/)).toBeDefined()
    expect(screen.getByRole('button', { name: '读取分流报告' })).toBeDefined()
  })

  it('shows the descriptive report and identifies a failed host submission', async () => {
    stubApi({}, { ...GOAL, status: 'blocked', review_failure_stage: 'adjudicating',
      review_failure_reason: 'host submission unavailable', reviews: [{ verdict: 'needs_work',
        summary: '独立审核已指出缺口', next_step: '补齐实现', criteria: [], constraints: [],
        report: { artifact: 'artifact://audit-report' } }],
    })
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    expect(await screen.findByRole('heading', { name: '宿主结论提交未完成' })).toBeDefined()
    expect(screen.getByText(/重试只恢复宿主提交/)).toBeDefined()
    expect(screen.getByRole('button', { name: '读取审核报告' })).toBeDefined()
    expect(screen.getByRole('button', { name: '重试提交' })).toBeDefined()
  })

  it('shows the plan and the todo list on the task tab', async () => {
    stubApi()
    render(<Panel />)
    await waitFor(() => {
      expect(screen.getByText(/按来源重建外壳/)).toBeDefined()
    })
    expect(screen.getByText('T0001: 重建外壳')).toBeDefined()
    expect(screen.getByText('revision 4')).toBeDefined()
    expect(screen.getByText('已确认')).toBeDefined()
  })

  it('renders the goal contract and submits a control as a revision-checked call', async () => {
    const calls = stubApi()
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    await waitFor(() => {
      expect(screen.getByText('把工作台重建起来')).toBeDefined()
    })
    expect(screen.getByText('人工验收')).toBeDefined()
    expect(screen.getByText('20 上限 · 已用 5 · 剩余 15')).toBeDefined()
    expect(screen.getByText(/照抄设计/)).toBeDefined()
    expect(screen.getByText('修订 3 · 每次变更按修订号提交；契约保持不变，预算仅由明确操作调整。')).toBeDefined()

    fireEvent.click(screen.getByRole('button', { name: '暂停' }))
    await waitFor(() => {
      expect(calls.some(url => url.includes('/goal/pause'))).toBe(true)
    })
  })

  it('explains an incomplete independent review and offers a review-only retry', async () => {
    const reviewCandidate = {
      goal_id: 'goal-1', contract_revision: 2, generation: 3, iteration: 5,
      conversation_seq: 42, reviewer_id: 'reviewer-v2',
    }
    const failedGoal: GoalSnapshot = {
      ...GOAL,
      contract_revision: 2,
      status: 'blocked',
      phase: 'idle',
      review_candidate: reviewCandidate,
      review_failure_reason: 'review timed out',
    }
    const calls = stubApi({}, failedGoal)
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))

    expect(await screen.findByText('审核未完成')).toBeDefined()
    expect(screen.getByRole('alert', { name: '独立审核状态' })).toBeDefined()
    expect(screen.getByText(/重试会复核已结束的本轮，不会重新执行任务/)).toBeDefined()
    fireEvent.click(screen.getByRole('button', { name: '重试审核' }))
    await waitFor(() => {
      const request = calls.find(url => url.includes('/goal/resume'))
      expect(request).toBeDefined()
    })
  })

  it('shows a versioned draft and submits confirmation or refinement through the host API', async () => {
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response(JSON.stringify({
      code: 200,
      message: '',
      data: { goal: DRAFT_GOAL, driven: true },
    }), { status: 200, headers: { 'Content-Type': 'application/json' } }))
    vi.stubGlobal('fetch', fetchMock)
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    await waitFor(() => {
      expect(screen.getByText('待确认')).toBeDefined()
      expect(screen.getByText('独立审核')).toBeDefined()
      expect(screen.getByText(/失败后附件状态显示失败原因/)).toBeDefined()
      expect(screen.getByText(/组件测试与服务器响应/)).toBeDefined()
    })

    fireEvent.change(screen.getByRole('textbox', { name: '目标草案修改意见' }), {
      target: { value: '把网络超时也纳入验收' },
    })
    fireEvent.click(screen.getByRole('button', { name: '提交修改意见' }))
    await waitFor(() => {
      const request = fetchMock.mock.calls.find(([url]) => String(url).includes('/goal/refine'))
      expect(request).toBeDefined()
      expect(JSON.parse(String(request?.[1]?.body))).toEqual({
        goal_id: 'draft-1', revision: 4, feedback: '把网络超时也纳入验收',
      })
    })

    fireEvent.click(screen.getByRole('button', { name: '确认并启动' }))
    await waitFor(() => {
      const request = fetchMock.mock.calls.find(([url]) => String(url).includes('/goal/confirm'))
      expect(request).toBeDefined()
      expect(JSON.parse(String(request?.[1]?.body))).toEqual({ goal_id: 'draft-1', revision: 4 })
    })
  })

  it('shows a stale goal control rejection and refreshes the snapshot', async () => {
    let goalReads = 0
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input)
      if (url.includes('/goal/pause')) {
        return new Response(JSON.stringify({ code: 409, message: 'stale goal ID or revision', data: null }), {
          status: 200, headers: { 'Content-Type': 'application/json' },
        })
      }
      if (url.includes('/goal')) goalReads += 1
      return new Response(JSON.stringify({ code: 200, message: '', data: { goal: GOAL, driven: true } }), {
        status: 200, headers: { 'Content-Type': 'application/json' },
      })
    })
    vi.stubGlobal('fetch', fetchMock)
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    await screen.findByText('把工作台重建起来')
    fireEvent.click(screen.getByRole('button', { name: '暂停' }))

    expect((await screen.findByRole('alert')).textContent).toContain('stale goal ID or revision')
    await waitFor(() => expect(goalReads).toBeGreaterThan(1))
  })

  it('does not carry a previous session action error into the next goal', async () => {
    let rejectPause!: (reason: Error) => void
    const pause = new Promise<Response>((_resolve, reject) => { rejectPause = reject })
    const fetchMock = vi.fn((input: RequestInfo | URL) => {
      const url = String(input)
      if (url.includes('/goal/pause')) return pause
      const goal = url.includes('/session-B/') ? { ...GOAL, id: 'goal-B', spec: { ...GOAL.spec, objective: 'B objective' } } : GOAL
      return Promise.resolve(new Response(JSON.stringify({
        code: 200, message: '', data: { goal, driven: true },
      }), { status: 200, headers: { 'Content-Type': 'application/json' } }))
    })
    vi.stubGlobal('fetch', fetchMock)
    const { rerender } = render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    await screen.findByText('把工作台重建起来')
    fireEvent.click(screen.getByRole('button', { name: '暂停' }))
    rerender(<Panel session={{ ...SESSION, id: 'session-B' }} />)
    expect(await screen.findByText('B objective')).toBeDefined()

    await act(async () => { rejectPause(new Error('old session network failure')) })
    expect(screen.queryByText('old session network failure')).toBeNull()
  })

  it('renders the latest review with per-criterion evidence', async () => {
    const reviewedGoal: GoalSnapshot = {
      ...GOAL,
      contract_revision: 2,
      review_candidate: {
        goal_id: 'goal-1', contract_revision: 2, generation: 1, iteration: 1,
        conversation_seq: 20, reviewer_id: 'reviewer-1',
      },
      spec: {
        ...GOAL.spec,
        scope: '只调整复制反馈和窄屏布局',
        suggested_actions: ['补充窄屏用例', '复核焦点恢复'],
        subgoals: [{ id: 'subgoal-1', objective: '完成复制体验' }],
        criteria: [
          { id: 'criterion-1', subgoal_id: 'subgoal-1', outcome: '复制成功可见', required_evidence: '组件测试' },
          { id: 'criterion-2', subgoal_id: 'subgoal-1', outcome: '窄屏可用', required_evidence: '浏览器测试' },
        ],
      },
      reviews: [{
        candidate: {
          goal_id: 'goal-1', contract_revision: 2, generation: 1, iteration: 1,
          conversation_seq: 20, reviewer_id: 'reviewer-1',
        },
        verdict: 'needs_work',
        summary: '复制行为通过，窄屏样式仍需调整。',
        next_step: '修复窄屏样式',
        original_request: { status: 'pass', reason: '已覆盖原始要求', evidence: ['src/CopyButton.tsx'] },
        criteria: [
          { criterion_id: 'criterion-1', status: 'pass', reason: '复制成功可见', evidence: ['tests/copy.test.ts'] },
          { criterion_id: 'criterion-2', status: 'unknown', reason: '缺少窄屏证据', evidence: ['tests/copy.spec.ts'] },
        ],
        constraints: [],
      }],
    }
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({
      code: 200, message: '', data: { goal: reviewedGoal, driven: true },
    }), { status: 200, headers: { 'Content-Type': 'application/json' } })))
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    expect(await screen.findByText(/最近审核：仍需工作（第 .* 轮）/)).toBeDefined()
    expect(screen.getByText(/缺少窄屏证据/)).toBeDefined()
    expect(screen.getByText(/证据：tests\/copy.spec.ts/)).toBeDefined()
    expect(screen.getByText('只调整复制反馈和窄屏布局')).toBeDefined()
    expect(screen.getByText('补充窄屏用例')).toBeDefined()
    expect(screen.getByText('部分通过 · 1/2 项验收标准通过')).toBeDefined()
  })

  it('renders the verifier wire mode and checker identity', async () => {
    const verifiedGoal: GoalSnapshot = {
      ...GOAL,
      spec: { ...GOAL.spec, acceptance: { mode: 'verifier', id: 'cargo-check' } },
    }
    stubApi({}, verifiedGoal)
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '目标' }))
    expect(await screen.findByText(/检查器验收（cargo-check）/)).toBeDefined()
  })

  it('lists the session tree on the files tab and opens a directory', async () => {
    const calls = stubApi()
    render(<Panel />)
    fireEvent.click(screen.getByRole('tab', { name: '文件' }))
    // Rows name what they do: a directory expands, a file opens.
    const directory = await screen.findByRole('button', { name: '展开目录 src' })
    expect(screen.getByRole('button', { name: '打开文件 README.md' })).toBeDefined()
    fireEvent.click(directory)
    await waitFor(() => {
      expect(calls.some(url => url.includes('path=src%2F'))).toBe(true)
    })
  })
})
