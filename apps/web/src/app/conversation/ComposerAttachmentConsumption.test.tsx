/**
 * A submission may only consume the attachment set it was created with.
 * Files chosen, pasted or staged after the submission started belong to the
 * next message and must survive an earlier acceptance.
 */

import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { SessionSummary } from '../../data/api.ts'
import type { ImageLimits } from '../../data/types.ts'
import { Composer } from './Composer.tsx'
import { useAttachments, type AttachmentsHandle } from './useAttachments.ts'
import { useStagedAttachments } from './useStagedAttachments.ts'

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

function image(name: string): File {
  return new File([new Uint8Array(16)], name, { type: 'image/png' })
}

interface Deferred<T> {
  promise: Promise<T>
  resolve: (value: T) => void
  reject: (reason: unknown) => void
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void
  let reject!: (reason: unknown) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

let uploadCount = 0

beforeEach(() => {
  localStorage.clear()
  uploadCount = 0
  vi.stubGlobal('URL', {
    createObjectURL: vi.fn(() => `blob:preview-${++uploadCount}`),
    revokeObjectURL: vi.fn(),
  })
  vi.stubGlobal('fetch', vi.fn(async (_url: string, init?: RequestInit) => {
    const file = init?.body as File | null
    const id = file === null ? 'att-null' : `att-${file.name.replace(/\W/g, '-')}`
    return new Response(JSON.stringify({
      code: 200,
      message: '',
      data: { id, mime: 'image/png', width: 10, height: 10, bytes: 16 },
    }), { status: 200, headers: { 'Content-Type': 'application/json' } })
  }))
})

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

function SessionComposer({
  handle,
  onSend,
}: {
  handle: { current: AttachmentsHandle | undefined }
  onSend: (text: string, ids: readonly string[]) => Promise<string | undefined>
}) {
  const attachments = useAttachments(SESSION, LIMITS)
  handle.current = attachments
  return (
    <Composer
      hero
      running={false}
      targetTurnId={null}
      disabled={false}
      attachments={attachments}
      canAttach
      model="deepseek-flash"
      onSend={onSend}
      onGuide={async () => undefined}
      onCommand={async () => ({ message: '', ok: true })}
      onStop={() => {}}
    />
  )
}

function ComposingComposer({
  handle,
  onSend,
}: {
  handle: { current: ReturnType<typeof useStagedAttachments> | undefined }
  onSend: (text: string, ids: readonly string[]) => Promise<string | undefined>
}) {
  const staged = useStagedAttachments()
  handle.current = staged
  return (
    <Composer
      hero
      running={false}
      targetTurnId={null}
      disabled={false}
      attachments={staged}
      canAttach
      model=""
      onSend={onSend}
      onGuide={async () => '会话尚未创建'}
      onCommand={async () => ({ message: '', ok: false })}
      onStop={() => {}}
      commandsEnabled={false}
      showModelSeat={false}
    />
  )
}

async function addImage(handle: () => AttachmentsHandle | undefined, name: string): Promise<void> {
  act(() => { handle()?.add([image(name)]) })
  await waitFor(() => {
    const entry = handle()?.entries.find(candidate => candidate.name === name)
    expect(entry?.status).toBe('ready')
  })
}

function submit(text: string): void {
  fireEvent.change(screen.getByLabelText('输入消息'), { target: { value: text } })
  fireEvent.click(screen.getByRole('button', { name: '发送' }))
}

describe('attachment consumption follows the submission snapshot', () => {
  it('keeps an image added while a text-only submission is pending', async () => {
    const call = deferred<string | undefined>()
    const handle = { current: undefined as AttachmentsHandle | undefined }
    render(<SessionComposer handle={handle} onSend={() => call.promise} />)

    submit('先修构建')
    await waitFor(() => { expect(handle.current?.entries).toHaveLength(0) })
    await addImage(() => handle.current, 'next.png')

    await act(async () => {
      call.resolve(undefined)
      await call.promise
    })
    await waitFor(() => { expect(handle.current?.ids).toEqual(['att-next-png']) })
  })

  it('removes only the submitted attachment when a newer one arrives', async () => {
    const call = deferred<string | undefined>()
    const handle = { current: undefined as AttachmentsHandle | undefined }
    render(<SessionComposer handle={handle} onSend={() => call.promise} />)

    await addImage(() => handle.current, 'first.png')
    expect(handle.current?.ids).toEqual(['att-first-png'])
    submit('带图发送')
    await waitFor(() => { expect(handle.current?.entries).toHaveLength(1) })

    await addImage(() => handle.current, 'second.png')
    expect(handle.current?.ids).toEqual(['att-first-png', 'att-second-png'])

    await act(async () => {
      call.resolve(undefined)
      await call.promise
    })
    await waitFor(() => { expect(handle.current?.ids).toEqual(['att-second-png']) })
  })

  it('keeps every attachment when the submission is refused', async () => {
    const call = deferred<string | undefined>()
    const handle = { current: undefined as AttachmentsHandle | undefined }
    render(<SessionComposer handle={handle} onSend={() => call.promise} />)

    await addImage(() => handle.current, 'first.png')
    submit('带图发送')
    await addImage(() => handle.current, 'second.png')
    await act(async () => {
      call.resolve('提交失败：场景拒绝')
      await call.promise
    })
    await waitFor(() => { expect(handle.current?.ids).toEqual(['att-first-png', 'att-second-png']) })
  })

  it('does not revive an attachment the reader removed meanwhile', async () => {
    const call = deferred<string | undefined>()
    const handle = { current: undefined as AttachmentsHandle | undefined }
    render(<SessionComposer handle={handle} onSend={() => call.promise} />)

    await addImage(() => handle.current, 'first.png')
    submit('带图发送')
    const key = handle.current?.entries[0]?.key
    expect(key).toBeDefined()
    act(() => { handle.current?.remove(key!) })
    await act(async () => {
      call.resolve(undefined)
      await call.promise
    })
    expect(handle.current?.entries).toHaveLength(0)
  })

  it('consumes the staged files captured by the composing first send', async () => {
    const call = deferred<string | undefined>()
    const handle = { current: undefined as ReturnType<typeof useStagedAttachments> | undefined }
    render(<ComposingComposer handle={handle} onSend={() => call.promise} />)

    act(() => { handle.current?.add([image('staged.png')]) })
    expect(handle.current?.entries).toHaveLength(1)
    submit('首条消息')
    await waitFor(() => { expect(handle.current?.entries).toHaveLength(1) })
    act(() => { handle.current?.add([image('later.png')]) })
    expect(handle.current?.entries).toHaveLength(2)

    await act(async () => {
      call.resolve(undefined)
      await call.promise
    })
    await waitFor(() => {
      expect(handle.current?.entries.map(entry => entry.name)).toEqual(['later.png'])
    })
  })
})
