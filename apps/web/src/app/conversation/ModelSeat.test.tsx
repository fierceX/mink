import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Composer } from './Composer.tsx'

afterEach(cleanup)

/** The composer with the minimal stubs the seat needs. */
function renderComposer(overrides: Partial<Parameters<typeof Composer>[0]> = {}) {
  const onModelChange = vi.fn()
  render(
    <Composer
      hero={false}
      running={false}
      targetTurnId={null}
      disabled={false}
      attachments={{ entries: [], add: () => {}, remove: () => {}, clear: () => {}, ids: [], blocked: false, notice: undefined }}
      canAttach
      model="flash"
      modelChoices={[{ alias: 'flash', model: 'deepseek-flash' }, { alias: 'pro', model: 'deepseek-v4-pro' }]}
      onModelChange={onModelChange}
      onSend={async () => undefined}
      onGuide={async () => undefined}
      onCommand={async () => ({ message: '', ok: true })}
      onStop={() => {}}
      {...overrides}
    />,
  )
  return onModelChange
}

describe('model seat', () => {
  it('lists the aliases the runtime honours and switches on selection', () => {
    const onModelChange = renderComposer()
    const seat = screen.getByRole('button', { name: '当前模型 flash' })
    expect(seat.hasAttribute('disabled')).toBe(false)
    fireEvent.click(seat)
    // Alias first, the model it resolves to alongside.
    fireEvent.click(screen.getByRole('menuitem', { name: /pro/ }))
    expect(onModelChange).toHaveBeenCalledWith('pro')
  })

  it('stays inert while the session is unavailable', () => {
    renderComposer({ disabled: true })
    expect(screen.getByRole('button', { name: '当前模型 flash' }).hasAttribute('disabled')).toBe(true)
  })

  it('asks for the vocabulary instead of looking dead when the list has not arrived', async () => {
    // A phone that lost the model request has an open session and an empty list;
    // the seat must still open, say what happened and offer the retry.
    renderComposer({ modelChoices: [], modelError: '网络错误' })
    const seat = screen.getByRole('button', { name: '当前模型 flash' })
    expect(seat.hasAttribute('disabled')).toBe(false)
    fireEvent.click(seat)
    expect(await screen.findByText(/读取模型失败：网络错误/)).toBeDefined()
  })
})

describe('command panel', () => {
  it('opens the copied shell, filters and places the pick in the draft', async () => {
    renderComposer()
    fireEvent.click(screen.getByRole('button', { name: '命令' }))

    // The shell owns its own search input and holds focus there.
    const search = await screen.findByLabelText('筛选命令')
    expect(screen.getByRole('option', { name: /\/details/ })).toBeDefined()
    fireEvent.change(search, { target: { value: 'details' } })
    expect(screen.queryByRole('option', { name: /\/plan$/ })).toBeNull()

    fireEvent.click(screen.getByRole('option', { name: /\/details/ }))
    const field = screen.getByLabelText('输入消息') as HTMLTextAreaElement
    // A pick writes the command for the reader to run: nothing is submitted here.
    expect(field.value).toBe('/details ')
  })

  it('leads the list with the image pick, disabled when the session takes none', async () => {
    renderComposer({ canAttach: false })
    fireEvent.click(screen.getByRole('button', { name: '命令' }))
    const attach = await screen.findByRole('option', { name: /添加图片/ })
    expect(attach.textContent).toContain('此会话不接受图片')
  })
})
