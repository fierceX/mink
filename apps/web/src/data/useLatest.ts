/**
 * Latest-value ref: a binding that changes between renders (the open session,
 * its runtime generation) is published to a ref so an asynchronous completion —
 * a submission's reply, a model read, a command's answer — can compare the
 * value it started with against the one in force now and drop a stale result
 * instead of writing it onto another session.
 */

import { useLayoutEffect, useRef } from 'react'

/**
 * Track the latest value of a binding.
 * @param value - the value as of this render.
 * @returns a ref whose `current` is that value after every commit.
 */
export function useLatest<T>(value: T): { current: T } {
  const ref = useRef(value)
  // A layout effect publishes before the browser paints and therefore before the
  // next event or promise continuation can observe it.
  useLayoutEffect(() => {
    ref.current = value
  })
  return ref
}
