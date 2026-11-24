import { useEffect } from 'react'
import type { SessionSummary } from '../data/api.ts'
import type { SessionConnection } from '../data/useSessionView.ts'
import { sessionTitle } from './sidebar/SessionBrowser.tsx'

export function useWorkbenchLifecycle({
  current,
  composingName,
  connection,
  running,
  refreshCatalog,
  checkVersion,
  reconnect,
  stop,
  toggleSidebar,
}: {
  current: SessionSummary | undefined
  composingName: string | undefined
  connection: SessionConnection
  running: boolean
  refreshCatalog: () => Promise<unknown>
  checkVersion: () => void
  reconnect: () => void
  stop: () => void
  toggleSidebar: () => void
}): void {
  // Sync document title to the active conversation or compose draft.
  useEffect(() => {
    if (current !== undefined) {
      document.title = `${sessionTitle(current)} — Mink`
    } else if (composingName !== undefined) {
      document.title = `新对话 (${composingName}) — Mink`
    } else {
      document.title = 'Mink'
    }
  }, [current, composingName])

  // Global workbench shortcuts: Cmd/Ctrl+B toggles the sidebar, Cmd/Ctrl+. interrupts.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.isComposing || event.defaultPrevented) return
      const mod = event.metaKey || event.ctrlKey
      if (!mod || event.altKey) return
      if (!event.shiftKey && event.key.toLowerCase() === 'b') {
        event.preventDefault()
        toggleSidebar()
        return
      }
      if (event.key === '.' && running) {
        event.preventDefault()
        stop()
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => { window.removeEventListener('keydown', onKeyDown) }
  }, [running, stop, toggleSidebar])

  // Reconnect on the events that say the network or the tab is back.
  useEffect(() => {
    const recover = (): void => {
      void refreshCatalog()
      checkVersion()
      if (current !== undefined && ['disconnected', 'offline', 'closed', 'failed'].includes(connection)) {
        reconnect()
      }
    }
    const onVisibility = (): void => {
      if (document.visibilityState === 'visible') recover()
    }
    window.addEventListener('online', recover)
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      window.removeEventListener('online', recover)
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [checkVersion, current, refreshCatalog, connection, reconnect])
}
