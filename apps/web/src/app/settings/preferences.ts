/**
 * Workbench preferences: the theme preference and the content font size follow
 * the source's own mechanics (a `data-ds-dark-theme` attribute on the body plus
 * the `--dsh-content-font-size` axis that every copied stylesheet reads), and
 * the process-display preference drives how much of a turn's process the
 * transcript shows. Stored per browser, since the browser is the host here.
 */

import { useCallback, useEffect, useState } from 'react'

/** Built-in theme preferences, in cube order. */
export const THEME_PREFERENCES = ['light', 'dark', 'system'] as const

/** Theme preference persisted by the Appearance row. */
export type ThemePreference = typeof THEME_PREFERENCES[number]

/** Smallest accepted content font size (px). */
export const FONT_SIZE_MIN = 10

/** Largest accepted content font size (px). */
export const FONT_SIZE_MAX = 22

/** Content font size when nothing is stored (px). */
export const DEFAULT_FONT_SIZE = 14

/** How much of a turn's process the transcript shows. */
export const PROCESS_DISPLAYS = ['concise', 'standard', 'detailed'] as const

/** Process-display preference. */
export type ProcessDisplay = typeof PROCESS_DISPLAYS[number]

/** Durable workbench preferences. */
export interface Preferences {
  theme: ThemePreference
  fontSize: number
  process: ProcessDisplay
}

const STORAGE_KEY = 'mink.workbench.preferences'

/** Defaults, used when nothing valid is stored. */
export const DEFAULT_PREFERENCES: Preferences = { theme: 'system', fontSize: DEFAULT_FONT_SIZE, process: 'standard' }

function isPreference(value: unknown): value is ThemePreference {
  return THEME_PREFERENCES.some(preference => preference === value)
}

function isProcessDisplay(value: unknown): value is ProcessDisplay {
  return PROCESS_DISPLAYS.some(display => display === value)
}

/** Read the stored preferences, ignoring anything unrecognized. */
export function loadPreferences(): Preferences {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (raw === null) return DEFAULT_PREFERENCES
    const parsed: unknown = JSON.parse(raw)
    if (typeof parsed !== 'object' || parsed === null) return DEFAULT_PREFERENCES
    const record = parsed as Record<string, unknown>
    const fontSize = record['fontSize']
    return {
      theme: isPreference(record['theme']) ? record['theme'] : DEFAULT_PREFERENCES.theme,
      fontSize: typeof fontSize === 'number' && Number.isInteger(fontSize)
        && fontSize >= FONT_SIZE_MIN && fontSize <= FONT_SIZE_MAX
        ? fontSize
        : DEFAULT_PREFERENCES.fontSize,
      process: isProcessDisplay(record['process']) ? record['process'] : DEFAULT_PREFERENCES.process,
    }
  } catch {
    return DEFAULT_PREFERENCES
  }
}

/** Persist preferences; a storage refusal leaves the session running. */
export function savePreferences(preferences: Preferences): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(preferences))
  } catch {
    // Private-mode or quota refusal: the preference still applies to this page.
  }
}

/** Whether the operating system asks for the dark palette. */
export function systemPrefersDark(): boolean {
  return typeof matchMedia === 'function' && matchMedia('(prefers-color-scheme: dark)').matches
}

/**
 * Apply preferences to the document: the dark attribute on the body, the
 * palette source marker, the document canvas color scheme and the content font
 * size axis the copied stylesheets read.
 * @param preferences - the preferences to apply.
 * @param dark - whether the dark palette is active (resolved by the caller).
 */
export function applyPreferences(preferences: Preferences, dark: boolean): void {
  document.documentElement.dataset['dsThemeSource'] = preferences.theme
  document.body.toggleAttribute('data-ds-dark-theme', dark)
  document.body.style.setProperty('--dsh-content-font-size', `${preferences.fontSize}px`)
}

export interface PreferencesHandle {
  preferences: Preferences
  update: (patch: Partial<Preferences>) => void
}

/**
 * Hold the workbench preferences, applying and persisting every change.
 * @returns the current preferences and their writer.
 */
export function usePreferences(): PreferencesHandle {
  const [preferences, setPreferences] = useState<Preferences>(() => loadPreferences())

  // The system preference only matters while the theme preference follows it.
  useEffect(() => {
    const apply = (): void => {
      applyPreferences(preferences, preferences.theme === 'dark'
        || (preferences.theme === 'system' && systemPrefersDark()))
    }
    apply()
    if (preferences.theme !== 'system' || typeof matchMedia !== 'function') return
    const query = matchMedia('(prefers-color-scheme: dark)')
    query.addEventListener('change', apply)
    return () => {
      query.removeEventListener('change', apply)
    }
  }, [preferences])

  const update = useCallback((patch: Partial<Preferences>) => {
    setPreferences((current) => {
      const next = { ...current, ...patch }
      savePreferences(next)
      return next
    })
  }, [])

  return { preferences, update }
}
