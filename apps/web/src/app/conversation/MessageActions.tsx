/**
 * Message chrome: the copy control under a message and, on an assistant turn,
 * the turn's recorded usage beside it. Drawn with the copied message-actions
 * sheet (28px hit targets, tertiary icons, hover fill). mink's conversation rows
 * carry no timestamps, so none are shown: the row states what it knows.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import {
  IconCheckOutlineRegular,
  IconCopyOutlineRegular,
  Tooltip,
  writeClipboard,
} from '../../ui/primitives/index.ts'
import css from './MessageIconActions.module.css'

/**
 * Copy one message's text; the control confirms in place for a moment.
 * @param props.text - the text to copy.
 * @param props.label - accessible name while idle.
 * @param props.info - trailing reading (a turn's usage), when there is one.
 * @returns the action row.
 */
export function MessageActions({ text, label, info }: {
  text: string
  label: string
  info?: string | undefined
}) {
  const [copied, setCopied] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  useEffect(() => () => {
    if (timer.current !== undefined) clearTimeout(timer.current)
  }, [])

  const copy = useCallback(() => {
    void writeClipboard(text).then((ok) => {
      if (!ok) return
      setCopied(true)
      if (timer.current !== undefined) clearTimeout(timer.current)
      timer.current = setTimeout(() => { setCopied(false) }, 1500)
    })
  }, [text])

  return (
    <div className={css.actions} data-clock="end" data-message-actions>
      <Tooltip label={copied ? '已复制' : label} side="top" delayMs={400}>
        <button
          type="button"
          className={css.action}
          aria-label={copied ? '已复制' : label}
          onClick={copy}
        >
          {copied ? <IconCheckOutlineRegular /> : <IconCopyOutlineRegular />}
        </button>
      </Tooltip>
      {info !== undefined && <span className={css.endInfo}>{info}</span>}
    </div>
  )
}
