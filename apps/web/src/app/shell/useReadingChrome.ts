/**
 * Mobile reading chrome: on a narrow viewport the header and composer step aside
 * while the reader scrolls down through the transcript, and come back on the
 * gestures the legacy workbench defines — a deliberate upward scroll, a tap on
 * content, Escape, or focusing the composer. Layout shifts and streaming updates
 * never move the chrome, and hiding is presentation only: it never stops a turn
 * and never closes the session.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
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

  // Leaving the narrow viewport, or locking input, always restores the chrome —
  // a hidden composer is only ever a reading state.
  useEffect(() => {
    if (!enabled || inputLocked) setHidden(false)
  }, [enabled, inputLocked])

  const reveal = useCallback(() => {
    gesture.current.reset()
    setHidden(false)
  }, [])

  const hide = useCallback(() => {
    // A tap is deliberate: the gesture detector is reset so a following scroll does
    // not read as the same movement.
    gesture.current.reset()
    setHidden(true)
  }, [])

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
    if (!enabled || !isUserScroll()) return
    const direction = userScrollDirection.current
    if (direction !== 0 && Math.sign(delta) !== direction) return
    const reading = gesture.current.consume(delta)
    if (reading === undefined) return
    setHidden(reading)
  }, [enabled, isUserScroll])

  return { hidden, reveal, hide, consumed, armUserScroll, isUserScroll, setInputLocked }
}
