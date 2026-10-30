import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { InputReceipt } from '../../data/types.ts'
import type { SessionSummary } from '../../data/api.ts'
import { Composer } from './Composer.tsx'
import { PendingInputs } from './PendingInputs.tsx'
import type { DraftAttachment } from './useAttachments.ts'

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
  status: 'running',
  path: '/tmp/demo/session-1',
}

/** Empty attachment handle; tests that exercise uploads build their own. */
function noAttachments(overrides: Partial<ComposerPropsAttachments> = {}) {
  const entries: DraftAttachment[] = overrides.entries ?? []
  return {
    entries,
    add: overrides.add ?? (() => {}),
    remove: overrides.remove ?? (() => {}),
    ids: overrides.ids ?? entries.flatMap(entry => (entry.attachment === undefined ? [] : [entry.attachment.id])),
    blocked: overrides.blocked ?? false,
    notice: overrides.notice,
  }
}

type ComposerPropsAttachments = {
  entries?: DraftAttachment[]
  add?: (files: readonly File[]) => void
  remove?: (key: string) => void
  ids?: string[]
  blocked?: boolean
  notice?: string | undefined
}

function receipt(overrides: Partial<InputReceipt> = {}): InputReceipt {
  return {
    input_id: 'input-1',
    revision: 1,
    turn_id: 'turn-1',
    status: 'pending',
    guidance: true,
    input: { request_id: 'req-1', text: '补充：先看侧栏', target_turn_id: 'turn-1', attachment_ids: [] },
    ...overrides,
  }
}

describe('composer', () => {
  it('sends a fresh turn when nothing is running', async () => {
    const onSend = vi.fn(async () => undefined)
    const onGuide = vi.fn(async () => undefined)
    render(
      <Composer
        hero
        running={false}
        targetTurnId={null}
        disabled={false}
        attachments={noAttachments()}
        canAttach
        model="deepseek-flash"
        onSend={onSend}
        onGuide={onGuide}
        onCommand={async () => ({ message: '', ok: true })}
        onStop={() => {}}
      />,
    )
    fireEvent.change(screen.getByLabelText('输入消息'), { target: { value: '重建会话区' } })
    fireEvent.click(screen.getByRole('button', { name: '发送' }))
    await waitFor(() => {
      expect(onSend).toHaveBeenCalledWith('重建会话区', [], [])
    })
    expect(onGuide).not.toHaveBeenCalled()
  })

  it('submits guidance bound to the running turn instead of a new turn', async () => {
    const onSend = vi.fn(async () => undefined)
    const onGuide = vi.fn(async () => undefined)
    render(
      <Composer
        hero={false}
        running
        targetTurnId="turn-7"
        disabled={false}
        attachments={noAttachments()}
        canAttach
        model="deepseek-flash"
        onSend={onSend}
        onGuide={onGuide}
        onCommand={async () => ({ message: '', ok: true })}
        onStop={() => {}}
      />,
    )
    fireEvent.click(screen.getByRole('button', { name: '停止' }))
    fireEvent.change(screen.getByLabelText('输入消息'), { target: { value: '补充要求' } })
    fireEvent.click(screen.getByRole('button', { name: '提交引导' }))
    await waitFor(() => {
      expect(onGuide).toHaveBeenCalledWith('补充要求', 'turn-7', [], [])
    })
    expect(onSend).not.toHaveBeenCalled()
  })

  it('refuses guidance before the turn id arrives and keeps the draft', () => {
    const onGuide = vi.fn(async () => undefined)
    render(
      <Composer
        hero={false}
        running
        targetTurnId={null}
        disabled={false}
        attachments={noAttachments()}
        canAttach
        model="deepseek-flash"
        onSend={async () => undefined}
        onGuide={onGuide}
        onCommand={async () => ({ message: '', ok: true })}
        onStop={() => {}}
      />,
    )
    fireEvent.change(screen.getByLabelText('输入消息'), { target: { value: '补充要求' } })
    fireEvent.click(screen.getByRole('button', { name: '提交引导' }))
    expect(onGuide).not.toHaveBeenCalled()
    expect(screen.getByText('当前会话尚未取得引导能力，请等待同步')).toBeDefined()
    expect((screen.getByLabelText('输入消息') as HTMLTextAreaElement).value).toBe('补充要求')
  })

  it('restores a failed draft only while the reader has typed nothing new', async () => {
    const onSend = vi.fn(async () => '提交失败：network down')
    render(
      <Composer
        hero
        running={false}
        targetTurnId={null}
        disabled={false}
        attachments={noAttachments()}
        canAttach
        model="deepseek-flash"
        onSend={onSend}
        onGuide={async () => undefined}
        onCommand={async () => ({ message: '', ok: true })}
        onStop={() => {}}
      />,
    )
    const field = screen.getByLabelText('输入消息') as HTMLTextAreaElement
    fireEvent.change(field, { target: { value: '会被恢复' } })
    fireEvent.click(screen.getByRole('button', { name: '发送' }))
    await waitFor(() => {
      expect(field.value).toBe('会被恢复')
    })
    expect(screen.getByText('提交失败：network down')).toBeDefined()

    // A draft written after the failure is never overwritten by a late restore.
    fireEvent.change(field, { target: { value: '新草稿' } })
    fireEvent.click(screen.getByRole('button', { name: '发送' }))
    await waitFor(() => {
      expect(onSend).toHaveBeenCalledTimes(2)
    })
  })
})

