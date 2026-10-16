import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import type { SessionSummary } from '../../data/api.ts'
import { SessionBrowser, groupSessions, rowTimeLabel, sessionTitle } from './SessionBrowser.tsx'

afterEach(cleanup)

function session(overrides: Partial<SessionSummary>): SessionSummary {
  return {
    project_key: 'demo',
    corrupt: false,
    id: 'session-1',
    alias: null,
    title: '重建会话区',
    cwd: '/work/mink',
    created_at: '',
    updated_at: new Date().toISOString(),
    modified_secs: 0,
    status: 'free',
    path: '/work/mink/session-1',
    ...overrides,
  }
}

const SESSIONS: SessionSummary[] = [
  session({ id: 's-1', title: '重建会话区', cwd: '/work/mink' }),
  session({ id: 's-2', title: '修复缓存', cwd: '/work/harness' }),
]

function renderBrowser() {
  const groups = groupSessions(SESSIONS)
  render(
    <SessionBrowser
      groups={groups}
      selectedKey={undefined}
      now={Date.now()}
      onOpen={() => {}}
      onCreate={() => {}}
      onDelete={() => {}}
    />,
  )
  return groups
}

/** Open the search field the way a reader does. */
function openSearch(): HTMLInputElement {
  fireEvent.click(screen.getByRole('button', { name: '搜索会话' }))
  return screen.getByLabelText('搜索') as HTMLInputElement
}

describe('session browser search', () => {
  it('lists project groups and session rows until a query narrows the catalog', () => {
    renderBrowser()
    expect(screen.getByRole('tree', { name: '会话' })).toBeDefined()
    expect(screen.getAllByText('重建会话区').length).toBeGreaterThan(0)
    expect(screen.getAllByText('修复缓存').length).toBeGreaterThan(0)
  })

  it('searches only the discovered catalog and switches to a result tree', () => {
    renderBrowser()
    const input = openSearch()
    fireEvent.change(input, { target: { value: '缓存' } })
    expect(screen.getByRole('tree', { name: '搜索结果' })).toBeDefined()
    expect(screen.queryAllByText('重建会话区')).toHaveLength(0)
    expect(screen.getAllByText('修复缓存').length).toBeGreaterThan(0)
    // The result row names the project the session lives in.
    expect(screen.getAllByText('harness').length).toBeGreaterThan(0)
  })

  it('keeps the keyword when the scope changes and narrows by that scope', () => {
    renderBrowser()
    const input = openSearch()
    fireEvent.change(input, { target: { value: 'mink' } })
    // "all" matches both the project name and the directory path.
    expect(screen.getAllByText('重建会话区').length).toBeGreaterThan(0)

    fireEvent.click(screen.getByRole('tab', { name: '项目' }))
    expect((screen.getByLabelText('搜索') as HTMLInputElement).value).toBe('mink')
    expect(screen.getAllByText('重建会话区').length).toBeGreaterThan(0)

    // A project-name scope never matches a session title.
    fireEvent.change(input, { target: { value: '缓存' } })
    expect(screen.getByText('没有匹配的会话')).toBeDefined()
  })

  it('clears the query and leaves the search field on the clear control', () => {
    renderBrowser()
    const input = openSearch()
    fireEvent.change(input, { target: { value: 'harness' } })
    fireEvent.click(screen.getByRole('button', { name: '清空筛选' }))
    expect((screen.getByLabelText('搜索') as HTMLInputElement).value).toBe('')
    expect(screen.getByRole('tree', { name: '会话' })).toBeDefined()
  })

  it('reports a query that matches nothing rather than an empty list', () => {
    renderBrowser()
    const input = openSearch()
    fireEvent.change(input, { target: { value: '没有这种东西' } })
    expect(screen.getByText('没有匹配的会话')).toBeDefined()
  })
})

describe('row labels', () => {
  it('falls back to the first prompt when a session has no title', () => {
    // A session opened from a request has no title of its own; the source
    // labels such a row by that request instead of showing a raw id.
    const subject = session({ title: null, alias: null, first_prompt: '把这个会话的标题补上' })
    expect(sessionTitle(subject)).toBe('把这个会话的标题补上')
    expect(sessionTitle(session({ title: null, alias: null, first_prompt: 'x'.repeat(80) })))
      .toBe(`${'x'.repeat(60)}…`)
  })

  it('prefers a real title and keeps the id as the last resort', () => {
    expect(sessionTitle(session({ title: '标题', first_prompt: '提示' }))).toBe('标题')
    expect(sessionTitle(session({ title: null, alias: null, first_prompt: null }))).toBe('session-1')
  })
})

describe('row timestamps', () => {
  it('reads the store timestamp as an absolute time, not an age', () => {
    // The store reports the last modification in epoch seconds; treating it as
    // an age produced stamps like "56 年" for a session written moments ago.
    const now = 1_700_000_000_000
    const subject = session({ updated_at: '', modified_secs: now / 1000 - 120 })
    expect(rowTimeLabel(subject, now)).toBe('2 分钟')
  })

  it('prefers a recorded updated_at and falls back to now without either', () => {
    const now = 1_700_000_000_000
    expect(rowTimeLabel(session({ updated_at: new Date(now - 3 * 3_600_000).toISOString(), modified_secs: 0 }), now))
      .toBe('3 小时')
    expect(rowTimeLabel(session({ updated_at: '', modified_secs: null }), now)).toBe('刚刚')
  })
})
