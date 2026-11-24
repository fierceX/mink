import { api, type SessionSummary } from './api.ts'
import { emptySession, type HumanInput, type SessionState } from './types.ts'
import { projectMessages } from './workbenchReducer.ts'
import type { AttachmentSnapshot, PendingSubmission } from './sessionDraft.ts'

export function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause)
}

/** The seq a history row carries; the server numbers every row it returns. */
export function rowSeq(row: Record<string, unknown> | undefined): number {
  const seq = row?.['seq']
  return typeof seq === 'number' ? seq : Number.NaN
}

/** Merge history rows into one ascending collection, deduplicated by seq.
 * A row the server did not number (a live commit missing its seq) is kept after
 * the numbered ones instead of being dropped. */
export function mergeRows(
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
export function seed(summary: SessionSummary, rows: readonly Record<string, unknown>[]): SessionState {
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
export function wireInput(input: PendingSubmission): HumanInput {
  return {
    request_id: input.request_id,
    text: input.text,
    target_turn_id: input.target_turn_id,
    attachment_ids: input.attachment_ids,
  }
}

/** The frozen attachment set a submission may consume. */
export function attachmentSnapshot(
  attachmentIds: readonly string[],
  attachmentKeys: readonly string[],
): AttachmentSnapshot {
  // Callers that cannot name one key per id leave the set unproven; guessing
  // from content ids would delete a later copy of the same image.
  if (attachmentIds.length !== attachmentKeys.length) return { known: false }
  return { known: true, keys: [...attachmentKeys] }
}

/** Whether a stored submission is exactly what the reader is sending again. */
export function sameSubmission(
  stored: PendingSubmission,
  text: string,
  targetTurnId: string | null,
  attachmentIds: readonly string[],
  attachmentKeys: readonly string[],
): boolean {
  if (stored.target_turn_id !== targetTurnId || stored.text !== text) return false
  if (stored.attachment_ids.length !== attachmentIds.length) return false
  if (!stored.attachment_ids.every((id, index) => id === attachmentIds[index])) return false
  const snapshot = stored.attachment_snapshot
  if (snapshot === undefined || !snapshot.known) return true
  // An unproven current key list cannot identify a different input; the stored
  // snapshot still names exactly what the accepted request consumed.
  if (attachmentKeys.length !== attachmentIds.length) return true
  return snapshot.keys.length === attachmentKeys.length
    && snapshot.keys.every((key, index) => key === attachmentKeys[index])
}

/** Ask the admission boundary whether it already holds this request. */
export async function receiptState(
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

export type SubmitOutcome =
  | { status: 'accepted' }
  | { status: 'refused'; message: string }
  | { status: 'unknown'; message: string }

/** Reader-facing failure text; undefined means accepted. */
export function failureOf(outcome: SubmitOutcome): string | undefined {
  return outcome.status === 'accepted' ? undefined : outcome.message
}

/** The one wording for a submission whose reception could not be settled. */
export const UNKNOWN_SEND = '无法确认本次输入是否已被服务端接收：稍后在输入队列中查看，或用同一内容重试'
export const UNKNOWN_GUIDE = '无法确认此引导是否已被服务端接收：稍后用同一内容重试，或在输入队列中查看'
