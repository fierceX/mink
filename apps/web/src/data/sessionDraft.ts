/**
 * Per-session draft: the text a reader has typed belongs to one session
 * identity, so switching sessions (or reloading the page) neither leaks it into
 * another session nor loses it. Storage is optional — a browser that refuses
 * localStorage still gets an in-memory draft for the mounted view.
 *
 * The key is read synchronously: a switch returns the destination draft in the
 * same render that changes the key, so a composer mounted for the destination
 * identity starts from its own text instead of an effect-delayed copy.
 */

import { useCallback, useState } from 'react'
import type { SessionSummary } from './api.ts'
import { requestId } from './request-id.ts'
import type { HumanInput } from './types.ts'

/** Stable identity of one session across catalog refreshes. */
export function sessionIdentity(session: Pick<SessionSummary, 'project_key' | 'id'>): string {
  return `${session.project_key}:${session.id}`
}

/** Storage key for one session's draft, matching the legacy workbench's shape. */
export function sessionDraftKey(session: Pick<SessionSummary, 'project_key' | 'id'>): string {
  return `mink.draft:${sessionIdentity(session)}`
}

/** Read a stored draft, tolerating a browser without storage. */
function readDraft(key: string): string {
  try {
    return localStorage.getItem(key) ?? ''
  } catch {
    return ''
  }
}

/**
 * Hold one session's draft. The text is derived from the key during render: a
 * new key is a new draft, immediately, so no intermediate render can hand the
 * previous session's text to the next session's composer.
 * @param key - the session's draft key.
 * @param initial - draft to fall back to when nothing is stored.
 * @returns the draft and the only writer for it.
 */
export function useSessionDraft(key: string | undefined, initial = ''): {
  text: string
  write: (text: string) => void
} {
  const read = (): string => (key === undefined ? initial : readDraft(key) || initial)
  const [draft, setDraft] = useState(() => ({ key, text: read() }))
  // Adjusting state during render is React's own pattern for a derived value:
  // the adjustment re-renders before children mount, so the composer for the
  // destination identity never sees the previous draft.
  let current = draft
  if (draft.key !== key) {
    current = { key, text: read() }
    setDraft(current)
  }

  const write = useCallback((next: string) => {
    // The writer always belongs to the key of the render that produced it; the
    // value is the whole draft, so the key and the text stay in step.
    setDraft({ key, text: next })
    if (key === undefined) return
    try {
      localStorage.setItem(key, next)
    } catch {
      // Storage is optional; the in-memory draft still works.
    }
  }, [key])

  return { text: current.text, write }
}

/** Storage key for one project's unsaved new conversation. */
export function composeDraftKey(cwd: string): string {
  return `mink.compose:${cwd}`
}

/** The unsaved conversation's stable id, text and first-submit request id. */
export interface ComposeDraft {
  draftId: string
  text: string
  requestId: string
}

/** Generate a draft id that survives retries and page reloads. */
export function newComposeDraftId(): string {
  const cryptoApi = globalThis.crypto
  if (typeof cryptoApi?.randomUUID === 'function') return `draft-${cryptoApi.randomUUID()}`
  return `draft-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`
}

/** Read one project's composing draft; a missing record gets a fresh id. */
export function readComposeDraft(cwd: string): ComposeDraft {
  try {
    const raw = localStorage.getItem(composeDraftKey(cwd))
    if (raw !== null) {
      const parsed: unknown = JSON.parse(raw)
      if (typeof parsed === 'object' && parsed !== null) {
        const draftId = (parsed as { draftId?: unknown }).draftId
        const text = (parsed as { text?: unknown }).text
        const request = (parsed as { requestId?: unknown }).requestId
        if (typeof draftId === 'string' && draftId !== '' && typeof text === 'string') {
          return {
            draftId,
            text,
            requestId: typeof request === 'string' && request !== '' ? request : requestId(),
          }
        }
      }
    }
  } catch {
    // Storage is optional; a fresh in-memory id still works.
  }
  return { draftId: newComposeDraftId(), text: '', requestId: requestId() }
}

/** Persist a composing draft; the text and id always travel together. */
export function writeComposeDraft(cwd: string, draft: ComposeDraft): void {
  try {
    localStorage.setItem(composeDraftKey(cwd), JSON.stringify(draft))
  } catch {
    // Storage is optional.
  }
}

/** Drop the composing draft once the conversation has been created. */
export function clearComposeDraft(cwd: string): void {
  try {
    localStorage.removeItem(composeDraftKey(cwd))
  } catch {
    // Storage is optional.
  }
}

/** Persist a draft directly for a session that has just been created. */
export function writeSessionDraft(session: Pick<SessionSummary, 'project_key' | 'id'>, text: string): void {
  try {
    localStorage.setItem(sessionDraftKey(session), text)
  } catch {
    // Storage is optional.
  }
}

/** Storage key for one session's drafted attachment ids. */
export function sessionAttachmentsKey(session: Pick<SessionSummary, 'project_key' | 'id'>): string {
  return `mink.attachments:${session.project_key}:${session.id}`
}

/** One stored draft attachment: the id is what the server knows, the name is copy. */
export interface StoredAttachment {
  id: string
  name: string
}

/** Read the stored attachment list for a session. */
export function readAttachments(key: string): StoredAttachment[] {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return []
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    return parsed.flatMap(entry => {
      if (typeof entry !== 'object' || entry === null) return []
      const id = (entry as { id?: unknown }).id
      const name = (entry as { name?: unknown }).name
      return typeof id === 'string' && typeof name === 'string' ? [{ id, name }] : []
    })
  } catch {
    return []
  }
}

/** Persist the drafted attachment list; storage is optional. */
export function writeAttachments(key: string, entries: readonly StoredAttachment[]): void {
  try {
    localStorage.setItem(key, JSON.stringify(entries))
  } catch {
    // Storage is optional.
  }
}

/** Storage key for one session's in-flight (uncertain) submission. */
export function sessionPendingKey(session: Pick<SessionSummary, 'project_key' | 'id'>): string {
  return `mink.pending:${session.project_key}:${session.id}`
}

/**
 * Read the submission this session left unresolved, if any: the request id
 * binds the words to one input, so a retry after a switch or a reload is the
 * same submission to the server's admission boundary instead of a second one.
 */
export function readPendingSubmission(key: string): HumanInput | undefined {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return undefined
    const parsed: unknown = JSON.parse(raw)
    if (typeof parsed !== 'object' || parsed === null) return undefined
    const entry = parsed as Partial<HumanInput>
    if (typeof entry.request_id !== 'string' || entry.request_id === '') return undefined
    if (typeof entry.text !== 'string') return undefined
    if (entry.target_turn_id !== null && typeof entry.target_turn_id !== 'string') return undefined
    if (!Array.isArray(entry.attachment_ids)) return undefined
    if (!entry.attachment_ids.every(id => typeof id === 'string')) return undefined
    return {
      request_id: entry.request_id,
      text: entry.text,
      target_turn_id: entry.target_turn_id,
      attachment_ids: [...entry.attachment_ids],
    }
  } catch {
    return undefined
  }
}

/** Persist one unresolved submission; `undefined` removes the record. */
export function writePendingSubmission(key: string, input: HumanInput | undefined): void {
  try {
    if (input === undefined) localStorage.removeItem(key)
    else localStorage.setItem(key, JSON.stringify(input))
  } catch {
    // Storage is optional.
  }
}
