import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { SseClient } from './sse'

/** Minimal EventSource double; the controller only needs the four callbacks. */
class FakeEventSource {
  static instances: FakeEventSource[] = []
  onopen: (() => void) | null = null
  onerror: (() => void) | null = null
  onmessage: ((event: { data: string }) => void) | null = null
  closed = false

  constructor(public url: string) {
    FakeEventSource.instances.push(this)
  }

  close() {
    this.closed = true
  }
}

describe('SseClient reconnection', () => {
  beforeEach(() => {
    FakeEventSource.instances = []
    vi.useFakeTimers()
    vi.stubGlobal('EventSource', FakeEventSource as unknown as typeof EventSource)
  })

  afterEach(() => {
    vi.useRealTimers()
    vi.unstubAllGlobals()
  })

  it('reports the gap and reconnects with backoff instead of staying closed', () => {
    const seen: string[] = []
    const client = new SseClient('s', event => seen.push(String(event.type)), (reason) => seen.push(reason))

    client.connect()
    expect(FakeEventSource.instances).toHaveLength(1)
    FakeEventSource.instances[0]!.onerror?.()
    expect(seen).toContain('transport')
    expect(FakeEventSource.instances).toHaveLength(1)

    // First retry waits the initial backoff (500ms), not a tight loop.
    vi.advanceTimersByTime(499)
    expect(FakeEventSource.instances).toHaveLength(1)
    vi.advanceTimersByTime(1)
    expect(FakeEventSource.instances).toHaveLength(2)

    // The reconnect's own failure doubles the delay (500 -> 1000).
    FakeEventSource.instances[1]!.onerror?.()
    vi.advanceTimersByTime(999)
    expect(FakeEventSource.instances).toHaveLength(2)
    vi.advanceTimersByTime(1)
    expect(FakeEventSource.instances).toHaveLength(3)
    client.close()
  })

  it('offline closes the stream and online reconnects', () => {
    const seen: string[] = []
    const client = new SseClient('s', event => seen.push(String(event.type)), (reason) => seen.push(reason))
    client.connect()
    const first = FakeEventSource.instances[0]!

    window.dispatchEvent(new Event('offline'))
    expect(seen).toContain('offline')
    expect(first.closed).toBe(true)

    window.dispatchEvent(new Event('online'))
    vi.advanceTimersByTime(500)
    expect(FakeEventSource.instances).toHaveLength(2)
    client.close()
  })

  it('a closed session stops the reconnect loop', () => {
    const seen: string[] = []
    const client = new SseClient('s', event => seen.push(String(event.type)), (reason) => seen.push(reason))
    client.connect()
    const first = FakeEventSource.instances[0]!
    first.onmessage?.({ data: JSON.stringify({ type: 'session_closed' }) })
    expect(seen).toContain('session_closed')

    vi.advanceTimersByTime(60_000)
    expect(FakeEventSource.instances).toHaveLength(1)
    client.close()
  })
})
