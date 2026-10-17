import { act, cleanup, renderHook } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { SessionSummary } from '../../data/api.ts'
import type { ImageLimits } from '../../data/types.ts'

const upload = vi.fn((..._args: unknown[]) => new Promise(() => {}))
vi.mock('../../data/api.ts', () => ({
  api: { upload: (...args: unknown[]) => upload(...args) },
  sessionUrl: (id: string, path: string) => `/api/sessions/${id}${path}`,
}))

import { useAttachments } from './useAttachments.ts'

const session = {
  id: 'session-1',
  project_key: 'project-1',
  cwd: '/tmp/work',
  title: 'fixture',
  alias: null,
  status: 'free',
  modified_secs: 0,
  created_at: '',
  updated_at: '',
  parent: null,
} as unknown as SessionSummary

const limits: ImageLimits = {
  kind: 'supported',
  allowed_mime: ['image/png'],
  max_image_bytes: 1024 * 1024,
  max_image_bytes_per_request: 2 * 1024 * 1024,
  max_images_per_request: 4,
}

const file = new File([new Uint8Array([1])], 'shot.png', { type: 'image/png' })

afterEach(() => {
  cleanup()
  localStorage.clear()
  vi.clearAllMocks()
})

describe('draft attachments by session identity', () => {
  it('keeps an in-flight upload when the same identity is re-read as a new object', () => {
    Object.defineProperty(URL, 'createObjectURL', { value: () => 'blob:preview', configurable: true })
    Object.defineProperty(URL, 'revokeObjectURL', { value: () => {}, configurable: true })

    const { result, rerender } = renderHook(
      (props: { session: SessionSummary }) => useAttachments(props.session, limits),
      { initialProps: { session } },
    )
    act(() => result.current.add([file]))
    expect(result.current.entries).toHaveLength(1)
    expect(result.current.entries[0]?.status).toBe('uploading')

    // A catalog refresh hands the view a fresh SessionSummary object for the
    // same project:id; the upload must not be reset (or orphaned) by it.
    rerender({ session: { ...session } })
    expect(result.current.entries).toHaveLength(1)
    expect(result.current.entries[0]?.status).toBe('uploading')
  })
})
