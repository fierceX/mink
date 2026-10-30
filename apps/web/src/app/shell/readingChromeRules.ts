/**
 * When the narrow-layout reading chrome must stay on screen.
 *
 * The chrome steps aside only for a reader who is only reading. Anything the
 * conversation is still waiting on — a focused composer, drafted attachments, a
 * running turn, a receipt the queue owes, an in-flight submission, a failure on
 * screen or a connection notice — keeps the header and input card visible.
 * Keeping this a pure predicate gives every pin one place and one truth table.
 */

/** What the conversation is currently waiting on. */
export interface ReadingChromePins {
  /** The composer region holds focus, including its own menus and pickers. */
  composerFocused: boolean
  /** Draft attachments waiting for the next message. */
  attachmentCount: number
  /** A turn is executing; its stop and guidance controls must stay reachable. */
  running: boolean
  /** Inputs the session still owes an answer for (queued, failed or awaiting). */
  awaitingInputs: number
  /** A submission is in flight or its receipt is unresolved. */
  submissionPending: boolean
  /** A failed open/submission the reader has not dismissed is on screen. */
  hasError: boolean
  /** The connection is connecting, lost, or just recovered. */
  connectionNotice: boolean
}

/**
 * Decide whether the reading chrome is pinned by conversation state.
 * @param pins - the conversation's current waiting state.
 * @returns true when the chrome must stay visible.
 */
export function readingChromePinned(pins: ReadingChromePins): boolean {
  return pins.composerFocused
    || pins.attachmentCount > 0
    || pins.running
    || pins.awaitingInputs > 0
    || pins.submissionPending
    || pins.hasError
    || pins.connectionNotice
}
