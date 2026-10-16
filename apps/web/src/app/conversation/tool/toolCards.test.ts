import { describe, expect, it } from 'vitest'
import type { ToolItem } from '../../../data/types.ts'
import { diffCard, readCard, searchCard, toolState, toolVariant } from './toolCards.ts'

function tool(overrides: Partial<ToolItem>): ToolItem {
  return {
    kind: 'tool',
    id: 'tool-1',
    name: 'Read',
    color: 'file',
    view: 'file',
    summary: '',
    input: '',
    ...overrides,
  }
}

describe('toolVariant', () => {
  it('maps mink tool names onto the copied row variants', () => {
    expect(toolVariant('Bash')).toBe('bash')
    expect(toolVariant('PythonSandbox')).toBe('bash')
    expect(toolVariant('Read')).toBe('read')
    expect(toolVariant('Write')).toBe('write')
    expect(toolVariant('Edit')).toBe('edit')
    expect(toolVariant('Grep')).toBe('search')
    expect(toolVariant('TodoRead')).toBe('others')
  })
})

describe('toolState', () => {
  it('reads an unsettled call as running and a settled failure as error', () => {
    expect(toolState(tool({}))).toBe('running')
    expect(toolState(tool({ result: 'ok', success: true }))).toBe('ok')
    expect(toolState(tool({ result: 'boom', success: false }))).toBe('error')
    expect(toolState(tool({ result: 'boom', failed: true }))).toBe('error')
  })
})

describe('readCard', () => {
  it('splits numbered lines and keeps the file line numbers', () => {
    const card = readCard(tool({
      input: '{"path":"src/main.rs"}',
      result: '12:fn main() {\n13:    println!("hi");\n14:}',
    }))
    expect(card).not.toBeNull()
    expect(card?.label).toBe('src/main.rs')
    expect(card?.lines).toEqual([
      { number: 12, text: 'fn main() {' },
      { number: 13, text: '    println!("hi");' },
      { number: 14, text: '}' },
    ])
    expect(card?.totalLines).toBe(14)
    expect(card?.lang).toBe('rust')
  })

  it('takes the hashline header as the path and refuses a non-numbered result', () => {
    const card = readCard(tool({ result: '[src/lib.rs#3F2A]\n7:pub mod tools;' }))
    expect(card?.label).toBe('src/lib.rs')
    expect(card?.lines).toEqual([{ number: 7, text: 'pub mod tools;' }])
    expect(readCard(tool({ result: 'not a numbered window' }))).toBeNull()
  })
})

describe('diffCard', () => {
  it('builds hunks from the Replace input', () => {
    const card = diffCard(tool({
      name: 'Edit',
      input: JSON.stringify({ path: 'src/a.ts', edits: [{ old_text: 'let a = 1', new_text: 'let a = 2' }] }),
      result: 'Edited src/a.ts',
    }))
    expect(card).toEqual({ diffs: [{ path: 'src/a.ts', oldText: 'let a = 1', newText: 'let a = 2' }] })
  })

  it('falls back to a unified diff in the result and refuses anything else', () => {
    const card = diffCard(tool({
      name: 'Edit',
      input: '{"path":"src/a.ts"}',
      result: '--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-let a = 1\n+let a = 2',
    }))
    expect(card?.diffs).toEqual([{ path: 'src/a.ts', oldText: 'let a = 1', newText: 'let a = 2' }])
    expect(diffCard(tool({ name: 'Edit', input: '{"path":"x"}', result: 'no diff here' }))).toBeNull()
  })
})

describe('searchCard', () => {
  it('groups grep matches by file in first-seen order', () => {
    const card = searchCard(tool({
      name: 'Grep',
      result: 'src/a.ts:3:const a = 1\nsrc/b.ts:9:const b = 2\nsrc/a.ts:12:const c = 3',
    }))
    expect(card).toMatchObject({
      kind: 'matches',
      total: 3,
      files: [
        { path: 'src/a.ts', matches: [{ lineNumber: 3, line: 'const a = 1' }, { lineNumber: 12, line: 'const c = 3' }] },
        { path: 'src/b.ts', matches: [{ lineNumber: 9, line: 'const b = 2' }] },
      ],
    })
  })

  it('reads a glob listing as a path card', () => {
    const card = searchCard(tool({ name: 'Glob', result: 'src/a.ts\nsrc/b.ts' }))
    expect(card).toMatchObject({ kind: 'paths', paths: ['src/a.ts', 'src/b.ts'], total: 2 })
    expect(searchCard(tool({ name: 'Grep', result: 'no matches' }))).toBeNull()
  })
})
