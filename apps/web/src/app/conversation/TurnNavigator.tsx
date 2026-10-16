/**
 * Turn navigator: a jump list over the loaded turns, drawn with the copied menu
 * surface (ui-primitives `Menu`, which supplies the height cap, in-menu
 * scrolling, arrow/Home/End walking and focus return to the trigger). The list
 * is the workbench's own projection: one row per user turn, numbered, with the
 * request text as its title.
 */

import { useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import { IconChevronDownOutlineRegular, IconFlatListOutlineRegular, Menu, MenuItemButton, Tooltip } from '../../ui/primitives/index.ts'
import { projectTurns } from '../../data/transcriptProjection.ts'
import type { TranscriptItem } from '../../data/types.ts'
import css from './TurnNavigator.module.css'

/** The excerpt length the legacy workbench uses for turn titles. */
const TITLE_LIMIT = 80

export interface TurnNavigatorProps {
  items: readonly TranscriptItem[]
  /** Whether the transcript is pinned to its tail. */
  following: boolean
  /** Scroll one turn into view (the shell owns the scroller). */
  onJump: (key: string) => void
  /** Re-pin and scroll back to the tail. */
  onFollow: () => void
}

/**
 * Render the reading controls anchored to the transcript's bottom corner.
 * @param props - the item stream plus the two reading actions.
 * @returns the controls, or null while there is nothing to navigate.
 */
export function TurnNavigator({ items, following, onJump, onFollow, leading }: TurnNavigatorProps & {
  /** Controls that belong to the same floating stack (the chrome's own reveal). */
  leading?: ReactNode
}) {
  const [open, setOpen] = useState(false)
  const turns = useMemo(() => projectTurns([...items]), [items])
  if (turns.length === 0 && leading === undefined) return null

  return (
    <div className={css.ops}>
      {leading}
      {!following && (
        <button type="button" className={css.latest} onClick={onFollow}>
          <IconChevronDownOutlineRegular size={14} />
          返回最新内容
        </button>
      )}
      <Menu
        open={open}
        align="end"
        side="top"
        onClose={() => { setOpen(false) }}
        anchor={(
          <Tooltip label="跳转已加载轮次" side="right" delayMs={400}>
          <button
            type="button"
            className={css.trigger}
            aria-label="跳转已加载轮次"
            aria-haspopup="menu"
            aria-expanded={open}
            onClick={() => { setOpen(value => !value) }}
          >
            <IconFlatListOutlineRegular size={16} />
          </button>
          </Tooltip>
        )}
      >
        <div className={css.list} role="group" aria-label="已加载轮次">
          {turns.map((turn, index) => (
            <MenuItemButton
              key={turn.key}
              onSelect={() => {
                setOpen(false)
                onJump(turn.key)
              }}
            >
              <span className={css.number}>{index + 1}</span>
              <span className={css.title}>
                {turn.user !== undefined && turn.user.kind === 'user' && turn.user.text !== ''
                  ? turn.user.text.slice(0, TITLE_LIMIT)
                  : '历史过程'}
              </span>
            </MenuItemButton>
          ))}
        </div>
      </Menu>
    </div>
  )
}
