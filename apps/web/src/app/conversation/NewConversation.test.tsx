import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { NewConversation } from './NewConversation.tsx'

afterEach(() => {
  cleanup()
  localStorage.clear()
})

describe('new conversation', () => {
  it('stages images until the first send creates the session', async () => {
    const firstSend = vi.fn(async () => undefined)
    render(
      <NewConversation
        cwd="/tmp/project"
        projectName="project"
        onFirstSend={firstSend}
        onCancel={() => {}}
      />,
    )

    const file = new File(['x'], 'shot.png', { type: 'image/png' })
    fireEvent.change(screen.getByLabelText('选择图片'), { target: { files: [file] } })
    fireEvent.change(screen.getByLabelText('输入消息'), { target: { value: '看看这张图' } })
    fireEvent.click(screen.getByRole('button', { name: '发送' }))

    await waitFor(() => { expect(firstSend).toHaveBeenCalledTimes(1) })
    const [text, files, draftId, requestId] = firstSend.mock.calls[0] as unknown as [string, File[], string, string]
    expect(text).toBe('看看这张图')
    expect(files).toHaveLength(1)
    expect(files[0]!.name).toBe('shot.png')
    expect(draftId).toMatch(/^draft-/)
    expect(requestId).toMatch(/[0-9a-f-]{36}/)
  })

  it('reuses the first-send request id after a remount', async () => {
    const first = render(
      <NewConversation cwd="/tmp/project" projectName="project" onFirstSend={async () => undefined} onCancel={() => {}} />,
    )
    fireEvent.change(screen.getByLabelText('输入消息'), { target: { value: '同一次提交' } })
    const stored = JSON.parse(localStorage.getItem('mink.compose:/tmp/project') ?? '{}') as { requestId?: string }
    expect(stored.requestId).toMatch(/[0-9a-f-]{36}/)
    first.unmount()

    const send = vi.fn(async (
      _text: string,
      _files: readonly File[],
      _draftId: string,
      _requestId: string,
    ) => undefined)
    render(<NewConversation cwd="/tmp/project" projectName="project" onFirstSend={send} onCancel={() => {}} />)
    fireEvent.click(screen.getByRole('button', { name: '发送' }))
    await waitFor(() => { expect(send).toHaveBeenCalledTimes(1) })
    expect(send.mock.calls[0]![3]).toBe(stored.requestId)
  })

  it('does not allow cancel while the first send is in flight', async () => {
    let finish!: () => void
    const pending = new Promise<void>((resolve) => { finish = resolve })
    render(
      <NewConversation
        cwd="/tmp/project"
        projectName="project"
        onFirstSend={async () => { await pending; return undefined }}
        onCancel={() => {}}
      />,
    )
    fireEvent.change(screen.getByLabelText('输入消息'), { target: { value: '提交中' } })
    fireEvent.click(screen.getByRole('button', { name: '发送' }))
    const cancel = screen.getByRole('button', { name: '取消' }) as HTMLButtonElement
    expect(cancel.disabled).toBe(true)
    finish()
    // Let the send settle before unmounting.
    await act(async () => { await pending })
  })

  it('persists the draft id and text across a remount', () => {
    const first = render(
      <NewConversation cwd="/tmp/project" projectName="project" onFirstSend={async () => undefined} onCancel={() => {}} />,
    )
    fireEvent.change(screen.getByLabelText('输入消息'), { target: { value: '保留的文字' } })
    const stored = JSON.parse(localStorage.getItem('mink.compose:/tmp/project') ?? '{}') as { draftId?: string; text?: string }
    expect(stored.text).toBe('保留的文字')
    expect(stored.draftId).toMatch(/^draft-/)
    first.unmount()

    render(
      <NewConversation cwd="/tmp/project" projectName="project" onFirstSend={async () => undefined} onCancel={() => {}} />,
    )
    expect((screen.getByLabelText('输入消息') as HTMLTextAreaElement).value).toBe('保留的文字')
  })
})
