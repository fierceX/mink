/**
 * Right detail panel: plan and todos, the current goal, the session's own file
 * tree and the spilled tool outputs. Chrome and content are drawn from the
 * copied tokens and atoms; the panel box mirrors ui-sidebar-right's geometry
 * (anchored to the column's right edge, hanging over the centre while the frame
 * gives it no track).
 */

import { useEffect, useRef, useState } from 'react'
import clsx from 'clsx'
import {
  Button,
  CodeBlock,
  languageForPath,
  MarkdownText,
  ReadBlock,
  StateDot,
  Tag,
} from '../../ui/primitives/index.ts'
import { api, type GoalSnapshot, type SessionSummary } from '../../data/api.ts'
import { goalCanAdjustBudget, goalCanResume, goalIsTerminal, goalResumeBudgetExhausted, goalReviewStageText, latestGoalContinuation } from '../../data/goalReview.ts'
import { goalBudget } from '../../data/goalBudget.ts'
import { MARKDOWN_LABELS } from '../conversation/labels.ts'
import { READ_LABELS } from '../conversation/tool/toolLabels.ts'
import { diagnosticGroups } from '../../data/diagnostics.ts'
import { sessionIdentity } from '../../data/sessionDraft.ts'
import type { SessionState } from '../../data/types.ts'
import { DETAIL_TABS, useDetails, type DetailTab } from './useDetails.ts'
import css from './DetailPanel.module.css'

const TAB_LABELS: Record<DetailTab, string> = {
  task: '任务',
  goal: '目标',
  files: '文件',
  outputs: '输出',
  diagnostics: '诊断',
}

/** Todo item status → the dot the row shows. */
function todoState(status: string): 'done' | 'ongoing' | 'idle' {
  if (status === 'completed') return 'done'
  if (status === 'in_progress') return 'ongoing'
  return 'idle'
}

