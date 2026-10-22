import { act, cleanup, renderHook, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, type SessionSummary } from '../../data/api.ts'
import type { GoalReview, GoalSnapshot } from '../../data/types.ts'
import { isVerifierAvailable, setVerifierAvailable } from '../../data/commands.ts'
import { useDetails } from './useDetails.ts'

/**
 * Regression from the workbench audit (W6): the goal tab's REST seed and the
 * live projection are one merge by revision. A seed that answers after a newer
 * live update must not roll the tab back to the older snapshot.
 */

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
})

const SESSION: SessionSummary = {
  id: 'session-1',
  project_key: 'demo',
  cwd: '/tmp/demo',
  title: null,
  alias: null,
  status: 'free',
  corrupt: false,
  created_at: '',
  updated_at: '',
  modified_secs: 0,
  path: '',
}

function goal(revision: number, status: string): GoalSnapshot {
  return { id: 'goal-1', revision, status, phase: 'working' }
}

type GoalAnswer = ReturnType<typeof api.goal> extends Promise<infer T> ? T : never

function ok(goal_: GoalSnapshot | null, driven: boolean): GoalAnswer {
  return { code: 200, message: '', data: { goal: goal_, driven } }
}

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((resolvePromise) => { resolve = resolvePromise })
  return { promise, resolve }
}

