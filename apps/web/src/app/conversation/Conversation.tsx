/**
 * Center column: the conversation surface. Header (title + run state), the
 * transcript scroller and the composer seat are laid out with the copied
 * ConversationRoot stylesheet; the blank session shows the hero instead of a
 * transcript.
 */

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { MarkdownDelegateProvider } from '../../ui/primitives/markdown/MarkdownDelegate.tsx'
import type { SessionSummary } from '../../data/api.ts'
import { Button, IconPanelLeftOutlineRegular, StateDot, Tooltip } from '../../ui/primitives/index.ts'
import type { SessionState } from '../../data/types.ts'
import type { ProcessDisplay } from '../settings/preferences.ts'
import type { InputReceipt } from '../../data/types.ts'
import { PendingInputs } from './PendingInputs.tsx'
import { useAttachments } from './useAttachments.ts'
import { Composer } from './Composer.tsx'
import { MinkMark } from '../brand/MinkMark.tsx'
import { IconChevronDownOutlineRegular, IconSettingsOutlineRegular } from '../../ui/primitives/index.ts'
import { Transcript } from './Transcript.tsx'
import { TurnNavigator } from './TurnNavigator.tsx'
import { StatsPills } from './StatsPills.tsx'
import navCss from './TurnNavigator.module.css'
import { GoalBar } from './GoalBar.tsx'
import type { GoalSnapshot } from '../../data/api.ts'
import type { ReadingChrome } from '../shell/useReadingChrome.ts'
import { useReadingScroll } from '../shell/useReadingScroll.ts'
import chatCss from './ChatView.module.css'
import expandCss from '../details/ExpandButton.module.css'
import rootCss from './ConversationRoot.module.css'
import heroCss from './HeroShell.module.css'

export interface ConversationProps {
  state: SessionState | undefined
  title: string
  /** Live session open in the server runtime; sending requires it. */
  open: boolean
  /** The session the composer and the pending strip act on. */
  session: SessionSummary | undefined
  /** Load/open/interrupt failure reported by the session view. */
  error: string | undefined
  /** Whether the right detail panel is showing. */
  detailsOpen: boolean
  onToggleDetails: () => void
  /** How much of each turn's process the transcript shows. */
  processDisplay: ProcessDisplay
  /** Queue a fresh turn; resolves to an error message, or undefined. */
  onSend: (text: string, attachmentIds: readonly string[]) => Promise<string | undefined>
  /** Queue guidance for the running turn. */
  onGuide: (text: string, turnId: string, attachmentIds: readonly string[]) => Promise<string | undefined>
  /** Fold a receipt the pending strip changed back into the projection. */
  onReceiptChanged: (receipt: InputReceipt) => void
  /** Run one slash command for this session. */
  onCommand: (text: string) => Promise<{ message: string; ok: boolean }>
  /** Bumped when a command asks the transcript to return to its tail. */
  followTick: number
  /** Message from the last command, shown under the header. */
  notice: string | undefined
  /** Mobile reading chrome, when the shell runs in its narrow layout. */
  chrome?: ReadingChrome | undefined
  /** Open one changed file in the detail panel's file tab. */
  onOpenFile: (path: string) => void
  /** Open one spilled tool output in the detail panel's output tab. */
  onOpenArtifact: (id: string) => void
  /** Try the session open again after a failure (another process may hold it). */
  onRetryOpen: () => void
  /** Open the sidebar drawer; absent when the sidebar has its own column. */
  onOpenSidebar?: (() => void) | undefined
  /** This session's stored draft, restored into the composer. */
  draft: string
  /** Persist the composer's draft with the session. */
  onDraftChange: (text: string) => void
  /** The model in force, as the session's runtime reports it. */
  modelLabel?: string | undefined
  /** Alias choices the session's runtime honours. */
  modelChoices: readonly { alias: string; model: string }[]
  /** The vocabulary read is in flight. */
  modelLoading?: boolean | undefined
  /** Why the vocabulary read failed, when it did. */
  modelError?: string | undefined
  /** Switch the session's model to one of those aliases. */
  onModelChange: (model: string) => void
  /** The chrome is stepped aside for reading. */
  chromeHidden?: boolean | undefined
  /** Bring it back from the reading stack. */
  onRevealChrome?: (() => void) | undefined
  /** Open the settings panel. */
  onOpenSettings?: (() => void) | undefined
  /** Whether older turns remain before the loaded window. */
  hasOlder: boolean
  /** Load one more page of history, keeping the reading position. */
  onLoadOlder: () => Promise<boolean>
  /** Run one revision-checked goal action; resolves to a refusal message. */
  onGoalAction: (
    action: 'pause' | 'resume' | 'cancel' | 'clear',
    goal: GoalSnapshot,
  ) => Promise<string | undefined>
  onStop: () => void
}

