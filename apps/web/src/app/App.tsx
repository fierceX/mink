/**
 * Workbench shell: three-column frame with the session sidebar, the
 * conversation and the detail panel, plus the settings dialog. Everything the
 * frame draws comes from the copied surfaces; this file owns the wiring.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { api, type GoalSnapshot, type SessionSummary } from '../data/api.ts'
import { useNow, useSessionCatalog } from '../data/useSessionCatalog.ts'
import {
  clearComposeDraft,
  sessionAttachmentsKey,
  sessionDraftKey,
  sessionPendingKey,
  useSessionDraft,
  writeAttachments,
  writePendingSubmission,
  writeSessionDraft,
  type PendingSubmission,
  type StoredAttachment,
} from '../data/sessionDraft.ts'
import { useLatest } from '../data/useLatest.ts'
import { useModelChoices } from '../data/useModelChoices.ts'
import { useSessionView } from '../data/useSessionView.ts'
import { projectName } from '../data/catalogFilter.ts'
import { SIDEBAR_DEFAULT } from './frame/columns.ts'
import { AppFrame } from './frame/AppFrame.tsx'
import { Conversation } from './conversation/Conversation.tsx'
import { NewConversation } from './conversation/NewConversation.tsx'
import { DetailPanel } from './details/DetailPanel.tsx'
import type { DetailTab } from './details/useDetails.ts'
import { SettingsDialog } from './settings/SettingsDialog.tsx'
import { NewProjectDialog } from './NewProjectDialog.tsx'
import { runCommand } from './commands.ts'
import { EmptyState } from './EmptyState.tsx'
import { NarrowBar } from './NarrowBar.tsx'
import { useReadingChrome } from './shell/useReadingChrome.ts'
import { useVisualViewport } from './shell/useVisualViewport.ts'
import { usePreferences } from './settings/preferences.ts'
import { groupSessions, sessionTitle } from './sidebar/SessionBrowser.tsx'
import { SidebarRoot } from './sidebar/SidebarRoot.tsx'

/** Key that identifies one session inside a project: `cwd:id`. */
function sessionKey(session: SessionSummary): string {
  return `${session.cwd}:${session.id}`
}

/** A conversation that exists only in the browser until its first send. */
interface ComposingProject {
  cwd: string
  name: string
}