describe('detail goal merge', () => {
  it('does not show the previous session goal while the next session seed is pending', async () => {
    const seedA = deferred<GoalAnswer>()
    const seedB = deferred<GoalAnswer>()
    vi.spyOn(api, 'goal')
      .mockReturnValueOnce(seedA.promise)
      .mockReturnValueOnce(seedB.promise)
    const sessionB = { ...SESSION, id: 'session-2' }
    const { result, rerender } = renderHook(
      ({ session }) => useDetails(session, 'goal', undefined),
      { initialProps: { session: SESSION } },
    )
    await act(async () => { seedA.resolve(ok(goal(1, 'active'), true)) })
    expect(result.current.goal.snapshot?.id).toBe('goal-1')

    rerender({ session: sessionB })
    expect(result.current.goal.snapshot).toBeNull()
    await act(async () => { seedB.resolve(ok({ ...goal(1, 'active'), id: 'goal-B' }, true)) })
    expect(result.current.goal.snapshot?.id).toBe('goal-B')
  })

  it('drops a goal seed when the same session runtime is replaced', async () => {
    const oldRuntime = deferred<GoalAnswer>()
    const newRuntime = deferred<GoalAnswer>()
    vi.spyOn(api, 'goal')
      .mockReturnValueOnce(oldRuntime.promise)
      .mockReturnValueOnce(newRuntime.promise)
    const { result, rerender } = renderHook(
      ({ generation }) => useDetails(SESSION, 'goal', undefined, { goal: undefined, generation }),
      { initialProps: { generation: 'runtime-old' } },
    )
    rerender({ generation: 'runtime-new' })
    expect(result.current.goal.snapshot).toBeNull()
    await act(async () => { oldRuntime.resolve(ok(goal(1, 'active'), true)) })
    expect(result.current.goal.snapshot).toBeNull()
    await act(async () => { newRuntime.resolve(ok({ ...goal(1, 'active'), id: 'new-runtime-goal' }, true)) })
    expect(result.current.goal.snapshot?.id).toBe('new-runtime-goal')
  })

  it('keeps the newer live revision when the REST seed answers late', async () => {
    const seed = deferred<GoalAnswer>()
    vi.spyOn(api, 'goal').mockReturnValue(seed.promise)
    const { result, rerender } = renderHook(
      ({ live }) => useDetails(SESSION, 'goal', undefined, { goal: live }),
      { initialProps: { live: goal(1, 'active') as GoalSnapshot | null } },
    )
    rerender({ live: goal(2, 'completed') as GoalSnapshot | null })
    await act(async () => { seed.resolve(ok(goal(1, 'active'), true)) })
    expect(result.current.goal.snapshot?.revision).toBe(2)
  })

  it('lets a newer REST seed land over an older live revision', async () => {
    vi.spyOn(api, 'goal').mockResolvedValue(ok(goal(4, 'paused'), true))
    const { result } = renderHook(
      () => useDetails(SESSION, 'goal', undefined, { goal: goal(3, 'active') }),
    )
    await waitFor(() => { expect(result.current.goal.snapshot?.revision).toBe(4) })
    expect(result.current.goal.driven).toBe(true)
  })

  it('clears the tab when the live projection clears the goal', async () => {
    vi.spyOn(api, 'goal').mockResolvedValue(ok(goal(1, 'active'), true))
    const { result, rerender } = renderHook(
      ({ live }) => useDetails(SESSION, 'goal', undefined, { goal: live }),
      { initialProps: { live: goal(1, 'active') as GoalSnapshot | null } },
    )
    rerender({ live: null })
    expect(result.current.goal.snapshot).toBeNull()
  })

  it('stays cleared when a seed that started earlier answers with the old goal', async () => {
    // A clear carries no revision, so order is the only thing that can say the
    // seed is stale — and the projection's own counter knows it.
    const seed = deferred<GoalAnswer>()
    vi.spyOn(api, 'goal').mockReturnValue(seed.promise)
    const { result, rerender } = renderHook(
      ({ live }) => useDetails(SESSION, 'goal', undefined, { goal: live }),
      { initialProps: { live: goal(1, 'active') as GoalSnapshot | null } },
    )
    rerender({ live: null })
    await act(async () => { seed.resolve(ok(goal(1, 'active'), true)) })
    expect(result.current.goal.snapshot).toBeNull()
  })

  it('does not let an old empty seed erase a goal created while it was in flight', async () => {
    const seed = deferred<GoalAnswer>()
    vi.spyOn(api, 'goal').mockReturnValue(seed.promise)
    const created: GoalSnapshot = { id: 'new-goal', revision: 1, status: 'active', phase: 'working' }
    const { result, rerender } = renderHook(
      ({ live }) => useDetails(SESSION, 'goal', undefined, { goal: live }),
      { initialProps: { live: null as GoalSnapshot | null } },
    )
    rerender({ live: created })
    await act(async () => { seed.resolve(ok(null, false)) })
    expect(result.current.goal.snapshot?.id).toBe('new-goal')
  })

  it('refetches full history instead of mixing an old revision with a live summary', async () => {
    const candidate1 = { goal_id: 'goal-1', contract_revision: 1, generation: 1, iteration: 1, conversation_seq: 1, reviewer_id: 'reviewer' }
    const candidate2 = { ...candidate1, generation: 2, iteration: 2, conversation_seq: 2 }
    const review = (text: string): GoalReview => ({
      verdict: 'satisfied', summary: text, next_step: '', criteria: [], constraints: [],
    })
    const full1: GoalSnapshot = {
      id: 'goal-1', revision: 1, status: 'active', phase: 'working',
      review_candidate: candidate1,
      reviews: [review('old')], iterations: [{ n: 1 }],
    }
    const full2: GoalSnapshot = {
      id: 'goal-1', revision: 2, status: 'completed', phase: 'done',
      review_candidate: candidate2,
      reviews: [review('old'), { ...review('new'), candidate: candidate2 }], iterations: [{ n: 1 }, { n: 2 }],
    }
    const seed1 = deferred<GoalAnswer>()
    const seed2 = deferred<GoalAnswer>()
    vi.spyOn(api, 'goal')
      .mockReturnValueOnce(seed1.promise)
      .mockReturnValueOnce(seed2.promise)
    const { result, rerender } = renderHook(
      ({ live }) => useDetails(SESSION, 'goal', undefined, { goal: live }),
      { initialProps: { live: undefined as GoalSnapshot | undefined } },
    )
    await act(async () => { seed1.resolve(ok(full1, true)) })
    expect(result.current.goal.snapshot?.iterations).toHaveLength(1)

    rerender({ live: {
      id: 'goal-1', revision: 2, status: 'completed', phase: 'done',
      review_candidate: candidate2,
      reviews: [{ ...review('new'), candidate: candidate2 }],
      continuations: [],
    } })
    // The summary must replace the old contract while the full read runs; it
    // may not be spread over revision-1 iterations and review history.
    expect(result.current.goal.snapshot?.revision).toBe(2)
    expect(result.current.goal.snapshot?.iterations).toBeUndefined()
    expect(result.current.goal.snapshot?.review_candidate).toEqual(candidate2)
    await waitFor(() => { expect(api.goal).toHaveBeenCalledTimes(2) })
    await act(async () => { seed2.resolve(ok(full2, true)) })
    expect(result.current.goal.snapshot?.iterations).toHaveLength(2)
    expect(result.current.goal.snapshot?.reviews).toHaveLength(2)
  })

  it('still takes the seed’s runtime flag when the revision is the same', async () => {
    vi.spyOn(api, 'goal').mockResolvedValue(ok(goal(2, 'active'), true))
    const { result } = renderHook(
      () => useDetails(SESSION, 'goal', undefined, { goal: goal(2, 'active') }),
    )
    await waitFor(() => { expect(result.current.goal.driven).toBe(true) })
    expect(result.current.goal.snapshot?.revision).toBe(2)
  })
})

describe('checker capability', () => {
  afterEach(() => { setVerifierAvailable(false) })

  it.each([true, false])(
    'records the deployment checker capability (%s) from the goal read',
    async (verifier) => {
      setVerifierAvailable(!verifier)
      vi.spyOn(api, 'goal').mockResolvedValue({
        code: 200,
        message: '',
        data: { goal: null, driven: true, verifier },
      })
      const { result } = renderHook(() => useDetails(SESSION, 'goal', undefined))
      await waitFor(() => { expect(result.current.goal.driven).toBe(true) })
      expect(isVerifierAvailable()).toBe(verifier)
    },
  )
})
