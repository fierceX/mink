import { act, cleanup, renderHook, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, type ApiResponse, type SessionLiveState, type SessionSummary } from './api.ts'
import { isVerifierAvailable, setVerifierAvailable } from './commands.ts'
import type { HumanInput, InputReceipt } from './types.ts'
import { receiptStampHolds, useSessionView } from './useSessionView.ts'

/**
 * Regressions from the workbench audit (W2, W3, W4): the view keeps one
 * committed message collection, a lost reply keeps its request identity, and a
 * completion that arrives after a session switch is dropped.
 */

// The stream is the only SSE surface the view owns; the tests drive its events.
const streams = vi.hoisted(() => [] as ((event: Record<string, unknown>) => void)[])
const disconnects = vi.hoisted(() => [] as ((reason: string) => void)[])
const opens = vi.hoisted(() => [] as (() => void)[])
vi.mock('./sse.ts', () => ({
  SseClient: class {
    constructor(
      _id: string,
      onChange: (event: Record<string, unknown>) => void,
      onDisconnect: (reason: string) => void,
      _project?: string,
      onOpen?: () => void,
    ) {
      streams.push(onChange)
      disconnects.push(onDisconnect)
      opens.push(onOpen ?? (() => {}))
    }
    connect(): void {}
    close(): void {}
  },
}))

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
  streams.length = 0
  disconnects.length = 0
  opens.length = 0
  // The unresolved-submission record is stored with the draft: one test's
  // record must not answer for the next test's session.
  localStorage.clear()
})

function session(id: string): SessionSummary {
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
  }
}

/**
 * One summary object per identity, held across renders: the view keys its
 * effect on the session, so a fresh object per render would loop.
 */
const SESSIONS = { A: session('A'), B: session('B') } as const

function ok<T>(data: T): ApiResponse<T> {
  return { code: 200, message: '', data }
}

/** The open endpoint's answer; the view only reads the `snapshot` flag. */
function live(id: string): SessionLiveState & { snapshot: boolean } {
  return { id, open: true, running: false, snapshot: true }
}

/** A receipt as the queue reports it, for a request the test submitted. */
function receiptFor(input: HumanInput, status: InputReceipt['status'] = 'pending'): InputReceipt {
  return {
    input_id: input.request_id,
    revision: 1,
    turn_id: input.target_turn_id ?? '',
    status,
    guidance: input.target_turn_id !== null,
    input,
  }
}

function snapshot(id: string, conversation: Record<string, unknown>[] = []): Record<string, unknown> {
  return {
    type: 'session_snapshot',
    generation: id,
    stream_sequence: 1,
    conversation,
    inputs: [],
    running: false,
    phase: 'idle',
    progress: [],
  }
}

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (cause: unknown) => void
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise
    reject = rejectPromise
  })
  return { promise, resolve, reject }
}

