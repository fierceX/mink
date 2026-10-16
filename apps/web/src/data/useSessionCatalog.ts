/**
 * Session catalog state: the discovered sessions the sidebar lists and the
 * open/delete/create actions the shell calls. Deliberately small — live turn
 * state and the event stream belong to the session controller.
 */

import { useCallback, useEffect, useState } from 'react'
import { api, type SessionSummary } from './api.ts'

export interface SessionCatalog {
  sessions: SessionSummary[]
  error: string | undefined
  loading: boolean
  refresh: () => Promise<void>
}

/** Load the session list and keep the list action results in the same state. */
export function useSessionCatalog(): SessionCatalog {
  const [sessions, setSessions] = useState<SessionSummary[]>([])
  const [error, setError] = useState<string | undefined>(undefined)
  const [loading, setLoading] = useState(true)

  const refresh = useCallback(async () => {
    try {
      const response = await api.listSessions()
      if (response.code !== 200) throw new Error(response.message || `code ${response.code}`)
      setSessions(response.data)
      setError(undefined)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  return { sessions, error, loading, refresh }
}

/**
 * A clock that ticks on a fixed cadence, so relative timestamps in a long-lived
 * list do not freeze at the moment the page loaded.
 * @param intervalMs - tick period.
 * @returns epoch ms, refreshed on the cadence.
 */
export function useNow(intervalMs = 60_000): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const timer = window.setInterval(() => {
      setNow(Date.now())
    }, intervalMs)
    return () => {
      window.clearInterval(timer)
    }
  }, [intervalMs])
  return now
}