describe('command mode', () => {
  function renderComposer(onCommand: (text: string) => Promise<{ message: string; ok: boolean }>) {
    render(
      <Composer
        hero
        running={false}
        targetTurnId={null}
        disabled={false}
        attachments={noAttachments()}
        canAttach
        model="deepseek-flash"
        onSend={async () => undefined}
        onGuide={async () => undefined}
        onCommand={onCommand}
        onStop={() => {}}
      />,
    )
    return screen.getByLabelText('输入消息') as HTMLTextAreaElement
  }

  it('narrows the list from the draft itself, with no second search box', async () => {
    renderComposer(async () => ({ message: '', ok: true }))
    const field = screen.getByLabelText('输入消息')
    expect(screen.queryByRole('listbox')).toBeNull()

    // Typing the slash opens the copied card; its rows follow the draft.
    fireEvent.change(field, { target: { value: '/deta' } })
    expect(await screen.findByRole('option', { name: /\/details/ })).toBeDefined()
    // The surface has no usable search box of its own: the draft is the query
    // (the copied card's own input is hidden by the shell's stylesheet, which the
    // browser lane asserts — jsdom applies no CSS).
    expect((field as HTMLTextAreaElement).value).toBe('/deta')

    // Deleting in the draft widens the list again — it never reappears on its own
    // from a box that was emptied.
    fireEvent.change(field, { target: { value: '/go' } })
    expect((await screen.findAllByRole('option', { name: /^\/goal\b/ })).length).toBeGreaterThan(0)
    expect(screen.queryByRole('option', { name: /\/details/ })).toBeNull()
    expect((field as HTMLTextAreaElement).value).toBe('/go')
  })

  it('runs the typed command on Enter without sending a turn', async () => {
    const onCommand = vi.fn(async () => ({ message: '已打开计划', ok: true }))
    const field = renderComposer(onCommand)
    fireEvent.change(field, { target: { value: '/plan' } })
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => {
      expect(onCommand).toHaveBeenCalledWith('/plan')
    })
    // A command is not a message: the draft clears and no turn was queued.
    await waitFor(() => {
      expect(field.value).toBe('')
    })
  })

  it('keeps a refused command in the draft with its message', async () => {
    const onCommand = vi.fn(async () => ({ message: '用法：/goal budget 轮次 分钟 [软Token]', ok: false }))
    const field = renderComposer(onCommand)
    fireEvent.change(field, { target: { value: '/goal budget 3' } })
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => {
      expect(field.value).toBe('/goal budget 3')
    })
    expect(screen.getByText(/用法：\/goal budget/)).toBeDefined()
  })

  it('leaves the surface on Escape without running anything', () => {
    const onCommand = vi.fn(async () => ({ message: '', ok: true }))
    const field = renderComposer(onCommand)
    fireEvent.change(field, { target: { value: '/goal' } })
    // The shell owns its keys: Escape closes it and returns focus to the draft,
    // which keeps what the reader typed.
    fireEvent.keyDown(screen.getByLabelText('筛选命令'), { key: 'Escape' })
    expect(field.value).toBe('/goal')
    expect(onCommand).not.toHaveBeenCalled()
  })
})

