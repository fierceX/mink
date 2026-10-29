import { afterEach, describe, expect, it } from 'vitest'
import {
  clearComposeDraft,
  composeDraftKey,
  readComposeDraft,
  writeComposeDraft,
  type ComposeDraft,
} from './sessionDraft.ts'

afterEach(() => {
  localStorage.clear()
})

describe('compose draft settlement', () => {
  it('does not clear a draft edited after the first send began', () => {
    const cwd = '/tmp/compose-draft'
    const submitted: ComposeDraft = {
      draftId: 'draft-1',
      requestId: 'request-1',
      text: 'submitted text',
    }
    const edited: ComposeDraft = { ...submitted, text: 'later edit' }
    writeComposeDraft(cwd, edited)

    clearComposeDraft(cwd, submitted)

    expect(readComposeDraft(cwd)).toEqual(edited)
    expect(localStorage.getItem(composeDraftKey(cwd))).not.toBeNull()
  })

  it('clears the draft when it still matches the accepted first send', () => {
    const cwd = '/tmp/compose-draft'
    const accepted: ComposeDraft = {
      draftId: 'draft-2',
      requestId: 'request-2',
      text: 'accepted text',
    }
    writeComposeDraft(cwd, accepted)

    clearComposeDraft(cwd, accepted)

    expect(localStorage.getItem(composeDraftKey(cwd))).toBeNull()
  })
})
