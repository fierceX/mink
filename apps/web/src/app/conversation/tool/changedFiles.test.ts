import { describe, expect, it } from 'vitest'
import type { ToolItem } from '../../../data/types.ts'
import { changedFilesOf } from './changedFiles.ts'

/** An Edit tool item whose result carries a unified diff. */
function edit(summary: string, result: string): ToolItem {
  return {
    kind: 'tool',
    id: 't-edit',
    key: 'live:1',
    name: 'Edit',
    summary,
    color: 'file',
    view: 'diff',
    input: JSON.stringify({ path: summary }),
    result,
    success: true,
  }
}

const RESULT = [
  '--- a/src/app.ts',
  '+++ b/src/app.ts',
  '@@ -1,2 +1,3 @@',
  ' keep',
  '-gone',
  '+added one',
  '+added two',
].join('\n')

describe('changed files of a turn', () => {
  it('collects paths with the same counts the diff card shows', () => {
    const files = changedFilesOf([edit('src/app.ts', RESULT)])
    expect(files).toHaveLength(1)
    expect(files[0]?.path).toBe('src/app.ts')
    expect(files[0]?.added).toBe(2)
    expect(files[0]?.deleted).toBe(1)
  })

  it('folds repeated edits of one path and ignores non-mutating tools', () => {
    const read: ToolItem = {
      kind: 'tool',
      id: 't-read',
      key: 'live:2',
      name: 'Read',
      summary: 'src/other.ts',
      color: 'file',
      view: 'file',
      input: JSON.stringify({ path: 'src/other.ts' }),
      result: '1:line',
      success: true,
    }
    const files = changedFilesOf([edit('src/app.ts', RESULT), read, edit('src/app.ts', RESULT)])
    expect(files).toHaveLength(1)
    expect(files[0]?.added).toBe(4)
    expect(files[0]?.deleted).toBe(2)
  })

  it('reports nothing for a turn that only read or ran commands', () => {
    expect(changedFilesOf([])).toEqual([])
  })
})