/** Restore the composing view from the URL, if this page reloaded into one. */
function initialComposing(): ComposingProject | undefined {
  if (typeof window === 'undefined') return undefined
  const params = new URLSearchParams(window.location.search)
  if (params.has('session')) return undefined
  const cwd = params.get('compose')
  return cwd === null || cwd === '' ? undefined : { cwd, name: projectName(cwd) }
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
  // A failure after the first send has already created the session belongs to
  // that conversation, not to the sidebar: the drawer may be closed when the
  // reader sees it.
  const [sessionNotice, setSessionNotice] = useState<string | undefined>(undefined)
  // The composing view: no server session exists until the first send.
  const [composing, setComposing] = useState<ComposingProject | undefined>(initialComposing)
  const [newProjectOpen, setNewProjectOpen] = useState(false)
  const [newProjectSeed, setNewProjectSeed] = useState('')
  // Session verbs are one at a time: a second click while the first request is
  // in flight would duplicate the session or the delete.
  const pending = useRef(false)
  // First-send requests can outlive the composing view. Their completion may
  // adopt the created session only while that same navigation is still current.
  const navigationEpoch = useRef(0)
  // The session in force now, for completions that resolve after a switch.
  const currentRef = useLatest(current)

  const groups = useMemo(() => groupSessions(catalog.sessions), [catalog.sessions])
  const view = useSessionView(current)
  const viewError = view.error
  const refreshCatalog = catalog.refresh
  // Reconnect on the events that say the network or the tab is back. A
  // connected view only refreshes the catalog; a dropped one also retries its
  // runtime, which is the case a browser reload used to be needed for.
  useEffect(() => {
    const recover = (): void => {
      void refreshCatalog()
      if (current !== undefined && ['disconnected', 'offline', 'closed', 'failed'].includes(view.connection)) {
        view.reconnect()
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
  }, [current, refreshCatalog, view.connection, view.reconnect])
  const previousConnection = useRef(view.connection)
  useEffect(() => {
    if (view.connection === 'connected' && previousConnection.current !== 'connected') {
      void refreshCatalog()
    }
    previousConnection.current = view.connection
  }, [refreshCatalog, view.connection])
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
    setComposing(undefined)
    setCurrent(match)
  }, [catalog.loading, catalog.sessions, current])

  const openSession = useCallback((session: SessionSummary) => {
    navigationEpoch.current += 1
    setCurrent(session)
    setComposing(undefined)
    setNotice(undefined)
    setSessionNotice(undefined)
    // On a phone the list is a drawer: choosing from it closes it, so the
    // conversation is what the reader sees next.
    setNarrowExpanded(false)
    // The session identity lives in the URL, so a reload (or a shared link)
    // returns to the same conversation instead of the empty state.
    const params = new URLSearchParams(window.location.search)
    params.set('session', session.id)
    params.set('project', session.project_key)
    params.delete('compose')
    window.history.replaceState(null, '', `${window.location.pathname}?${params.toString()}`)
  }, [])

  /** Enter the browser-only conversation under one project. */
  const startComposing = useCallback((cwd: string, name: string) => {
    navigationEpoch.current += 1
    setCurrent(undefined)
    setComposing({ cwd, name })
    setNotice(undefined)
    setSessionNotice(undefined)
    setRestorePending(false)
    const params = new URLSearchParams(window.location.search)
    params.delete('session')
    params.delete('project')
    params.set('compose', cwd)
    window.history.replaceState(null, '', `${window.location.pathname}?${params.toString()}`)
  }, [])

  const cancelComposing = useCallback(() => {
    navigationEpoch.current += 1
    setComposing(undefined)
    const params = new URLSearchParams(window.location.search)
    params.delete('compose')
    const query = params.toString()
    window.history.replaceState(null, '', query === '' ? window.location.pathname : `${window.location.pathname}?${query}`)
  }, [])

  const openNewProject = useCallback((seed: string) => {
    setNewProjectSeed(seed)
    setNewProjectOpen(true)
  }, [])

  /**
   * First send: create the session, move the staged attachments into its own
   * store, and admit the turn through the same input queue normal turns use.
   * A refusal before creation keeps the composing view; a failure after it
   * adopts the session and keeps the text as that session's draft.
   */
  const sendFirst = useCallback(async (
    text: string,
    files: readonly File[],
    draftId: string,
    firstRequestId: string,
  ): Promise<string | undefined> => {
    if (composing === undefined) return '未选择项目'
    if (pending.current) return '正在创建会话，请稍候'
    pending.current = true
    const startedAtEpoch = navigationEpoch.current
    const mayAdopt = (): boolean => navigationEpoch.current === startedAtEpoch
    const project = composing
    let created: SessionSummary | undefined
    const adopt = (session: SessionSummary, message: string): void => {
      writeSessionDraft(session, text)
      if (mayAdopt()) {
        clearComposeDraft(project.cwd, { draftId, requestId: firstRequestId, text })
        openSession(session)
        setSessionNotice(message)
      }
      void catalog.refresh()
    }
    try {
      const firstPrompt = text.trim() === '' ? undefined : text
      const response = await api.createSession({
        cwd: project.cwd,
        draftId,
        ...(firstPrompt === undefined ? {} : { firstPrompt }),
      })
      if (response.code !== 200) return response.message || `code ${response.code}`
      created = response.data
      // Attachment upload requires an active server runtime. Opening first also
      // makes a first-send with images follow the same session lifecycle as text.
      const opened = await api.openSession(created.id, created.project_key)
      if (opened.code !== 200) {
        adopt(created, `会话已创建，但打开失败：${opened.message || `code ${opened.code}`}`)
        return undefined
      }
      const attachmentIds: string[] = []
      const storedAttachments: StoredAttachment[] = []
      const attachmentKeys: string[] = []
      for (const file of files) {
        const upload = await api.upload(created.id, file, created.project_key)
        if (upload.code !== 200) {
          adopt(created, `会话已创建，但附件上传失败：${upload.message || `code ${upload.code}`}`)
          return undefined
        }
        attachmentIds.push(upload.data.id)
        const key = `stored-${upload.data.id}-${storedAttachments.length + 1}`
        attachmentKeys.push(key)
        storedAttachments.push({ key, id: upload.data.id, name: file.name })
        // Keep each successful upload recoverable if a later image fails or
        // the browser reloads before the first input reaches the server.
        writeAttachments(sessionAttachmentsKey(created), storedAttachments)
      }
      const input: PendingSubmission = {
        request_id: firstRequestId,
        text,
        target_turn_id: null,
        attachment_ids: attachmentIds,
        ...(attachmentKeys.length === 0 ? {} : { attachment_keys: attachmentKeys }),
      }
      writePendingSubmission(sessionPendingKey(created), input)
      const submitted = await api.submitInput(created.id, {
        request_id: input.request_id,
        text: input.text,
        target_turn_id: input.target_turn_id,
        attachment_ids: input.attachment_ids,
      }, created.project_key)
      writePendingSubmission(sessionPendingKey(created), undefined)
      if (submitted.code !== 200) {
        adopt(created, `会话已创建，但首条消息提交失败：${submitted.message || `code ${submitted.code}`}`)
        return undefined
      }
      writeAttachments(sessionAttachmentsKey(created), [])
      clearComposeDraft(project.cwd, { draftId, requestId: firstRequestId, text })
      if (mayAdopt()) openSession(created)
      void catalog.refresh()
      return undefined
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      if (created !== undefined) {
        adopt(created, `会话已创建，但首条消息结果未知：${message}`)
        return undefined
      }
      return message
    } finally {
      pending.current = false
    }
  }, [catalog, composing, openSession])

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
        setDetailPath(effects.path)
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
              const project = groups[0]
              if (project === undefined) openNewProject('')
              else startComposing(project.cwd, project.name)
            }}
            onAddProject={() => { openNewProject(groups[0]?.cwd ?? '') }}
            onOpenSettings={() => { setSettingsOpen(true) }}
            onOpenSession={openSession}
            onCreateIn={(group) => { startComposing(group.cwd, group.name) }}
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
            {composing !== undefined && current === undefined && !restorePending
              ? (
                <NewConversation
                  key={composing.cwd}
                  cwd={composing.cwd}
                  projectName={composing.name}
                  onFirstSend={sendFirst}
                  onCancel={cancelComposing}
                />
              )
              : restorePending
                ? <div className="mk-pending" data-session-restore-pending />
                : current === undefined
                  ? (
                    <EmptyState
                      project={groups[0]}
                      notice={notice ?? catalog.error}
                      onCreate={(group) => {
                        if (group === undefined) openNewProject('')
                        else startComposing(group.cwd, group.name)
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
                    title={sessionTitle(current)}
                    open={view.open}
                    connection={view.connection}
                    onReconnect={view.reconnect}
                    error={viewError}
                    detailsOpen={rightbarWidth > 0}
                    processDisplay={preferences.process}
                    notice={commandNotice ?? sessionNotice}
                    // Reading chrome is a narrow-layout affordance: a desktop
                    // view must not attach its click gestures at all.
                    chrome={compact ? chrome : undefined}
                    onOpenFile={(path) => {
                      setDetailTab('files')
                      setDetailPath(path)
                      setRightbarWidth(width => (width > 0 ? width : 360))
                    }}
                    draft={draft.text}
                    pendingDraft={view.pendingDraft}
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
                    receiptStamp={view.receiptStamp}
                    onCommand={runSessionCommand}
                    followTick={followTick}
                    onStop={view.stop}
                    chromeHidden={compact && chrome.hidden}
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
      {newProjectOpen && (
        <NewProjectDialog
          seedPath={newProjectSeed}
          onClose={() => { setNewProjectOpen(false) }}
          onReady={(project) => {
            setNewProjectOpen(false)
            startComposing(project.cwd, project.name)
          }}
        />
      )}
    </>
  )
}
