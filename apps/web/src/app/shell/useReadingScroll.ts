/**
 * Transcript → reading-chrome wiring: only a scroll the reader performed moves
 * the mobile chrome. Wheel and touch events mark the next ~700ms as user scroll,
 * so the layout restores and streaming appends the transcript performs on its own
 * never hide the header or the composer.
 */

import { useEffect } from 'react'
import type { RefObject } from 'react'
import type { ReadingChrome } from './useReadingChrome.ts'

/**
 * Feed the transcript's user scrolls into the chrome gesture detector.
 * @param chrome - the shell's chrome state, when the narrow layout is active.
 * @param element - the transcript's scroll container.
 */
export function useReadingScroll(
  chrome: ReadingChrome | undefined,
  element: RefObject<HTMLDivElement | null>,
): void {
  useEffect(() => {
    const box = element.current
    if (chrome === undefined || box === null) return

    // A scroll inside a nested viewer (process body, file preview) is that viewer's
    // own reading position, not the transcript's.
    const targetIsInner = (event: Event): boolean => {
      const target = event.target
      return target instanceof Element && target.closest('[data-inner-scroll]') !== null
    }
    const onWheel = (event: WheelEvent): void => {
      if (event.ctrlKey || event.shiftKey) return
      if (Math.abs(event.deltaY) <= Math.abs(event.deltaX)) return
      if (targetIsInner(event)) return
      // Positive deltaY moves toward the newest output, which is what the chrome
      // reads as a gesture; the sign travels with the arm so the transcript's own
      // settling cannot be mistaken for the reader's.
      chrome.armUserScroll(Math.sign(event.deltaY))
    }
    const onTouchStart = (): void => {
      chrome.armUserScroll()
    }
    const onKeyDown = (event: KeyboardEvent): void => {
      // Keys a menu or picker already handles are that control's, not a reading
      // gesture for the transcript.
      const active = document.activeElement
      if (active instanceof Element && active.closest('[role="menu"], [role="listbox"]') !== null) return
      if (!['PageDown', 'PageUp', 'ArrowDown', 'ArrowUp', 'Home', 'End', ' '].includes(event.key)) return
      chrome.armUserScroll(event.key === 'PageDown' || event.key === 'ArrowDown' || event.key === 'End' || event.key === ' ' ? 1 : -1)
    }
    let previous = box.scrollTop
    const onScroll = (): void => {
      const current = box.scrollTop
      const delta = current - previous
      previous = current
      chrome.consumed(delta)
    }

    box.addEventListener('wheel', onWheel, { passive: true })
    box.addEventListener('touchstart', onTouchStart, { passive: true })
    box.addEventListener('keydown', onKeyDown)
    box.addEventListener('scroll', onScroll, { passive: true })
    return () => {
      box.removeEventListener('wheel', onWheel)
      box.removeEventListener('touchstart', onTouchStart)
      box.removeEventListener('keydown', onKeyDown)
      box.removeEventListener('scroll', onScroll)
    }
  }, [chrome, element])

  useEffect(() => {
    const box = element.current
    if (chrome === undefined || box === null) return
    // A tap on content toggles the reading state (never on a control or a text
    // selection): one gesture for both directions, which is what a reader expects
    // from tapping the page.
    const onClick = (event: MouseEvent): void => {
      const target = event.target
      if (target instanceof Element && target.closest('button,a,summary,input,textarea,select,pre,code') !== null) return
      if (window.getSelection()?.toString() !== '') return
      if (chrome.hidden) chrome.reveal()
      else chrome.hide()
    }
    box.addEventListener('click', onClick)
    return () => {
      box.removeEventListener('click', onClick)
    }
  }, [chrome, element])
}
