/**
 * Composer: the copied InputBar card holds a plain textarea and the primary
 * control. Enter submits, Shift+Enter keeps the newline. While a turn runs the
 * primary control stays available and submits the draft as guidance bound to
 * that turn's id, with stop as its own control beside it; the draft is restored
 * when the submission fails and the reader has not typed since.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import { IconPlusOutlineMedium } from '../../ui/primitives/index.ts'
import type { ReactNode } from 'react'
import { ComposerAttachments, DropOverlay, useDocumentDrop } from './ComposerAttachments.tsx'
import { useCommandPopup, useCommandPopupOpen } from './useCommandPopup.ts'
import { PopupSelectView } from '../../ui/commands/PopupSelectView.tsx'
import { COMMAND_LOCALE, commandText } from '../commands/locale.ts'
import { Menu, MenuItemButton, Tooltip } from '../../ui/primitives/index.ts'
import type { DraftAttachment } from './useAttachments.ts'
import { commandHintsFor, inCommandMode } from '../../data/commands.ts'
import css from './InputBar.module.css'
import modelCss from './ModelSeat.module.css'

/** The source composer's send arrow (ui-conversation InputBar). */
function SendGlyph() {
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden>
      <path
        d="M8.3125 0.980183C8.66767 1.0531 8.97902 1.20418 9.2627 1.43233C9.48724 1.61297 9.73029 1.85793 9.97949 2.10714L14.707 6.83468L13.293 8.24874L9 3.95577V15.0417H7V3.95577L2.70703 8.24874L1.29297 6.83468L6.02051 2.10714C6.26971 1.85793 6.51277 1.61297 6.7373 1.43233C6.97662 1.23986 7.28445 1.04402 7.6875 0.980183C7.8973 0.947006 8.1031 0.95516 8.3125 0.980183Z"
        fill="currentColor"
      />
    </svg>
  )
}

/** The source composer's stop square. */
function StopGlyph() {
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden>
      <rect x="3" y="3" width="10" height="10" rx="3" fill="currentColor" />
    </svg>
  )
}

export interface ComposerProps {
  /** Docked under the transcript, or standing alone on the hero. */
  hero: boolean
  /** A turn is executing. */
  running: boolean
  /** The running turn guidance binds to; absent before the id arrives. */
  targetTurnId: string | null
  /** The session cannot accept input right now. */
  disabled: boolean
  /** Queue waiting above the card, when the session has one. */
  pending?: ReactNode
  /** Draft attachments and their verbs. */
  attachments: {
    entries: readonly DraftAttachment[]
    add: (files: readonly File[]) => void
    remove: (key: string) => void
    /** Ids ready to reference from the submitted input. */
    ids: string[]
    /** Uploads in flight or a refused file block submission. */
    blocked: boolean
    notice: string | undefined
  }
  /** Whether this session accepts image attachments at all. */
  canAttach: boolean
  /** The session's active model, shown in the source's model seat. */
  model: string
  /** Alias choices this session's runtime honours, when it reported them. */
  modelChoices?: readonly { alias: string; model: string }[] | undefined
  /** The vocabulary read is in flight. */
  modelLoading?: boolean | undefined
  /** Why the vocabulary read failed, when it did. */
  modelError?: string | undefined
  /** Switch the session's model; the seat keeps its label until told otherwise. */
  onModelChange?: ((model: string) => void) | undefined
  /** Draft to start from: this session's stored text, when it has one. */
  initialDraft?: string | undefined
  /** Report every draft write so the owner can keep it with the session. */
  onDraftChange?: ((text: string) => void) | undefined
  /** Submit a fresh turn; resolves to an error message, or undefined on success. */
  onSend: (
    text: string,
    attachmentIds: readonly string[],
    attachmentKeys: readonly string[],
  ) => Promise<string | undefined>
  /** Submit guidance for the running turn. */
  onGuide: (
    text: string,
    turnId: string,
    attachmentIds: readonly string[],
    attachmentKeys: readonly string[],
  ) => Promise<string | undefined>
  /** Run one slash command; resolves to its message and whether it succeeded. */
  onCommand: (text: string) => Promise<{ message: string; ok: boolean }>
  /** Slash commands need a session; the new-conversation composer hides them. */
  commandsEnabled?: boolean | undefined
  /** The model seat belongs to a session's runtime. */
  showModelSeat?: boolean | undefined
  /** Report whether the composer itself is mid-operation or showing a failure. */
  onOperationStateChange?: ((state: ComposerOperationState) => void) | undefined
  onStop: () => void
}

