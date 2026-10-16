import { afterEach, describe, expect, it, vi } from 'vitest'
import { requestId } from './request-id.ts'

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

describe('requestId', () => {
  afterEach(() => { vi.unstubAllGlobals() })

  it('uses crypto.randomUUID when the page has it (a secure context)', () => {
    vi.stubGlobal('crypto', { randomUUID: () => '11111111-2222-4333-8444-555555555555' })
    expect(requestId()).toBe('11111111-2222-4333-8444-555555555555')
  })

  it('falls back to getRandomValues where randomUUID is absent (plain HTTP on a LAN)', () => {
    // The field case: a phone opening the workbench over http://192.168.x.x, where
    // randomUUID does not exist and every submission used to fail before leaving
    // the page.
    vi.stubGlobal('crypto', {
      getRandomValues: (bytes: Uint8Array) => {
        bytes.fill(7)
        return bytes
      },
    })
    const id = requestId()
    expect(id).toMatch(UUID)
  })

  it('still produces a shaped id with no crypto at all', () => {
    vi.stubGlobal('crypto', undefined)
    expect(requestId()).toMatch(UUID)
  })
})
