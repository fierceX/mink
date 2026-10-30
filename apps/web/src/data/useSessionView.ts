/**
 * Session view state: open a session, project its history through mink's own
 * reducer, follow the live event stream, and submit or interrupt turns. The
 * projection, the SSE client and the reducer are the same code the Vue
 * workbench runs, so both frontends agree on what a session shows.
 *
 * Every asynchronous completion is checked against the session identity it
 * started under: a reply, a receipt or a page of history that arrives after the
 * reader switched sessions is dropped instead of being written onto the open
 * one. A submission whose reply never arrived keeps its request id, so the
 * reader's retry is the same input to the server's admission boundary.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import { api, type SessionSummary } from './api.ts'
import { SseClient } from './sse.ts'
import { emptySession, type GoalSnapshot, type HumanInput, type InputReceipt, type SessionState } from './types.ts'
import { applyGoalRead, projectMessages, reduceWorkbench } from './workbenchReducer.ts'
import { requestId } from './request-id.ts'
import {
  forgetAcceptedAttachments as forgetStoredAttachments,
  readPendingSubmission,
  sessionAttachmentsKey,
  sessionIdentity,
  sessionPendingKey,
  writePendingSubmission,
  type PendingSubmission,
} from './sessionDraft.ts'
import { setVerifierAvailable } from './commands.ts'
import { useLatest } from './useLatest.ts'

/** How many conversation turns one history page asks for. */
const TAIL_LIMIT = 20

/** How the live stream currently stands, for the reconnect control. */
export type SessionConnection =
  | 'connecting'
  | 'connected'
  | 'disconnected'
  | 'offline'
  | 'closed'
  | 'failed'

export interface SessionView {
  state: SessionState | undefined
  /** The session runtime holds this session open; sending requires it. */
  open: boolean
  /** Live-stream state; `closed` means the runtime must be reopened. */
  connection: SessionConnection
  error: string | undefined
  /** Try the open again after a failure; the projection reloads the same way. */
  retry: () => void
  /** Reopen the runtime and rebuild the projection from its snapshot. */
  reconnect: () => void
  /** An unresolved submission's body; restores the composer after a return. */
  pendingDraft: string | undefined
  /** Queue a fresh turn; resolves to an error message, or undefined on success. */
  send: (
    text: string,
    attachmentIds?: readonly string[],
    attachmentKeys?: readonly string[],
  ) => Promise<string | undefined>
  /** Queue guidance bound to the running turn. */
  guide: (
    text: string,
    turnId: string,
    attachmentIds?: readonly string[],
    attachmentKeys?: readonly string[],
  ) => Promise<string | undefined>
  /** Fold a receipt the panel changed back into the projection. */
  mergeReceipt: (receipt: InputReceipt, started?: ReceiptStamp) => void
  /** Capture the view epoch/runtime generation before a receipt mutation. */
  receiptStamp: () => ReceiptStamp
  /** Whether older turns remain before the loaded window. */
  hasOlder: boolean
  /** Load the previous page of history; resolves true when rows were added. */
  loadOlder: () => Promise<boolean>
  /**
   * Re-read the goal after a command changed it: a REST answer carries no
   * stream watermark, so it is applied only while the projection has not
   * advanced since the read started.
   */
  refreshGoal: () => void
  stop: () => void
}

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause)
}

/** The seq a history row carries; the server numbers every row it returns. */
function rowSeq(row: Record<string, unknown> | undefined): number {
  const seq = row?.['seq']
  return typeof seq === 'number' ? seq : Number.NaN
}

/** Merge history rows into one ascending collection, deduplicated by seq.
 * A row the server did not number (a live commit missing its seq) is kept after
 * the numbered ones instead of being dropped. */
function mergeRows(
  existing: readonly Record<string, unknown>[],
  incoming: readonly Record<string, unknown>[],
): Record<string, unknown>[] {
  const bySeq = new Map<number, Record<string, unknown>>()
  const unnumbered: Record<string, unknown>[] = []
  for (const row of [...existing, ...incoming]) {
    const seq = rowSeq(row)
    if (Number.isFinite(seq)) {
      if (!bySeq.has(seq)) bySeq.set(seq, row)
    } else if (!unnumbered.includes(row)) {
      unnumbered.push(row)
    }
  }
  return [...bySeq.entries()].sort(([a], [b]) => a - b).map(([, row]) => row).concat(unnumbered)
}

