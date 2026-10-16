/**
 * Goal bar: the active goal as a card above the composer, drawn with the source's
 * own goal-bar sheet (36px bar, goal glyph, label, ellipsized objective, round
 * icon actions). The runtime owns the contract, so every control here is a
 * compare-and-set call on the current revision and a refusal is shown in place
 * rather than guessed at.
 */

import { useCallback, useRef, useState } from 'react'
import {
  IconCloseOutlineRegular,
  IconGoalOutlineRegular,
  IconPauseOutlineRegular,
  IconPlayOutlineRegular,
  IconTrashOutlineRegular,
} from '../../ui/primitives/index.ts'
import type { GoalSnapshot } from '../../data/api.ts'
import { goalCanResume, goalIsTerminal, goalResumeBudgetExhausted, goalReviewStageText, latestGoalContinuation } from '../../data/goalReview.ts'
import { goalBudget } from '../../data/goalBudget.ts'
import css from './GoalBar.module.css'

/** The bar's own words for a status. */
function statusLabel(goal: GoalSnapshot): string {
  if (goal.status === 'failed') return '本次运行失败'
  if (goal.review_failure_reason != null) return goal.review_failure_stage === 'adjudicating' ? '结论提交待重试' : '审核待重试'
  const acceptance = goal.spec?.acceptance
  const mode = typeof acceptance === 'object' && acceptance !== null
    ? (acceptance as { mode?: unknown }).mode
    : acceptance
  if (
    goal.status === 'blocked'
    && goal.reviews?.at(-1)?.verdict === 'satisfied'
    && mode === 'human'
  ) return '待人工验收'
  if (goal.status === 'active' && goal.phase === 'verifying') return goalReviewStageText(goal)
  if (goal.status === 'active' && goal.phase === 'idle' && latestGoalContinuation(goal) !== undefined) return '本轮部分完成，继续执行'
  if (goal.status === 'active') return '进行中的目标'
  if (goal.status === 'paused') return '已暂停的目标'
  if (goal.status === 'blocked') return '受阻的目标'
  if (goal.status === 'budget_exhausted') return '额度耗尽的目标'
  return '目标'
}

export interface GoalBarProps {
  goal: GoalSnapshot | null
  /** Run one goal action and answer with the error message, when refused. */
  onAction: (
    action: 'pause' | 'resume' | 'cancel' | 'clear',
    goal: GoalSnapshot,
  ) => Promise<string | undefined>
}

/**
 * Render the goal bar.
 * @param props.goal - the session's goal snapshot, or null when it has none.
 * @param props.onAction - the shell's revision-checked goal control.
 * @returns the bar, or null without a live goal.
 */
export function GoalBar({ goal, onAction }: GoalBarProps) {
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | undefined>(undefined)
  // React state disables the controls on the next render; the ref closes the
  // same-render window so two clicks cannot submit the same CAS twice.
  const inFlight = useRef(false)
  const run = useCallback((action: 'pause' | 'resume' | 'cancel' | 'clear') => {
    if (goal === null || inFlight.current) return
    inFlight.current = true
    setPending(true)
    setError(undefined)
    void onAction(action, goal).then((failure) => {
      if (failure !== undefined) setError(failure)
    }).finally(() => {
      inFlight.current = false
      setPending(false)
    })
  }, [goal, onAction])

  if (goal === null || goal.hidden === true) return null
  const terminal = goalIsTerminal(goal)
  const remaining = goalBudget(goal)?.remaining_turns
  const title = `${goal.spec?.objective ?? ''}${remaining === undefined ? '' : ` · 剩余 ${remaining} 轮`}`
  const reviewFailure = goal.review_failure_reason
  const acceptance = goal.spec?.acceptance
  const acceptanceMode = typeof acceptance === 'object' && acceptance !== null
    ? (acceptance as { mode?: unknown }).mode
    : acceptance
  const awaitingHumanApproval = goal.status === 'blocked'
    && goal.reviews?.at(-1)?.verdict === 'satisfied'
    && acceptanceMode === 'human'
  const reviewing = goal.status === 'active' && reviewFailure == null && goal.phase === 'verifying' && !awaitingHumanApproval

  return (
    <div className={css.dock} data-goal-bar data-goal-status={goal.status}>
      <div className={css.bar} title={title}>
        <span className={css.goalGlyph}><IconGoalOutlineRegular size={14} /></span>
        <span className={css.label}>{statusLabel(goal)}</span>
        {error === undefined
          ? <span className={css.objective}>{goal.spec?.objective ?? ''}</span>
          : <span className={css.error} role="alert">{error}</span>}
        <div className={css.actions}>
          {(goalCanResume(goal) && (goal.status !== 'active' || reviewFailure != null)) && (
            <button
              type="button"
              className={css.iconBtn}
              disabled={pending || goalResumeBudgetExhausted(goal) || (goal.pending_effects?.length ?? 0) > 0}
              aria-label={goal.status === 'failed' ? '重试目标' : reviewFailure == null ? '恢复目标' : goal.review_failure_stage === 'adjudicating' ? '重试宿主提交' : '重试独立审核'}
              onClick={() => { run('resume') }}
            >
              <IconPlayOutlineRegular size={14} />
            </button>
          )}
          {!terminal && goal.status === 'active' && (
            <button
              type="button"
              className={css.iconBtn}
              disabled={pending}
              aria-label="暂停目标"
              onClick={() => { run('pause') }}
            >
              <IconPauseOutlineRegular size={14} />
            </button>
          )}
          {!terminal && (
            <button
              type="button"
              className={css.iconBtn}
              disabled={pending}
              aria-label="取消目标"
              onClick={() => { run('cancel') }}
            >
              <IconCloseOutlineRegular size={14} />
            </button>
          )}
          {terminal && (
            <button
              type="button"
              className={css.iconBtn}
              disabled={pending}
              aria-label="清除目标"
              onClick={() => { run('clear') }}
            >
              <IconTrashOutlineRegular size={14} />
            </button>
          )}
        </div>
      </div>
      {reviewing && (
        <p className={css.reviewNotice} role="status" data-goal-review-status="running">
          {goal.review_stage == null
            ? '执行轮已结束，独立审核者正在只读核对目标、验收项与证据；审核通过前不会宣布目标完成。'
            : `${goalReviewStageText(goal)}：明确未完成时可继续推进，完成声明仍须独立证据验收。`}
        </p>
      )}
      {reviewFailure != null && (
        <p className={css.reviewNotice} role="alert" data-goal-review-status="failed">
          {goal.review_failure_stage === 'adjudicating' ? '宿主结论提交未完成' : '独立审核未完成'}：{reviewFailure}。目标工作尚未被判失败；{goal.review_failure_stage === 'adjudicating' ? '有效报告已保存，可在目标详情重试提交。' : '可在目标详情重试审核。'}
        </p>
      )}
    </div>
  )
}
