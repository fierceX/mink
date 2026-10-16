import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderHook, act } from '@testing-library/react'
import {
  DEFAULT_PREFERENCES,
  FONT_SIZE_MAX,
  FONT_SIZE_MIN,
  applyPreferences,
  loadPreferences,
  savePreferences,
  usePreferences,
} from './preferences.ts'

afterEach(() => {
  localStorage.clear()
  document.body.removeAttribute('data-ds-dark-theme')
  document.documentElement.removeAttribute('data-ds-theme-source')
  document.body.style.removeProperty('--dsh-content-font-size')
  vi.unstubAllGlobals()
})

describe('preferences', () => {
  it('falls back to the defaults and ignores stored values outside the contract', () => {
    expect(loadPreferences()).toEqual(DEFAULT_PREFERENCES)
    localStorage.setItem('mink.workbench.preferences', JSON.stringify({
      theme: 'neon',
      fontSize: 99,
      process: 'verbose',
    }))
    expect(loadPreferences()).toEqual(DEFAULT_PREFERENCES)
    localStorage.setItem('mink.workbench.preferences', '{not json')
    expect(loadPreferences()).toEqual(DEFAULT_PREFERENCES)
  })

  it('round-trips a valid preference document', () => {
    savePreferences({ theme: 'dark', fontSize: FONT_SIZE_MIN, process: 'detailed' })
    expect(loadPreferences()).toEqual({ theme: 'dark', fontSize: FONT_SIZE_MIN, process: 'detailed' })
  })

  it('applies the dark attribute, the source marker and the font-size axis', () => {
    applyPreferences({ theme: 'dark', fontSize: 16, process: 'standard' }, true)
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(true)
    expect(document.documentElement.dataset['dsThemeSource']).toBe('dark')
    expect(document.body.style.getPropertyValue('--dsh-content-font-size')).toBe('16px')
  })

  it('resolves the system preference through matchMedia and persists changes', () => {
    const listeners: (() => void)[] = []
    vi.stubGlobal('matchMedia', (query: string) => ({
      matches: query.includes('dark'),
      addEventListener: (_: string, listener: () => void) => { listeners.push(listener) },
      removeEventListener: () => {},
    }))
    const { result } = renderHook(() => usePreferences())
    expect(result.current.preferences.theme).toBe('system')
    // system + prefers dark → the dark palette is on without an explicit choice.
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(true)

    act(() => {
      result.current.update({ theme: 'light', fontSize: FONT_SIZE_MAX })
    })
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(false)
    expect(loadPreferences()).toEqual({ theme: 'light', fontSize: FONT_SIZE_MAX, process: 'standard' })
  })
})
