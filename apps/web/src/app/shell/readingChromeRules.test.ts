import { describe, expect, it } from 'vitest'
import { readingChromePinned, type ReadingChromePins } from './readingChromeRules.ts'

const CLEAR: ReadingChromePins = {
  composerFocused: false,
  attachmentCount: 0,
  running: false,
  awaitingInputs: 0,
  submissionPending: false,
  hasError: false,
  connectionNotice: false,
}

describe('reading chrome pins', () => {
  it('is free to hide only when nothing is waiting', () => {
    expect(readingChromePinned(CLEAR)).toBe(false)
  })

  it('pins for each waiting condition on its own', () => {
    const pins: (keyof ReadingChromePins)[] = [
      'composerFocused',
      'running',
      'submissionPending',
      'hasError',
      'connectionNotice',
    ]
    for (const key of pins) {
      expect(readingChromePinned({ ...CLEAR, [key]: true }), key).toBe(true)
    }
    expect(readingChromePinned({ ...CLEAR, attachmentCount: 1 })).toBe(true)
    expect(readingChromePinned({ ...CLEAR, awaitingInputs: 1 })).toBe(true)
  })

  it('stays pinned for every combination of conditions', () => {
    const flags = [
      'composerFocused',
      'running',
      'submissionPending',
      'hasError',
      'connectionNotice',
    ] as const
    for (let mask = 0; mask < 1 << flags.length; mask += 1) {
      const pins: ReadingChromePins = { ...CLEAR }
      for (const [index, key] of flags.entries()) pins[key] = (mask & (1 << index)) !== 0
      const counts = (mask % 4) as 0 | 1 | 2 | 3
      pins.attachmentCount = counts === 0 ? 0 : 1
      pins.awaitingInputs = counts === 0 ? 0 : counts - 1
      expect(readingChromePinned(pins), `mask ${mask}`).toBe(mask !== 0)
    }
  })
})
