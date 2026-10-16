import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import type { TranscriptItem } from '../../data/types.ts'
import { Transcript } from './Transcript.tsx'

afterEach(cleanup)

const ITEMS: TranscriptItem[] = [
  { kind: 'user', text: '把工作台重建起来', key: 'input:1' },
  { kind: 'thinking', text: '先看侧栏的节奏。\n再动手。', key: 'live:2' },
  {
    kind: 'tool',
    id: 'tool-1',
    name: 'Bash',
    color: 'exec',
    view: 'command',
    summary: 'ls apps/web',
    input: '{"command":"ls apps/web"}',
    result: 'src\ndist',
    exitCode: 0,
    key: 'message:3:0',
  },
  { kind: 'text', text: '已按来源重建外壳。', key: 'message:4:0' },
]

describe('artifact references', () => {
  const withArtifact: TranscriptItem[] = [
    { kind: 'user', text: '跑一下测试', key: 'input:1' },
    { kind: 'text', text: '输出见 artifact://run-1，另外 artifact://run-1 与 artifact://doc/2。', key: 'message:2:0' },
  ]

  it('turns a cited spilled output into an entry point, deduplicated', () => {
    const opened: string[] = []
    render(<Transcript items={withArtifact} running={false} onOpenArtifact={id => { opened.push(id) }} />)
    const chips = screen.getAllByRole('button', { name: /打开产物/ })
    // `doc/2` is not a valid artifact id (the server rejects slashes), so only
    // the two valid ids are offered, and the repeated one appears once.
    expect(chips.map(chip => chip.getAttribute('aria-label'))).toEqual([
      '打开产物 run-1',
      '打开产物 doc',
    ])
    fireEvent.click(chips[0]!)
    expect(opened).toEqual(['run-1'])
  })

  it('shows nothing when the text cites none', () => {
    render(<Transcript items={[{ kind: 'text', text: '没有引用。', key: 'message:1:0' }]} running={false} onOpenArtifact={() => {}} />)
    expect(screen.queryByRole('button', { name: /打开产物/ })).toBeNull()
  })
})

describe('transcript', () => {
  it('renders the user bubble, the process summary and the assistant prose', () => {
    render(<Transcript items={ITEMS} running={false} />)
    expect(screen.getByText('把工作台重建起来')).toBeDefined()
    expect(screen.getByText('已按来源重建外壳。')).toBeDefined()
    // Process header carries the derived summary, collapsed until opened.
    expect(screen.getByText('执行过程 · 执行 1 次')).toBeDefined()
    expect(screen.queryByText('Bash · ls apps/web')).toBeNull()
  })

  it('opens the process group into the tool row and its terminal result', () => {
    render(<Transcript items={ITEMS} running={false} />)
    fireEvent.click(screen.getByText('执行过程 · 执行 1 次'))
    // The copied row is itself the disclosure target; its accessible name is
    // the title plus the summary it renders collapsed.
    fireEvent.click(screen.getByRole('button', { name: /Bash/ }))
    // The copied terminal card carries the run state, the prompt line and the output.
    expect(screen.getByText('完成')).toBeDefined()
    expect(screen.getAllByText('ls apps/web').length).toBeGreaterThan(0)
    expect(screen.getByText('src')).toBeDefined()
    expect(screen.getByText('dist')).toBeDefined()
  })

  it('summarizes a reasoning block on one line and opens the full text', () => {
    render(<Transcript items={ITEMS} running={false} />)
    fireEvent.click(screen.getByText('执行过程 · 执行 1 次'))
    fireEvent.click(screen.getByText('思考'))
    expect(screen.getByText(/再动手/)).toBeDefined()
  })
})