describe('session view identity and history', () => {
  it('keeps messages committed after the snapshot when an older page loads', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    vi.spyOn(api, 'conversation').mockResolvedValue(ok([{ seq: 1, role: 'user', content: '更早的轮次' }]))
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A', [{ seq: 10, role: 'user', content: '最近一轮' }])) })
    act(() => {
      streams[0]({
        type: 'conversation_committed',
        generation: 'A',
        stream_sequence: 2,
        conversation_seq: 11,
        message: { role: 'assistant', content: '快照之后的新消息' },
      })
    })
    expect(result.current.state?.messages?.some(message => message.seq === 11)).toBe(true)

    // The page is older than the snapshot: it prepends, and the live message
    // stays in the projection instead of being rebuilt away.
    await act(async () => { await result.current.loadOlder() })
    expect(result.current.state?.messages?.some(message => message.seq === 11)).toBe(true)
    expect(result.current.state?.messages?.some(message => message.seq === 1)).toBe(true)
  })

  it('does not clear another session’s running state when a send fails late', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const pending = deferred<ApiResponse<InputReceipt>>()
    vi.spyOn(api, 'submitInput').mockReturnValue(pending.promise)
    const { result, rerender } = renderHook(({ selected }) => useSessionView(selected), {
      initialProps: { selected: SESSIONS.A },
    })
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })
    let sending!: Promise<string | undefined>
    act(() => { sending = result.current.send('hello') })
    rerender({ selected: SESSIONS.B })
    await waitFor(() => { expect(streams.length).toBe(2) })
    act(() => { streams[1]({ ...snapshot('B'), running: true }) })

    await act(async () => {
      pending.reject(new Error('A 的请求断开'))
      await sending
    })
    // B is still running: the failure belongs to the session that sent it, and
    // its error must not surface under B either.
    expect(result.current.state?.running).toBe(true)
    expect(result.current.error).toBeUndefined()
  })

  it('reuses the first request id when a lost reply is retried with the same content', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const submit = vi.spyOn(api, 'submitInput')
      .mockRejectedValueOnce(new Error('回复丢失'))
      .mockImplementationOnce(async (_id, input) => ok(receiptFor(input)))
    const lookup = vi.spyOn(api, 'inputs').mockResolvedValue(ok([]))
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })

    await act(async () => { await result.current.send('只执行一次') })
    await act(async () => { await result.current.send('只执行一次') })

    // Either the same id is resubmitted, or the queue was asked whether it
    // already held the first one; never a second id for the same words.
    const first = submit.mock.calls[0][1].request_id
    const second = submit.mock.calls[1][1].request_id
    expect(second === first || lookup.mock.calls.length > 0).toBe(true)
  })

  it('keeps an unresolved submission when the reader leaves and comes back', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const submit = vi.spyOn(api, 'submitInput')
      .mockRejectedValueOnce(new Error('回复丢失'))
      .mockImplementationOnce(async (_id, input) => ok(receiptFor(input)))
    vi.spyOn(api, 'inputs').mockResolvedValue(ok([]))
    const { result, rerender } = renderHook(({ selected }) => useSessionView(selected), {
      initialProps: { selected: SESSIONS.A },
    })
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })
    await act(async () => { await result.current.send('只执行一次') })
    const firstId = submit.mock.calls[0][1].request_id

    // A visit to another session, then back: the request identity belongs to A
    // and must still be there, exactly like A's draft.
    rerender({ selected: SESSIONS.B })
    await waitFor(() => { expect(streams.length).toBe(2) })
    rerender({ selected: SESSIONS.A })
    await waitFor(() => { expect(streams.length).toBe(3) })
    act(() => { streams[2](snapshot('A')) })
    await act(async () => { await result.current.send('只执行一次') })
    expect(submit.mock.calls[1][1].request_id).toBe(firstId)
  })

  it('keeps the unresolved submission across a reload', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const submit = vi.spyOn(api, 'submitInput')
      .mockRejectedValueOnce(new Error('回复丢失'))
      .mockImplementationOnce(async (_id, input) => ok(receiptFor(input)))
    vi.spyOn(api, 'inputs').mockResolvedValue(ok([]))
    const first = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })
    await act(async () => { await first.result.current.send('只执行一次') })
    const firstId = submit.mock.calls[0][1].request_id

    // The draft survives a reload, so the submission it belongs to must too.
    first.unmount()
    const second = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(2) })
    act(() => { streams[1](snapshot('A')) })
    await act(async () => { await second.result.current.send('只执行一次') })
    expect(submit.mock.calls[1][1].request_id).toBe(firstId)
  })

  it('does not roll back a newer runtime generation when an old send fails', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const pending = deferred<ApiResponse<InputReceipt>>()
    vi.spyOn(api, 'submitInput').mockReturnValue(pending.promise)
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('runtime-old')) })
    let sending!: Promise<string | undefined>
    act(() => { sending = result.current.send('hello') })

    // The runtime was replaced and is running a turn now; the answer to the old
    // request belongs to the runtime that is gone.
    act(() => { streams[0]({ ...snapshot('runtime-new'), running: true, current_turn: 'new-turn' }) })
    await act(async () => {
      pending.reject(new Error('旧请求失败'))
      expect(await sending).toBeDefined()
    })
    expect(result.current.state?.running).toBe(true)
    expect(result.current.error).toBeUndefined()
  })

  it('treats guidance the server already holds as accepted', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    let requested = ''
    vi.spyOn(api, 'submitInput').mockImplementationOnce(async (_id, input) => {
      requested = input.request_id
      throw new Error('回复丢失')
    })
    const stored = (): InputReceipt => receiptFor({
      request_id: requested,
      text: '继续',
      target_turn_id: 'turn-1',
      attachment_ids: [],
    })
    vi.spyOn(api, 'inputs').mockImplementation(async (_id, _project, requestId) =>
      ok(requestId === requested ? [stored()] : []))
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })

    let outcome: string | undefined = 'unset'
    await act(async () => { outcome = await result.current.guide('继续', 'turn-1') })
    // A receipt in the queue is a delivered guidance, not a failure: the
    // composer must clear its draft rather than offer the same words again.
    expect(outcome).toBeUndefined()
  })

  it('drops a history page that answers after the view was reopened', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const page = deferred<ApiResponse<Record<string, unknown>[]>>()
    vi.spyOn(api, 'conversation').mockReturnValue(page.promise)
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A', [{ seq: 10, role: 'user', content: '最近一轮' }])) })

    let loading!: Promise<boolean>
    act(() => { loading = result.current.loadOlder() })
    // Reopening the session replaces the projection; the page in flight belongs
    // to the closed generation and must not land on the reset window.
    act(() => { result.current.retry() })
    await waitFor(() => { expect(streams.length).toBe(2) })
    act(() => { streams[1](snapshot('A', [{ seq: 10, role: 'user', content: '最近一轮' }])) })
    await act(async () => {
      page.resolve(ok([{ seq: 1, role: 'user', content: '更早的轮次' }]))
      expect(await loading).toBe(false)
    })
    expect(result.current.state?.messages?.some(message => message.seq === 1)).toBe(false)
  })
})

