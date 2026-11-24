import { afterEach, describe, expect, it } from 'vitest'
import {
  composeDraftKey,
  readComposeDraft,
  settleComposeDraft,
  writeComposeDraft,
  type ComposeDraft,
} from './sessionDraft.ts'

afterEach(() => {
  localStorage.clear()
})

const submitted: ComposeDraft = {
  draftId: 'draft-1',
  requestId: 'request-1',
  text: 'submitted text',
}

describe('compose draft settlement', () => {
  it('drops the composing record once its first send is settled', () => {
    writeComposeDraft('/tmp/compose-draft', submitted)

    settleComposeDraft('/tmp/compose-draft', submitted)

    expect(localStorage.getItem(composeDraftKey('/tmp/compose-draft'))).toBeNull()
  })

  it('drops the emptied box the composer leaves behind', () => {
    // The composer clears the box before submitting, so the record that is
    // settled carries no text. Keeping it would freeze a draft id and an
    // admission id that the server already bound to the first message.
    const cwd = '/tmp/compose-draft'
    writeComposeDraft(cwd, { ...submitted, text: '' })

    settleComposeDraft(cwd, submitted)

    expect(localStorage.getItem(composeDraftKey(cwd))).toBeNull()
  })

  it('keeps text typed after the send under fresh ids', () => {
    const cwd = '/tmp/compose-draft'
    writeComposeDraft(cwd, { ...submitted, text: 'later edit' })

    settleComposeDraft(cwd, submitted)

    const settled = readComposeDraft(cwd)
    expect(settled.text).toBe('later edit')
    expect(settled.draftId).not.toBe(submitted.draftId)
    expect(settled.requestId).not.toBe(submitted.requestId)
  })

  it('leaves a record the reader replaced with another draft alone', () => {
    const cwd = '/tmp/compose-draft'
    const other: ComposeDraft = { draftId: 'draft-2', requestId: 'request-2', text: 'other words' }
    writeComposeDraft(cwd, other)

    settleComposeDraft(cwd, submitted)

    expect(readComposeDraft(cwd)).toEqual(other)
  })
})