describe('pending inputs', () => {
  it('offers edit and withdraw while the server has not started applying', () => {
    render(
      <PendingInputs receipts={[receipt()]} session={SESSION} onChanged={() => {}} />,
    )
    expect(screen.getByText('待应用')).toBeDefined()
    expect(screen.getByText('补充：先看侧栏')).toBeDefined()
    expect(screen.getByRole('button', { name: '编辑' })).toBeDefined()
    expect(screen.getByRole('button', { name: '撤回' })).toBeDefined()
  })

  it('shows only the resume verb for an unapplied input and none once applied', () => {
    const { rerender } = render(
      <PendingInputs
        receipts={[receipt({ status: 'unapplied' })]}
        session={SESSION}
        onChanged={() => {}}
      />,
    )
    expect(screen.getByText('未应用')).toBeDefined()
    expect(screen.getByRole('button', { name: '用于下一轮' })).toBeDefined()

    rerender(
      <PendingInputs receipts={[receipt({ status: 'applying' })]} session={SESSION} onChanged={() => {}} />,
    )
    expect(screen.getByText('应用中')).toBeDefined()
    expect(screen.queryByRole('button', { name: '撤回' })).toBeNull()
  })

  it('withdraws through the revision-checked call and reports the receipt back', async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({
      code: 200,
      message: '',
      data: receipt({ status: 'withdrawn', revision: 2 }),
    }), { status: 200, headers: { 'Content-Type': 'application/json' } }))
    vi.stubGlobal('fetch', fetchMock)
    const onChanged = vi.fn()
    render(<PendingInputs receipts={[receipt()]} session={SESSION} onChanged={onChanged} />)

    fireEvent.click(screen.getByRole('button', { name: '撤回' }))
    await waitFor(() => {
      expect(onChanged).toHaveBeenCalled()
    })
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toContain('/api/sessions/session-1/inputs/input-1')
    expect(init.method).toBe('DELETE')
    expect(JSON.parse(String(init.body))).toEqual({ revision: 1 })
  })
})

describe('a submission that fails', () => {
  it('returns the words with a reason instead of an emptied box', async () => {
    const onSend = vi.fn(() => Promise.reject(new Error('无法连接服务')))
    render(
      <Composer
        hero
        running={false}
        targetTurnId={null}
        disabled={false}
        attachments={noAttachments()}
        canAttach
        model="deepseek-flash"
        onSend={onSend}
        onGuide={async () => undefined}
        onCommand={async () => ({ ok: true, message: '' })}
        onStop={() => {}}
      />,
    )
    const field = screen.getByLabelText('输入消息') as HTMLTextAreaElement
    fireEvent.change(field, { target: { value: '会被拒绝的输入' } })
    fireEvent.click(screen.getByRole('button', { name: '发送' }))

    // The reason is announced where the reader can see it, and the draft is back.
    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toContain('无法连接服务')
    })
    expect(field.value).toBe('会被拒绝的输入')
  
})

  it('reports a failed submission as operation state until the draft is edited', async () => {
    let resolveSend!: (value: string | undefined) => void
    const onSend = vi.fn(() => new Promise<string | undefined>((resolve) => { resolveSend = resolve }))
    const operation = vi.fn()
    render(
      <Composer
        hero
        running={false}
        targetTurnId={null}
        disabled={false}
        attachments={noAttachments()}
        canAttach
        model="deepseek-flash"
        onSend={onSend}
        onGuide={async () => undefined}
        onCommand={async () => ({ ok: true, message: '' })}
        onOperationStateChange={operation}
        onStop={() => {}}
      />,
    )
    const field = screen.getByLabelText('输入消息') as HTMLTextAreaElement
    fireEvent.change(field, { target: { value: '发送失败场景' } })
    fireEvent.click(screen.getByRole('button', { name: '发送' }))
    await waitFor(() => { expect(operation).toHaveBeenLastCalledWith({ pending: true, failed: false }) })

    resolveSend('提交失败：场景拒绝')
    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toContain('提交失败')
      expect(operation).toHaveBeenLastCalledWith({ pending: false, failed: true })
    })

    // Editing dismisses the failure, so the next reading gesture may hide the chrome.
    fireEvent.change(field, { target: { value: '改过的草稿' } })
    await waitFor(() => {
      expect(screen.queryByRole('alert')).toBeNull()
      expect(operation).toHaveBeenLastCalledWith({ pending: false, failed: false })
    })
  })

  it('shows a successful command as a notice, never as a failure', async () => {
    const operation = vi.fn()
    render(
      <Composer
        hero
        running={false}
        targetTurnId={null}
        disabled={false}
        attachments={noAttachments()}
        canAttach
        model="deepseek-flash"
        onSend={async () => undefined}
        onGuide={async () => undefined}
        onCommand={async () => ({ ok: true, message: '已打开计划' })}
        onOperationStateChange={operation}
        onStop={() => {}}
      />,
    )
    const field = screen.getByLabelText('输入消息')
    fireEvent.change(field, { target: { value: '/plan' } })
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => {
      expect(document.querySelector('[data-composer-notice]')?.textContent).toBe('已打开计划')
    })
    expect(screen.queryByRole('alert')).toBeNull()
    expect(operation).toHaveBeenLastCalledWith({ pending: false, failed: false })
  })
})
