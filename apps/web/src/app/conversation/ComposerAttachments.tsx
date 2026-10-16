/**
 * Draft attachment rail: 64px thumbnails with a corner remove control, drawn
 * with the source's own composer-attachment styles, plus the full-viewport drop
 * invitation. Images are the only kind the workbench accepts today, so the rail
 * renders image items only.
 */

import { createPortal } from 'react-dom'
import clsx from 'clsx'
import { useEffect, useRef, useState } from 'react'
import { IconCloseFillRegular } from '../../ui/primitives/index.ts'
import type { DraftAttachment } from './useAttachments.ts'
import css from './ComposerAttachments.module.css'
import layoutCss from './AttachmentsLayout.module.css'
import overlayCss from './DropOverlay.module.css'

/**
 * Render the draft rail.
 * @param props.entries - drafted files, in the order they were chosen.
 * @param props.onRemove - drop one entry from the draft.
 * @returns the rail, or null while the draft carries no files.
 */
export function ComposerAttachments({ entries, onRemove }: {
  entries: readonly DraftAttachment[]
  onRemove: (key: string) => void
}) {
  if (entries.length === 0) return null
  return (
    <div className={clsx(css.rail, layoutCss.rail)} data-composer-attachments={entries.length}>
      {entries.map(entry => (
        <div key={entry.key} className={css.imageItem} data-attachment-status={entry.status}>
          <span className={css.thumbnail}>
            <img src={entry.preview} alt={entry.name} />
          </span>
          <button
            type="button"
            className={css.remove}
            aria-label={`移除附件：${entry.name}`}
            onClick={() => { onRemove(entry.key) }}
          >
            <IconCloseFillRegular size={14} />
          </button>
        </div>
      ))}
    </div>
  )
}

/**
 * Render the drop invitation while a file drag is over the page.
 * @param props.disabled - drops are refused (no image capability, or the error).
 * @param props.title - headline copy.
 * @param props.desc - limits line, shown while drops are accepted.
 * @returns the overlay layer.
 */
export function DropOverlay({ disabled, title, desc }: {
  disabled: boolean
  title: string
  desc?: string | undefined
}) {
  return createPortal(
    <div className={overlayCss.mask} role="status">
      <div className={overlayCss.wrap}>
        <div className={overlayCss.title}>{title}</div>
        {!disabled && desc !== undefined && <div className={overlayCss.desc}>{desc}</div>}
      </div>
    </div>,
    document.body,
  )
}

/**
 * Track document-level drag state so a drop anywhere on the page adds files.
 * @param enabled - whether the page accepts drops right now.
 * @param onFiles - receive the dropped files.
 * @returns the current drag state.
 */
export function useDocumentDrop(enabled: boolean, onFiles: (files: readonly File[]) => void): boolean {
  const [dragging, setDragging] = useState(false)
  const files = useRef(onFiles)
  files.current = onFiles

  useEffect(() => {
    const onDragEnter = (event: globalThis.DragEvent): void => {
      if (!enabled) return
      event.preventDefault()
      setDragging(true)
    }
    // Keep the browser from opening a file dragged over the page.
    const onDragOver = (event: globalThis.DragEvent): void => {
      if (!enabled) return
      event.preventDefault()
    }
    const onDragLeave = (event: globalThis.DragEvent): void => {
      if (event.relatedTarget === null) setDragging(false)
    }
    const onDrop = (event: globalThis.DragEvent): void => {
      setDragging(false)
      if (!enabled) return
      event.preventDefault()
      const dropped = [...event.dataTransfer?.files ?? []]
      if (dropped.length > 0) files.current(dropped)
    }
    document.addEventListener('dragenter', onDragEnter)
    document.addEventListener('dragover', onDragOver)
    document.addEventListener('dragleave', onDragLeave)
    document.addEventListener('drop', onDrop)
    return () => {
      document.removeEventListener('dragenter', onDragEnter)
      document.removeEventListener('dragover', onDragOver)
      document.removeEventListener('dragleave', onDragLeave)
      document.removeEventListener('drop', onDrop)
    }
  }, [enabled])

  return dragging
}
