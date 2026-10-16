/**
 * Changed-files card: what one turn edited, drawn with the source's own
 * changed-files sheet (filled header with a file-type tile, per-file rows with
 * added/deleted counts, and a fold control past four rows). Rows open the file
 * in the detail panel's file tab, which is where this workbench shows contents.
 */

import { useMemo, useState } from 'react'
import {
  FileTypeIcon,
  HoverCard,
  IconChevronDownOutlineRegular,
  IconChevronUpOutlineRegular,
} from '../../ui/primitives/index.ts'
import previewCss from './ChangedFilePreview.module.css'
import type { ChangedFile } from './tool/changedFiles.ts'
import css from './ChangedFiles.module.css'

/**
 * Render the turn's changed files.
 * @param props.files - changed files, in first-seen order.
 * @param props.onOpen - open one file in the detail panel.
 * @returns the card, or null when the turn changed nothing.
 */
export function ChangedFiles({ files, onOpen }: {
  files: readonly ChangedFile[]
  onOpen: (path: string) => void
}) {
  const [expanded, setExpanded] = useState(false)
  if (files.length === 0) return null

  const added = files.reduce((sum, file) => sum + file.added, 0)
  const deleted = files.reduce((sum, file) => sum + file.deleted, 0)
  const single = files.length === 1 ? files[0] : undefined
  const foldable = files.length > 4
  const rows = foldable && !expanded ? files.slice(0, 4) : files
  const counts = (
    <span className={css.statCounts}>
      <span className={css.added}>{`+${added}`}</span>
      <span className={css.deleted}>{`-${deleted}`}</span>
    </span>
  )
  // Every row answers a hover with the hunks the turn recorded for it: the same
  // preview idea the source's changed-files card uses, drawn from data already in
  // the transcript instead of a second fetch.
  const previews = useMemo(() => new Map(files.map(file => [file.path, file.diffs])), [files])

  return (
    <div className={css.card} data-changed-files data-single={single !== undefined || undefined}>
      <button
        type="button"
        className={css.header}
        aria-label={single === undefined ? `查看 ${files.length} 个改动文件` : `查看 ${single.path} 的改动`}
        onClick={() => { onOpen(single?.path ?? files[0]!.path) }}
      >
        <span className={css.tile}>
          <FileTypeIcon path={single?.path ?? files[0]!.path} size={20} />
        </span>
        <span className={css.titles}>
          <span className={css.title}>
            {single === undefined ? `已编辑 ${files.length} 个文件` : `已编辑 ${single.path}`}
          </span>
          <span className={css.stat}>
            {counts}
            <span className={css.previewHint}>在侧栏预览</span>
          </span>
        </span>
      </button>
      {single === undefined && (
        <ul className={css.list}>
          {rows.map(file => (
            <li key={file.path}>
              <HoverCard
                variant="preview"
                openDelayMs={500}
                content={<ChangePreview path={file.path} diffs={previews.get(file.path) ?? []} added={file.added} deleted={file.deleted} />}
                anchor={(
                  <button
                    type="button"
                    className={css.row}
                    aria-label={`查看 ${file.path} 的改动`}
                    onClick={() => { onOpen(file.path) }}
                  >
                    <span className={css.path}>{file.path}</span>
                    <span className={css.counts}>
                      <span className={css.added}>{`+${file.added}`}</span>
                      <span className={css.deleted}>{`-${file.deleted}`}</span>
                    </span>
                  </button>
                )}
              />
            </li>
          ))}
        </ul>
      )}
      {foldable && (
        <button
          type="button"
          className={css.toggle}
          aria-expanded={expanded}
          aria-label={expanded ? `收起文件列表（${files.length} 个）` : `展开全部 ${files.length} 个文件`}
          onClick={() => { setExpanded(value => !value) }}
        >
          <span>{expanded ? '收起' : `全部 ${files.length} 个文件`}</span>
          {expanded ? <IconChevronUpOutlineRegular /> : <IconChevronDownOutlineRegular />}
        </button>
      )}
    </div>
  )
}

/** Hunks shown in one row's preview before it cuts off. */
const PREVIEW_LINES = 14

/**
 * The hover preview for one changed file: its path, its counts and the hunks the
 * turn recorded, cut to a readable height.
 * @param props.path - the file the row names.
 * @param props.diffs - hunks recorded for that path.
 * @param props.added - lines added in the turn.
 * @param props.deleted - lines removed in the turn.
 * @returns the preview card body.
 */
function ChangePreview({ path, diffs, added, deleted }: {
  path: string
  diffs: ChangedFile['diffs']
  added: number
  deleted: number
}) {
  // The recorded hunks carry before/after text; the preview shows the new side
  // with the added lines marked, so the row answers with what changed.
  const lines = diffs.flatMap(diff => diff.newText.split('\n').map(text => ({ text, added: true })))
  const shown = lines.slice(0, PREVIEW_LINES)
  return (
    <div className={previewCss.root} data-changed-file-preview>
      <div className={previewCss.head}>
        <span className={previewCss.path}>{path}</span>
        <span className={previewCss.counts}>
          <span className={previewCss.added}>{`+${added}`}</span>
          <span className={previewCss.deleted}>{`-${deleted}`}</span>
        </span>
      </div>
      <pre className={previewCss.body}>
        {shown.map((line, index) => (
          <span key={index} className={previewCss.add}>
            {`+${line.text}\n`}
          </span>
        ))}
      </pre>
      {lines.length > shown.length && <div className={previewCss.more}>{`还有 ${lines.length - shown.length} 行`}</div>}
    </div>
  )
}
