import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, type ApiResponse, type SessionLiveState, type SessionSummary } from '../data/api.ts'
import { App } from './App.tsx'

/**
 * Regressions from the workbench audit (W1, W8) at the real shell: switching
 * between two empty sessions restores the destination draft, and a session can
 * be created in a working directory the reader types even when nothing has been
 * discovered yet.
 */

const streams = vi.hoisted(() => [] as ((event: Record<string, unknown>) => void)[])
vi.mock('../data/sse.ts', () => ({
  SseClient: class {
    constructor(_id: string, onChange: (event: Record<string, unknown>) => void) {
      streams.push(onChange)
    }
    connect(): void {}
    close(): void {}
  },
}))

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
  streams.length = 0
  localStorage.clear()
})

function session(id: string, overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id,
    project_key: 'p',
    cwd: '/tmp/p',
    title: id,
    alias: null,
    status: 'free',
    corrupt: false,
    created_at: '',
    updated_at: '',
    modified_secs: 0,
    path: '',
    ...overrides,
  }
}

function ok<T>(data: T): ApiResponse<T> {
  return { code: 200, message: '', data }
}

/** The open endpoint's answer; the shell only reads the `snapshot` flag. */
function live(id: string): SessionLiveState & { snapshot: boolean } {
  return { id, open: true, running: false, snapshot: true }
}

function snapshot(id: string): Record<string, unknown> {
  return {
    type: 'session_snapshot',
    generation: id,
    stream_sequence: 1,
    conversation: [],
    inputs: [],
    running: false,
    phase: 'idle',
    progress: [],
  }
}

/** The shared mocks the shell needs before a session can be open at all. */
function stubShell(): void {
  vi.spyOn(api, 'version').mockResolvedValue(ok({ bundle: '' }))
  vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('session')))
  vi.spyOn(api, 'modelChoices').mockResolvedValue(ok({ model: 'm', resolved: 'm', choices: [] }))
}

describe('session switching and creation', () => {
  it('restores the destination draft when the sidebar switches between empty sessions', async () => {
    stubShell()
    vi.spyOn(api, 'listSessions').mockResolvedValue(ok([session('A'), session('B')]))
    localStorage.setItem('mink.draft:p:A', 'A 自己的草稿')
    localStorage.setItem('mink.draft:p:B', 'B 自己的草稿')
    render(<App />)
    await waitFor(() => { expect(screen.getByText('A')).toBeDefined() })

    fireEvent.click(screen.getByText('A'))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })
    const field = (): HTMLTextAreaElement => screen.getByLabelText('输入消息') as HTMLTextAreaElement
    fireEvent.change(field(), { target: { value: 'A 自己的草稿' } })

    fireEvent.click(screen.getByText('B'))
    await waitFor(() => { expect(streams.length).toBe(2) })
    act(() => { streams[1](snapshot('B')) })
    // B's composer mounts with B's identity: the draft it starts from is B's,
    // not the text the reader typed in A.
    expect(field().value).toBe('B 自己的草稿')
  })

  it('creates a session in a working directory the reader types', async () => {
    stubShell()
    vi.spyOn(api, 'listSessions').mockResolvedValue(ok([]))
    const created = session('fresh', { cwd: '/tmp/fresh' })
    const create = vi.spyOn(api, 'createSession').mockResolvedValue(ok(created))
    render(<App />)

    // Nothing discovered yet: the empty state still offers the new-task flow.
    const empty = await waitFor(() => {
      const node = document.querySelector('[data-empty-state]')
      expect(node).not.toBeNull()
      return node as HTMLElement
    })
    fireEvent.click(within(empty).getByRole('button', { name: '新建任务' }))
    fireEvent.change(screen.getByLabelText('工作目录'), { target: { value: '/tmp/fresh' } })
    fireEvent.change(screen.getByLabelText('任务名称'), { target: { value: '新目录里的任务' } })
    fireEvent.click(screen.getByRole('button', { name: '创建' }))

    await waitFor(() => { expect(create).toHaveBeenCalledWith('/tmp/fresh', '新目录里的任务') })
  })

  it('keeps the typed directory and the refusal when the create fails', async () => {
    stubShell()
    vi.spyOn(api, 'listSessions').mockResolvedValue(ok([]))
    vi.spyOn(api, 'createSession').mockResolvedValue({ code: 400, message: '工作目录不可用', data: undefined as never })
    render(<App />)

    const empty = await waitFor(() => {
      const node = document.querySelector('[data-empty-state]')
      expect(node).not.toBeNull()
      return node as HTMLElement
    })
    fireEvent.click(within(empty).getByRole('button', { name: '新建任务' }))
    fireEvent.change(screen.getByLabelText('工作目录'), { target: { value: '/nope' } })
    fireEvent.click(screen.getByRole('button', { name: '创建' }))

    await waitFor(() => { expect(screen.getByRole('alert').textContent).toBe('工作目录不可用') })
    // The dialog stays open with the path the reader typed, so it can be fixed
    // in place instead of being retyped.
    expect((screen.getByLabelText('工作目录') as HTMLInputElement).value).toBe('/nope')
  })
})

