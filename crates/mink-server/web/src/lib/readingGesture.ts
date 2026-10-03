/** A deliberate scroll direction, not layout or streaming updates, changes mobile chrome. */
export function readingGesture(threshold = 48) {
  let distance = 0;
  return {
    reset() { distance = 0; },
    consume(delta: number): boolean | undefined {
      if (!Number.isFinite(delta) || Math.abs(delta) < 2) return;
      if (Math.sign(delta) !== Math.sign(distance)) distance = 0;
      distance += delta;
      if (Math.abs(distance) < threshold) return;
      const reading = distance > 0;
      distance = 0;
      return reading;
    },
  };
}
