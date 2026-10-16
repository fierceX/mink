import { beforeEach, describe, expect, it, vi } from 'vitest'
import {
  readAttachments,
  readPendingSubmission,
  sessionAttachmentsKey,
  sessionDraftKey,
  sessionPendingKey,
  useSessionDraft,
  writeAttachments,
  writePendingSubmission,
} from './sessionDraft.ts'
import { act, cleanup, render } from '@testing-library/react'
import { useState } from 'react'
import { afterEach } from 'vitest'

beforeEach(() => {
  localStorage.clear()
})

afterEach(cleanup)

const SESSION = { project_key: 'demo', id: 'session-1' }

describe('session drafts', () => {
  it('keys a draft by the session identity', () => {
    expect(sessionDraftKey(SESSION)).toBe('mink.draft:demo:session-1')
    expect(sessionAttachmentsKey(SESSION)).toBe('mink.attachments:demo:session-1')
    expect(sessionPendingKey(SESSION)).toBe('mink.pending:demo:session-1')
  })

  it('round-trips an unresolved submission and tolerates a corrupt record', () => {
    const key = sessionPendingKey(SESSION)
    expect(readPendingSubmission(key)).toBeUndefined()
    const input = { request_id: 'req-1', text: '只执行一次', target_turn_id: null, attachment_ids: ['att-1'] }
    writePendingSubmission(key, input)
    expect(readPendingSubmission(key)).toEqual(input)

    localStorage.setItem(key, 'not json')
    expect(readPendingSubmission(key)).toBeUndefined()
    localStorage.setItem(key, JSON.stringify({ request_id: '', text: 'x', target_turn_id: null, attachment_ids: [] }))
    expect(readPendingSubmission(key)).toBeUndefined()

    writePendingSubmission(key, undefined)
    expect(readPendingSubmission(key)).toBeUndefined()
  })

  it('stores what the reader types and reads it back', () => {
    const handle = { current: undefined as ReturnType<typeof useSessionDraft> | undefined }
    function Probe({ keyName }: { keyName: string }) {
      handle.current = useSessionDraft(keyName)
      return <textarea aria-label="probe" value={handle.current.text} onChange={() => {}} />
    }
    const { rerender } = render(<Probe keyName="mink.draft:demo:session-1" />)
    act(() => { handle.current!.write('草稿一') })
    expect(localStorage.getItem('mink.draft:demo:session-1')).toBe('草稿一')

    // A different session is a different draft: the hook reloads instead of
    // carrying the first one over.
    rerender(<Probe keyName="mink.draft:demo:session-2" />)
    expect(handle.current!.text).toBe('')
    act(() => { handle.current!.write('草稿二') })
    rerender(<Probe keyName="mink.draft:demo:session-1" />)
    expect(handle.current!.text).toBe('草稿一')
  })

  it('tolerates a browser without storage and ignores a corrupt list', () => {
    const getItem = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('denied') })
    const setItem = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('denied') })
    const handle = { current: undefined as ReturnType<typeof useSessionDraft> | undefined }
    function Probe() {
      handle.current = useSessionDraft('mink.draft:demo:session-1')
      return null
    }
    render(<Probe />)
    act(() => { handle.current!.write('仍然可用') })
    expect(handle.current!.text).toBe('仍然可用')
    getItem.mockRestore()
    setItem.mockRestore()

    localStorage.setItem('mink.attachments:demo:session-1', 'not json')
    expect(readAttachments('mink.attachments:demo:session-1')).toEqual([])
    writeAttachments('mink.attachments:demo:session-1', [{ id: 'a', name: 'a.png' }])
    expect(readAttachments('mink.attachments:demo:session-1')).toEqual([{ id: 'a', name: 'a.png' }])
  })

  it('hands a keyed child the destination draft in the render that switched', () => {
    // The audit's W1 counterexample: a child that copies the draft once at
    // mount (as the keyed composer does) must start from the draft of the
    // identity it was mounted for, never from the session the reader left.
    localStorage.setItem('mink.draft:demo:session-1', '草稿一')
    localStorage.setItem('mink.draft:demo:session-2', '草稿二')
    const mounted: string[] = []
    function Child({ initial }: { initial: string }) {
      const [text] = useState(initial)
      mounted.push(text)
      return <span>{text}</span>
    }
    function Parent({ session }: { session: string }) {
      const draft = useSessionDraft(`mink.draft:demo:${session}`)
      return <Child key={session} initial={draft.text} />
    }
    const { rerender } = render(<Parent session="session-1" />)
    expect(mounted.at(-1)).toBe('草稿一')
    rerender(<Parent session="session-2" />)
    expect(mounted.at(-1)).toBe('草稿二')
  })
})
