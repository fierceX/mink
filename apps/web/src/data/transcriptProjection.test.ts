import { describe, expect, it } from 'vitest'
import { isInternalProjection, projectTurns } from './transcriptProjection.ts'
import type { TranscriptItem } from './types.ts'


describe('internal projections', () => {
  it('keeps runtime checkpoints out of the transcript', () => {
    // The projection is typed against the real item union; only what the filter
    // reads matters here.
    const items = [
      { kind: 'user', key: 'u1', text: '真正的输入' },
      { kind: 'user', key: 'sync', text: '<todo-sync revision="2">state</todo-sync>' },
      { kind: 'user', key: 'cur', text: '\n<current-todos revision="5" pending="1">Call TodoRead…</current-todos>' },
      { kind: 'user', key: 'sum', text: '<compacted-summary>…</compacted-summary>' },
      { kind: 'message', key: 'a1', text: '回答' },
    ] as unknown as TranscriptItem[]
    const turns = projectTurns(items)
    // One reader turn, and none of the projections became a bubble.
    expect(turns).toHaveLength(1)
    expect(turns[0].user?.kind === 'user' ? turns[0].user.text : undefined).toBe('真正的输入')
    expect(isInternalProjection('<todo-sync revision="1">x</todo-sync>')).toBe(true)
    expect(isInternalProjection('  <current-todos revision="1">x</current-todos>')).toBe(true)
    expect(isInternalProjection('普通输入里提到 <todo-sync> 不算投影')).toBe(false)
  })
})
