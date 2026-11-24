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
import {
  consumableAttachmentKeys,
  type AttachmentSnapshot,
  type AttachmentsMergedDetail,
  type StoredAttachment,
} from '../../data/sessionDraft.ts'
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
  /** A composing-stage file; the upload starts after the session exists. */
  file?: File
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
  /** Message explaining why a file was refused, or undefined. */
  notice: string | undefined
}

/** Build one ready rail entry from a stored draft attachment. */
function storedDraft(
  session: SessionSummary,
  stored: StoredAttachment,
  key: string,
): DraftAttachment {
  return {
    key,
    name: stored.name,
    bytes: 0,
    preview: sessionUrl(session.id, `/attachments/${stored.id}`, session.project_key),
    status: 'ready',
    attachment: { id: stored.id, mime: '', width: 0, height: 0, bytes: 0 },
  }
}

/** Append stored entries without disturbing identities already on the rail. */
function mergeStored(
  session: SessionSummary,
  current: DraftAttachment[],
  stored: readonly StoredAttachment[],
): DraftAttachment[] {
  const used = new Set(current.map(entry => entry.key))
  const next = [...current]
  for (const entry of stored) {
    const base = entry.key ?? `stored-${entry.id}`
    if (entry.key !== undefined) {
      if (used.has(entry.key)) continue
      used.add(entry.key)
      next.push(storedDraft(session, entry, entry.key))
      continue
    }
    let key = base
    let suffix = 1
    while (used.has(key)) {
      suffix += 1
      key = `${base}-${suffix}`
    }
    used.add(key)
    next.push(storedDraft(session, entry, key))
  }
  return next.length === current.length ? current : next
}

/** Rebuild the rail from the ids this session stored. */
function restore(session: SessionSummary | undefined, storageKey: string | undefined): DraftAttachment[] {
  if (session === undefined || storageKey === undefined) return []
  return mergeStored(session, [], readAttachments(storageKey))
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
  // A per-hook prefix keeps generated identities unique across reloads, where
  // the counter starts over while restored entries keep their old keys.
  const draftPrefix = useRef(`draft-${Math.random().toString(36).slice(2, 8)}`)
  const entriesRef = useRef(entries)
  entriesRef.current = entries
  const blobUrlsRef = useRef<Set<string>>(new Set())

  useEffect(() => {
    const blobUrls = blobUrlsRef.current
    return () => {
      for (const url of blobUrls) URL.revokeObjectURL(url)
      blobUrls.clear()
    }
  }, [])

  useEffect(() => {
    if (storageKey === undefined) return
    writeAttachments(storageKey, entries.flatMap(entry =>
      entry.attachment === undefined
        ? []
        : [{ key: entry.key, id: entry.attachment.id, name: entry.name }]))
  }, [entries, storageKey])

  useEffect(() => {
    if (storageKey === undefined || session === undefined) return
    const sync = (stored: readonly StoredAttachment[]): void => {
      setEntries(current => mergeStored(session, current, stored))
    }
    const onMerged = (event: Event): void => {
      const detail = (event as CustomEvent<AttachmentsMergedDetail>).detail
      if (detail?.key !== storageKey || !Array.isArray(detail.entries)) return
      sync(detail.entries)
    }
    // Another tab's upload merge arrives as a storage event; syncing also
    // closes the mount/merge race before this listener exists.
    const onStorage = (event: StorageEvent): void => {
      if (event.key === storageKey) sync(readAttachments(storageKey))
    }
    window.addEventListener('mink:attachments-merged', onMerged)
    window.addEventListener('storage', onStorage)
    sync(readAttachments(storageKey))
    return () => {
      window.removeEventListener('mink:attachments-merged', onMerged)
      window.removeEventListener('storage', onStorage)
    }
  }, [storageKey, session])

  useEffect(() => {
    if (storageKey === undefined) return
    const onConsumed = (event: Event): void => {
      const detail = (event as CustomEvent<{
        key: string
        snapshot?: AttachmentSnapshot
      }>).detail
      if (detail?.key !== storageKey) return
      // One consumption rule with the storage layer: only entries the frozen
      // snapshot names may leave; an unknown/empty snapshot removes nothing.
      const consumed = consumableAttachmentKeys(detail.snapshot)
      if (consumed.size === 0) return
      setEntries(current => current.filter(entry =>
        entry.attachment === undefined || !consumed.has(entry.key)))
    }
    window.addEventListener('mink:attachments-consumed', onConsumed)
    return () => window.removeEventListener('mink:attachments-consumed', onConsumed)
  }, [storageKey])

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
    const current = entriesRef.current
    const added: { entry: DraftAttachment; file: File }[] = []
    let total = current.reduce((sum, entry) => sum + entry.bytes, 0)
    let nextNotice: string | undefined
    for (const file of files) {
      if (current.length + added.length >= maxCount) {
        nextNotice = `最多选择 ${maxCount} 张图片`
        break
      }
      const format = file.type.replace('image/', '').replace('jpg', 'jpeg')
      if (!formats.includes(format)) {
        nextNotice = `不支持的图片格式：${file.type || file.name}`
        continue
      }
      if (file.size > perImage || total + file.size > perRequest) {
        nextNotice = '图片超过会话字节上限'
        continue
      }
      total += file.size
      counter.current += 1
      const key = `${draftPrefix.current}-${counter.current}`
      const preview = URL.createObjectURL(file)
      blobUrlsRef.current.add(preview)
      added.push({
        file,
        entry: { key, name: file.name, bytes: file.size, preview, status: 'uploading' },
      })
    }
    if (nextNotice !== undefined) setNotice(nextNotice)
    if (added.length === 0) return

    const acceptedEntries = added.map(item => item.entry)
    entriesRef.current = [...current, ...acceptedEntries]
    setEntries(latest => [...latest, ...acceptedEntries])

    for (const { entry, file } of added) {
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
            blobUrlsRef.current.delete(entry.preview)
            URL.revokeObjectURL(entry.preview)
            setEntries(latest => latest.map(candidate => (candidate.key !== entry.key
              ? candidate
              : {
                  ...candidate,
                  status: 'ready',
                  attachment: response.data,
                  preview: sessionUrl(session.id, `/attachments/${response.data.id}`, session.project_key),
                })))
            return
          }
        } catch (cause) {
          failure = cause instanceof Error ? cause.message : String(cause)
        }
        setEntries(latest => latest.map(candidate => (candidate.key === entry.key
          ? { ...candidate, status: 'failed', error: failure }
          : candidate)))
      })()
    }
  }, [limits, session])

  const remove = useCallback((key: string) => {
    const existing = entriesRef.current.find(candidate => candidate.key === key)
    if (existing !== undefined && existing.preview.startsWith('blob:')) {
      blobUrlsRef.current.delete(existing.preview)
      URL.revokeObjectURL(existing.preview)
    }
    entriesRef.current = entriesRef.current.filter(candidate => candidate.key !== key)
    setEntries(current => current.filter(candidate => candidate.key !== key))
  }, [])

  const ids = useMemo(
    () => entries.flatMap(entry => (entry.attachment === undefined ? [] : [entry.attachment.id])),
    [entries],
  )
  const blocked = entries.some(entry => entry.status !== 'ready') || ids.length !== entries.length

  return { entries, add, remove, blocked, ids, notice }
}