/** A reader this close to the end counts as following the tail. */
const TAIL_SLACK = 24

/** Follow the transcript tail while the session is at its end. */
function useStickToBottom(
  state: SessionState | undefined,
  element: React.RefObject<HTMLDivElement | null>,
  followTick: number,
) {
  const pinned = useRef(true)
  // Whether the transcript is at its tail is visible to the reader (the
  // return-to-latest control), so the ref is mirrored into state.
  const [following, setFollowing] = useState(true)
  const length = state?.items.length ?? 0
  useEffect(() => {
    const box = element.current
    if (box === null) return
    const onScroll = () => {
      const atTail = box.scrollHeight - box.scrollTop - box.clientHeight < 80
      pinned.current = atTail
      setFollowing(atTail)
    }
    box.addEventListener('scroll', onScroll)
    return () => {
      box.removeEventListener('scroll', onScroll)
    }
  }, [element])
  useEffect(() => {
    // An explicit follow request re-pins before it scrolls: the reader asked for
    // the tail, so the gesture wins over the position they had scrolled to.
    if (followTick > 0) {
      pinned.current = true
      setFollowing(true)
    }
    const box = element.current
    if (box === null || !pinned.current) return
    box.scrollTop = box.scrollHeight
  }, [element, length, followTick])

  const follow = useCallback(() => {
    pinned.current = true
    setFollowing(true)
    const box = element.current
    if (box !== null) box.scrollTop = box.scrollHeight
  }, [element])

  /** Jump to one turn: reading position is deliberate, so following stops. */
  const jump = useCallback((key: string) => {
    const box = element.current
    if (box === null) return
    const row = box.querySelector<HTMLElement>(`[data-turn-key="${CSS.escape(key)}"]`)
    if (row === null) return
    pinned.current = false
    setFollowing(false)
    row.scrollIntoView({ block: 'start' })
  }, [element])

  return { following, follow, jump }
}

/**
 * The server refuses to open a session another process holds by lease; a reader
 * can do something about that, so the card names the state and offers the retry.
 */
function readerReason(message: string): string {
  if (/locked by another process/i.test(message)) {
    return '此会话正被另一个进程占用（例如桌面端终端）；在那边结束后重试即可。'
  }
  return message
}

/** Why a session could not be opened, with the one action that helps. */
function OpenFailure({ reason, onRetry }: { reason: string; onRetry: () => void }) {
  return (
    <div className={chatCss.openFailure} data-open-failure="" role="status">
      <span className={chatCss.openFailureTitle}>无法打开此会话</span>
      <span className={chatCss.openFailureBody}>{readerReason(reason)}</span>
      <Button size="sm" variant="outline" onClick={onRetry}>重试</Button>
    </div>
  )
}

/**
 * Render the conversation column.
 * @param props.state - projected session state, or undefined before it loads.
 * @param props.title - session title for the header.
 * @param props.open - whether the server runtime holds the session open.
 * @param props.onSend - submit a turn.
 * @param props.onStop - interrupt the running turn.
 * @returns the conversation element tree.
 */
