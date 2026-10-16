import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { SessionSummary } from '../data/api.ts'
import { App } from './App.tsx'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

function session(overrides: Partial<SessionSummary>): SessionSummary {
  return {
    project_key: 'demo',
    corrupt: false,
    id: 'session-1',
    alias: null,
    title: '把工作台重建起来',
    cwd: '/tmp/demo',
    created_at: '2026-10-10T00:00:00Z',
    updated_at: new Date().toISOString(),
    modified_secs: 0,
    status: 'free',
    path: '/tmp/demo/session-1',
    ...overrides,
  }
}

describe('workbench shell', () => {
  it('lists the discovered sessions grouped by working directory', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({
      code: 200,
      message: 'ok',
      data: [
        session({ id: 'session-1', title: '把工作台重建起来', cwd: '/tmp/demo' }),
        session({ id: 'session-2', title: '第二个会话', cwd: '/tmp/demo' }),
        session({ id: 'session-3', title: '另一个项目', cwd: '/tmp/other' }),
      ],
    }), { status: 200, headers: { 'Content-Type': 'application/json' } }))
    vi.stubGlobal('fetch', fetchMock)

    render(<App />)

    await waitFor(() => {
      expect(screen.getByText('把工作台重建起来')).toBeDefined()
    })
    expect(screen.getByText('第二个会话')).toBeDefined()
    expect(screen.getByText('另一个项目')).toBeDefined()
    // Project rows carry the directory's last segment, not the full path.
    expect(screen.getByText('demo')).toBeDefined()
    expect(screen.getByText('other')).toBeDefined()
    expect(screen.getByRole('button', { name: '新建任务' })).toBeDefined()
  })

  it('offers a way to start when no session is open', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({
      code: 200,
      message: 'ok',
      data: [session({ id: 'session-1', title: '把工作台重建起来', cwd: '/tmp/demo' })],
    }), { status: 200, headers: { 'Content-Type': 'application/json' } }))
    vi.stubGlobal('fetch', fetchMock)

    render(<App />)
    // The empty state names the project a new session would join, rather than
    // offering a picker the workbench does not have.
    await waitFor(() => {
      expect(screen.getByText(/在 demo 新建任务/)).toBeDefined()
    })
    expect(screen.getByText(/选择 demo 里的会话/)).toBeDefined()
  })

  it('reports a failed session load instead of rendering an empty list', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => { throw new Error('connect refused') }))

    render(<App />)

    await waitFor(() => {
      expect(screen.getByRole('status').textContent).toContain('connect refused')
    })
  })
})
