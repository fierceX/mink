/**
 * Goal boundary card: a draft discussion, a goal start or a continuation,
 * shown in the transcript where the runtime wrote it. These markers are
 * internal user messages, so without a card the reader sees nothing between
 * two goal iterations; the card names the boundary it marks.
 */

import { IconGoalOutlineRegular } from '../../ui/primitives/index.ts'
import type { GoalMessageItem } from '../../data/types.ts'
import css from './GoalNotice.module.css'

const LABELS: Record<string, string> = {
  draft: '目标草案讨论',
  start: '开始目标',
  continuation: '继续执行目标',
}

/**
 * Render one goal boundary.
 * @param props.item - the boundary row from the projection.
 * @returns the card.
 */
export function GoalNotice({ item }: { item: GoalMessageItem }) {
  const { goal } = item
  return (
    <div className={css.root} data-goal-notice={goal.kind}>
      <span className={css.glyph}><IconGoalOutlineRegular size={14} /></span>
      <span className={css.label}>{LABELS[goal.kind] ?? '目标推进'}</span>
      <span className={css.objective} title={goal.objective}>{goal.objective}</span>
      <span className={css.meta}>
        {goal.kind === 'draft'
          ? `契约 r${goal.contract_revision} · ${goal.feedback ? '修改意见' : '等待意见'}`
          : `第 ${goal.iteration} 轮 · 契约 r${goal.contract_revision}`}
      </span>
    </div>
  )
}
