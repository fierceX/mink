/**
 * Mink brand mark: the M glyph the sidebar and hero show in the seat the DSH
 * shell fills with its own whale mark. Drawn from two strokes, no brand asset
 * is copied.
 */

/** Render the mark at the requested box size. */
export function MinkMark({ size = 24 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M4 18.5V8.2a1.6 1.6 0 0 1 2.75-1.1L12 12.6l5.25-5.5A1.6 1.6 0 0 1 20 8.2v10.3" />
    </svg>
  )
}
