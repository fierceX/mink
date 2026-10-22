/**
 * Detail-panel data: plan and todos, the goal snapshot, the artifact list and
 * the session's own file tree. One loader per tab, guarded by a revision token
 * so a slow response for a tab the reader already left never lands.
 */

import { useCallback, useEffect, useRef, useState, type Dispatch, type SetStateAction } from 'react'
import {
  api,
  type FileItem,
  type GoalSnapshot,
  type SessionSummary,
  type TodoSnapshot,
} from '../../data/api.ts'
import { sessionIdentity } from '../../data/sessionDraft.ts'
import { setVerifierAvailable } from '../../data/commands.ts'

/** Tabs the panel can show, in strip order. */
export const DETAIL_TABS = ['task', 'goal', 'files', 'outputs', 'diagnostics'] as const

/** One detail tab. */
export type DetailTab = typeof DETAIL_TABS[number]

export interface DetailsState {
  plan: { plan: string | null; draft: string | null }
  todo: TodoSnapshot
  /** Goal snapshot plus whether a live runtime is driving it. */
  goal: { snapshot: GoalSnapshot | null; driven: boolean }
  artifacts: { id: string; tool?: string }[]
  /** Opened artifact, when one is selected. */
  artifact: { name: string; content: string } | undefined
  files: FileItem[]
  /** Directory the listing belongs to, empty at the project root. */
  directory: string
  /** Opened file path and its content, when one is selected. */
  file: { path: string; content: string } | undefined
  loading: boolean
  error: string | undefined
  refresh: () => void
}

/**
 * Whether an incoming goal should replace the one on screen. A cleared goal is
 * always the newest truth; otherwise the higher revision of the same goal wins,
 * so a REST seed cannot roll back a live event and the live mirror cannot roll
 * back a newer seed.
 */
function goalSupersedes(current: GoalSnapshot | null, incoming: GoalSnapshot | null): boolean {
  if (incoming === null) return true
  if (current === null) return true
  if (current.id !== incoming.id) return true
  return incoming.revision > current.revision
}

/** State that is visible only to the session/runtime identity that produced it. */
function useIdentityState<T>(
  identity: string,
  epoch: number,
  view: React.MutableRefObject<{ identity: string; epoch: number }>,
  initial: T,
): [T, Dispatch<SetStateAction<T>>] {
  const [slot, setSlot] = useState(() => ({ identity, value: initial }))
  let current = slot
  if (slot.identity !== identity) {
    current = { identity, value: initial }
    setSlot(current)
  }
  const write = useCallback<Dispatch<SetStateAction<T>>>((next) => {
    setSlot(previous => {
      if (previous.identity !== identity || view.current.identity !== identity || view.current.epoch !== epoch) return previous
      const value = typeof next === 'function'
        ? (next as (previous: T) => T)(previous.value)
        : next
      if (Object.is(value, previous.value)) return previous
      return { identity, value }
    })
  }, [identity, epoch, view])
  return [current.value, write]
}

/**
 * Load the selected tab's data for one session.
 * @param summary - the open session, or undefined when none is selected.
 * @param tab - the visible tab.
 * @param path - the opened artifact or file path, when the tab has one.
 * @returns the tab data and its reload control.
 */
