import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import type { SessionSummary } from '../../data/api.ts'
import type { SessionConnection } from '../../data/useSessionView.ts'
import { Conversation } from './Conversation.tsx'

afterEach(cleanup)

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
})
