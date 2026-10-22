import { act, cleanup, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { useStagedAttachments } from './useStagedAttachments.ts'

beforeEach(() => {
  vi.stubGlobal('URL', {
    createObjectURL: vi.fn(() => 'blob:staged'),
    revokeObjectURL: vi.fn(),
  })
})

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

function image(name: string, size: number): File {
  const file = new File(['x'], name, { type: 'image/png' })
  Object.defineProperty(file, 'size', { value: size })
  return file
}

describe('staged attachments', () => {
  it('enforces the request-wide byte ceiling before a session exists', () => {
    const { result } = renderHook(() => useStagedAttachments())
    act(() => {
      result.current.add([
        image('a.png', 5 * 1024 * 1024),
        image('b.png', 5 * 1024 * 1024),
        image('c.png', 5 * 1024 * 1024),
        image('d.png', 5 * 1024 * 1024),
      ])
    })
    expect(result.current.entries).toHaveLength(3)
    expect(result.current.files).toHaveLength(3)
    expect(result.current.notice).toContain('总大小')
  })
})
