/**
 * The shell is sized from what the device actually shows.
 *
 * CSS `dvh` covers modern browsers, but a phone browser without it (or one whose
 * on-screen keyboard shrinks only the visual viewport) keeps the layout height:
 * the bottom of the app then sits behind the browser's own bar, the input card
 * overlaps the transcript, and scrolling exposes empty space. `visualViewport`
 * reports the visible box in every browser that has one, so its height and offset
 * are published as custom properties the stylesheets read.
 */

import { useEffect } from 'react'

/** Height of the visible viewport, in px, as a CSS length. */
export const VIEWPORT_HEIGHT = '--mk-viewport-height'

/** Offset of the visible viewport from the layout viewport, in px. */
export const VIEWPORT_TOP = '--mk-viewport-top'

/**
 * Keep the shell's height and offset equal to the visible viewport.
 */
export function useVisualViewport(): void {
  useEffect(() => {
    // `visualViewport` is absent in jsdom and in some embedded browsers.
    const viewport: VisualViewport | null = window.visualViewport ?? null
    if (viewport === null) return
    const root = document.documentElement
    const apply = (): void => {
      root.style.setProperty(VIEWPORT_HEIGHT, `${Math.round(viewport.height)}px`)
      root.style.setProperty(VIEWPORT_TOP, `${Math.round(viewport.offsetTop)}px`)
      // A browser that panned the page to reach a focused field leaves the shell
      // behind its window; the app owns the whole page, so put the pan back.
      if (viewport.offsetTop !== 0 || window.scrollY !== 0) window.scrollTo(0, 0)
    }
    apply()
    viewport.addEventListener('resize', apply)
    viewport.addEventListener('scroll', apply)
    window.addEventListener('orientationchange', apply)
    window.addEventListener('focusout', apply)
    document.addEventListener('visibilitychange', apply)
    return () => {
      viewport.removeEventListener('resize', apply)
      viewport.removeEventListener('scroll', apply)
      window.removeEventListener('orientationchange', apply)
      window.removeEventListener('focusout', apply)
      document.removeEventListener('visibilitychange', apply)
    }
  }, [])
}
