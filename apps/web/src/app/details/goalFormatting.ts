import type { GoalSnapshot } from '../../data/api.ts'
import { goalReviewStageText } from '../../data/goalReview.ts'

/** Todo item status → the dot the row shows. */
export function todoState(status: string): 'done' | 'ongoing' | 'idle' {
  if (status === 'completed') return 'done'
  if (status === 'in_progress') return 'ongoing'
  return 'idle'
}

/** Remaining goal time, in the workbench's own words. */
export function remainingText(seconds: number): string {
  if (seconds <= 0) return '已到期'
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟`
  const hours = Math.floor(minutes / 60)
  return hours < 24 ? `${hours} 小时 ${minutes % 60} 分钟` : `${Math.floor(hours / 24)} 天 ${hours % 24} 小时`
}

/** Acceptance mode of a goal spec, as the vocabulary freezes it. */
export function acceptanceText(goal: GoalSnapshot): string {
  const acceptance = goal.spec?.acceptance
  const mode = typeof acceptance === 'object' && acceptance !== null
    ? (acceptance as { mode?: unknown }).mode
    : acceptance
  switch (mode) {
    case 'agent_reviewed': return '独立审核'
    case 'model_reported': return '历史模型自报'
    case 'verifier': {
      const id = typeof acceptance === 'object' && acceptance !== null
        ? (acceptance as { id?: unknown }).id
        : undefined
      return typeof id === 'string' && id !== '' ? `检查器验收（${id}）` : '检查器验收'
    }
    case 'verified': return '检查器验收'
    case 'human_approved':
    case 'human': return '人工验收'
    default: return '未知验收方式'
  }
}

export function completionBasisText(basis: string): string {
  switch (basis) {
    case 'agent_reviewed': return '独立审核通过'
    case 'model_reported': return '历史模型自述'
    case 'verified': return '检查器通过'
    case 'human_approved': return '人工批准'
    default: return basis
  }
}

export function awaitingHumanApproval(goal: GoalSnapshot): boolean {
  const acceptance = goal.spec?.acceptance
  const mode = typeof acceptance === 'object' && acceptance !== null
    ? (acceptance as { mode?: unknown }).mode
    : acceptance
  return goal.status === 'blocked'
    && goal.reviews?.at(-1)?.verdict === 'satisfied'
    && mode === 'human'
}

export function goalStatusText(goal: GoalSnapshot): string {
  if (goal.status === 'draft') return '待确认'
  if (goal.review_failure_reason != null) return goal.review_failure_stage === 'adjudicating' ? '宿主结论提交未完成' : '审核未完成'
  if (awaitingHumanApproval(goal)) return '待人工验收'
  if (goal.status === 'active' && goal.phase === 'verifying') return goal.review_stage == null ? '审核中' : goalReviewStageText(goal)
  switch (goal.status) {
    case 'active': return '执行中'
    case 'paused': return '已暂停'
    case 'blocked': return '已阻塞'
    case 'budget_exhausted': return '额度耗尽'
    case 'completed': return '已完成'
    case 'cancelled': return '已取消'
    case 'failed': return '失败'
    default: return goal.status
  }
}

export function goalPhaseText(phase: string): string {
  switch (phase) {
    case 'executing': return '执行中'
    case 'verifying': return '审核中'
    case 'idle': return '空闲'
    default: return phase
  }
}

export function reviewStatusText(status: string): string {
  switch (status) {
    case 'pass': return '通过'
    case 'fail': return '未通过'
    case 'unknown': return '未知'
    default: return status
  }
}

export function reviewVerdictText(verdict: string): string {
  switch (verdict) {
    case 'satisfied': return '满足'
    case 'needs_work': return '仍需工作'
    case 'blocked': return '外部阻断'
    case 'inconclusive': return '无法判断'
    default: return verdict
  }
}

export function currentGoalReview(goal: GoalSnapshot) {
  const review = goal.reviews?.at(-1)
  if (review === undefined || review.candidate === undefined) return review
  const candidate = review.candidate
  if (candidate.goal_id !== goal.id || candidate.contract_revision !== goal.contract_revision) return undefined
  return review
}

export function currentCandidateReview(goal: GoalSnapshot) {
  const review = currentGoalReview(goal)
  if (review?.candidate === undefined || goal.review_candidate == null) return undefined
  const candidate = review.candidate
  const current = goal.review_candidate
  return candidate.goal_id === current.goal_id
    && candidate.contract_revision === current.contract_revision
    && (candidate.generation ?? 0) === (current.generation ?? 0)
    && candidate.iteration === current.iteration
    && candidate.conversation_seq === current.conversation_seq
    && candidate.reviewer_id === current.reviewer_id
    ? review
    : undefined
}

export function subgoalProgress(goal: GoalSnapshot, subgoalId: string): string {
  const criteria = goal.spec?.criteria?.filter(criterion => criterion.subgoal_id === subgoalId) ?? []
  if (criteria.length === 0) return '无关联验收标准'
  const review = currentCandidateReview(goal)
  if (review === undefined) return '待审核'
  if (review.report !== undefined) return '详见独立审核报告'
  const findings = new Map(review.criteria.map(item => [item.criterion_id, item.status]))
  const passed = criteria.filter(item => findings.get(item.id) === 'pass').length
  const failed = criteria.filter(item => findings.get(item.id) === 'fail').length
  if (passed === criteria.length) return `审核通过 · ${passed}/${criteria.length} 项验收标准通过`
  if (passed > 0) return `部分通过 · ${passed}/${criteria.length} 项验收标准通过`
  if (failed > 0) return `未通过 · ${failed}/${criteria.length} 项验收标准通过`
  return `待判断 · 0/${criteria.length} 项验收标准通过`
}

/** Numbered file view for a text file opened from the tree. */
export function fileLines(content: string): { number: number; text: string }[] {
  return content.split('\n').map((text, index) => ({ number: index + 1, text }))
}

/** Extensions the panel renders as a document rather than as source. */
export const MARKDOWN_PATTERN = /\.(md|markdown)$/i

/**
 * The directory above one the panel is showing: `undefined` at the root, which
 * the caller turns back into the root listing.
 */
export function parentPath(directory: string): string | undefined {
  const trimmed = directory.replace(/\/$/, '')
  const cut = trimmed.lastIndexOf('/')
  if (cut < 0) return undefined
  return `${trimmed.slice(0, cut)}/`
}
