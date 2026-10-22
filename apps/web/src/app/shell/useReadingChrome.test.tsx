import { act, renderHook } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { useReadingChrome } from './useReadingChrome.ts'

describe('reading chrome', () => {
  it('does not hide outside the narrow layout', () => {
    const { result } = renderHook(() => useReadingChrome(false))

    act(() => { result.current.hide() })

    expect(result.current.hidden).toBe(false)
  })

  it('hides and reveals inside the narrow layout', () => {
    const { result } = renderHook(() => useReadingChrome(true))

    act(() => { result.current.hide() })
    expect(result.current.hidden).toBe(true)

    act(() => { result.current.reveal() })
    expect(result.current.hidden).toBe(false)
  })

  it('restores the chrome when the layout leaves the narrow breakpoint', () => {
    const { result, rerender } = renderHook(
      ({ enabled }: { enabled: boolean }) => useReadingChrome(enabled),
      { initialProps: { enabled: true } },
    )

    act(() => { result.current.hide() })
    expect(result.current.hidden).toBe(true)

    rerender({ enabled: false })
    expect(result.current.hidden).toBe(false)
  })
})
