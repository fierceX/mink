import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { SettingsDialog } from './SettingsDialog.tsx'
import { DEFAULT_PREFERENCES, type Preferences } from './preferences.ts'

afterEach(cleanup)

describe('settings dialog', () => {
  it('writes the theme cube and the stepper through the onChange patch', () => {
    const onChange = vi.fn()
    render(
      <SettingsDialog
        preferences={DEFAULT_PREFERENCES}
        onChange={onChange}
        onClose={() => {}}
      />,
    )
    expect(screen.getByRole('dialog')).toBeDefined()
    // The system cube is the persisted selection, not the resolved palette.
    expect(screen.getByRole('button', { name: /跟随系统/ }).getAttribute('aria-pressed')).toBe('true')

    fireEvent.click(screen.getByRole('button', { name: /深色/ }))
    expect(onChange).toHaveBeenCalledWith({ theme: 'dark' })

    fireEvent.click(screen.getByRole('button', { name: '增大字号' }))
    expect(onChange).toHaveBeenCalledWith({ fontSize: DEFAULT_PREFERENCES.fontSize + 1 })
  })

  it('disables the stepper at the contract edges and reports the chosen process display', () => {
    const onChange = vi.fn()
    const preferences: Preferences = { theme: 'light', fontSize: 22, process: 'standard' }
    render(<SettingsDialog preferences={preferences} onChange={onChange} onClose={() => {}} />)
    expect(screen.getByRole('button', { name: '增大字号' })).toHaveProperty('disabled', true)
    expect(screen.getByRole('button', { name: '减小字号' })).toHaveProperty('disabled', false)

    fireEvent.click(screen.getByRole('tab', { name: '简洁' }))
    expect(onChange).toHaveBeenCalledWith({ process: 'concise' })
  })
})
