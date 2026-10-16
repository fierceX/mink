import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import type { SessionSummary } from '../../data/api.ts'
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
})
