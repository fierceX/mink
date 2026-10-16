/**
 * A deliberate forward scroll, not layout or streaming updates, steps the mobile
 * chrome aside. Scrolling back is reading too: the way out is a tap on the content
 * or the control that sits in the corner (returning the chrome on an upward scroll
 * made every reread flicker the header and the composer).
 */
export function readingGesture(threshold = 48) {
  let distance = 0;
  return {
    reset() { distance = 0; },
    consume(delta: number): boolean | undefined {
      if (!Number.isFinite(delta) || Math.abs(delta) < 2) return;
      if (delta < 0) {
        // Backwards: never a state change, and it clears the forward run.
        distance = 0;
        return;
      }
      distance += delta;
      if (distance < threshold) return;
      distance = 0;
      return true;
    },
  };
}
