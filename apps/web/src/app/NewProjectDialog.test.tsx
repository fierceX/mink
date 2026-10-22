import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, type ApiResponse } from '../data/api.ts'
import { NewProjectDialog } from './NewProjectDialog.tsx'

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
})

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}

describe('new project dialog', () => {
  it('drops a directory check that finishes after the dialog closed', async () => {
    const check = deferred<ApiResponse<{ cwd: string; project_key: string; name: string }>>()
    vi.spyOn(api, 'workspace').mockReturnValue(check.promise)
    const ready = vi.fn()
    const { unmount } = render(
      <NewProjectDialog seedPath="/tmp/project" onClose={() => {}} onReady={ready} />,
    )
    fireEvent.change(screen.getByLabelText('工作目录'), { target: { value: '/tmp/project' } })
    fireEvent.click(screen.getByRole('button', { name: '打开目录' }))
    unmount()
    await act(async () => {
      check.resolve({ code: 200, message: '', data: { cwd: '/tmp/project', project_key: 'p', name: 'project' } })
    })
    expect(ready).not.toHaveBeenCalled()
  })
})