/** Seed the projection from the session summary and its conversation tail. */
function seed(summary: SessionSummary, rows: readonly Record<string, unknown>[]): SessionState {
  const state = emptySession(summary.id, summary.title ?? summary.alias ?? summary.id)
  state.tokensIn = summary.tokens_in ?? null
  state.tokensOut = summary.tokens_out ?? null
  state.cacheReadTokens = summary.cache_read_tokens ?? null
  state.contextTokens = summary.last_context_tokens ?? state.tokensIn ?? 0
  const reduced = projectMessages(state, [...rows])
  reduced.running = false
  reduced.workState = 'idle'
  return reduced
}

/** The wire form of a submission record; local bookkeeping never leaves here. */
function wireInput(input: PendingSubmission): HumanInput {
  return {
    request_id: input.request_id,
    text: input.text,
    target_turn_id: input.target_turn_id,
    attachment_ids: input.attachment_ids,
  }
}

/** Whether a stored submission is exactly what the reader is sending again. */
function sameSubmission(
  stored: PendingSubmission,
  text: string,
  targetTurnId: string | null,
  attachmentIds: readonly string[],
  attachmentKeys: readonly string[],
): boolean {
  if (stored.target_turn_id !== targetTurnId || stored.text !== text) return false
  if (stored.attachment_ids.length !== attachmentIds.length) return false
  if (!stored.attachment_ids.every((id, index) => id === attachmentIds[index])) return false
  // Entry identity matters: the same content id can belong to a different draft
  // entry (a re-chosen identical image), and that is a different input.
  const storedKeys = stored.attachment_keys
  if (storedKeys === undefined) return true
  return storedKeys.length === attachmentKeys.length
    && storedKeys.every((key, index) => key === attachmentKeys[index])
}

/** Ask the admission boundary whether it already holds this request. */
async function receiptState(
  target: SessionSummary,
  requestId: string,
): Promise<'accepted' | 'absent' | 'unknown'> {
  try {
    const lookup = await api.inputs(target.id, target.project_key, requestId)
    if (lookup.code !== 200 || !Array.isArray(lookup.data)) return 'unknown'
    return lookup.data.some(receipt => receipt.input_id === requestId) ? 'accepted' : 'absent'
  } catch {
    return 'unknown'
  }
}

/**
 * How one submission attempt ended.
 * - `accepted`: the queue holds it (or already held it): the draft may clear.
 * - `refused`: the server answered with a refusal: the words come back.
 * - `unknown`: no reply arrived; the request id is kept for an idempotent retry.
 * The distinction is what a retry needs: only `unknown` may reuse the id, and
 * only a refused or unknown attempt may restore the draft.
 */
export type SubmitOutcome =
  | { status: 'accepted' }
  | { status: 'refused'; message: string }
  | { status: 'unknown'; message: string }

/** Reader-facing failure text; undefined means accepted. */
function failureOf(outcome: SubmitOutcome): string | undefined {
  return outcome.status === 'accepted' ? undefined : outcome.message
}

/** The one wording for a submission whose reception could not be settled. */
const UNKNOWN_SEND = '无法确认本次输入是否已被服务端接收：稍后在输入队列中查看，或用同一内容重试'
const UNKNOWN_GUIDE = '无法确认此引导是否已被服务端接收：稍后用同一内容重试，或在输入队列中查看'

/**
 * What a request started under. A completion may touch the projection or its
 * error only while every part still holds: the same session is selected (the
 * open-effect epoch has not moved) and the runtime that answered is still the
 * one the projection mirrors — same generation, same turn in progress. A later
 * snapshot from a reopened runtime is the newer truth, so an older HTTP answer
 * must not roll it back.
 */
interface ViewStamp {
  identity: string | undefined
  epoch: number
  runtime: string | undefined
  turn: string | null
}

/** The parts of a request's view that a receipt may not outlive. */
export interface ReceiptStamp {
  identity: string | undefined
  epoch: number
  runtime: string | undefined
}

/** A receipt may land only while the view epoch and runtime generation hold. */
export function receiptStampHolds(started: ReceiptStamp, now: ReceiptStamp): boolean {
  return started.identity === now.identity
    && started.epoch === now.epoch
    && started.runtime === now.runtime
}

