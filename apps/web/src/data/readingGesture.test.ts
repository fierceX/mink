import { describe, expect, it } from 'vitest'
import { readingGesture } from './readingGesture.ts'

describe('readingGesture', () => {
  it('steps aside after a deliberate forward scroll', () => {
    const gesture = readingGesture()
    expect(gesture.consume(1)).toBeUndefined()
    expect(gesture.consume(20)).toBeUndefined()
    expect(gesture.consume(27)).toBeUndefined()
    expect(gesture.consume(2)).toBe(true)
  })

  it('never changes state on a backward scroll', () => {
    // Backwards is reading too: the way out is a tap, so a reread must not flicker
    // the header and the composer.
    const gesture = readingGesture()
    expect(gesture.consume(-40)).toBeUndefined()
    expect(gesture.consume(-38)).toBeUndefined()
    expect(gesture.consume(-200)).toBeUndefined()
  })

  it('clears a forward run that turns back', () => {
    const gesture = readingGesture()
    gesture.consume(40)
    expect(gesture.consume(-10)).toBeUndefined()
    expect(gesture.consume(20)).toBeUndefined()
    expect(gesture.consume(30)).toBe(true)
  })

  it('ignores noise and layout-driven deltas', () => {
    const gesture = readingGesture()
    expect(gesture.consume(Number.NaN)).toBeUndefined()
    expect(gesture.consume(Number.POSITIVE_INFINITY)).toBeUndefined()
    expect(gesture.consume(1)).toBeUndefined()
    expect(gesture.consume(60)).toBe(true)
    // A completed run starts over.
    expect(gesture.consume(10)).toBeUndefined()
  })
})
