/**
 * Artifact chips: every `artifact://<id>` a message text cites becomes an entry
 * point that opens that spilled output in the detail panel. The copied markdown
 * sanitizer anchors only http(s)/mailto, so without this the reference would be
 * inert text — the entry point must not be lost.
 */

import { useMemo } from 'react'
import { IconArchiveOutlineRegular } from '../../ui/primitives/index.ts'
import css from './ArtifactRefs.module.css'

/** Every distinct artifact id a text cites, in first-seen order. */
export function artifactIdsIn(text: string): string[] {
  const found = text.matchAll(/artifact:\/\/([A-Za-z0-9._-]+)/g)
  return [...new Set([...found].map(match => match[1]!))]
}

/**
 * Render the chips for one message.
 * @param props.text - the message text.
 * @param props.onOpen - open one artifact in the detail panel.
 * @returns the chip row, or null when the text cites none.
 */
export function ArtifactRefs({ text, onOpen }: { text: string; onOpen: (id: string) => void }) {
  const ids = useMemo(() => artifactIdsIn(text), [text])
  if (ids.length === 0) return null
  return (
    <div className={css.row} data-artifact-refs={ids.length}>
      {ids.map(id => (
        <button
          key={id}
          type="button"
          className={css.chip}
          aria-label={`打开产物 ${id}`}
          onClick={() => { onOpen(id) }}
        >
          <IconArchiveOutlineRegular size={12} />
          {`artifact://${id}`}
        </button>
      ))}
    </div>
  )
}