export function useDetails(
  summary: SessionSummary | undefined,
  tab: DetailTab,
  path: string | undefined,
  live?: { goal: GoalSnapshot | null | undefined; generation?: string },
): DetailsState {
  const sessionKey = summary === undefined ? 'closed' : sessionIdentity(summary)
  const identity = `${sessionKey}:${live?.generation ?? 'unknown'}`
  const view = useRef({ identity, epoch: 0 })
  if (view.current.identity !== identity) {
    view.current = { identity, epoch: view.current.epoch + 1 }
  }
  const viewEpoch = view.current.epoch
  const [plan, setPlan] = useIdentityState<DetailsState['plan']>(identity, viewEpoch, view, { plan: null, draft: null })
  const [todo, setTodo] = useIdentityState<TodoSnapshot>(identity, viewEpoch, view, {})
  const [goal, setGoal] = useIdentityState<DetailsState['goal']>(identity, viewEpoch, view, { snapshot: null, driven: false })
  const [artifacts, setArtifacts] = useIdentityState<{ id: string; tool?: string }[]>(identity, viewEpoch, view, [])
  const [artifact, setArtifact] = useIdentityState<{ name: string; content: string } | undefined>(identity, viewEpoch, view, undefined)
  const [files, setFiles] = useIdentityState<FileItem[]>(identity, viewEpoch, view, [])
  const [directory, setDirectory] = useIdentityState(identity, viewEpoch, view, '')
  const [file, setFile] = useIdentityState<{ path: string; content: string } | undefined>(identity, viewEpoch, view, undefined)
  const [loading, setLoading] = useIdentityState(identity, viewEpoch, view, false)
  const [error, setError] = useIdentityState<string | undefined>(identity, viewEpoch, view, undefined)
  const revision = useRef(0)
  const [ticket, setTicket] = useState(0)

  const refresh = useCallback(() => {
    if (view.current.identity !== identity || view.current.epoch !== viewEpoch) return
    setTicket(value => value + 1)
  }, [identity, viewEpoch])

  // The projection already carries the goal the runtime publishes
  // (`goal_updated`), so the tab follows it instead of holding a snapshot from
  // the moment it was opened; the fetch below only seeds it. Every publication
  // also moves a counter: it is the order that decides who is newer, which a
  // `null` (a clear) or a fresh goal id cannot express by itself.
  const liveGoal = live?.goal
  const liveVersion = useRef(0)
  // A full seed for a new revision is fetched by bumping this nonce. It is not
  // a dependency on the live object: a clear also removes id/revision and must
  // not start a seed that could resurrect the cleared goal.
  const [goalSeed, setGoalSeed] = useState(0)
  const lastSeedGoal = useRef<string | undefined>(
    liveGoal === null || liveGoal === undefined ? undefined : `${liveGoal.id}:${liveGoal.revision}`,
  )
  // A signature, not object identity: the caller may hand over a fresh object
  // with the same value on every render, and processing it would setGoal in a
  // loop. The JSON is small (the stream carries a summary) and includes every
  // field a live publication can change.
  const liveKey = liveGoal === undefined
    ? undefined
    : liveGoal === null
      ? 'none'
      : JSON.stringify(liveGoal)
  const lastLiveKey = useRef<string | undefined>(undefined)
  useEffect(() => {
    if (liveGoal === undefined || liveKey === undefined) return
    if (lastLiveKey.current === liveKey) return
    lastLiveKey.current = liveKey
    liveVersion.current += 1
    if (liveGoal === null) {
      lastSeedGoal.current = undefined
    } else {
      const key = `${liveGoal.id}:${liveGoal.revision}`
      if (lastSeedGoal.current !== key) {
        lastSeedGoal.current = key
        setGoalSeed(seed => seed + 1)
      }
    }
    setGoal(current => {
      if (liveGoal === null) {
        return current.snapshot === null ? current : { snapshot: null, driven: current.driven }
      }
      if (current.snapshot === null || current.snapshot.id !== liveGoal.id) {
        return { snapshot: liveGoal, driven: current.driven }
      }
      if (liveGoal.revision < current.snapshot.revision) return current
      if (liveGoal.revision === current.snapshot.revision) {
        // Same contract revision: the stream owns status/waiting fields, while
        // the full read owns iteration/review history. Merge only here; a
        // newer revision must not inherit the previous revision's heavy fields.
        return { snapshot: { ...current.snapshot, ...liveGoal }, driven: current.driven }
      }
      // A revision jump replaces the detail snapshot with the summary. The
      // effect below sees the new revision and fetches `full=true` once, so old
      // iterations/reports cannot label the new contract.
      return { snapshot: liveGoal, driven: current.driven }
    })
  }, [liveGoal, liveKey])

  useEffect(() => {
    const target = summary
    if (target === undefined) {
      setPlan({ plan: null, draft: null })
      setTodo({})
      setGoal({ snapshot: null, driven: false })
      setArtifacts([])
      setArtifact(undefined)
      setFiles([])
      setDirectory('')
      setFile(undefined)
      return
    }
    const token = ++revision.current
    const current = (): boolean => revision.current === token
      && view.current.identity === identity
      && view.current.epoch === viewEpoch
    // The projection's own counter when the seed starts: if it moved while the
    // seed was in flight, the live goal (a clear included) is the newer truth.
    const liveAtStart = liveVersion.current
    // An empty seed cannot erase a goal the projection announced after the
    // seed started. A null live value is different: it is the clear itself and
    // is handled by the live effect above.
    const liveGoalAtStart = liveGoal
    setLoading(true)
    setError(undefined)
    void (async () => {
      try {
        if (tab === 'task') {
          const [planResponse, todoResponse] = await Promise.all([
            api.plan(target.id, target.project_key),
            api.todo(target.id, target.project_key),
          ])
          if (!current()) return
          if (planResponse.code !== 200 || todoResponse.code !== 200) {
            throw new Error(planResponse.message || todoResponse.message || '读取计划失败')
          }
          setPlan(planResponse.data)
          setTodo(todoResponse.data.todos ?? {})
        } else if (tab === 'goal') {
          const response = await api.goal(target.id, target.project_key, true)
          if (!current()) return
          if (response.code !== 200) throw new Error(response.message || '读取目标失败')
          // The goal endpoint reports the deployment's checker capability; the
          // command palette must offer `/goal verified` only when it exists.
          setVerifierAvailable(response.data.verifier === true)
          if (liveVersion.current === liveAtStart) {
            // Nothing in the projection moved while the seed was in flight: it
            // still describes the same moment, so it may fill the tab (including
            // "no goal"). An equal revision keeps the live snapshot and still
            // takes the seed's runtime flag.
            setGoal(current => {
              const seed = response.data.goal
              if (seed === null && liveGoalAtStart !== undefined && liveGoalAtStart !== null) {
                return current
              }
              if (seed !== null && current.snapshot !== null
                && seed.id === current.snapshot.id && seed.revision === current.snapshot.revision) {
                // The seed carries the fields the stream omits: keep the live
                // summary's newer status and fill the detail history under it.
                return { snapshot: { ...current.snapshot, ...seed }, driven: response.data.driven }
              }
              return goalSupersedes(current.snapshot, seed)
                ? { snapshot: seed, driven: response.data.driven }
                : { ...current, driven: response.data.driven }
            })
          } else {
            // The projection advanced: its goal wins, and only a seed that names
            // exactly the revision now shown may still report the runtime flag.
            setGoal(current => {
              const seed = response.data.goal
              if (seed === null || current.snapshot === null) return current
              if (seed.id !== current.snapshot.id || seed.revision !== current.snapshot.revision) return current
              return { ...current, driven: response.data.driven }
            })
          }
        } else if (tab === 'files') {
          // One endpoint answers both ways: a directory comes back as a listing,
          // and a file only answers the read (`raw`) form — asking to list a file
          // is a 404, which is how the panel tells the two apart. Passing `raw`
          // for every path is why opening a directory failed.
          const rel = path ?? ''
          const listed = await api.files(target.id, rel, false, target.project_key)
          if (!current()) return
          if (listed.code === 200 && listed.data.items !== undefined) {
            setFiles(listed.data.items)
            setDirectory(rel)
            setFile(undefined)
          } else if (listed.code === 404 || listed.code === 400) {
            const read = await api.files(target.id, rel, true, target.project_key)
            if (!current()) return
            if (read.code !== 200 || read.data.content === undefined) {
              throw new Error(read.message || '读取文件失败')
            }
            setFile({ path: read.data.path ?? rel, content: read.data.content })
          } else {
            throw new Error(listed.message || '读取文件失败')
          }
        } else if (path === undefined) {
          const response = await api.artifacts(target.id, target.project_key)
          if (!current()) return
          if (response.code !== 200) throw new Error(response.message || '读取产物失败')
          setArtifacts(response.data.artifacts)
          setArtifact(undefined)
        } else {
          const response = await api.artifact(target.id, path, target.project_key)
          if (!current()) return
          if (response.code !== 200) throw new Error(response.message || '读取产物失败')
          setArtifact(response.data)
        }
      } catch (cause) {
        if (!current()) return
        setError(cause instanceof Error ? cause.message : String(cause))
      } finally {
        if (current()) setLoading(false)
      }
    })()
  }, [summary, tab, path, ticket, identity, viewEpoch, goalSeed])

  return {
    plan, todo, goal, artifacts, artifact, files, directory, file,
    loading, error, refresh,
  }
}
