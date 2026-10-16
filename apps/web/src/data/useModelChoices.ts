/**
 * Model vocabulary for one session: the aliases its runtime resolves through and
 * the label of the model in force. It is read from the runtime (the same
 * resolver the terminal uses) so the seat offers exactly what can be switched
 * to, and it is re-read after a switch so the label follows the runtime's own
 * report rather than the request.
 *
 * A read or a switch belongs to the identity it started under: an answer that
 * arrives after the reader switched sessions is dropped, so the seat never
 * shows one session's model while another is open.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import { api, type SessionSummary } from './api.ts'
import { sessionIdentity } from './sessionDraft.ts'

/** One alias and the model it resolves to. */
export interface ModelChoice {
  alias: string
  model: string
}

/** What the workbench knows about a session's model. */
export interface ModelState {
  /** The model in force, as configured. */
  model: string
  /** The model it resolves to. */
  resolved: string
  choices: ModelChoice[]
  /** The read is in flight: the seat shows it rather than an empty menu. */
  loading: boolean
  /** Why the read failed, when it did: the seat offers the retry. */
  error: string | undefined
}

const EMPTY: ModelState = { model: '', resolved: '', choices: [], loading: false, error: undefined }

/** Vocabulary plus a switch the runtime accepted (its response is authoritative). */
export interface ModelVocabulary extends ModelState {
  refresh: () => Promise<void>
  beginSwitch: () => (result: { model: string } | { error: string }) => boolean
}

/**
 * Read one session's model state.
 * @param session - the open session, when one is.
 * @param open - whether its runtime is open (the state is read from the runtime).
 * @returns the state plus a refresher.
 */
export function useModelChoices(session: SessionSummary | undefined, open: boolean): ModelVocabulary {
  const id = session?.id
  const project = session?.project_key
  const identity = session === undefined ? undefined : sessionIdentity(session)
  // The identity is part of the state, so every writer can prove it still owns
  // the seat before it lands.
  const [slot, setSlot] = useState<{ identity: string | undefined; state: ModelState }>(
    () => ({ identity, state: EMPTY }),
  )
  // Reads are ordered: every refresh and every accepted switch takes the next
  // number, and only the newest one may write. Identity alone cannot tell an
  // answer that is still current from one that lost a race — least of all when
  // the reader goes A → B → A and the first A request comes back last.
  const requestSeq = useRef(0)
  const lifecycle = useRef({ identity, epoch: 0 })
  if (lifecycle.current.identity !== identity) {
    lifecycle.current = { identity, epoch: lifecycle.current.epoch + 1 }
  }
  const viewEpoch = lifecycle.current.epoch
  let view = slot
  if (slot.identity !== identity) {
    view = { identity, state: EMPTY }
    setSlot(view)
  }

  const refresh = useCallback(async () => {
    const request = ++requestSeq.current
    const live = (): boolean => request === requestSeq.current
      && lifecycle.current.identity === identity
      && lifecycle.current.epoch === viewEpoch
    if (identity === undefined || !open) {
      if (live()) setSlot(previous => (previous.identity === identity ? { identity, state: EMPTY } : previous))
      return
    }
    const patch = (update: (state: ModelState) => ModelState): void => {
      if (!live()) return
      setSlot(previous => (previous.identity === identity
        ? { identity, state: update(previous.state) }
        : previous))
    }
    patch(state => ({ ...state, loading: true, error: undefined }))
    let response
    try {
      response = await api.modelChoices(id!, project)
    } catch (cause) {
      // A phone that lost one request must not end up with a dead seat: the reason
      // is kept so the menu can say it and offer the retry.
      patch(state => ({
        ...state,
        loading: false,
        error: cause instanceof Error ? cause.message : String(cause),
      }))
      return
    }
    if (response.code !== 200) {
      patch(state => ({
        ...state,
        choices: [],
        loading: false,
        error: response.message || `code ${response.code}`,
      }))
      return
    }
    patch(() => ({ ...response.data, loading: false, error: undefined }))
  }, [id, project, open, identity, viewEpoch])

  useEffect(() => { void refresh() }, [refresh])

  // Reserve the request order when the POST starts. The returned settle
  // function can only write while this exact view and request are still live;
  // A→B→A therefore retires the first A switch along with its old GETs.
  const beginSwitch = useCallback(() => {
    const request = ++requestSeq.current
    const epoch = viewEpoch
    const live = (): boolean => request === requestSeq.current
      && lifecycle.current.identity === identity
      && lifecycle.current.epoch === epoch
    const patch = (update: (state: ModelState) => ModelState): void => {
      if (!live()) return
      setSlot(previous => (previous.identity === identity
        ? { identity, state: update(previous.state) }
        : previous))
    }
    patch(state => ({ ...state, loading: true, error: undefined }))
    return (result: { model: string } | { error: string }): boolean => {
      if (!live()) return false
      if ('error' in result) {
        patch(state => ({ ...state, loading: false, error: result.error }))
        return true
      }
      patch(state => ({ ...state, model: result.model, loading: false, error: undefined }))
      // The server response is authoritative for the accepted label; a fresh
      // read then confirms the runtime's model and available aliases.
      void refresh()
      return true
    }
  }, [identity, viewEpoch, refresh])

  return { ...view.state, refresh, beginSwitch }
}
