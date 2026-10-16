/**
 * Draft attachments: the bytes go to the session's attachment store, the draft
 * keeps only the ids and a preview URL, and the context never carries the image
 * itself — a Read of the stored path is what reaches the model.
 *
 * The session's own capability snapshot decides the accepted formats and byte
 * ceilings; without one the composer refuses the file instead of uploading it.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { api, sessionUrl, type SessionSummary } from '../../data/api.ts'
import { readAttachments, sessionAttachmentsKey, writeAttachments } from '../../data/sessionDraft.ts'
import type { Attachment, ImageLimits } from '../../data/types.ts'

/** Hard ceiling the workbench applies on top of the session's own limits. */
const WORKBENCH_MAX_IMAGE_BYTES = 16 * 1024 * 1024

/** Draft entry for one chosen file. */
export interface DraftAttachment {
  key: string
  name: string
  bytes: number
  /** Object URL while uploading, the stored attachment URL once accepted. */
  preview: string
  status: 'uploading' | 'ready' | 'failed'
  attachment?: Attachment
  error?: string
}

export interface AttachmentsHandle {
  entries: DraftAttachment[]
  /** A file the reader chose, pasted or dropped. */
  add: (files: readonly File[]) => void
  remove: (key: string) => void
  /** Uploads in flight or refused files block submission. */
  blocked: boolean
  /** Ids ready to reference from an input. */
  ids: string[]
  clear: () => void
  /** Message explaining why a file was refused, or undefined. */
  notice: string | undefined
}

/** Rebuild the rail from the ids this session stored. */
function restore(session: SessionSummary | undefined, storageKey: string | undefined): DraftAttachment[] {
  if (session === undefined || storageKey === undefined) return []
  return readAttachments(storageKey).map(stored => ({
    key: `stored-${stored.id}`,
    name: stored.name,
    bytes: 0,
    preview: sessionUrl(session.id, `/attachments/${stored.id}`, session.project_key),
    status: 'ready' as const,
    attachment: { id: stored.id, mime: '', width: 0, height: 0, bytes: 0 },
  }))
}

/** Accepted format list from the session capability, normalized to mime names. */
function allowedFormats(limits: ImageLimits | null): string[] {
  return limits === null ? [] : limits.allowed_mime.map(mime => mime.replace('image/', '').replace('jpg', 'jpeg'))
}

/**
 * Hold the draft's attachments for one session.
 * @param session - the open session, or undefined when none is selected.
 * @param limits - the session's image capability, from its snapshot.
 * @returns the entries and their verbs.
 */
export function useAttachments(session: SessionSummary | undefined, limits: ImageLimits | null): AttachmentsHandle {
  const storageKey = session === undefined ? undefined : sessionAttachmentsKey(session)
  // Draft attachments belong to the session identity: a stored id is enough to
  // rebuild the rail (the preview is the server's own copy, no blob needed).
  const [entries, setEntries] = useState<DraftAttachment[]>(() => restore(session, storageKey))
  const [notice, setNotice] = useState<string | undefined>(undefined)
  const counter = useRef(0)

  useEffect(() => {
    setEntries(restore(session, storageKey))
  }, [session, storageKey])

  useEffect(() => {
    if (storageKey === undefined) return
    writeAttachments(storageKey, entries.flatMap(entry =>
      entry.attachment === undefined ? [] : [{ id: entry.attachment.id, name: entry.name }]))
  }, [entries, storageKey])

  const add = useCallback((files: readonly File[]) => {
    if (session === undefined) return
    setNotice(undefined)
    if (limits === null) {
      setNotice('此会话不支持图片输入')
      return
    }
    const formats = allowedFormats(limits)
    const perImage = Math.min(limits.max_image_bytes, WORKBENCH_MAX_IMAGE_BYTES)
    const perRequest = Math.min(limits.max_image_bytes_per_request, WORKBENCH_MAX_IMAGE_BYTES)
    const maxCount = Math.min(8, limits.max_images_per_request)
    setEntries((current) => {
      const next = [...current]
      let total = next.reduce((sum, entry) => sum + entry.bytes, 0)
      for (const file of files) {
        if (next.length >= maxCount) {
          setNotice(`最多选择 ${maxCount} 张图片`)
          break
        }
        const format = file.type.replace('image/', '').replace('jpg', 'jpeg')
        if (!formats.includes(format)) {
          setNotice(`不支持的图片格式：${file.type || file.name}`)
          continue
        }
        if (file.size > perImage || total + file.size > perRequest) {
          setNotice('图片超过会话字节上限')
          continue
        }
        total += file.size
        counter.current += 1
        const key = `draft-${counter.current}`
        const preview = URL.createObjectURL(file)
        next.push({ key, name: file.name, bytes: file.size, preview, status: 'uploading' })
        // One async body owns both failure kinds: a refused upload (the server
        // answers with a non-200 code inside a 2xx envelope) and a lost request
        // both leave the entry failed with its reason, never stuck uploading.
        void (async () => {
          let failure: string
          try {
            const response = await api.upload(session.id, file, session.project_key)
            if (response.code !== 200) {
              failure = response.message || `code ${response.code}`
            } else {
              setEntries(latest => latest.map((entry) => {
                if (entry.key !== key) return entry
                URL.revokeObjectURL(entry.preview)
                return {
                  ...entry,
                  status: 'ready',
                  attachment: response.data,
                  preview: sessionUrl(session.id, `/attachments/${response.data.id}`, session.project_key),
                }
              }))
              return
            }
          } catch (cause) {
            failure = cause instanceof Error ? cause.message : String(cause)
          }
          setEntries(latest => latest.map(entry => (entry.key === key
            ? { ...entry, status: 'failed', error: failure }
            : entry)))
        })()
      }
      return next
    })
  }, [limits, session])

  const remove = useCallback((key: string) => {
    setEntries((current) => {
      const entry = current.find(candidate => candidate.key === key)
      if (entry !== undefined && entry.preview.startsWith('blob:')) URL.revokeObjectURL(entry.preview)
      return current.filter(candidate => candidate.key !== key)
    })
  }, [])

  const clear = useCallback(() => {
    setEntries((current) => {
      for (const entry of current) {
        if (entry.preview.startsWith('blob:')) URL.revokeObjectURL(entry.preview)
      }
      return []
    })
    setNotice(undefined)
  }, [])

  const ids = useMemo(
    () => entries.flatMap(entry => (entry.attachment === undefined ? [] : [entry.attachment.id])),
    [entries],
  )
  const blocked = entries.some(entry => entry.status !== 'ready') || ids.length !== entries.length

  return { entries, add, remove, blocked, ids, clear, notice }
}