/** Remaining goal time, in the workbench's own words. */
function remainingText(seconds: number): string {
  if (seconds <= 0) return '已到期'
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟`
  const hours = Math.floor(minutes / 60)
  return hours < 24 ? `${hours} 小时 ${minutes % 60} 分钟` : `${Math.floor(hours / 24)} 天 ${hours % 24} 小时`
}

/** Acceptance mode of a goal spec, as the vocabulary freezes it. */
function acceptanceText(goal: GoalSnapshot): string {
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

function completionBasisText(basis: string): string {
  switch (basis) {
    case 'agent_reviewed': return '独立审核通过'
    case 'model_reported': return '历史模型自述'
    case 'verified': return '检查器通过'
    case 'human_approved': return '人工批准'
    default: return basis
  }
}

function awaitingHumanApproval(goal: GoalSnapshot): boolean {
  const acceptance = goal.spec?.acceptance
  const mode = typeof acceptance === 'object' && acceptance !== null
    ? (acceptance as { mode?: unknown }).mode
    : acceptance
  return goal.status === 'blocked'
    && goal.reviews?.at(-1)?.verdict === 'satisfied'
    && mode === 'human'
}

function goalStatusText(goal: GoalSnapshot): string {
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

function goalPhaseText(phase: string): string {
  switch (phase) {
    case 'executing': return '执行中'
    case 'verifying': return '审核中'
    case 'idle': return '空闲'
    default: return phase
  }
}

function reviewStatusText(status: string): string {
  switch (status) {
    case 'pass': return '通过'
    case 'fail': return '未通过'
    case 'unknown': return '未知'
    default: return status
  }
}

function reviewVerdictText(verdict: string): string {
  switch (verdict) {
    case 'satisfied': return '满足'
    case 'needs_work': return '仍需工作'
    case 'blocked': return '外部阻断'
    case 'inconclusive': return '无法判断'
    default: return verdict
  }
}

function currentGoalReview(goal: GoalSnapshot) {
  const review = goal.reviews?.at(-1)
  if (review === undefined || review.candidate === undefined) return review
  const candidate = review.candidate
  if (candidate.goal_id !== goal.id || candidate.contract_revision !== goal.contract_revision) return undefined
  return review
}

function currentCandidateReview(goal: GoalSnapshot) {
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

function subgoalProgress(goal: GoalSnapshot, subgoalId: string): string {
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
function fileLines(content: string): { number: number; text: string }[] {
  return content.split('\n').map((text, index) => ({ number: index + 1, text }))
}

/** Extensions the panel renders as a document rather than as source. */
const MARKDOWN_PATTERN = /\.(md|markdown)$/i

/**
 * The directory above one the panel is showing: `undefined` at the root, which
 * the caller turns back into the root listing.
 * @param directory - the listing's own path, with its trailing slash.
 * @returns the parent's path, or undefined at the first level.
 */
function parentPath(directory: string): string | undefined {
  const trimmed = directory.replace(/\/$/, '')
  const cut = trimmed.lastIndexOf('/')
  if (cut < 0) return undefined
  return `${trimmed.slice(0, cut)}/`
}

/**
 * The file the tree opened: a document renders as markdown (its fences
 * highlighted by the copied pipeline), source renders as a highlighted code
 * block, and anything else keeps the numbered plain view.
 */
function FilePreview({ path, content }: { path: string; content: string }) {
  if (MARKDOWN_PATTERN.test(path)) {
    return (
      <div className={css.fileDocument} data-file-preview="markdown">
        <MarkdownText text={content} streaming={false} labels={MARKDOWN_LABELS} variant="compact" />
      </div>
    )
  }
  const language = languageForPath(path)
  if (language !== undefined) {
    return (
      <div data-file-preview="code">
        <CodeBlock
          code={content}
          lang={language}
          lineNumbers
          copyLabel="复制文件"
          copiedLabel="已复制"
        />
      </div>
    )
  }
  return (
    <ReadBlock
      label={path}
      lines={fileLines(content)}
      totalLines={fileLines(content).length}
      labels={READ_LABELS}
      maxLines={64}
    />
  )
}

/** The goal tab: facts, the frozen contract and the CAS-checked controls. */
function GoalTab({ session, goal, driven, refresh, onReport }: {
  session: SessionSummary
  goal: GoalSnapshot | null
  driven: boolean
  refresh: () => void
  onReport: (artifactId: string) => void
}) {
  const [busy, setBusy] = useState(false)
  const inFlight = useRef(false)
  const [error, setError] = useState<string | undefined>(undefined)
  const [feedback, setFeedback] = useState('')
  const [budgetTurns, setBudgetTurns] = useState('')
  const [budgetMinutes, setBudgetMinutes] = useState('')
  const [nowUnixSecs, setNowUnixSecs] = useState(() => Math.floor(Date.now() / 1000))
  const deadline = goal?.spec?.limits?.deadline_unix_secs ?? goal?.budget?.deadline_unix_secs
  useEffect(() => {
    if (deadline === undefined) return
    const updateClock = () => setNowUnixSecs(Math.floor(Date.now() / 1000))
    updateClock()
    const timer = window.setInterval(() => {
      updateClock()
    }, 60_000)
    return () => window.clearInterval(timer)
  }, [deadline])
  const budget = goal === null ? undefined : goalBudget(goal, nowUnixSecs)
  const act = (action: 'pause' | 'resume' | 'cancel' | 'clear' | 'approve' | 'reconcile' | 'confirm' | 'refine' | 'budget') => {
    if (goal === null || busy || inFlight.current) return
    if (action === 'refine' && !feedback.trim()) {
      setError('请先填写草案修改意见')
      return
    }
    const budgetChanges: { max_turns?: number; max_minutes?: number } = {}
    if (action === 'budget') {
      if (!budgetTurns.trim() && !budgetMinutes.trim()) {
        setError('请填写要调整的总轮数或分钟数')
        return
      }
      for (const [value, key, maximum] of [[budgetTurns, 'max_turns', 4_294_967_295], [budgetMinutes, 'max_minutes', Math.floor(Number.MAX_SAFE_INTEGER / 60)]] as const) {
        if (!value.trim()) continue
        const number = Number(value)
        if (!Number.isSafeInteger(number) || number <= 0 || number > maximum) {
          setError('预算必须为有效的正整数')
          return
        }
        budgetChanges[key] = number
      }
    }
    let evidence: string | undefined
    if (action === 'approve' || action === 'reconcile') {
      evidence = window.prompt(action === 'approve' ? '人工验收说明' : '副作用对账结论') ?? undefined
      if (evidence === undefined) return
    }
    inFlight.current = true
    setBusy(true)
    setError(undefined)
    const body: Parameters<typeof api.goalAction>[2] = { goal_id: goal.id, revision: goal.revision, ...budgetChanges }
    if (action === 'refine') body.feedback = feedback.trim()
    if (evidence !== undefined) body.evidence = evidence
    void (async () => {
      try {
        const response = await api.goalAction(session.id, action, body, session.project_key)
        if (response.code !== 200) throw new Error(response.message || `code ${response.code}`)
        if (action === 'refine') setFeedback('')
        if (action === 'budget') { setBudgetTurns(''); setBudgetMinutes('') }
        refresh()
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause))
        if (cause instanceof Error && /409|stale goal|revision/i.test(cause.message)) refresh()
      } finally {
        inFlight.current = false
        setBusy(false)
      }
    })()
  }
  if (goal === null) {
    return (
      <div className={css.body}>
        <h2 className={css.sectionTitle}>目标</h2>
        <p className={css.hint}>本会话没有目标。</p>
      </div>
    )
  }
  return (
    <div className={css.body}>
      <h2 className={css.sectionTitle}>
        目标
        <Tag tone="neutral">{goalStatusText(goal)}</Tag>
        <Tag tone="quiet">
          {goal.review_failure_reason != null
            ? '等待重试'
            : awaitingHumanApproval(goal)
              ? '等待人工批准'
              : goal.status === 'paused' ? '等待恢复'
              : goal.phase === 'verifying' && goal.review_stage != null ? goalReviewStageText(goal) : goalPhaseText(goal.phase)}
        </Tag>
      </h2>
      <p className={css.hint}>{goal.spec?.objective ?? '—'}</p>
      {goal.spec?.original_request != null && goal.spec.original_request !== goal.spec.objective && (
        <p className={css.hint}>原始请求：{goal.spec.original_request}</p>
      )}
      <dl className={css.facts}>
        <dt>验收</dt>
        <dd>{acceptanceText(goal)}{goal.completion_basis == null ? '' : `（${completionBasisText(goal.completion_basis)}）`}</dd>
        <dt>轮次</dt>
        <dd>
          {budget === undefined
            ? '—'
            : `${budget.max_turns} 上限 · 已用 ${budget.turns_used} · 剩余 ${budget.remaining_turns}`}
        </dd>
        <dt>绝对时限</dt>
        <dd>{budget === undefined ? '—' : remainingText(budget.remaining_secs)}</dd>
        <dt>阻断门槛</dt>
        <dd>
          {budget === undefined
            ? '—'
            : `${budget.blocker_rounds ?? budget.blocked_rounds}/${budget.blocked_after_consecutive_rounds} 轮`}
        </dd>
        <dt>Token 软限额</dt>
        <dd>{budget === undefined ? '—' : budget.soft_token_limit ?? '无'}</dd>
      </dl>
      {goal.spec?.scope != null && goal.spec.scope !== '' && (
        <section aria-label="范围">
          <h3 className={css.subsectionTitle}>范围</h3>
          <p className={css.hint}>{goal.spec.scope}</p>
        </section>
      )}
      {(goal.spec?.suggested_actions?.length ?? 0) > 0 && (
        <section aria-label="建议操作">
          <h3 className={css.subsectionTitle}>建议操作</h3>
          <ul className={css.list}>
            {goal.spec?.suggested_actions?.map((action, index) => <li key={`${index}:${action}`}>{action}</li>)}
          </ul>
        </section>
      )}
      {(goal.spec?.subgoals?.length ?? 0) > 0 && (
        <section aria-label="子目标">
          <h3 className={css.subsectionTitle}>子目标</h3>
          <ul className={css.list}>
            {goal.spec?.subgoals?.map(subgoal => (
              <li key={subgoal.id}>
                <strong>{subgoal.objective}</strong>
                <span className={css.hint}>{subgoalProgress(goal, subgoal.id)}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
      {(goal.spec?.criteria?.length ?? 0) > 0 && (
        <section aria-label="验收标准">
          <h3 className={css.subsectionTitle}>验收标准与证据</h3>
          <ul className={css.list}>
            {goal.spec?.criteria?.map(criterion => (
              <li key={criterion.id}>
                <strong>{criterion.outcome}</strong>
                <span className={css.hint}>证据：{criterion.required_evidence}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
      {(goal.spec?.acceptance_criteria?.length ?? 0) > 0 && (
        <ul className={css.list}>
          {(goal.spec?.criteria?.length ?? 0) === 0 && goal.spec?.acceptance_criteria?.map(criterion => (
            <li key={criterion}>· {criterion}</li>
          ))}
        </ul>
      )}
      {(goal.spec?.constraints?.length ?? 0) > 0 && (
        <ul className={css.list}>
          {goal.spec?.constraints?.map((constraint, index) => {
            const check = currentCandidateReview(goal)?.constraints[index]
            return (
              <li key={constraint}>
                · {constraint}{check === undefined ? '' : ` — ${reviewStatusText(check.status)}：${check.reason}`}
                {check !== undefined && check.evidence.length > 0 && <span className={css.hint}>证据：{check.evidence.join('、')}</span>}
              </li>
            )
          })}
        </ul>
      )}
      {(() => {
        const continuation = latestGoalContinuation(goal)
        if (continuation === undefined) return null
        return <section aria-label="轻量分流">
          <h3 className={css.subsectionTitle}>第 {continuation.proposal.candidate.iteration} 轮：本轮部分完成，继续执行</h3>
          <p className={css.hint}>{continuation.proposal.summary}</p>
          <p className={css.hint}>剩余要求：{continuation.proposal.remaining_requirements.join('、')}</p>
          <p className={css.hint}>下一步：{continuation.proposal.next_step}</p>
          <p className={css.hint}>本轮未进行完整证据验收；此前审核结果不代表最新产物已通过。</p>
          <Button size="sm" variant="outline" onClick={() => onReport(continuation.report.replace(/^artifact:\/\//, ''))}>读取分流报告</Button>
        </section>
      })()}
      {(() => {
        const review = currentGoalReview(goal)
        if (review === undefined) return null
        return (
          <section aria-label="最近审核">
            <h3 className={css.subsectionTitle}>最近审核：{reviewVerdictText(review.verdict)}（第 {review.candidate?.iteration ?? '未知'} 轮）</h3>
            <p className={css.hint}>{review.summary}</p>
            {review.report !== undefined && <Button size="sm" variant="outline" onClick={() => onReport(review.report!.artifact.replace(/^artifact:\/\//, ''))}>读取审核报告</Button>}
            {review.original_request !== undefined && (
              <p className={css.hint}>
                原始要求：{reviewStatusText(review.original_request.status)} — {review.original_request.reason}
                {review.original_request.evidence.length > 0 && ` · 证据：${review.original_request.evidence.join('、')}`}
              </p>
            )}
            <ul className={css.list}>
              {review.criteria.map(check => {
                const criterion = goal.spec?.criteria?.find(item => item.id === check.criterion_id)
                return (
                  <li key={check.criterion_id}>
                    {criterion?.outcome ?? check.criterion_id} — {reviewStatusText(check.status)}：{check.reason}
                    {check.evidence.length > 0 && <span className={css.hint}>证据：{check.evidence.join('、')}</span>}
                  </li>
                )
              })}
            </ul>
            {review.next_step !== '' && <p className={css.hint}>下一步：{review.next_step}</p>}
          </section>
        )
      })()}
      {goal.review_failure_reason != null && (
        <section aria-label="独立审核状态" role="alert">
          <h3 className={css.subsectionTitle}>{goal.review_failure_stage === 'adjudicating' ? '宿主结论提交未完成' : '独立审核未完成'}</h3>
          <p className={css.hint}>{goal.review_failure_stage === 'adjudicating' ? '独立审核报告已保存；重试只恢复宿主提交，不重新执行任务。若证据已变化则重新审核。' : '本轮结果尚未被判定为满足或不满足；重试会复核已结束的本轮，不会重新执行任务。'}</p>
          <p className={css.hint}>审核原因：{goal.review_failure_reason}</p>
        </section>
      )}
      {goal.reason != null && <p className={css.hint}>决策：{goal.reason}</p>}
      {goal.status === 'failed' && <p className={css.hint}>本次运行失败，已停止自动执行。可显式重试原目标，保留契约、历史和已用额度。过期或轮次耗尽时先调整预算，再明确重试；不会自动重新执行。</p>}
      {goalCanResume(goal) && goalResumeBudgetExhausted(goal, nowUnixSecs) && <p className={css.hint}>恢复所需额度已耗尽，请先调整预算。</p>}
      {goal.status !== 'draft' && goalCanAdjustBudget(goal) && (
        <section aria-label="调整目标预算">
          <h3 className={css.subsectionTitle}>调整预算</h3>
          <label className={css.feedbackLabel}>总轮数上限
            <input className={css.goalFeedback} type="number" min="1" step="1" value={budgetTurns} placeholder={String(budget?.max_turns ?? '')} onChange={event => { setBudgetTurns(event.currentTarget.value) }} />
          </label>
          <label className={css.feedbackLabel}>从现在起允许分钟数
            <input className={css.goalFeedback} type="number" min="1" step="1" value={budgetMinutes} onChange={event => { setBudgetMinutes(event.currentTarget.value) }} />
          </label>
          <p className={css.hint}>未填写项保持不变；只调整预算，不自动恢复，Token 软限额保持不变。</p>
          <Button size="sm" variant="outline" disabled={busy || !driven || (!budgetTurns.trim() && !budgetMinutes.trim())} onClick={() => { act('budget') }}>调整预算</Button>
        </section>
      )}
      {goal.status === 'draft' ? (
        <>
          <p className={css.hint}>草案 ID {goal.id} · 修订 {goal.revision} · 确认后才开始执行。</p>
          {goal.draft_ready === false && <p className={css.hint}>草案正在整理或等待补充，完成前不能确认。</p>}
          <label className={css.feedbackLabel}>
            修改意见
            <textarea
              className={css.goalFeedback}
              aria-label="目标草案修改意见"
              value={feedback}
              onChange={event => { setFeedback(event.currentTarget.value) }}
              placeholder="例如：保留公开 API，并为失败路径补充可观察证据"
              rows={3}
            />
          </label>
          <div className={css.actions}>
            <Button size="sm" variant="outline" disabled={busy || !driven || goal.draft_ready === false} onClick={() => { act('confirm') }}>确认并启动</Button>
            <Button size="sm" variant="outline" disabled={busy || !driven || !feedback.trim()} onClick={() => { act('refine') }}>提交修改意见</Button>
            <Button size="sm" variant="ghost" disabled={busy || !driven} onClick={() => { act('cancel') }}>取消草案</Button>
          </div>
        </>
      ) : (
        <div className={css.actions}>
          {goal.status === 'active' && <Button size="sm" variant="outline" disabled={busy || !driven} onClick={() => { act('pause') }}>暂停</Button>}
          {goalCanResume(goal) && <Button size="sm" variant="outline" disabled={busy || !driven || goalResumeBudgetExhausted(goal, nowUnixSecs) || (goal.pending_effects?.length ?? 0) > 0} onClick={() => { act('resume') }}>
            {goal.status === 'failed' ? '重试目标' : goal.review_failure_reason == null ? '恢复' : goal.review_failure_stage === 'adjudicating' ? '重试提交' : '重试审核'}
          </Button>}
          {awaitingHumanApproval(goal) && <Button size="sm" variant="outline" disabled={busy || !driven} onClick={() => { act('approve') }}>批准完成</Button>}
          {(goal.status === 'blocked' || goal.status === 'failed') && (goal.pending_effects?.length ?? 0) > 0 && <Button size="sm" variant="outline" disabled={busy || !driven} onClick={() => { act('reconcile') }}>对账副作用</Button>}
          {!goalIsTerminal(goal) && <Button size="sm" variant="outline" disabled={busy || !driven} onClick={() => { act('cancel') }}>取消</Button>}
          {goalIsTerminal(goal) && <Button size="sm" variant="ghost" disabled={busy || !driven} onClick={() => { act('clear') }}>清除</Button>}
        </div>
      )}
      {!driven && <p className={css.hint}>会话未打开：当前是落盘只读镜像，控制需要先打开会话。</p>}
      {goal.status !== 'draft' && <p className={css.hint}>修订 {goal.revision} · 每次变更按修订号提交；契约保持不变，预算仅由明确操作调整。</p>}
      {error !== undefined && <p className={css.error} role="alert">{error}</p>}
    </div>
  )
}

/**
 * Render the detail panel.
 * @param props.session - the open session.
 * @param props.width - resolved track width in px.
 * @param props.onClose - collapse the panel.
 * @returns the panel element tree.
 */
/** Diagnostics tab: the same counters the terminal's stats snapshot shows. */
function DiagnosticsTab({ session, state }: { session: SessionSummary; state: SessionState | undefined }) {
  const groups = diagnosticGroups(state ?? null, session.cwd)
  const problems = (state?.items ?? []).filter(item => item.kind === 'error' || item.kind === 'signal')
  return (
    <div className={css.body}>
      {groups.map(group => (
        <section key={group.title} className={css.group}>
          <h4 className={css.groupTitle}>{group.title}</h4>
          <dl className={css.facts}>
            {group.rows.map(([label, value]) => (
              <div key={label} className={css.fact}>
                <dt>{label}</dt>
                <dd>{value}</dd>
              </div>
            ))}
          </dl>
          {group.title === 'Token'
            ? <p className={css.note}>命中率 = 缓存读取 ÷（未缓存输入 + 缓存读取 + 缓存创建）</p>
            : null}
        </section>
      ))}
      {problems.length > 0 && (
        <section className={css.group}>
          <h4 className={css.groupTitle}>本轮问题</h4>
          {problems.map((item, index) => (
            <p key={index} className={css.problem}>{'text' in item ? item.text : ''}</p>
          ))}
        </section>
      )}
      <p className={css.hint}>诊断只读：口径与终端 StatsSnapshot 一致，缺失的旧统计显示 —。</p>
    </div>
  )
}

export function DetailPanel({
  session, width, tab, path, onTabChange, onPathChange, onClose, state,
}: {
  session: SessionSummary
  width: number
  /** The live projection, for the diagnostics counters. */
  state: SessionState | undefined
  /** Visible tab; owned by the shell so commands can elect one. */
  tab: DetailTab
  /** Opened artifact or file, when the active tab has one. */
  path: string | undefined
  onTabChange: (tab: DetailTab) => void
  onPathChange: (path: string | undefined) => void
  onClose: () => void
}) {
  const details = useDetails(session, tab, path, { goal: state?.goal, generation: state?.generation })

  const selectTab = (next: DetailTab): void => {
    onTabChange(next)
    onPathChange(undefined)
  }
  const setPath = onPathChange

  return (
    <div className={css.panel} data-detail-panel={tab} style={{ width }}>
      <div className={css.panelBody}>
        <div className={css.tabStrip} role="tablist" aria-label="详情">
          {DETAIL_TABS.map(candidate => (
            <button
              key={candidate}
              type="button"
              role="tab"
              className={css.tab}
              aria-pressed={candidate === tab}
              aria-selected={candidate === tab}
              onClick={() => { selectTab(candidate) }}
            >
              {TAB_LABELS[candidate]}
            </button>
          ))}
          <Button
            size="sm"
            variant="ghost"
            className={clsx(css.close)}
            aria-label="收起详情"
            onClick={onClose}
          >
            收起
          </Button>
        </div>

        {tab === 'diagnostics' && <DiagnosticsTab session={session} state={state} />}

        {tab === 'task' && (
          <div className={css.body}>
            <h2 className={css.sectionTitle}>
              当前计划
              <Tag tone={details.plan.plan === null ? 'outline' : 'success'}>
                {details.plan.plan === null ? (details.plan.draft === null ? '无' : '草稿') : '已确认'}
              </Tag>
            </h2>
            {details.plan.plan !== null || details.plan.draft !== null
              ? (
                <MarkdownText
                  text={details.plan.plan ?? details.plan.draft ?? ''}
                  labels={MARKDOWN_LABELS}
                  variant="compact"
                />
              )
              : <p className={css.hint}>尚无计划</p>}
            <h2 className={css.sectionTitle}>
              Todo
              {details.todo.revision === undefined
                ? null
                : <Tag tone="quiet">revision {details.todo.revision}</Tag>}
            </h2>
            {(details.todo.items?.length ?? 0) === 0
              ? <p className={css.hint}>尚无待办</p>
              : (
                <ul className={css.list}>
                  {details.todo.items?.map(item => (
                    <li key={item.id} className={css.row}>
                      <StateDot state={todoState(item.status)} />
                      <span className={css.grow}>{item.id}: {item.content}</span>
                    </li>
                  ))}
                </ul>
              )}
            <p className={css.hint}>计划与 Todo 为只读；调整要求写进输入区。</p>
            <div className={css.actions}>
              <Button size="sm" variant="outline" disabled={details.loading} onClick={details.refresh}>刷新</Button>
            </div>
          </div>
        )}

        {tab === 'goal' && (
          <GoalTab
            key={`${sessionIdentity(session)}:${state?.generation ?? 'unknown'}:${details.goal.snapshot?.id ?? 'no-goal'}`}
            session={session}
            goal={details.goal.snapshot}
            driven={details.goal.driven}
            refresh={details.refresh}
            onReport={(id) => { onTabChange('outputs'); setPath(id) }}
          />
        )}

        {tab === 'files' && (
          <div className={css.body}>
            <h2 className={css.sectionTitle}>
              文件
              <span className={css.hint}>{details.directory === '' ? '项目根目录' : `${details.directory}/`}</span>
            </h2>
            {/* Inside a directory, going back up is the reader's next move; the
                root needs no control because there is nothing above it. */}
            {details.directory !== '' && (
              <div className={css.actions}>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => { setPath(parentPath(details.directory)) }}
                >
                  {parentPath(details.directory) === undefined ? '返回项目根目录' : '返回上级'}
                </Button>
              </div>
            )}
            {details.file === undefined
              ? (details.files.length === 0
                ? <p className={css.hint}>{details.loading ? '读取中…' : '此目录为空'}</p>
                : (
                  <ul className={css.list}>
                    {details.files.map(entry => (
                      <li key={entry.name}>
                        <button
                          type="button"
                          className={css.fileButton}
                          aria-label={entry.dir ? `展开目录 ${entry.name}` : `打开文件 ${entry.name}`}
                          onClick={() => {
                            const base = details.directory === '' ? '' : `${details.directory}/`
                            setPath(`${base}${entry.name}${entry.dir ? '/' : ''}`)
                          }}
                        >
                          {entry.dir ? '▸' : '·'} {entry.name}
                        </button>
                      </li>
                    ))}
                  </ul>
                ))
              : (
                <>
                  <div className={css.actions}>
                    <Button size="sm" variant="outline" onClick={() => { setPath(undefined) }}>返回目录</Button>
                  </div>
                  <FilePreview path={details.file.path} content={details.file.content} />
                </>
              )}
            {details.error !== undefined && <p className={css.error} role="alert">{details.error}</p>}
          </div>
        )}

        {tab === 'outputs' && (
          <div className={css.body}>
            <h2 className={css.sectionTitle}>工具输出</h2>
            {details.artifact === undefined
              ? (details.artifacts.length === 0
                ? <p className={css.hint}>{details.loading ? '读取中…' : '没有落盘的超长输出'}</p>
                : (
                  <ul className={css.list}>
                    {details.artifacts.map(entry => (
                      <li key={entry.id}>
                        <button
                          type="button"
                          className={css.fileButton}
                          onClick={() => { setPath(entry.id) }}
                        >
                          {entry.tool === undefined ? entry.id : `${entry.tool} · ${entry.id}`}
                        </button>
                      </li>
                    ))}
                  </ul>
                ))
              : (
                <>
                  <div className={css.actions}>
                    <Button size="sm" variant="outline" onClick={() => { setPath(undefined) }}>返回列表</Button>
                  </div>
                  <ReadBlock
                    label={details.artifact.name}
                    lines={fileLines(details.artifact.content)}
                    totalLines={fileLines(details.artifact.content).length}
                    labels={READ_LABELS}
                    maxLines={64}
                  />
                </>
              )}
            {details.error !== undefined && <p className={css.error} role="alert">{details.error}</p>}
          </div>
        )}
      </div>
    </div>
  )
}
