import { emptySession } from '../../data/types.ts'
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { SessionSummary } from '../../data/api.ts'
import type { SessionConnection } from '../../data/useSessionView.ts'
import { ChangedFiles } from './ChangedFiles.tsx'
import { Conversation } from './Conversation.tsx'

afterEach(() => { cleanup(); vi.unstubAllGlobals() })

/** The session the open failed for. */
const SESSION: SessionSummary = {
  project_key: 'demo',
  corrupt: false,
  id: 'session-1',
  alias: null,
  title: 'fixture session',
  cwd: '/tmp/demo',
  created_at: '2026-10-10T00:00:00Z',
  updated_at: '2026-10-10T00:00:00Z',
  modified_secs: 0,
  status: 'free',
  path: '/tmp/demo/session-1',
}

describe('open failure', () => {
  it('offers a retry beside the error', () => {
    let retried = 0
    render(
      <Conversation
        state={undefined}
        session={SESSION}
        title="fixture session"
        open={false}
        error="session is locked by another process"
        detailsOpen
        processDisplay="standard"
        notice={undefined}
        hasOlder={false}
        onLoadOlder={async () => false}
        onGoalAction={async () => undefined}
        onOpenFile={() => {}}
        onOpenArtifact={() => {}}
        onRetryOpen={() => { retried += 1 }}
        draft=""
        onDraftChange={() => {}}
        modelChoices={[]}
        onModelChange={() => {}}
        onSend={async () => undefined}
        onGuide={async () => undefined}
        onReceiptChanged={() => {}}
        onCommand={async () => ({ message: '', ok: true })}
        followTick={0}
        onToggleDetails={() => {}}
        onStop={() => {}}
      />,
    )

    // Another process may hold the session (the terminal, another workbench):
    // the transcript carries a card that says so and offers the retry, and its
    // wording names the reader's next move rather than the server's error code.
    const card = screen.getByText('无法打开此会话')
    expect(card).toBeDefined()
    expect(screen.getByText(/正被另一个进程占用/)).toBeDefined()
    fireEvent.click(screen.getByRole('button', { name: '重试' }))
    expect(retried).toBe(1)
  })

  it('offers a manual reconnect after the runtime closed', () => {
    let reconnected = 0
    render(
      <Conversation
        state={undefined}
        session={SESSION}
        title="fixture session"
        open={false}
        connection="closed"
        onReconnect={() => { reconnected += 1 }}
        error={undefined}
        detailsOpen
        processDisplay="standard"
        notice={undefined}
        hasOlder={false}
        onLoadOlder={async () => false}
        onGoalAction={async () => undefined}
        onOpenFile={() => {}}
        onOpenArtifact={() => {}}
        onRetryOpen={() => {}}
        draft=""
        onDraftChange={() => {}}
        modelChoices={[]}
        onModelChange={() => {}}
        onSend={async () => undefined}
        onGuide={async () => undefined}
        onReceiptChanged={() => {}}
        onCommand={async () => ({ message: '', ok: true })}
        followTick={0}
        onToggleDetails={() => {}}
        onStop={() => {}}
      />,
    )

    const reconnect = screen.getByRole('button', { name: '重新连接会话' })
    expect(reconnect.textContent).toContain('会话已关闭，重新打开')
    fireEvent.click(reconnect)
    expect(reconnected).toBe(1)
  })

  it('only celebrates a recovery after a real disconnect', () => {
    const element = (connection: SessionConnection) => (
      <Conversation
        state={undefined}
        session={SESSION}
        title="fixture session"
        open
        connection={connection}
        onReconnect={() => {}}
        error={undefined}
        detailsOpen
        processDisplay="standard"
        notice={undefined}
        hasOlder={false}
        onLoadOlder={async () => false}
        onGoalAction={async () => undefined}
        onOpenFile={() => {}}
        onOpenArtifact={() => {}}
        onRetryOpen={() => {}}
        draft=""
        onDraftChange={() => {}}
        modelChoices={[]}
        onModelChange={() => {}}
        onSend={async () => undefined}
        onGuide={async () => undefined}
        onReceiptChanged={() => {}}
        onCommand={async () => ({ message: '', ok: true })}
        followTick={0}
        onToggleDetails={() => {}}
        onStop={() => {}}
      />
    )
    const { rerender } = render(element('connecting'))
    rerender(element('connected'))
    // The first open starts at connecting and must not read as a recovery.
    expect(screen.queryByText('已恢复')).toBeNull()

    rerender(element('disconnected'))
    rerender(element('connected'))
    expect(screen.getByText('已恢复')).toBeDefined()
  })

  it('shows actionError as a dismissible banner without rendering OpenFailure', () => {
    let dismissed = 0
    render(
      <Conversation
        state={undefined}
        session={SESSION}
        title="fixture session"
        open
        connection="connected"
        error="send failed"
        actionError="send failed"
        onDismissActionError={() => { dismissed += 1 }}
        detailsOpen
        processDisplay="standard"
        notice={undefined}
        hasOlder={false}
        onLoadOlder={async () => false}
        onGoalAction={async () => undefined}
        onOpenFile={() => {}}
        onOpenArtifact={() => {}}
        onRetryOpen={() => {}}
        draft=""
        onDraftChange={() => {}}
        modelLabel="flash"
        modelChoices={[{ alias: 'flash', model: 'deepseek-chat' }]}
        onModelChange={() => {}}
        onSend={async () => undefined}
        onGuide={async () => undefined}
        onReceiptChanged={() => {}}
        onCommand={async () => ({ message: '', ok: true })}
        followTick={0}
        onToggleDetails={() => {}}
        onStop={() => {}}
      />,
    )
    expect(screen.queryByText('无法打开此会话')).toBeNull()
    const banner = document.querySelector('[data-action-error]')
    expect(banner?.textContent).toContain('send failed')
    expect(screen.getByRole('button', { name: /当前模型 flash/ })).toBeDefined()
    fireEvent.click(screen.getByRole('button', { name: '关闭' }))
    expect(dismissed).toBe(1)
  })

  it('ChangedFiles handles empty to non-empty transitions cleanly', () => {
    const { rerender } = render(<ChangedFiles files={[]} onOpen={() => {}} />)
    expect(document.querySelector('[data-changed-files]')).toBeNull()
    rerender(
      <ChangedFiles
        files={[{
          path: 'src/app.ts',
          added: 3,
          deleted: 1,
          diffs: [{ path: 'src/app.ts', oldText: 'a', newText: 'b' }],
        }]}
        onOpen={() => {}}
      />,
    )
    expect(screen.getByText('已编辑 src/app.ts')).toBeDefined()
  })
})

