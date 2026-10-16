import { act, cleanup, renderHook, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, type SessionSummary } from './api.ts'
import { useModelChoices } from './useModelChoices.ts'

/**
 * Regression from the workbench audit (W5): a vocabulary read that resolves
 * after the reader switched sessions must not replace the open session's model,
 * and neither must a switch the reader cancelled by leaving.
 */

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
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

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((resolvePromise) => { resolve = resolvePromise })
  return { promise, resolve }
}

type ModelAnswer = Awaited<ReturnType<typeof api.modelChoices>>

/** One vocabulary answer as the endpoint reports it. */
function answer(model: string): ModelAnswer {
  return { code: 200, message: '', data: { model, resolved: model, choices: [] } }
}

describe('model vocabulary identity', () => {
  it('keeps the open session’s model when the previous read answers late', async () => {
    const late = deferred<{ code: number; message: string; data: unknown }>()
    vi.spyOn(api, 'modelChoices').mockImplementation((id) => (id === 'A'
      ? late.promise as ReturnType<typeof api.modelChoices>
      : Promise.resolve({ code: 200, message: '', data: { model: 'B-model', resolved: 'B-model', choices: [] } })))
    const { result, rerender } = renderHook(({ selected }) => useModelChoices(selected, true), {
      initialProps: { selected: session('A') },
    })
    rerender({ selected: session('B') })
    await waitFor(() => { expect(result.current.model).toBe('B-model') })

    await act(async () => {
      late.resolve({ code: 200, message: '', data: { model: 'A-model', resolved: 'A-model', choices: [] } })
    })
    expect(result.current.model).toBe('B-model')
  })

  it('drops a switch accepted for a session the reader already left', async () => {
    vi.spyOn(api, 'modelChoices').mockImplementation((id) => Promise.resolve({
      code: 200,
      message: '',
      data: { model: `${id}-model`, resolved: `${id}-model`, choices: [] },
    }))
    const { result, rerender } = renderHook(({ selected }) => useModelChoices(selected, true), {
      initialProps: { selected: session('A') },
    })
    await waitFor(() => { expect(result.current.model).toBe('A-model') })
    let settleSwitch!: ReturnType<typeof result.current.beginSwitch>
    act(() => { settleSwitch = result.current.beginSwitch() })
    rerender({ selected: session('B') })
    await waitFor(() => { expect(result.current.model).toBe('B-model') })

    act(() => { settleSwitch({ model: 'A-late' }) })
    expect(result.current.model).toBe('B-model')
  })

  it('keeps an accepted switch over a read that answers later', async () => {
    // The seat reads the model when it opens; a switch accepted in the meantime
    // is newer than that read, whatever the read answers.
    const late = deferred<ModelAnswer>()
    vi.spyOn(api, 'modelChoices')
      .mockReturnValueOnce(late.promise)
      .mockResolvedValue(answer('new-model'))
    const { result } = renderHook(() => useModelChoices(session('A'), true))
    let settleSwitch!: ReturnType<typeof result.current.beginSwitch>
    act(() => { settleSwitch = result.current.beginSwitch() })
    act(() => { settleSwitch({ model: 'new-model' }) })
    await act(async () => {
      late.resolve(answer('old-model'))
    })
    expect(result.current.model).toBe('new-model')
  })

  it('rejects the first A read after going A to B and back to A', async () => {
    // Identity alone would accept this answer: the reader is on A again. The
    // request that is still current is the second one, so the first must not
    // land on top of it.
    const late = deferred<ModelAnswer>()
    vi.spyOn(api, 'modelChoices')
      .mockReturnValueOnce(late.promise)
      .mockResolvedValue(answer('current-model'))
    const { result, rerender } = renderHook(({ selected }) => useModelChoices(selected, true), {
      initialProps: { selected: session('A') },
    })
    rerender({ selected: session('B') })
    rerender({ selected: session('A') })
    await waitFor(() => { expect(result.current.model).toBe('current-model') })
    await act(async () => {
      late.resolve(answer('stale-model'))
    })
    expect(result.current.model).toBe('current-model')
  })

  it('rejects the first A switch response after going A to B and back to A', async () => {
    vi.spyOn(api, 'modelChoices').mockImplementation(async (id) => answer(`${id}-fresh`))
    const { result, rerender } = renderHook(({ selected }) => useModelChoices(selected, true), {
      initialProps: { selected: session('A') },
    })
    await waitFor(() => { expect(result.current.model).toBe('A-fresh') })
    let settleFirstSwitch!: ReturnType<typeof result.current.beginSwitch>
    act(() => { settleFirstSwitch = result.current.beginSwitch() })
    rerender({ selected: session('B') })
    rerender({ selected: session('A') })
    await waitFor(() => { expect(result.current.model).toBe('A-fresh') })

    act(() => { settleFirstSwitch({ model: 'A-stale-switch' }) })
    expect(result.current.model).toBe('A-fresh')
  })
})
