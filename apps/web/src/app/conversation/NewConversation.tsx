/**
 * Composing view for a conversation that does not exist yet. Nothing is sent to
 * the server until the reader submits the first message; the draft id and text
 * are persisted per project, while selected images stay in memory until the
 * session exists and they can be uploaded to its attachment store.
 */

import { useCallback, useEffect, useState } from 'react'
import { readComposeDraft, writeComposeDraft } from '../../data/sessionDraft.ts'
import { Button, IconFolderCloseRegular } from '../../ui/primitives/index.ts'
import { MinkMark } from '../brand/MinkMark.tsx'
import { Composer } from './Composer.tsx'
import { useStagedAttachments } from './useStagedAttachments.ts'
import heroCss from './HeroShell.module.css'
import rootCss from './ConversationRoot.module.css'
import css from './NewConversation.module.css'

export interface NewConversationProps {
  cwd: string
  projectName: string
  /** A project-validation or first-send refusal that happened outside the card. */
  notice?: string | undefined
  /** Create the session and submit the first turn. Resolves to a refusal text. */
  onFirstSend: (
    text: string,
    files: readonly File[],
    draftId: string,
    requestId: string,
  ) => Promise<string | undefined>
  onCancel: () => void
}

/**
 * Render the blank-session composer.
 * @param props - working directory, draft actions and the first-send callback.
 * @returns the composing view.
 */
export function NewConversation({ cwd, projectName, notice, onFirstSend, onCancel }: NewConversationProps) {
  const initial = readComposeDraft(cwd)
  const [draftId] = useState(initial.draftId)
  const [requestId] = useState(initial.requestId)
  const [text, setText] = useState(initial.text)
  const [sending, setSending] = useState(false)
  const staged = useStagedAttachments()

  // Every write carries the stable draft id, so a reload or a create retry is
  // the same conversation request rather than a second empty session.
  useEffect(() => {
    writeComposeDraft(cwd, { draftId, text, requestId })
  }, [cwd, draftId, text, requestId])

  const write = useCallback((value: string) => {
    setText(value)
  }, [])

  const send = useCallback(async (value: string): Promise<string | undefined> => {
    if (value.trim() === '' && staged.files.length > 0) return '首条消息需要包含文字'
    const total = staged.files.reduce((sum, file) => sum + file.size, 0)
    if (total > 16 * 1024 * 1024) return '图片总大小超过 16MB 上限'
    setSending(true)
    try {
      return await onFirstSend(value, staged.files, draftId, requestId)
    } finally {
      setSending(false)
    }
  }, [draftId, onFirstSend, requestId, staged.files])

  return (
    <div className={rootCss.root} data-new-conversation data-project-cwd={cwd}>
      <div className={rootCss.body}>
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
            <p className={css.project} title={cwd}>
              <IconFolderCloseRegular size={13} />
              <span className={css.projectName}>{projectName}</span>
              <span className={css.projectPath}>{cwd}</span>
            </p>
            {notice !== undefined && notice !== '' && (
              <p className={css.notice} role="alert">{notice}</p>
            )}
            <div className={heroCss.body}>
              <Composer
                hero
                running={false}
                targetTurnId={null}
                disabled={sending}
                attachments={{ ...staged, blocked: sending }}
                canAttach
                model=""
                modelChoices={[]}
                onSend={(value) => send(value)}
                onGuide={async () => '会话尚未创建，请先发送首条消息'}
                onCommand={async () => ({ message: '发送首条消息后可使用命令', ok: false })}
                initialDraft={initial.text}
                onDraftChange={write}
                onStop={() => {}}
                commandsEnabled={false}
                showModelSeat={false}
              />
            </div>
            <div className={css.actions}>
              <span className={css.hint}>首次发送后才会在服务器上创建会话。</span>
              <Button size="sm" variant="ghost" disabled={sending} onClick={onCancel}>取消</Button>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
