/**
 * Workbench shell: three-column frame with the session sidebar, the
 * conversation and the detail panel, plus the settings dialog. Everything the
 * frame draws comes from the copied surfaces; this file owns the wiring.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { api, type GoalSnapshot, type SessionSummary } from '../data/api.ts'
import { useNow, useSessionCatalog } from '../data/useSessionCatalog.ts'
import { sessionDraftKey, useSessionDraft } from '../data/sessionDraft.ts'
import { useLatest } from '../data/useLatest.ts'
import { useModelChoices } from '../data/useModelChoices.ts'
import { useSessionView } from '../data/useSessionView.ts'
import { SIDEBAR_DEFAULT } from './frame/columns.ts'
import { AppFrame } from './frame/AppFrame.tsx'
import { Conversation } from './conversation/Conversation.tsx'
import { DetailPanel } from './details/DetailPanel.tsx'
import type { DetailTab } from './details/useDetails.ts'
import { SettingsDialog } from './settings/SettingsDialog.tsx'
import { NewSessionDialog } from './NewSessionDialog.tsx'
import { runCommand } from './commands.ts'
import { EmptyState } from './EmptyState.tsx'
import { NarrowBar } from './NarrowBar.tsx'
import { useReadingChrome } from './shell/useReadingChrome.ts'
import { useVisualViewport } from './shell/useVisualViewport.ts'
import { usePreferences } from './settings/preferences.ts'
import { groupSessions } from './sidebar/SessionBrowser.tsx'
import { SidebarRoot } from './sidebar/SidebarRoot.tsx'

/** Key that identifies one session inside a project: `cwd:id`. */
function sessionKey(session: SessionSummary): string {
  return `${session.cwd}:${session.id}`
}