/** Operation state the owner needs for read-mode pins (never display text). */
export interface ComposerOperationState {
  /** A command or submission is in flight. */
  pending: boolean
  /** The current error line is a failure, not a success notice. */
  failed: boolean
}

/**
 * Render the input card.
 * @param props - hero/docked placement, run state and the three actions.
 * @returns the composer element tree.
 */
export function Composer({
  hero, running, targetTurnId, disabled, pending, attachments, canAttach, model,
  modelChoices, modelLoading, modelError, onModelChange, initialDraft, onDraftChange, onSend, onGuide, onCommand, onStop,
  commandsEnabled = true, showModelSeat = true, onOperationStateChange,
}: ComposerProps) {
  const [modelMenuOpen, setModelMenuOpen] = useState(false)
  // The owner keys this composer by session identity, so the initial draft is
  // this session's own; every write is reported so it survives a switch.
  const [text, setText] = useState(initialDraft ?? '')
  const [busy, setBusy] = useState(false)
  // A failure and a success notice are different states: only the failure pins
  // the reading chrome, and only until the reader edits or retries.
  const [failure, setFailure] = useState<string | undefined>(undefined)
  const [notice, setNotice] = useState<string | undefined>(undefined)
  // Draft revision: every write bumps it, so a late failure only restores text
  // the reader has not replaced in the meantime.
  const revision = useRef(0)
  const area = useRef<HTMLTextAreaElement>(null)
  const picker = useRef<HTMLInputElement>(null)

  const write = useCallback((value: string) => {
    revision.current += 1
    setText(value)
    setFailure(undefined)
    setNotice(undefined)
    onDraftChange?.(value)
  }, [onDraftChange])

  // The owner derives reading-chrome pins from the operation, not from the
  // text of a message: success notices must not lock the chrome forever.
  useEffect(() => {
    onOperationStateChange?.({ pending: busy, failed: failure !== undefined })
  }, [busy, failure, onOperationStateChange])

  // The add control opens the copied popupSelect shell: one surface for the whole
  // command contract, with its own search, highlight and single-flight pick.
  const commandPopup = useCommandPopup({
    write,
    pickFiles: () => { picker.current?.click() },
    canAttach,
    focusComposer: () => { area.current?.focus() },
  })
  const panelOpen = useCommandPopupOpen(commandPopup.controller)
  const dragging = useDocumentDrop(canAttach && disabled ? false : canAttach, attachments.add)
  const commandMode = commandsEnabled && inCommandMode(text)
  // One copied surface serves both entries: the add control opens the whole
  // contract, `/` opens it carrying the draft's query. While it is open its own
  // search box holds focus, so the composer stops feeding it.
  useEffect(() => {
    if (!commandMode) return
    const query = text.slice(1)
    if (panelOpen) commandPopup.setQuery(query)
    else commandPopup.openWith(query)
  }, [commandMode, commandPopup, panelOpen, text])
  const runCommand = useCallback((line: string) => {
    const payload = line.trim()
    if (payload === '' || busy) return
    setFailure(undefined)
    setNotice(undefined)
    setBusy(true)
    const submittedAt = revision.current
    write('')
    void onCommand(payload).then((result) => {
      if (!result.ok) {
        // A refused command keeps the draft so the reader can fix it in place.
        if (revision.current === submittedAt + 1) write(payload)
        setFailure(result.message)
        return
      }
      setNotice(result.message)
    }).finally(() => {
      setBusy(false)
    })
  }, [busy, onCommand, write])

  const submit = useCallback(() => {
    if (commandMode) {
      runCommand(text)
      return
    }
    const payload = text.trim()
    const ids = attachments.ids
    // Freeze the attachment set this submission carries: an acceptance consumes
    // exactly these draft identities, never files chosen while it was in flight.
    const submittedKeys = attachments.entries.map(entry => entry.key)
    if ((payload === '' && attachments.entries.length === 0) || disabled || busy || attachments.blocked) return
    if (running && targetTurnId === null) {
      setFailure('当前会话尚未取得引导能力，请等待同步')
      return
    }
    const submittedAt = revision.current
    setFailure(undefined)
    setNotice(undefined)
    setBusy(true)
    write('')
    const call = running && targetTurnId !== null
      ? onGuide(payload, targetTurnId, ids, submittedKeys)
      : onSend(payload, ids, submittedKeys)
    void call.then((failure) => {
      if (failure === undefined) return
      // Restore only a draft nobody has replaced since the submission.
      if (revision.current === submittedAt + 1) write(payload)
      setFailure(failure)
    }).catch((cause: unknown) => {
      // A rejected submission (the request never reached the runtime, or its
      // answer was unreadable) is the same situation for the reader: the words
      // come back with a reason instead of a silently emptied box.
      if (revision.current === submittedAt + 1) write(payload)
      setFailure(cause instanceof Error ? cause.message : String(cause))
    }).finally(() => {
      setBusy(false)
    })
  }, [attachments, busy, commandMode, disabled, onGuide, onSend, runCommand, running, targetTurnId, text, write])

  return (
    <div className={hero ? css.hero : css.root}>
      {dragging && (
        <DropOverlay
          disabled={!canAttach || disabled}
          title={canAttach && !disabled ? '松开以添加图片' : '此会话不接受图片'}
          desc="图片只进入会话附件目录，模型经 Read 读取后才看到"
        />
      )}
      {pending}
      {attachments.notice !== undefined && (
        <p className={css.notice} role="status">{attachments.notice}</p>
      )}
      {/* A submission that was refused (or never reached the runtime) says so here,
          above the card: on a phone the trailing status line is out of the
          reader's sight, and an emptied box with no reason reads as a dead UI. */}
      {failure !== undefined && (
        <p className={css.submitError} role="alert" data-composer-error="">{failure}</p>
      )}
      {notice !== undefined && notice !== '' && (
        <p className={css.notice} role="status" data-composer-notice="">{notice}</p>
      )}
      <div className={css.card} data-mink-composer data-composer-card>
        {/* The copied shell renders its own card above the composer while open. */}
        {commandsEnabled && (
          <PopupSelectView
            popup={commandPopup.controller}
            t={(key, params) => commandText(COMMAND_LOCALE, key, params)}
          />
        )}
        <ComposerAttachments entries={attachments.entries} onRemove={attachments.remove} />
        {/* One scrollport caps the draft at the seat's 14 lines (the copied
            sheet's own number); the textarea grows inside it. */}
        <div className={css.scroll}>
          <div className={css.grow}>
            <textarea
              ref={area}
              className={css.input}
              value={text}
              rows={1}
              placeholder={commandMode
                ? '输入命令，Tab 补全，Enter 执行，Esc 退出'
                : running ? '补充要求，Enter 提交引导' : '描述任务，Enter 发送，Shift+Enter 换行'}
              aria-label="输入消息"
              disabled={disabled}
              onChange={(event) => {
                write(event.target.value)
              }}
              onPaste={(event) => {
                const files = [...event.clipboardData.files].filter(file => file.type.startsWith('image/'))
                if (files.length === 0) return
                event.preventDefault()
                attachments.add(files)
              }}
              onKeyDown={(event) => {
                if (event.nativeEvent.isComposing) return
                // The draft owns the query, so its keys drive the surface: the
                // textarea keeps the caret and there is no second search box to
                // delete into.
                if (commandMode) {
                  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
                    event.preventDefault()
                    commandPopup.move(event.key === 'ArrowDown' ? 1 : -1)
                    return
                  }
                  if (event.key === 'Tab') {
                    const only = commandHintsFor(text).length === 1 ? commandHintsFor(text)[0] : undefined
                    if (only !== undefined) {
                      event.preventDefault()
                      write(only.command)
                    }
                    return
                  }
                  if (event.key === 'Enter' && !event.shiftKey) {
                    event.preventDefault()
                    // A draft that already names a command runs it; anything else
                    // settles the highlighted row into the draft, so arguments can
                    // follow. With no row to settle, Enter submits as ever.
                    const exact = commandHintsFor(text).find(hint => hint.command === text)
                    if (exact !== undefined) {
                      submit()
                      return
                    }
                    if (commandHintsFor(text).length > 0) {
                      commandPopup.selectActive()
                      return
                    }
                    submit()
                    return
                  }
                  if (event.key === 'Escape') {
                    event.preventDefault()
                    commandPopup.close()
                    return
                  }
                }
                if (event.key !== 'Enter' || event.shiftKey) return
                event.preventDefault()
                submit()
              }}
            />
          </div>
        </div>
        <div className={css.row}>
          <div className={css.tools}>
            <input
              ref={picker}
              type="file"
              accept="image/*"
              multiple
              hidden
              disabled={disabled || !canAttach}
              aria-label="选择图片"
              onChange={(event) => {
                const files = [...event.target.files ?? []]
                if (files.length > 0) attachments.add(files)
                event.target.value = ''
              }}
            />
            {/* The source's round add control opens the command shell; before a
                session exists it is just the attachment picker. */}
            <Tooltip label={commandsEnabled ? '命令' : '添加图片'} side="top" delayMs={400}>
              <button
                type="button"
                className={css.add}
                aria-label={commandsEnabled ? '命令' : '添加图片'}
                aria-haspopup={commandsEnabled ? 'listbox' : undefined}
                aria-expanded={commandsEnabled ? panelOpen : undefined}
                disabled={disabled || (!commandsEnabled && !canAttach)}
                onMouseDown={(event) => { event.preventDefault() }}
                onClick={() => {
                  if (!commandsEnabled) {
                    picker.current?.click()
                    return
                  }
                  if (panelOpen) commandPopup.close()
                  else commandPopup.openAll()
                }}
              >
                <IconPlusOutlineMedium size={14} />
              </button>
            </Tooltip>
          </div>
          <div className={panelOpen ? css.trailingActive : css.trailing}>
            <span className={css.standardControls} hidden={panelOpen}>
              {showModelSeat && (
                <Menu
                  open={modelMenuOpen}
                  align="end"
                  side="top"
                  onClose={() => { setModelMenuOpen(false) }}
                  anchor={(
                    <button
                      type="button"
                      className={css.select}
                      aria-label={`当前模型 ${model === '' ? '未记录' : model}`}
                      aria-haspopup="menu"
                      aria-expanded={modelMenuOpen}
                      // An open session can always ask for a model: an empty list means
                      // the read has not answered yet, not that choosing is impossible.
                      disabled={disabled}
                      onClick={() => { setModelMenuOpen(value => !value) }}
                    >
                      {model === '' ? '模型未记录' : model}
                    </button>
                  )}
                >
                  <div className={modelCss.list} role="group" aria-label="切换模型">
                    {modelLoading === true && (modelChoices ?? []).length === 0 && (
                      <span className={modelCss.status}>正在读取模型…</span>
                    )}
                    {modelLoading !== true && (modelChoices ?? []).length === 0 && (
                      <span className={modelCss.status} data-model-unavailable>
                        {modelError === undefined ? '此会话没有可切换的模型' : `读取模型失败：${modelError}`}
                      </span>
                    )}
                    {(modelChoices ?? []).map(choice => (
                      <MenuItemButton
                        key={choice.alias}
                        onSelect={() => {
                          setModelMenuOpen(false)
                          onModelChange?.(choice.alias)
                        }}
                      >
                        <span className={modelCss.alias}>{choice.alias}</span>
                        <span className={modelCss.target}>{choice.model}</span>
                      </MenuItemButton>
                    ))}
                  </div>
                </Menu>
              )}
            </span>
            {/* The composer's own line answers for the states the card cannot show
                (an unavailable session, an upload in flight); a refused submission
                is announced above the card instead. */}
            <span className={panelOpen ? css.activityExpanded : css.activity} role="status">
              {disabled ? '会话不可用' : attachments.blocked ? '附件上传中…' : ''}
            </span>
            {running && (
              <Tooltip label="停止" side="top" delayMs={400}>
                <button type="button" className={css.primary} aria-label="停止" onClick={onStop}>
                  <StopGlyph />
                </button>
              </Tooltip>
            )}
            <Tooltip label={running ? '提交引导' : '发送'} side="top" delayMs={400}>
              <button
                type="button"
                className={css.primary}
                aria-label={running ? '提交引导' : '发送'}
                disabled={disabled || busy || attachments.blocked || (text.trim() === '' && attachments.ids.length === 0)}
                onMouseDown={(event) => { event.preventDefault() }}
                onClick={submit}
              >
                <SendGlyph />
              </button>
            </Tooltip>
          </div>
        </div>
      </div>
    </div>
  )
}
