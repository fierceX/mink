import { useCallback, useEffect, useRef, useState } from 'react'
import type { SessionState } from '../../data/types.ts'

/** A reader this close to the end counts as following the tail. */
export const TAIL_SLACK = 24

/** A pending scroll-anchor measurement across prepending older history rows. */
export interface ScrollAnchor {
  row: HTMLElement | undefined
  rowTop: number
  generation: string | undefined
  heightBefore: number
  topBefore: number
}

/** Follow the transcript tail while the session is at its end. */
export function useStickToBottom(
  state: SessionState | undefined,
  element: React.RefObject<HTMLDivElement | null>,
  content: React.RefObject<HTMLDivElement | null>,
  anchorRestore: React.RefObject<ScrollAnchor | null>,
  followTick: number,
) {
  const pinned = useRef(true)
  // Whether the transcript is at its tail is visible to the reader (the
  // return-to-latest control), so the ref is mirrored into state.
  const [following, setFollowing] = useState(true)
  const items = state?.items
  const length = items?.length ?? 0

  useEffect(() => {
    const box = element.current
    if (box === null) return
    const onScroll = () => {
      const atTail = box.scrollHeight - box.scrollTop - box.clientHeight < 80
      pinned.current = atTail
      setFollowing(atTail)
    }
    box.addEventListener('scroll', onScroll)
    return () => {
      box.removeEventListener('scroll', onScroll)
    }
  }, [element])

  useEffect(() => {
    const target = content.current
    const box = element.current
    if (target === null || box === null || typeof ResizeObserver !== 'function') return
    const observer = new ResizeObserver(() => {
      if (anchorRestore.current !== null || !pinned.current) return
      box.scrollTop = box.scrollHeight
    })
    observer.observe(target)
    return () => { observer.disconnect() }
  }, [element, content, anchorRestore, length])

  useEffect(() => {
    // An explicit follow request re-pins before it scrolls: the reader asked for
    // the tail, so the gesture wins over the position they had scrolled to.
    if (followTick > 0) {
      pinned.current = true
      setFollowing(true)
    }
    const box = element.current
    if (box === null || anchorRestore.current !== null || !pinned.current) return
    box.scrollTop = box.scrollHeight
  }, [element, anchorRestore, length, followTick])

  const follow = useCallback(() => {
    pinned.current = true
    setFollowing(true)
    const box = element.current
    if (box !== null) box.scrollTop = box.scrollHeight
  }, [element])

  /** Jump to one turn: reading position is deliberate, so following stops. */
  const jump = useCallback((key: string) => {
    const box = element.current
    if (box === null) return
    const row = box.querySelector<HTMLElement>(`[data-turn-key="${CSS.escape(key)}"]`)
    if (row === null) return
    pinned.current = false
    setFollowing(false)
    row.scrollIntoView({ block: 'start' })
  }, [element])

  return { following, follow, jump }
}