export function App() {
  const catalog = useSessionCatalog()
  const now = useNow()
  const [sidebarWidth, setSidebarWidth] = useState(SIDEBAR_DEFAULT)
  // The right track is closed until the reader opens the detail panel.
  const [rightbarWidth, setRightbarWidth] = useState(0)
  const [narrowExpanded, setNarrowExpanded] = useState(false)
  // Mirrors the frame's own breakpoint: a narrow frame draws the sidebar as a
  // drawer, and the centre's header is then the way back into it.
  const [narrow, setNarrow] = useState(false)
  // The URL's session is restored once, after the catalog knows about it; until
  // then the centre stays blank instead of flashing the empty state.
  const restored = useRef(false)
  const [restorePending, setRestorePending] = useState(() =>
    typeof window !== 'undefined' && new URLSearchParams(window.location.search).has('session'))
  const [settingsOpen, setSettingsOpen] = useState(false)
  // A page that reloaded into an older bundle than the server serves half-works in
  // ways that look like product bugs; comparing names makes it say so.
  const [staleBundle, setStaleBundle] = useState(false)
  // The shell follows the visible viewport on every browser, keyboard included.
  useVisualViewport()
  // The detail panel is controlled here so commands can elect a tab.
  const [detailTab, setDetailTab] = useState<DetailTab>('task')
  const [detailPath, setDetailPath] = useState<string | undefined>(undefined)
  const [followTick, setFollowTick] = useState(0)
  const [commandNotice, setCommandNotice] = useState<string | undefined>(undefined)
  // Narrow viewports read in a chrome-free mode; the media query below decides
  // whether that layout is in play at all.
  const [compact, setCompact] = useState(() =>
    typeof matchMedia === 'function' && matchMedia('(max-width: 767px)').matches)
  const { preferences, update } = usePreferences()
  const [current, setCurrent] = useState<SessionSummary | undefined>(undefined)
  const [notice, setNotice] = useState<string | undefined>(undefined)
  // The new-task dialog: the working directory is typed here, with the
  // discovered projects offered as candidates. It stays open (and keeps what
  // was typed) until the create succeeds or the reader cancels.
  const [newTaskCwd, setNewTaskCwd] = useState<string | undefined>(undefined)
  const [newTaskError, setNewTaskError] = useState<string | undefined>(undefined)
  const [newTaskBusy, setNewTaskBusy] = useState(false)
  // Session verbs are one at a time: a second click while the first request is
  // in flight would duplicate the session or the delete.
  const pending = useRef(false)
  // The session in force now, for completions that resolve after a switch.
  const currentRef = useLatest(current)

  const groups = useMemo(() => groupSessions(catalog.sessions), [catalog.sessions])
  const view = useSessionView(current)
  const viewError = view.error
  // The draft lives with the session identity, not with the mounted composer.
  const draft = useSessionDraft(current === undefined ? undefined : sessionDraftKey(current))
  // The model vocabulary comes from the session's runtime (the same resolver the
  // CLI uses), so the seat offers exactly what can be switched to.
  const modelVocabulary = useModelChoices(current, view.open)
  const beginModelSwitch = modelVocabulary.beginSwitch
  const switchModel = useCallback((model: string) => {
    const target = current
    if (target === undefined) return
    const settle = beginModelSwitch()
    void (async () => {
      try {
        const response = await api.setModel(target.id, model, target.project_key)
        if (response.code !== 200) {
          const message = response.message || `code ${response.code}`
          if (settle({ error: message })) setCommandNotice(`切换模型失败：${message}`)
          return
        }
        // The seat takes the runtime's accepted label, then rereads the live
        // vocabulary. The reservation is checked against the initiating view.
        settle({ model: response.data.label !== '' ? response.data.label : model })
      } catch (cause) {
        const message = cause instanceof Error ? cause.message : String(cause)
        if (settle({ error: message })) setCommandNotice(`切换模型失败：${message}`)
      }
    })()
  }, [current, beginModelSwitch])

  // Chrome steps aside only in the narrow layout, with a session open and no
  // overlay covering it; hiding is presentation only.
  const chrome = useReadingChrome(compact && current !== undefined && rightbarWidth === 0 && !settingsOpen)
  useEffect(() => {
    if (typeof matchMedia !== 'function') return
    const query = matchMedia('(max-width: 767px)')
    const onChange = (): void => { setCompact(query.matches) }
    query.addEventListener('change', onChange)
    return () => { query.removeEventListener('change', onChange) }
  }, [])

  useEffect(() => {
    let live = true
    void api.version().then((response) => {
      if (!live || response.code !== 200) return
      const served = response.data.bundle
      const loaded = [...document.querySelectorAll('script[src]')]
        .map(node => node.getAttribute('src') ?? '')
        .find(src => src.includes('/assets/'))
      if (served === '' || loaded === undefined || loaded.includes(served)) return
      setStaleBundle(true)
    }).catch(() => {})
    return () => { live = false }
  }, [])

  useEffect(() => {
    if (restored.current || current !== undefined) return
    const params = new URLSearchParams(window.location.search)
    const id = params.get('session')
    if (id === null) {
      setRestorePending(false)
      return
    }
    const project = params.get('project')
    const match = catalog.sessions.find(session => session.id === id
      && (project === null || session.project_key === project))
    // The catalog may not have answered yet: wait for it rather than giving up.
    if (match === undefined) {
      if (catalog.loading) return
      setRestorePending(false)
      return
    }
    restored.current = true
    setRestorePending(false)
    setCurrent(match)
  }, [catalog.loading, catalog.sessions, current])

  const openSession = useCallback((session: SessionSummary) => {
    setCurrent(session)
    setNotice(undefined)
    // On a phone the list is a drawer: choosing from it closes it, so the
    // conversation is what the reader sees next.
    setNarrowExpanded(false)
    // The session identity lives in the URL, so a reload (or a shared link)
    // returns to the same conversation instead of the empty state.
    const params = new URLSearchParams(window.location.search)
    params.set('session', session.id)
    params.set('project', session.project_key)
    window.history.replaceState(null, '', `${window.location.pathname}?${params.toString()}`)
  }, [])

  const createSession = useCallback(async (cwd: string, name: string) => {
    if (pending.current) return
    pending.current = true
    setNewTaskBusy(true)
    setNewTaskError(undefined)
    try {
      // Let the server name the session when the reader did not; a name that is
      // already taken inside the project resolves to that session.
      const response = await api.createSession(cwd, name === '' ? undefined : name)
      if (response.code !== 200) {
        setNewTaskError(response.message || `code ${response.code}`)
        return
      }
      setNewTaskCwd(undefined)
      const created = response.data
      if (created !== undefined && created.id !== undefined) openSession(created)
      await catalog.refresh()
    } catch (cause) {
      setNewTaskError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      pending.current = false
      setNewTaskBusy(false)
    }
  }, [catalog, openSession])

  const deleteSession = useCallback(async (session: SessionSummary) => {
    if (pending.current) return
    pending.current = true
    try {
      const response = await api.deleteSession(session.id, session.project_key)
      if (response.code !== 200) throw new Error(response.message || `code ${response.code}`)
      if (current !== undefined && sessionKey(current) === sessionKey(session)) setCurrent(undefined)
      await catalog.refresh()
    } catch (cause) {
      setNotice(cause instanceof Error ? cause.message : String(cause))
    } finally {
      pending.current = false
    }
  }, [catalog, current])

  /** One goal control: compare-and-set on the snapshot's revision, always. */
  const runGoalAction = useCallback(async (
    action: 'pause' | 'resume' | 'cancel' | 'clear',
    goal: GoalSnapshot,
  ): Promise<string | undefined> => {
    if (current === undefined) return '会话未打开'
    // A transport failure must reach the reader as an error line: the bar's
    // promise contract is "resolve to a message", never a rejected promise.
    try {
      const response = await api.goalAction(current.id, action, {
        goal_id: goal.id,
        revision: goal.revision,
      }, current.project_key)
      if (response.code !== 200) return response.message || `code ${response.code}`
      // The action changed the goal: fold the authoritative answer in now
      // instead of waiting for the lifecycle event to arrive.
      view.refreshGoal()
      return undefined
    } catch (cause) {
      return cause instanceof Error ? cause.message : String(cause)
    }
  }, [current, view.refreshGoal])

  /** Run a slash command against the open session and apply its effects. */
  const runSessionCommand = useCallback(async (text: string): Promise<{ message: string; ok: boolean }> => {
    const target = current
    if (target === undefined) return { message: '会话未打开', ok: false }
    const result = await runCommand(text, {
      session: target,
      state: view.state,
      createGoal: async (body) => {
        const response = await api.createGoal(target.id, body, target.project_key)
        // The goal bar is the feedback for a created goal: no extra notice.
        return response.code === 200
          ? { message: '', ok: true, effects: { openDetails: 'goal', refreshGoal: true } }
          : { message: `创建目标失败：${response.message || `code ${response.code}`}`, ok: false }
      },
    })
    // The command ran against the session that owned the composer: a reply that
    // arrives after a switch must not move the panel or say anything about the
    // session now on screen.
    if (currentRef.current?.id !== target.id || currentRef.current?.project_key !== target.project_key) {
      return { message: result.message, ok: result.ok }
    }
    // A command typed here answers in the composer's own line, right beside the
    // input; the notice line is for failures raised outside it (goal actions, a
    // refused model switch), so the same message never appears twice.
    setCommandNotice(undefined)
    if (result.ok) {
      const effects = result.effects
      if (effects?.openDetails !== undefined) {
        setDetailTab(effects.openDetails)
        setDetailPath(undefined)
        setRightbarWidth(width => (width > 0 ? width : 360))
      }
      if (effects?.follow === true) setFollowTick(tick => tick + 1)
      // A goal command changed the goal: re-read it so the bar and the open
      // detail tab show the authoritative revision without waiting for the
      // lifecycle event.
      if (effects?.refreshGoal === true) view.refreshGoal()
      // `/model`, `/flash` and `/pro` change the session's model in the runtime:
      // re-read it so the seat shows the runtime's own answer.
      if (effects?.refreshModel === true) void modelVocabulary.refresh()
    }
    return { message: result.message, ok: result.ok }
  }, [current, currentRef, modelVocabulary, view.state])

  return (
    <>
      <AppFrame
        sidebarWidth={sidebarWidth}
        onSidebarWidthChange={setSidebarWidth}
        rightbarWidth={rightbarWidth}
        onRightbarWidthChange={setRightbarWidth}
        narrowExpanded={narrowExpanded}
        onNarrowClose={() => { setNarrowExpanded(false) }}
        onNarrowChange={setNarrow}
        sidebar={(width, collapsed) => (
          <SidebarRoot
            collapsed={collapsed}
            width={width}
            groups={groups}
            selectedKey={current === undefined ? undefined : sessionKey(current)}
            now={now}
            notice={notice}
            onToggle={() => {
              if (sidebarWidth === 0) setSidebarWidth(SIDEBAR_DEFAULT)
              else setSidebarWidth(0)
              setNarrowExpanded(sidebarWidth === 0)
            }}
            onNewSession={() => {
              setNewTaskError(undefined)
              setNewTaskCwd(groups[0]?.cwd ?? '')
            }}
            onOpenSettings={() => { setSettingsOpen(true) }}
            onOpenSession={openSession}
            onCreateIn={(group) => {
              setNewTaskError(undefined)
              setNewTaskCwd(group.cwd)
            }}
            onDeleteSession={(session) => { void deleteSession(session) }}
          />
        )}
        center={(
          <>
            {/* On a phone the sidebar is a drawer, and the empty state has no
                header of its own: the bar is the one way in that always exists. */}
            {staleBundle && (
              <div className="mk-stale" role="status">
                页面版本比服务端旧，部分功能可能不工作。
                <button type="button" onClick={() => { window.location.reload() }}>刷新</button>
              </div>
            )}
            {/* Without a session there is no header to carry these, so the phone
                gets its own bar; with one, the header's own row holds them and a
                second bar would only cost the reader height. */}
            {narrow && !chrome.hidden && current === undefined && (
              <NarrowBar
                onOpenSidebar={() => { setNarrowExpanded(true) }}
                onOpenSettings={() => { setSettingsOpen(true) }}
              />
            )}
            {restorePending
              ? <div className="mk-pending" data-session-restore-pending />
              : current === undefined
                ? (
                  <EmptyState
                    project={groups[0]}
                    notice={notice ?? catalog.error}
                    onCreate={(group) => {
                      setNewTaskError(undefined)
                      setNewTaskCwd(group?.cwd ?? '')
                    }}
                  />
                )
                : (
                  <Conversation
                    // One session identity, one view: a switch must not carry the
                    // draft, attachments or reading position of another over.
                    key={`${current.project_key}:${current.id}`}
                    state={view.state}
                    session={current}
                    title={current.title ?? current.alias ?? current.id}
                    open={view.open}
                    error={viewError}
                    detailsOpen={rightbarWidth > 0}
                    processDisplay={preferences.process}
                    notice={commandNotice}
                    chrome={chrome}
                    onOpenFile={(path) => {
                      setDetailTab('files')
                      setDetailPath(path)
                      setRightbarWidth(width => (width > 0 ? width : 360))
                    }}
                    draft={draft.text}
                    onDraftChange={draft.write}
                    // The runtime's own report wins over the transcript's echo: a
                    // switch is confirmed by re-reading it, not by the request.
                    modelLabel={modelVocabulary.model !== '' ? modelVocabulary.model : undefined}
                    modelChoices={modelVocabulary.choices}
                    modelLoading={modelVocabulary.loading}
                    modelError={modelVocabulary.error}
                    onModelChange={switchModel}
                    onRetryOpen={view.retry}
                    onOpenArtifact={(id) => {
                      setDetailTab('outputs')
                      setDetailPath(id)
                      setRightbarWidth(width => (width > 0 ? width : 360))
                    }}
                    hasOlder={view.hasOlder}
                    onLoadOlder={view.loadOlder}
                    onGoalAction={runGoalAction}
                    onToggleDetails={() => { setRightbarWidth(360) }}
                    onSend={view.send}
                    onGuide={view.guide}
                    onReceiptChanged={view.mergeReceipt}
                    onCommand={runSessionCommand}
                    followTick={followTick}
                    onStop={view.stop}
                    chromeHidden={chrome.hidden}
                    onRevealChrome={chrome.reveal}
                    onOpenSidebar={narrow ? () => { setNarrowExpanded(true) } : undefined}
                    onOpenSettings={() => { setSettingsOpen(true) }}
                  />
                )}
          </>
        )}
        rightbar={current === undefined ? undefined : (width) => (
          <DetailPanel
            session={current}
            width={width}
            tab={detailTab}
            path={detailPath}
            state={view.state}
            onTabChange={(next) => { setDetailTab(next); setDetailPath(undefined) }}
            onPathChange={setDetailPath}
            onClose={() => { setRightbarWidth(0) }}
          />
        )}
      />
      {settingsOpen && (
        <SettingsDialog
          preferences={preferences}
          onChange={update}
          onClose={() => { setSettingsOpen(false) }}
        />
      )}
      {newTaskCwd !== undefined && (
        <NewSessionDialog
          projects={groups.map(group => ({ cwd: group.cwd, name: group.name }))}
          seedCwd={newTaskCwd}
          busy={newTaskBusy}
          error={newTaskError}
          onClose={() => {
            if (newTaskBusy) return
            setNewTaskCwd(undefined)
          }}
          onCreate={(cwd, name) => { void createSession(cwd, name) }}
        />
      )}
    </>
  )
}
