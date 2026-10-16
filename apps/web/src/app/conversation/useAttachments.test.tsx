import { act, cleanup, render, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { SessionSummary } from '../../data/api.ts'
import type { ImageLimits } from '../../data/types.ts'
import { useAttachments } from './useAttachments.ts'
import { ComposerAttachments } from './ComposerAttachments.tsx'

beforeEach(() => {
  // Draft attachments are persisted per session, so one test must not inherit
  // the previous test's rail.
  localStorage.clear()
})

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
  created_at: '',
  updated_at: '',
  modified_secs: null,
  status: 'free',
  path: '/tmp/demo/session-1',
}

const LIMITS: ImageLimits = {
  kind: 'supported',
  allowed_mime: ['png', 'jpeg', 'webp'],
  max_image_bytes: 4 * 1024 * 1024,
  max_image_bytes_per_request: 8 * 1024 * 1024,
  max_images_per_request: 8,
}

function image(name = 'shot.png', type = 'image/png', bytes = 1024): File {
  return new File([new Uint8Array(bytes)], name, { type })
}

/** Render the hook behind a probe that exposes its handle. */
function renderAttachments(limits: ImageLimits | null) {
  const handle = { current: undefined as ReturnType<typeof useAttachments> | undefined }
  function Probe() {
    handle.current = useAttachments(SESSION, limits)
    return <ComposerAttachments entries={handle.current.entries} onRemove={handle.current.remove} />
  }
  render(<Probe />)
  return () => handle.current!
}

function stubUpload(attachmentId = 'att-1') {
  const fetchMock = vi.fn(async () => new Response(JSON.stringify({
    code: 200,
    message: '',
    data: { id: attachmentId, mime: 'image/png', width: 10, height: 10, bytes: 1024 },
  }), { status: 200, headers: { 'Content-Type': 'application/json' } }))
  vi.stubGlobal('fetch', fetchMock)
  return fetchMock
}

describe('draft attachments', () => {
  it('uploads a chosen image and exposes its id for the input', async () => {
    const fetchMock = stubUpload('att-9')
    const handle = renderAttachments(LIMITS)
    act(() => { handle().add([image()]) })
    await waitFor(() => {
      expect(handle().entries[0]?.status).toBe('ready')
    })
    expect(handle().ids).toEqual(['att-9'])
    expect(handle().blocked).toBe(false)
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toContain('/api/sessions/session-1/attachments')
    expect(init.method).toBe('POST')
  })

  it('refuses files when the session has no image capability', () => {
    // A session without a snapshot refuses instead of uploading blindly.
    const probe = renderAttachmentsProbe()
    act(() => { probe().add([image()]) })
    expect(probe().entries).toHaveLength(0)
    expect(probe().notice).toBe('此会话不支持图片输入')
  })

  it('restores the session’s stored attachments and keeps the list in step', async () => {
    stubUpload('att-7')
    localStorage.setItem('mink.attachments:demo:session-1', JSON.stringify([{ id: 'att-7', name: 'kept.png' }]))
    const handle = renderAttachments(LIMITS)
    expect(handle().entries.map(entry => entry.name)).toEqual(['kept.png'])
    expect(handle().ids).toEqual(['att-7'])
    expect(handle().blocked).toBe(false)

    act(() => { handle().remove('stored-att-7') })
    expect(JSON.parse(localStorage.getItem('mink.attachments:demo:session-1') ?? '[]')).toEqual([])
  })

  it('refuses an unsupported format and an oversized image', () => {
    stubUpload()
    const handle = renderAttachments(LIMITS)
    act(() => { handle().add([image('movie.mp4', 'video/mp4')]) })
    expect(handle().notice).toBe('不支持的图片格式：video/mp4')
    act(() => { handle().add([image('huge.png', 'image/png', 5 * 1024 * 1024)]) })
    expect(handle().notice).toBe('图片超过会话字节上限')
    expect(handle().entries).toHaveLength(0)
  })

  it('caps the count at the session limit and lets a failed upload be removed', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => {
      throw new Error('upload refused')
    }))
    const handle = renderAttachments({ ...LIMITS, max_images_per_request: 1 })
    act(() => { handle().add([image('a.png')]) })
    await waitFor(() => {
      expect(handle().entries[0]?.status).toBe('failed')
    })
    // A refused upload blocks submission until the reader removes it.
    expect(handle().blocked).toBe(true)
    act(() => { handle().add([image('b.png')]) })
    expect(handle().notice).toBe('最多选择 1 张图片')
    act(() => { handle().remove(handle().entries[0]!.key) })
    expect(handle().entries).toHaveLength(0)
    expect(handle().blocked).toBe(false)
  })

  it('leaves uploading when the server refuses the file, and says why', async () => {
    // The refusal arrives as a non-200 code inside a 2xx envelope, which used
    // to be thrown from the success branch and stranded the entry in
    // `uploading` forever — blocking every later submission.
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({
      code: 413,
      message: '图片过大',
      data: null,
    }), { status: 413, headers: { 'Content-Type': 'application/json' } })))
    const handle = renderAttachments(LIMITS)
    act(() => { handle().add([image('big.png')]) })
    await waitFor(() => {
      expect(handle().entries[0]?.status).toBe('failed')
    })
    expect(handle().entries[0]?.error).toBe('图片过大')
    expect(handle().blocked).toBe(true)
  })
})

/** Second probe for the capability-less case, kept separate for a clean render. */
function renderAttachmentsProbe() {
  const handle = { current: undefined as ReturnType<typeof useAttachments> | undefined }
  function Probe() {
    handle.current = useAttachments(SESSION, null)
    return null
  }
  render(<Probe />)
  return () => handle.current!
}