/**
 * Follow one session for as long as it is selected.
 * @param summary - the open session, or undefined when none is selected.
 * @returns the projected state and the two turn actions.
 */
export function useSessionView(summary: SessionSummary | undefined): SessionView {
  const [state, setState] = useState<SessionState | undefined>(undefined)
  const [open, setOpen] = useState(false)
  const [connection, setConnection] = useState<SessionConnection>('connecting')
  const [error, setError] = useState<string | undefined>(undefined)
  // Bumping this re-runs the open effect for the same session.
  const [attempt, setAttempt] = useState(0)
  const [hasOlder, setHasOlder] = useState(false)
  // The loaded history rows, oldest first: the projection is rebuilt from them
  // whenever a page is prepended.
  const rows = useRef<Record<string, unknown>[]>([])
  const generation = useRef(0)
  // The identity of the view in force now, for completions that resolve late.
  const identity = summary === undefined ? undefined : sessionIdentity(summary)
  const identityRef = useLatest(identity)
  // The projection as of the latest render, for stamp checks in completions.
  const stateRef = useLatest(state)
  // The unresolved submission of each session, keyed by identity: leaving a
  // session must not take its request identity away, and the storage copy keeps
  // it across a reload the way the draft is kept.
  const uncertain = useRef<Map<string, HumanInput>>(new Map())
  const loadedIdentity = useRef<string | undefined>(undefined)

  /** The stamp a request starts under; every part must still hold when it ends. */
  const stampNow = (): ViewStamp => ({
    identity: identityRef.current,
    epoch: generation.current,
    runtime: stateRef.current?.generation,
    turn: stateRef.current?.currentTurn ?? null,
  })
  const stampHolds = (stamp: ViewStamp): boolean => {
    const now = stampNow()
    return now.identity === stamp.identity
      && now.epoch === stamp.epoch
      && now.runtime === stamp.runtime
      && now.turn === stamp.turn
  }
  /** Capture the receipt boundary before an input mutation leaves the view. */
  const receiptStamp = useCallback((): ReceiptStamp => {
    const now = stampNow()
    return { identity: now.identity, epoch: now.epoch, runtime: now.runtime }
  }, [])

  /** The submission this session left unresolved, from memory or storage. */
  const storedSubmission = (target: SessionSummary): PendingSubmission | undefined => {
    const key = sessionPendingKey(target)
    const remembered = uncertain.current.get(key)
    if (remembered !== undefined) return remembered
    const stored = readPendingSubmission(key)
    if (stored !== undefined) uncertain.current.set(key, stored)
    return stored
  }

  /** Keep a submission until its ending is known; the record is per identity. */
  const rememberSubmission = (target: SessionSummary, input: PendingSubmission): void => {
    const key = sessionPendingKey(target)
    uncertain.current.set(key, input)
    writePendingSubmission(key, input)
  }

  /** Forget the submission only once the server settled it. */
  const forgetSubmission = (target: SessionSummary, requestId: string): void => {
    const key = sessionPendingKey(target)
    if (uncertain.current.get(key)?.request_id !== requestId) return
    uncertain.current.delete(key)
    writePendingSubmission(key, undefined)
  }

  /** Remove only the draft entries an accepted input consumed, preserving newer
   * ones and any later copy of the same content-addressed image. */
  const forgetAcceptedAttachments = (target: SessionSummary, input: PendingSubmission): void => {
    forgetStoredAttachments(
      sessionAttachmentsKey(target),
      input.attachment_keys ?? [],
      input.attachment_ids,
    )
  }

  useEffect(() => {
    // The boundary rows belong to one identity; a switch starts them over. A
    // submission record outlives the view, so it is deliberately not cleared
    // here — that is what lets the reader retry after coming back.
    if (loadedIdentity.current !== identity) {
      loadedIdentity.current = identity
      rows.current = []
      setHasOlder(false)
    }
    if (summary === undefined) {
      setState(undefined)
      setOpen(false)
      setConnection('disconnected')
      setError(undefined)
      return
    }
    const token = ++generation.current
    const current = (): boolean => generation.current === token
    let client: SseClient | undefined
    setState(emptySession(summary.id, summary.title ?? summary.alias ?? summary.id))
    setOpen(false)
    setConnection('connecting')
    setError(undefined)

    void (async () => {
      try {
        const opened = await api.openSession(summary.id, summary.project_key)
        if (!current()) return
        if (opened.code !== 200) throw new Error(opened.message || `code ${opened.code}`)
        setOpen(true)
        // A runtime that offers the snapshot stream seeds the projection itself
        // (conversation, inputs, resources, goal and image capability in one
        // frame); otherwise the history tail is the seed.
        const snapshotMode = (opened.data as { snapshot?: boolean } | null)?.snapshot === true
        if (!snapshotMode) {
          const tail = await api.conversation(summary.id, {
            limit: TAIL_LIMIT,
            tail: true,
            project: summary.project_key,
          })
          if (!current()) return
          if (tail.code !== 200 || !Array.isArray(tail.data)) {
            throw new Error(tail.message || '历史加载失败')
          }
          rows.current = tail.data as Record<string, unknown>[]
          setHasOlder(rows.current.length > 0)
          setState(seed(summary, rows.current))
        }
        const stream = new SseClient(
          summary.id,
          (raw) => {
            if (!current()) return
            // The snapshot carries the same history rows the tail fetch would:
            // keep them so the older-history control knows where the window
            // starts.
            if (raw.type === 'session_snapshot' && Array.isArray(raw.conversation)) {
              rows.current = raw.conversation as Record<string, unknown>[]
              const seq = rowSeq(rows.current[0])
              // The server states whether its capped tail dropped history. Older
              // servers omit the field; the first seq remains the fallback.
              setHasOlder(typeof raw.has_older === 'boolean'
                ? raw.has_older
                : rows.current.length > 0 && (!Number.isFinite(seq) || seq > 1))
              setConnection('connected')
              setOpen(true)
            }
            if (raw.type === 'phase_updated' && raw.phase === 'closed') {
              setConnection('closed')
              setOpen(false)
            }
            setState(previous => (previous === undefined ? previous : reduceWorkbench(previous, raw)))
          },
          (reason) => {
            if (!current()) return
            // Keep the last known running/work state: the reconnect's
            // authoritative snapshot decides it. Faking `running:false` here
            // flips the composer from guidance back to a new turn.
            setConnection(reason === 'session_closed'
              ? 'closed'
              : reason === 'offline' ? 'offline' : 'disconnected')
            if (reason === 'session_closed') setOpen(false)
            setState(previous => (previous === undefined
              ? previous
              : { ...previous, desynced: true }))
          },
          summary.project_key,
          () => {
            // `onopen` only proves the transport accepted the subscription;
            // until the snapshot arrives the projection is still the old one.
            // Legacy streams have no snapshot frame and keep the old signal.
            if (current() && !snapshotMode) setConnection('connected')
          },
          snapshotMode,
        )
        client = stream
        stream.connect()
      } catch (cause) {
        if (!current()) return
        setConnection('failed')
        setError(messageOf(cause))
      }
    })()

    return () => {
      generation.current += 1
      client?.close()
    }
  }, [summary, attempt, identity])

  // A fresh turn goes through the same input queue as guidance — the queue is
  // the session's only admission boundary, and it is the only path that carries
  // attachments. `target_turn_id: null` is what makes it a new turn.
  const send = useCallback(async (text: string, attachmentIds: readonly string[] = [], attachmentKeys: readonly string[] = []): Promise<string | undefined> => {
    const target = summary
    if (target === undefined) return '会话未打开'
    const view = sessionIdentity(target)
    const started = stampNow()
    const stored = storedSubmission(target)
    let input: PendingSubmission
    if (stored !== undefined && sameSubmission(stored, text, null, attachmentIds, attachmentKeys)) {
      // An earlier attempt may already hold this input: resolve it before
      // minting a second request id for the same words.
      const reconciled = await receiptState(target, stored.request_id)
      if (reconciled === 'accepted') {
        forgetSubmission(target, stored.request_id)
        forgetAcceptedAttachments(target, stored)
        return undefined
      }
      if (reconciled === 'unknown') return UNKNOWN_SEND
      input = stored
    } else {
      input = {
        request_id: requestId(),
        text,
        target_turn_id: null,
        attachment_ids: [...attachmentIds],
        ...(attachmentKeys.length === 0 ? {} : { attachment_keys: [...attachmentKeys] }),
      }
    }
    // The record goes in before the request: a reload in the middle must not
    // lose the identity the retry depends on.
    rememberSubmission(target, input)
    if (identityRef.current === view) {
      setState(previous => (previous === undefined
        ? previous
        : { ...previous, running: true, workState: 'waiting' }))
    }
    let outcome: SubmitOutcome
    try {
      const response = await api.submitInput(target.id, wireInput(input), target.project_key)
      if (response.code !== 200) {
        forgetSubmission(target, input.request_id)
        outcome = { status: 'refused', message: response.message || `code ${response.code}` }
      } else {
        forgetSubmission(target, input.request_id)
        forgetAcceptedAttachments(target, input)
        outcome = { status: 'accepted' }
      }
    } catch (cause) {
      // The reply never arrived: the server may hold the input already, so the
      // next attempt reuses this request id instead of asking for the turn twice.
      outcome = { status: 'unknown', message: messageOf(cause) }
    }
    // Only a runtime that has not moved on may have its projection rolled back:
    // a newer snapshot is the newer truth, and the old failure stays its own.
    if (outcome.status !== 'accepted' && stampHolds(started)) {
      setState(previous => (previous === undefined ? previous : { ...previous, running: false }))
      setError(outcome.message)
    }
    return failureOf(outcome)
  }, [summary, identityRef, stateRef])

  // Guidance rides the session's own input queue: the request id binds the
  // content, and the receipt comes back through the projection (`inputs_updated`).
  const guide = useCallback(async (text: string, turnId: string, attachmentIds: readonly string[] = [], attachmentKeys: readonly string[] = []): Promise<string | undefined> => {
    const target = summary
    if (target === undefined) return '会话未打开'
    const stored = storedSubmission(target)
    let input: PendingSubmission
    if (stored !== undefined && sameSubmission(stored, text, turnId, attachmentIds, attachmentKeys)) {
      const reconciled = await receiptState(target, stored.request_id)
      // A receipt the server already holds is not a failure: the guidance is in
      // the queue, and the strip above the composer shows it.
      if (reconciled === 'accepted') {
        forgetSubmission(target, stored.request_id)
        forgetAcceptedAttachments(target, stored)
        return undefined
      }
      if (reconciled === 'unknown') return UNKNOWN_GUIDE
      input = stored
    } else {
      input = {
        request_id: requestId(),
        text,
        target_turn_id: turnId,
        attachment_ids: [...attachmentIds],
        ...(attachmentKeys.length === 0 ? {} : { attachment_keys: [...attachmentKeys] }),
      }
    }
    rememberSubmission(target, input)
    let outcome: SubmitOutcome
    try {
      const response = await api.submitInput(target.id, wireInput(input), target.project_key)
      if (response.code !== 200) {
        forgetSubmission(target, input.request_id)
        outcome = { status: 'refused', message: `提交失败：${response.message || `code ${response.code}`}` }
      } else {
        forgetSubmission(target, input.request_id)
        forgetAcceptedAttachments(target, input)
        outcome = { status: 'accepted' }
      }
    } catch (cause) {
      // Resolve an uncertain reception before offering a retry.
      const reconciled = await receiptState(target, input.request_id)
      if (reconciled === 'accepted') {
        forgetSubmission(target, input.request_id)
        forgetAcceptedAttachments(target, input)
        outcome = { status: 'accepted' }
      } else if (reconciled === 'absent') {
        forgetSubmission(target, input.request_id)
        outcome = { status: 'refused', message: `提交失败：${messageOf(cause)}` }
      } else {
        outcome = { status: 'unknown', message: UNKNOWN_GUIDE }
      }
    }
    return failureOf(outcome)
  }, [summary])

  const loadOlder = useCallback(async (): Promise<boolean> => {
    const target = summary
    if (target === undefined || !hasOlder) return false
    const view = sessionIdentity(target)
    // A retry of the same session replaces the projection: a page fetched under
    // the previous generation would land on a reset window and hide the tail.
    const token = generation.current
    const firstSeq = rowSeq(rows.current[0])
    if (!Number.isFinite(firstSeq) || firstSeq <= 1) {
      if (identityRef.current === view) setHasOlder(false)
      return false
    }
    const response = await api.conversation(target.id, {
      limit: TAIL_LIMIT,
      beforeSeq: firstSeq,
      project: target.project_key,
    })
    // A page that arrives after a switch (or a reopen) belongs to the session
    // generation that asked for it, not to the one on screen now.
    if (identityRef.current !== view || generation.current !== token) return false
    if (response.code !== 200 || !Array.isArray(response.data)) return false
    const older = response.data as Record<string, unknown>[]
    if (older.length === 0) {
      setHasOlder(false)
      return false
    }
    rows.current = mergeRows(rows.current, older)
    setState((previous) => {
      if (previous === undefined) return previous
      // `state.messages` is the only committed collection: the page merges into
      // it by seq, so rows delivered live after the snapshot are not rebuilt
      // away by an older page. Streaming rows are not history rows: keep them
      // on top.
      const messages = mergeRows(previous.messages ?? [], older)
      const live = previous.items.filter(item =>
        String(item.key).startsWith('live:') && (item.kind === 'text' || item.kind === 'thinking'))
      const merged = projectMessages(previous, messages)
      merged.items = [...merged.items, ...live]
      merged.running = previous.running
      merged.workState = previous.workState
      merged.waitElapsedSecs = previous.waitElapsedSecs
      merged.activeSubAgents = previous.activeSubAgents
      return merged
    })
    return true
  }, [hasOlder, summary, identityRef])

  const mergeReceipt = useCallback((receipt: InputReceipt, started?: ReceiptStamp) => {
    // The strip is keyed by session, but its request may answer after a switch:
    // only the identity that asked for the change may fold the receipt in. The
    // receipt's own revision is the second guard — an older receipt can never
    // replace a newer one, whichever session it came from.
    if (identityRef.current !== identity) return
    if (started !== undefined) {
      const now = stampNow()
      if (!receiptStampHolds(started, {
        identity: now.identity,
        epoch: now.epoch,
        runtime: now.runtime,
      })) return
    }
    setState((previous) => {
      if (previous === undefined) return previous
      const receipts = [...previous.inputs ?? []]
      const index = receipts.findIndex(item => item.input_id === receipt.input_id)
      // Only fold a change into a receipt this projection already holds. A
      // reply that raced a runtime restart / reopen must not invent a row the
      // authoritative snapshot never contained.
      if (index < 0) return previous
      if (receipts[index].revision <= receipt.revision) receipts[index] = receipt
      return { ...previous, inputs: receipts }
    })
  }, [identity, identityRef])

  /**
   * Re-read the goal after a command changed it and fold the answer in while it
   * is still newer than the projection: a REST read carries no watermark, so an
   * answer that raced a stream event is dropped instead of rolling it back. A
   * successful read also records whether this deployment has a checker.
   */
  const refreshGoal = useCallback((): void => {
    const target = summary
    if (target === undefined) return
    const view = sessionIdentity(target)
    const started = stampNow()
    const readSeq = stateRef.current?.lastSeq ?? 0
    void api.goal(target.id, target.project_key).then((response) => {
      if (identityRef.current !== view || !stampHolds(started)) return
      if (response.code !== 200) return
      setVerifierAvailable(response.data.verifier === true)
      const goal = (response.data.goal ?? null) as GoalSnapshot | null
      setState(previous => (previous === undefined
        ? previous
        : applyGoalRead(previous, goal, readSeq)))
    }).catch(() => {
      // A failed read is not a projection error: the stream stays the source of
      // truth and the next lifecycle event updates the bar.
    })
  }, [summary, identityRef, stateRef])

  const stop = useCallback(() => {
    const target = summary
    if (target === undefined) return
    const started = stampNow()
    void api.interrupt(target.id, target.project_key).catch((cause: unknown) => {
      // A stop that failed on a runtime the projection has already replaced is
      // not an error of the session on screen now.
      if (!stampHolds(started)) return
      setError(messageOf(cause))
    })
  }, [summary, stateRef])

  const pendingDraft = summary === undefined ? undefined : storedSubmission(summary)?.text

  const reconnect = useCallback(() => {
    if (summary === undefined) return
    setError(undefined)
    setConnection('connecting')
    setAttempt(value => value + 1)
  }, [summary])

  return {
    state,
    open,
    connection,
    error,
    retry: reconnect,
    reconnect,
    send,
    guide,
    mergeReceipt,
    hasOlder,
    loadOlder,
    refreshGoal,
    stop,
    pendingDraft,
    receiptStamp,
  }
}