it('keeps the visible turn anchored when a live append precedes a history page', async () => {
  vi.stubGlobal('matchMedia', () => ({ matches: false, addEventListener() {}, removeEventListener() {} }))
  let finish!: (added: boolean) => void
  const page = new Promise<boolean>(resolve => { finish = resolve })
  const view = (keys: string[]) => (
    <Conversation state={{ ...emptySession(SESSION.id, 'title'),
      items: keys.map(key => ({ kind: 'user', text: key, key: `input:${key}` })) }}
        session={SESSION}
        title="fixture session"
        open
        error={undefined}
        detailsOpen
        processDisplay="standard"
        notice={undefined}
        hasOlder
        onLoadOlder={() => page}
        onGoalAction={async () => undefined}
        onOpenFile={() => {}}
        onOpenArtifact={() => {}}
        onRetryOpen={() => {}}
        draft=""
        onDraftChange={() => {}}
        modelChoices={[]}
        onModelChange={() => {}}
        onSend={async () => undefined}
        onGuide={async () => undefined}
        onReceiptChanged={() => {}}
        onCommand={async () => ({ message: '', ok: true })}
        followTick={0}
        onToggleDetails={() => {}}
        onStop={() => {}}
    />
  )
  const { rerender } = render(view(['current']))
  const box = document.querySelector<HTMLElement>('[data-conversation-scroll]')!
  const current = document.querySelector<HTMLElement>('[data-turn-key="input:current"]')!
  let height = 500
  let currentTop = 10
  Object.defineProperty(box, 'scrollHeight', { get: () => height })
  current.getBoundingClientRect = () => ({ top: currentTop, bottom: currentTop + 50 } as DOMRect)
  box.scrollTop = 100
  fireEvent.scroll(box)
  fireEvent.click(screen.getByRole('button', { name: '加载更早的轮次' }))
  height += 30
  rerender(view(['current', 'live']))
  expect(box.scrollTop).toBe(100)
  height += 60
  currentTop += 60
  await act(async () => {
    rerender(view(['older', 'current', 'live']))
    finish(true)
  })
  expect(box.scrollTop).toBe(160)
})