export function Conversation({
  state, title, session, open, error, detailsOpen, onToggleDetails, processDisplay,
  onSend, onGuide, onReceiptChanged, onCommand, followTick, notice, chrome,
  hasOlder, onLoadOlder, onGoalAction, onOpenFile, onOpenArtifact, onRetryOpen, onOpenSidebar, draft, onDraftChange,
  modelLabel, modelChoices, modelLoading, modelError, onModelChange, onStop,
  chromeHidden: chromeHiddenProp, onRevealChrome, onOpenSettings,
}: ConversationProps) {
  const scroller = useRef<HTMLDivElement>(null)
  const [loadingOlder, setLoadingOlder] = useState(false)
  const reading = useStickToBottom(state, scroller, followTick)
  useReadingScroll(chrome, scroller)
  // The reading controls keep clear of the input card, so its height is published
  // as it changes (the goal bar and the card both move it). A callback ref, because
  // the seat only exists once the session has a transcript to read.
  const seatObserver = useRef<ResizeObserver | null>(null)
  const seatRef = useCallback((node: HTMLDivElement | null) => {
    seatObserver.current?.disconnect()
    seatObserver.current = null
    if (node === null) {
      document.documentElement.style.removeProperty('--mk-seat-height')
      return
    }
    const publish = (): void => {
      document.documentElement.style.setProperty('--mk-seat-height', `${Math.round(node.getBoundingClientRect().height)}px`)
    }
    publish()
    const observer = new ResizeObserver(publish)
    observer.observe(node)
    seatObserver.current = observer
  }, [])

  // Image capability comes from the session's own snapshot; without one the
  // composer refuses files instead of uploading them.
  const imageLimits = state?.imageLimits ?? null
  const attachments = useAttachments(session, imageLimits)
  const items = state?.items ?? []
  const running = state?.running === true
  const blank = items.length === 0
  const workState = state?.workState ?? 'idle'
  // Only inputs the session still owes an answer for are listed; applied and
  // withdrawn receipts stay in the projection but leave the composer alone. A
  // running turn is not a reason to keep the chrome: reading a streaming answer is
  // the common case, and one tap (or the corner control) brings the stop button
  // back. Queued guidance still pins it — that is an action the reader just took.
  const chromeHidden = (chrome?.hidden ?? false) && (state?.inputs ?? []).every(
    receipt => receipt.status === 'applied' || receipt.status === 'withdrawn',
  )
  // Hiding the chrome hands the input region's height to the transcript, and that
  // height change lands mid-gesture: this record keeps the reader's distance from
  // the live end across it, so the taller reading area shows the same lines instead
  // of jumping — and, with the flow change, of losing them entirely.
  const metrics = useRef({ rawGap: 0, scrollTop: 0 })
  useEffect(() => {
    const box = scroller.current
    if (box === null) return
    metrics.current = { rawGap: box.scrollHeight - box.scrollTop - box.clientHeight, scrollTop: box.scrollTop }
  })
  useLayoutEffect(() => {
    const box = scroller.current
    if (box === null) return
    // A reader inside the history keeps their line: the content did not move, only
    // the viewport grew, so the offset is left alone (hiding the chrome changes the
    // box's bottom padding, which sits below the content and moves nothing). A
    // reader at the end is pinned there, or the space the chrome gives back would
    // show as emptiness under the last turn.
    if (metrics.current.rawGap <= TAIL_SLACK && box.scrollHeight > box.clientHeight) {
      box.scrollTop = box.scrollHeight - box.clientHeight
    }
    metrics.current = {
      rawGap: box.scrollHeight - box.scrollTop - box.clientHeight,
      scrollTop: box.scrollTop,
    }
  }, [chromeHidden])

  const pending = session === undefined
    ? null
    : (
      <PendingInputs
        receipts={(state?.inputs ?? []).filter(receipt => receipt.status !== 'applied' && receipt.status !== 'withdrawn')}
        session={session}
        onChanged={onReceiptChanged}
      />
    )

  return (
    <div className={rootCss.root} data-chrome={chromeHidden ? 'hidden' : 'shown'}>
      <header className={rootCss.header} data-chrome-hidden={chromeHidden || undefined}>
        <div className={rootCss.titleRow}>
          {/* A drawer covers its own toggle, so the centre carries one too. */}
          {onOpenSidebar !== undefined && (
            <Tooltip label="展开侧栏" side="bottom" delayMs={400}>
              <Button
                size="sm"
                className={expandCss.button}
                aria-label="展开侧栏"
                data-sidebar-open-button
                onClick={onOpenSidebar}
              >
                <IconPanelLeftOutlineRegular className={expandCss.icon} />
              </Button>
            </Tooltip>
          )}
          <div className={rootCss.titleCluster}>
            <span className={rootCss.crumbCurrent}>{title}</span>
            <StateDot state={running ? 'ongoing' : workState === 'error' ? 'error' : 'idle'} />
          </div>
          {onOpenSettings !== undefined && (
            <Tooltip label="设置" side="bottom" delayMs={400}>
              <Button
                size="sm"
                className={expandCss.button}
                aria-label="设置"
                data-header-settings
                onClick={onOpenSettings}
              >
                <IconSettingsOutlineRegular className={expandCss.icon} />
              </Button>
            </Tooltip>
          )}
          {/* The way into a hidden panel: absent while the panel is shown. */}
          {!detailsOpen && (
            <Tooltip label="展开详情" side="bottom" delayMs={400}>
              <Button
                size="sm"
                className={expandCss.button}
                aria-label="展开详情"
                data-details-expand
                onClick={onToggleDetails}
              >
                <IconPanelLeftOutlineRegular className={expandCss.icon} />
              </Button>
            </Tooltip>
          )}
        </div>
      </header>
      {notice !== undefined && notice !== '' && (
        <p className={rootCss.notice} role="status">{notice}</p>
      )}
      <div className={rootCss.body}>
        <div
          className={rootCss.scrollBody}
          data-conversation-scroll=""
          ref={scroller}
            // `artifact://` references open the output tab, the way the legacy
            // workbench intercepts them; every other link keeps its own behavior.
            onClickCapture={(event) => {
              const anchor = event.target instanceof Element ? event.target.closest('a') : null
              const href = anchor?.getAttribute('href') ?? ''
              if (!href.startsWith('artifact://')) return
              event.preventDefault()
              onOpenArtifact(href.slice('artifact://'.length))
            }}
          >
          {/* A session that cannot be opened says so where the reader is looking
              — the transcript — with what to do about it, instead of a status
              line wedged under the header. */}
          {error !== undefined && (
            <OpenFailure reason={error} onRetry={onRetryOpen} />
          )}
          {blank
            ? (
              <div className={heroCss.root}>
                <div className={heroCss.stack}>
                  <div className={heroCss.headline}>
                    <span className={heroCss.fishHitbox}>
                      <span className={heroCss.fish}>
                        <MinkMark size={34} />
                      </span>
                    </span>
                    <span className={heroCss.titleGroup}>
                      <span>今天想让 Mink 做什么？</span>
                    </span>
                  </div>
                  <div className={heroCss.body}>
                    <Composer
                      hero
                      running={running}
                      targetTurnId={state?.currentTurn ?? null}
                      disabled={!open}
                      pending={pending}
                      attachments={attachments}
                      canAttach={imageLimits !== null}
                      model={state?.model ?? ''}
                      onSend={onSend}
                      onGuide={onGuide}
                      onCommand={onCommand}
                      initialDraft={draft}
                      onDraftChange={onDraftChange}
                      onStop={onStop}
                    />
                  </div>
                </div>
              </div>
            )
            : (
              <>
                {hasOlder && (
                  <div className={navCss.older}>
                    <button
                      type="button"
                      disabled={loadingOlder}
                      onClick={() => {
                        const box = scroller.current
                        // Prepending rows shifts everything down: hold the
                        // reader's viewport on the turn they were reading.
                        const heightBefore = box?.scrollHeight ?? 0
                        const topBefore = box?.scrollTop ?? 0
                        setLoadingOlder(true)
                        void onLoadOlder().finally(() => {
                          setLoadingOlder(false)
                          requestAnimationFrame(() => {
                            if (box === null) return
                            box.scrollTop = topBefore + (box.scrollHeight - heightBefore)
                          })
                        })
                      }}
                    >
                      {loadingOlder ? '加载中…' : '加载更早的轮次'}
                    </button>
                  </div>
                )}
                <MarkdownDelegateProvider
                  openFile={onOpenFile}
                  openExternalLink={(href) => { window.open(href, '_blank', 'noopener,noreferrer') }}
                >
                  <Transcript
                    items={items}
                    running={running}
                    processDisplay={processDisplay}
                    outcomes={state?.outcomes}
                    onOpenFile={onOpenFile}
                    onOpenArtifact={onOpenArtifact}
                  />
                </MarkdownDelegateProvider>
                {/* The reading column and the composer are separate regions: the
                    gap keeps the last row from sitting against the input card. */}
                <div className={rootCss.tailGap} aria-hidden />
                <TurnNavigator
                  items={items}
                  following={reading.following}
                  onJump={reading.jump}
                  onFollow={reading.follow}
                  leading={chromeHiddenProp === true && onRevealChrome !== undefined
                    ? (
                      <button
                        type="button"
                        className="mk-reveal"
                        aria-label="退出阅读模式"
                        onClick={onRevealChrome}
                      >
                        <IconChevronDownOutlineRegular size={16} />
                      </button>
                    )
                    : undefined}
                />
              </>
            )}
        </div>
        {/* The composer is a sibling of the scrolling transcript, so it docks at
            the bottom of the column however short the history is. On a phone it
            also leaves the flow while the reader is in the transcript — but only
            when there is nothing to send: a draft or a pending attachment keeps
            the input on screen. */}
        {!blank && (
          <div
            className={rootCss.composerSeat}
            ref={seatRef}
            data-mink-composer-seat
            data-chrome-hidden={chromeHidden || undefined}
            data-chrome-collapsed={(chromeHidden && draft === '' && attachments.entries.length === 0) || undefined}
          >
            <GoalBar goal={state?.goal ?? null} onAction={onGoalAction} />
            <Composer
              hero={false}
              running={running}
              targetTurnId={state?.currentTurn ?? null}
              disabled={!open}
              pending={pending}
              attachments={attachments}
              canAttach={imageLimits !== null}
              model={modelLabel ?? state?.model ?? ''}
              onSend={onSend}
              onGuide={onGuide}
              onCommand={onCommand}
              initialDraft={draft}
              onDraftChange={onDraftChange}
              modelChoices={modelChoices}
              modelLoading={modelLoading}
              modelError={modelError}
              onModelChange={onModelChange}
              onStop={onStop}
            />
            <StatsPills state={state} />
          </div>
        )}
      </div>
    </div>
  )
}