describe('reconnect', () => {
  it('keeps connecting until the snapshot, not the transport open', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { opens[0]!() })
    expect(result.current.connection).toBe('connecting')
    act(() => { streams[0](snapshot('A')) })
    expect(result.current.connection).toBe('connected')
  })

  it('reopens a closed runtime and rebuilds from the new snapshot', async () => {
    const open = vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })
    expect(result.current.connection).toBe('connected')

    act(() => { disconnects[0]!('session_closed') })
    await waitFor(() => { expect(result.current.connection).toBe('closed') })
    expect(result.current.open).toBe(false)

    act(() => { result.current.reconnect() })
    await waitFor(() => { expect(streams.length).toBe(2) })
    expect(open).toHaveBeenCalledTimes(2)
    await waitFor(() => { expect(result.current.connection).toBe('connecting') })
    act(() => { streams[1](snapshot('A')) })
    expect(result.current.connection).toBe('connected')
    expect(result.current.open).toBe(true)
  })
})

describe('goal re-read after a command', () => {
  it('applies an authoritative goal read the projection has not superseded', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    vi.spyOn(api, 'goal').mockResolvedValue({
      code: 200,
      message: '',
      data: {
        goal: { id: 'g1', revision: 3, status: 'paused', phase: 'idle' },
        driven: true,
        verifier: true,
      },
    })
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })

    act(() => { result.current.refreshGoal() })
    await waitFor(() => { expect(result.current.state?.goal?.revision).toBe(3) })
    expect(isVerifierAvailable()).toBe(true)
    setVerifierAvailable(false)
  })

  it('drops a goal read that raced a newer stream event', async () => {
    vi.spyOn(api, 'openSession').mockResolvedValue(ok(live('A')))
    const pending = deferred<Awaited<ReturnType<typeof api.goal>>>()
    vi.spyOn(api, 'goal').mockReturnValue(pending.promise)
    const { result } = renderHook(() => useSessionView(SESSIONS.A))
    await waitFor(() => { expect(streams.length).toBe(1) })
    act(() => { streams[0](snapshot('A')) })

    act(() => { result.current.refreshGoal() })
    // The runtime publishes a newer goal before the read answers: the stream is
    // the newer truth and the stale answer must not roll it back.
    act(() => {
      streams[0]({
        type: 'goal_updated',
        generation: 'A',
        stream_sequence: 5,
        goal: { id: 'g1', revision: 9, status: 'active', phase: 'idle' },
      })
    })
    await act(async () => {
      pending.resolve({
        code: 200,
        message: '',
        data: {
          goal: { id: 'g1', revision: 2, status: 'paused', phase: 'idle' },
          driven: true,
        },
      })
    })
    expect(result.current.state?.goal?.revision).toBe(9)
  })
})

describe('receipt stamp guard', () => {
  it('drops a late receipt after a runtime or view epoch change', () => {
    const started = { identity: 'p:s', epoch: 3, runtime: 'gen-1' }
    expect(receiptStampHolds(started, { ...started })).toBe(true)
    expect(receiptStampHolds(started, { ...started, runtime: 'gen-2' })).toBe(false)
    expect(receiptStampHolds(started, { ...started, epoch: 4 })).toBe(false)
    expect(receiptStampHolds(started, { ...started, identity: 'p:other' })).toBe(false)
  })
})
