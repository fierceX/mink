/**
 * Mobile reading chrome: on a narrow viewport the header and composer step aside
 * while the reader scrolls down through the transcript, and come back on the
 * gestures the legacy workbench defines — a deliberate upward scroll, a tap on
 * content, Escape, or focusing the composer. Layout shifts and streaming updates
 * never move the chrome. Hiding is presentation only: it never stops a turn and
 * never closes the session; while the conversation is waiting on anything (focus,
 * attachments, a running turn, queued receipts, a submission in flight, an error
 * or a connection notice) the chrome stays visible.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { readingGesture } from '../../data/readingGesture.ts'

/** Layout slot the chrome occupies, so the transcript can keep its position. */
export interface ReadingChrome {
  /** True while the chrome is stepped aside. */
  hidden: boolean
  /** Bring the chrome back (the corner control, Escape). */
  reveal: () => void
  /** Step the chrome aside (a tap on the content, while it is on screen). */
  hide: () => void
  /** Feed one user scroll delta into the gesture detector. */
  consumed: (delta: number) => void
  /**
   * A user scroll is in flight; layout-driven scrolls must not be counted.
   * @param direction - the direction the reader moved in (0 when unknown, as for a
   *   touch that has not moved yet): a delta against it is the page settling, not
   *   the reader.
   */
  armUserScroll: (direction?: number) => void
  /** The gesture detector only counts deltas right after a user scroll. */
  isUserScroll: () => boolean
  /** Composer focus pins the chrome: typing must never happen off-screen. */
  setInputLocked: (locked: boolean) => void
}

/**
 * Track the mobile reading chrome for one transcript.
 * @param enabled - the viewport is narrow enough for the chrome to step aside.
 * @returns the chrome state and the transcript's callbacks.
 */
export function useReadingChrome(enabled: boolean): ReadingChrome {
  const gesture = useRef(readingGesture())
  const userScrollUntil = useRef(0)
  const userScrollDirection = useRef(0)
  const [hidden, setHidden] = useState(false)
  const [inputLocked, setInputLocked] = useState(false)
  // The one predicate every hide path shares: the chrome steps aside only in
  // the narrow layout, and only while the conversation does not need it on
  // screen (focus, attachments, a running turn, an awaiting receipt, an error or
  // a connection notice). Revealing is always allowed.
  const canHide = enabled && !inputLocked

  // Leaving the narrow viewport, or a condition that pins the chrome, always
  // restores it — this is the one place a state already hidden is brought back.
  useEffect(() => {
    if (!canHide) setHidden(false)
  }, [canHide])

  const reveal = useCallback(() => {
    gesture.current.reset()
    setHidden(false)
  }, [])

  const hide = useCallback(() => {
    // Reading chrome exists only in the narrow layout. The transcript's click
    // handler is mounted whenever the shell passes this object, so a desktop
    // tap must not be able to hide the header and composer — and a locked
    // conversation must never lose the controls it is waiting on.
    if (!canHide) return
    // A tap is deliberate: the gesture detector is reset so a following scroll does
    // not read as the same movement.
    gesture.current.reset()
    setHidden(true)
  }, [canHide])

  useEffect(() => {
    const onKey = (event: KeyboardEvent): void => {
      if (event.key !== 'Escape' || event.isComposing || !hidden) return
      reveal()
    }
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('keydown', onKey)
    }
  }, [hidden, reveal])

  const armUserScroll = useCallback((direction = 0) => {
    userScrollUntil.current = performance.now() + 700
    userScrollDirection.current = Math.sign(direction)
  }, [])

  const isUserScroll = useCallback(
    () => performance.now() < userScrollUntil.current,
    [],
  )

  const consumed = useCallback((delta: number) => {
    if (!canHide || !isUserScroll()) return
    const direction = userScrollDirection.current
    if (direction !== 0 && Math.sign(delta) !== direction) return
    const reading = gesture.current.consume(delta)
    if (reading === undefined) return
    setHidden(true)
  }, [canHide, isUserScroll])

  // The transcript's effects depend on this object. Keeping it stable across
  // renders avoids unbinding and rebinding the scroll/click listeners on every
  // streaming frame.
  return useMemo(
    () => ({ hidden, reveal, hide, consumed, armUserScroll, isUserScroll, setInputLocked }),
    [hidden, reveal, hide, consumed, armUserScroll, isUserScroll, setInputLocked],
  )
}
