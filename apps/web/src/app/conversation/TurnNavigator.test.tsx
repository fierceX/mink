import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { TranscriptItem } from '../../data/types.ts'
import { TurnNavigator } from './TurnNavigator.tsx'

afterEach(cleanup)

/** Three user turns; the middle one carries a request long enough to truncate. */
const ITEMS: TranscriptItem[] = [
  { kind: 'user', key: 'message:1:0', text: '第一个请求' },
  { kind: 'text', key: 'message:1:1', text: '回答一' },
  { kind: 'user', key: 'message:2:0', text: 'A'.repeat(200) },
  { kind: 'text', key: 'message:2:1', text: '回答二' },
  { kind: 'user', key: 'message:3:0', text: '第三个请求' },
  { kind: 'text', key: 'message:3:1', text: '回答三' },
]

describe('turn navigator', () => {
  it('lists the loaded turns by number with their request text', () => {
    render(<TurnNavigator items={ITEMS} following onJump={() => {}} onFollow={() => {}} />)
    fireEvent.click(screen.getByRole('button', { name: '跳转已加载轮次' }))
    const list = screen.getByRole('group', { name: '已加载轮次' })
    const rows = list.querySelectorAll('[role="menuitem"]')
    expect(rows.length).toBe(3)
    expect(rows[0]?.textContent).toContain('1')
    expect(rows[0]?.textContent).toContain('第一个请求')
    expect(rows[2]?.textContent).toContain('第三个请求')
    // A guidance row belongs to the turn it was sent during, so it adds no row.
    expect(rows.length).toBe(3)
  })

  it('truncates a long request so the popup cannot be widened by it', () => {
    render(<TurnNavigator items={ITEMS} following onJump={() => {}} onFollow={() => {}} />)
    fireEvent.click(screen.getByRole('button', { name: '跳转已加载轮次' }))
    const long = screen.getByText(/A{20}/)
    expect(long.textContent?.length).toBe(80)
  })

  it('jumps to the chosen turn and closes the list', () => {
    const onJump = vi.fn()
    render(<TurnNavigator items={ITEMS} following onJump={onJump} onFollow={() => {}} />)
    fireEvent.click(screen.getByRole('button', { name: '跳转已加载轮次' }))
    const rows = screen.getByRole('group', { name: '已加载轮次' }).querySelectorAll('[role="menuitem"]')
    fireEvent.click(rows[0]!)
    expect(onJump).toHaveBeenCalledWith('message:1:0')
    expect(screen.queryByRole('group', { name: '已加载轮次' })).toBeNull()
  })

  it('offers the return-to-latest control only while reading above the tail', () => {
    const onFollow = vi.fn()
    const { rerender } = render(
      <TurnNavigator items={ITEMS} following onJump={() => {}} onFollow={onFollow} />,
    )
    expect(screen.queryByRole('button', { name: /返回最新内容/ })).toBeNull()

    rerender(<TurnNavigator items={ITEMS} following={false} onJump={() => {}} onFollow={onFollow} />)
    fireEvent.click(screen.getByRole('button', { name: /返回最新内容/ }))
    expect(onFollow).toHaveBeenCalled()
  })

  it('renders nothing for a session without turns', () => {
    render(<TurnNavigator items={[]} following onJump={() => {}} onFollow={() => {}} />)
    expect(screen.queryByRole('button', { name: '跳转已加载轮次' })).toBeNull()
  })
})
