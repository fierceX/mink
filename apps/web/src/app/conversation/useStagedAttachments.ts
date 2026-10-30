/**
 * Attachments for a conversation that does not exist yet. The bytes stay in
 * memory (and a blob preview) until the first send creates the session; only
 * then are they uploaded to the session's own attachment store. A reload drops
 * the files, which is the one thing a browser cannot restore without the
 * original File objects; the text draft is persisted separately.
 */

import { useCallback, useMemo, useRef, useState } from 'react'
import type { AttachmentsHandle, DraftAttachment } from './useAttachments.ts'

/** Hard ceiling the workbench applies before the session's own limits exist. */
const STAGED_MAX_IMAGES = 8
const STAGED_MAX_IMAGE_BYTES = 16 * 1024 * 1024
const STAGED_MAX_REQUEST_BYTES = 16 * 1024 * 1024
const STAGED_FORMATS = ['png', 'jpeg', 'gif', 'webp']

export interface StagedAttachmentsHandle extends AttachmentsHandle {
  /** Files still waiting for the session that will receive them. */
  files: File[]
}

/**
 * Hold image files for the composing view.
 * @returns the composer rail's entries and the files to upload after creation.
 */
export function useStagedAttachments(): StagedAttachmentsHandle {
  const [entries, setEntries] = useState<DraftAttachment[]>([])
  const [notice, setNotice] = useState<string | undefined>(undefined)
  const counter = useRef(0)

  const add = useCallback((files: readonly File[]) => {
    setNotice(undefined)
    setEntries((current) => {
      const next = [...current]
      let total = next.reduce((sum, entry) => sum + entry.bytes, 0)
      for (const file of files) {
        if (next.length >= STAGED_MAX_IMAGES) {
          setNotice(`最多选择 ${STAGED_MAX_IMAGES} 张图片`)
          break
        }
        const format = file.type.replace('image/', '').replace('jpg', 'jpeg')
        if (!file.type.startsWith('image/') || !STAGED_FORMATS.includes(format)) {
          setNotice(`不支持的图片格式：${file.type || file.name}`)
          continue
        }
        if (file.size > STAGED_MAX_IMAGE_BYTES) {
          setNotice('图片超过 16MB 上限')
          continue
        }
        if (total + file.size > STAGED_MAX_REQUEST_BYTES) {
          setNotice('图片总大小超过 16MB 上限')
          continue
        }
        total += file.size
        counter.current += 1
        next.push({
          key: `staged-${counter.current}`,
          name: file.name,
          bytes: file.size,
          preview: URL.createObjectURL(file),
          status: 'ready',
          file,
        })
      }
      return next
    })
  }, [])

  const remove = useCallback((key: string) => {
    setEntries((current) => {
      const entry = current.find(candidate => candidate.key === key)
      if (entry !== undefined) URL.revokeObjectURL(entry.preview)
      return current.filter(candidate => candidate.key !== key)
    })
  }, [])

  const files = useMemo(
    () => entries.flatMap(entry => (entry.file === undefined ? [] : [entry.file])),
    [entries],
  )

  return {
    entries,
    add,
    remove,
    ids: [],
    blocked: false,
    notice,
    files,
  }
}
