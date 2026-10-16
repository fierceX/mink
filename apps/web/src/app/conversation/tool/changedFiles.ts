/**
 * Changed files of one turn: the Edit/Write results it recorded, folded per path.
 * Counts come from the same diff extraction the tool cards use, so the card and
 * the rows inside it can never disagree with the cards above them.
 */

import type { TranscriptItem } from '../../../data/types.ts'
import { diffTotals, type DiffBlockProps } from '../../../ui/primitives/index.ts'
import { diffCard } from './toolCards.ts'

/** One file a turn changed. */
export interface ChangedFile {
  /** Path as the tool named it. */
  path: string
  added: number
  deleted: number
  /** The hunks the turn recorded for this path, for the row's hover preview. */
  diffs: DiffBlockProps['diffs']
}

/** Top-level fold threshold the source's card uses. */
export const CHANGED_FILES_FOLD = 4

const MUTATING = ['Edit', 'Write']

/**
 * Collect the turn's changed files.
 * @param items - the turn's transcript items.
 * @returns one entry per path, in first-seen order; empty when nothing changed.
 */
export function changedFilesOf(items: readonly TranscriptItem[]): ChangedFile[] {
  const byPath = new Map<string, ChangedFile>()
  for (const item of items) {
    if (item.kind !== 'tool' || !MUTATING.includes(item.name) || item.result === undefined) continue
    const card = diffCard(item)
    if (card === null || card.diffs.length === 0) continue
    const totals = diffTotals(card.diffs)
    const path = card.diffs[0]?.path ?? item.summary
    if (path === '') continue
    const existing = byPath.get(path)
    if (existing === undefined) {
      byPath.set(path, {
        path,
        added: totals.added,
        deleted: totals.removed,
        diffs: card.diffs,
      })
    } else {
      byPath.set(path, {
        path,
        added: existing.added + totals.added,
        deleted: existing.deleted + totals.removed,
        diffs: [...existing.diffs, ...card.diffs],
      })
    }
  }
  return [...byPath.values()]
}