describe('goal command effects', () => {
  it('re-reads the goal when a slash command changed it', async () => {
    stubShell()
    vi.spyOn(api, 'listSessions').mockResolvedValue(ok([session('A')]))
    const goalRead = vi.spyOn(api, 'goal').mockResolvedValue(ok({ goal: null, driven: false, verifier: false }))
    const budget = vi.spyOn(api, 'goalAction').mockResolvedValue(ok({
      goal: { id: 'g1', revision: 2, status: 'active', phase: 'idle' },
    }))
    render(<App />)
    await waitFor(() => { expect(screen.getByText('A')).toBeDefined() })
    fireEvent.click(screen.getByText('A'))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => {
      streams[0]({
        ...snapshot('A'),
        goal: { id: 'g1', revision: 1, status: 'active', phase: 'idle' },
      })
    })

    const field = screen.getByLabelText('输入消息') as HTMLTextAreaElement
    fireEvent.change(field, { target: { value: '/goal budget 5 60' } })
    fireEvent.keyDown(field, { key: 'Enter' })

    await waitFor(() => { expect(budget).toHaveBeenCalled() })
    // The command answered with the new revision, so the shell re-reads the
    // authoritative goal instead of waiting for the lifecycle event.
    await waitFor(() => { expect(goalRead).toHaveBeenCalledWith('A', 'p') })
  })
})

describe('goal bar failures', () => {
  async function openGoalSession(): Promise<void> {
    // The shell keeps the selected session in the query string; a previous
    // test's selection must not make `A` ambiguous with its breadcrumb.
    window.history.replaceState(null, '', '/')
    // jsdom has no matchMedia, and the session browser consults it while a row
    // is selected.
    vi.stubGlobal('matchMedia', (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }))
    stubShell()
    vi.spyOn(api, 'listSessions').mockResolvedValue(ok([session('A')]))
    render(<App />)
    await waitFor(() => { expect(screen.getByText('A')).toBeDefined() })
    fireEvent.click(screen.getByText('A'))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => {
      streams[0]({
        ...snapshot('A'),
        // A blank session shows the hero instead of the composer seat, and the
        // goal bar lives beside the composer: history is what puts it on screen.
        conversation: [{ seq: 1, role: 'user', content: '任务' }],
        goal: { id: 'g1', revision: 1, status: 'active', phase: 'idle' },
      })
    })
  }

  function goalBarError(): string | undefined {
    return document.querySelector('[data-goal-bar] [role="alert"]')?.textContent ?? undefined
  }

  it('shows a rejected goal action instead of an unhandled rejection', async () => {
    const action = vi.spyOn(api, 'goalAction').mockRejectedValue(new Error('Failed to fetch'))
    await openGoalSession()

    fireEvent.click(screen.getByRole('button', { name: '暂停目标' }))
    await waitFor(() => { expect(action).toHaveBeenCalled() })
    // A transport failure is a reader-visible error line, not a silent no-op.
    await waitFor(() => { expect(goalBarError()).toContain('Failed to fetch') })
  })

  it('shows the refusal receipt a goal action returned', async () => {
    vi.spyOn(api, 'goalAction').mockResolvedValue({
      code: 409,
      message: 'stale goal ID or revision; reread goal_snapshot',
      data: null,
    } as unknown as Awaited<ReturnType<typeof api.goalAction>>)
    await openGoalSession()

    fireEvent.click(screen.getByRole('button', { name: '暂停目标' }))
    await waitFor(() => {
      expect(goalBarError()).toContain('stale goal ID or revision')
    })
  })
})
