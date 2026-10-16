/**
 * One tool call, drawn with the copied tool-row design (ui-tool ToolRow):
 * the same disclosure row, the same summary/suffix/diff-total treatment and the
 * same per-family bodies — terminal, diff, read, search — with the generic
 * in/out card as the fallback. The ask-question, image, web and inspection
 * branches of the source are absent here because mink has no such surfaces yet.
 */

import { memo, useMemo, useState } from 'react'
import clsx from 'clsx'
import {
  DiffBlock,
  DisclosureRow,
  IconAgentPresetOutlineRegular,
  IconApiOutlineRegular,
  IconBrowseOutlineRegular,
  IconChecklistOutlineRegular,
  IconEditOutlineRegular,
  IconGoalOutlineRegular,
  IconPlanOutlineRegular,
  IconSearchOutlineRegular,
  IconSparkleRegular,
  MarkdownText,
  ReadBlock,
  SearchBlock,
  TerminalBlock,
  TextShimmer,
  diffTotals,
} from '../../../ui/primitives/index.ts'
import type { ToolItem } from '../../../data/types.ts'
import {
  diffCard,
  filePathOf,
  planPresentation,
  goalReportCard,
  planCard,
  readCard,
  searchCard,
  subAgentCard,
  todoBlocks,
  todoCard,
  todoChangeSummary,
  todoStatusLabel,
  toolArgs,
  toolState,
  toolVariant,
} from './toolCards.ts'
import { DIFF_LABELS, READ_LABELS, SEARCH_LABELS, TERMINAL_LABELS } from './toolLabels.ts'
import { MARKDOWN_LABELS } from '../labels.ts'
import css from './ToolRow.module.css'

/**
 * Business icon per tool family, taken from the source's own variant mapping
 * (ui-tool GenericToolCard VARIANT_ICONS and details-row detailIcon) instead of
 * a state dot: the row's state already rides the summary's colour and shimmer.
 */
function toolIcon(name: string) {
  if (['Bash', 'Python', 'PythonSandbox'].includes(name)) return <IconApiOutlineRegular size={14} />
  if (name === 'Read') return <IconBrowseOutlineRegular size={14} />
  if (['Edit', 'Write'].includes(name)) return <IconEditOutlineRegular size={14} />
  if (['Grep', 'Glob'].includes(name)) return <IconSearchOutlineRegular size={14} />
  if (['TodoRead', 'TodoWrite', 'TodoAdvance'].includes(name)) return <IconChecklistOutlineRegular />
  if (name.startsWith('Plan')) return <IconPlanOutlineRegular size={14} />
  if (name === 'GoalReport') return <IconGoalOutlineRegular size={14} />
  if (name === 'SubAgent') return <IconAgentPresetOutlineRegular size={14} />
  return <IconSparkleRegular size={14} />
}

/** Highest line count a card shows before its middle collapses. */
const CARD_MAX_LINES = 16

/** Command line a command tool ran, read out of its argument JSON. */
function commandOf(item: ToolItem): string {
  const args = toolArgs(item)
  const command = args?.['command'] ?? args?.['script'] ?? args?.['script_file']
  return typeof command === 'string' && command !== '' ? command : item.summary
}

/** Human wording for the hidden run-state announcement. */
function stateStatus(state: ReturnType<typeof toolState>): string | null {
  switch (state) {
    case 'running': return '执行中'
    case 'error': return '失败'
    case 'stopped': return '已停止'
    default: return null
  }
}

/**
 * Render one tool call.
 * @param props.item - the transcript tool item.
 * @returns the tool row with its lazily mounted result card.
 */
export const ToolCard = memo(function ToolCard({ item, onOpenFile, onOpenArtifact }: {
  item: ToolItem
  /** Open the file this call names in the detail panel's file tab. */
  onOpenFile?: ((path: string) => void) | undefined
  /** Open the output this call spilled into once it passed the size guard. */
  onOpenArtifact?: ((id: string) => void) | undefined
}) {
  const [expanded, setExpanded] = useState(false)
  const state = toolState(item)
  const variant = toolVariant(item.name)
  const terminal = variant === 'bash'
  const diff = variant === 'edit' ? diffCard(item) : null
  const read = variant === 'read' ? readCard(item) : null
  const search = variant === 'search' ? searchCard(item) : null
  const delegation = subAgentCard(item)
  // The legacy card offered both entry points inline: the file the call touched,
  // and the artifact a long result was written to.
  const filePath = filePathOf(item)
  const todos = todoCard(item)
  const blocks = todoBlocks(item)
  const changes = todoChangeSummary(item)
  const plan = todoCard(item) === null ? planCard(item) : null
  const planMeta = planPresentation(item)
  const report = goalReportCard(item)
  const result = item.result ?? null
  const card = terminal || diff !== null || read !== null || search !== null || result === null ? null : result
  const expandable = state !== 'running' && (result !== null || read !== null)
  const open = expanded && expandable
  const status = stateStatus(state)
  const totals = useMemo(() => (diff === null ? null : diffTotals(diff.diffs)), [diff])

  const collapsedContent = (
    <>
      <span className={css.sep} data-shimmer-decoration aria-hidden />
      <span className={clsx(css.summary, state === 'error' && css.errorSummary)}>
        <TextShimmer>{item.summary === '' ? item.name : item.summary}</TextShimmer>
      </span>
      {totals !== null && state === 'ok' && (
        <TextShimmer className={clsx(css.summarySuffix, css.diffStat)}>
          <span className={css.diffAdded}>{`+${totals.added}`}</span>{' '}
          <span className={css.diffRemoved}>{`-${totals.removed}`}</span>
        </TextShimmer>
      )}
    </>
  )

  return (
    <div className={css.root} data-variant={variant} data-tool={item.name} data-state={state}>
      {status !== null && <span className={css.visuallyHidden}>{status}</span>}
      <DisclosureRow
        rowClassName={css.row}
        leadingClassName={css.leading}
        titleClassName={css.title}
        icon={toolIcon(item.name)}
        title={item.name}
        running={state === 'running'}
        open={open}
        expandable={expandable}
        expandOnRowClick
        keepContentWhenOpen
        onToggle={() => {
          setExpanded(value => !value)
        }}
        collapsedContent={collapsedContent}
      >
        {open && (
          <div className={css.bodyWrap}>
            {terminal
              ? (
                <TerminalBlock
                  className={css.terminalBody}
                  command={commandOf(item)}
                  output={result ?? ''}
                  {...(item.exitCode === undefined || item.exitCode === null ? {} : { exitCode: item.exitCode })}
                  labels={TERMINAL_LABELS}
                />
              )
              : diff !== null
                ? <DiffBlock {...diff} labels={DIFF_LABELS} maxLines={CARD_MAX_LINES} className={css.diffBody} />
                : read !== null
                  ? <ReadBlock {...read} labels={READ_LABELS} maxLines={CARD_MAX_LINES} className={css.readBody} />
                  : search !== null
                    ? (
                      <SearchBlock
                        {...search}
                        labels={SEARCH_LABELS}
                        maxLines={CARD_MAX_LINES}
                        className={css.searchBody}
                      />
                    )
                    : report !== null
                      ? (
                        <div className={css.reportCard} data-goal-report-card>
                          <span className={css.reportHead}>
                            {report.kind !== '' && <span className={css.reportKind}>{report.kind}</span>}
                            {report.iteration !== null && <span className={css.reportIteration}>{`第 ${report.iteration} 轮`}</span>}
                          </span>
                          {report.summary !== '' && <span className={css.reportSummary}>{report.summary}</span>}
                          {report.nextStep !== '' && (
                            <span className={css.reportLine}>
                              <span className={css.reportLabel}>下一步</span>
                              {report.nextStep}
                            </span>
                          )}
                          {report.criteria.length > 0 && (
                            <span className={css.reportBlock}>
                              <span className={css.reportLabel}>验收条件</span>
                              {report.criteria.map((entry, index) => (
                                <span key={index} className={css.reportEntry}>{entry}</span>
                              ))}
                            </span>
                          )}
                          {report.evidence.length > 0 && (
                            <span className={css.reportBlock}>
                              <span className={css.reportLabel}>证据</span>
                              {report.evidence.map((entry, index) => (
                                <span key={index} className={css.reportEntry}>{entry}</span>
                              ))}
                            </span>
                          )}
                        </div>
                      )
                      : blocks.length > 0
                      ? (
                        <div className={css.todoCard} data-todo-card data-todo-blocks>
                          {changes !== '' && <span className={css.todoChanges}>{changes}</span>}
                          {blocks.map((block, index) => (
                            <div key={index} className={css.todoBlock}>
                              {(block.revision !== undefined || block.counts !== undefined) && (
                                <span className={css.todoMeta}>
                                  {block.revision !== undefined && `revision ${block.revision}`}
                                  {block.counts !== undefined && (
                                    ` · 待办 ${block.counts.pending} · 进行中 ${block.counts.in_progress} · 已完成 ${block.counts.completed}`
                                  )}
                                </span>
                              )}
                              {block.lines?.map((line, lineIndex) => (
                                <span key={lineIndex} className={css.todoChange}>{line}</span>
                              ))}
                              {block.note !== undefined && <span className={css.todoMeta}>{block.note}</span>}
                              {block.tasks.map(task => (
                                <div key={task.id} className={css.todoRow} data-status={task.status}>
                                  <span className={css.todoId}>{task.id}</span>
                                  <span className={css.todoContent}>{task.text}</span>
                                  <span className={css.todoStatus}>{todoStatusLabel(task.status)}</span>
                                </div>
                              ))}
                            </div>
                          ))}
                          <RawDisclosure result={item.result} />
                        </div>
                      )
                      : todos !== null
                      ? (
                        <div className={css.todoCard} data-todo-card>
                          {changes !== '' && <span className={css.todoChanges}>{changes}</span>}
                          <span className={css.todoMeta} data-todo-head>
                            {todos.revision !== null && `revision ${todos.revision}`}
                            {todos.counts !== null && (
                              `${todos.revision === null ? '' : ' · '}待办 ${todos.counts.pending} · 进行中 ${todos.counts.in_progress} · 已完成 ${todos.counts.completed}`
                            )}
                          </span>
                          {todos.entries.map(entry => (
                            <div key={`${entry.id}:${entry.content}`} className={css.todoRow} data-status={entry.status}>
                              <span className={css.todoId}>{entry.id}</span>
                              <span className={css.todoContent}>{entry.content}</span>
                              <span className={css.todoStatus}>{todoStatusLabel(entry.status)}</span>
                            </div>
                          ))}
                          {todos.entries.length === 0 && (
                            <span className={css.todoMeta}>没有待办项</span>
                          )}
                          <RawDisclosure result={item.result} />
                        </div>
                      )
                      : plan !== null || planMeta !== null
                      ? (
                        <div className={css.planCard} data-plan-card>
                          {planMeta !== null && planMeta.transition !== '' && (
                            <span className={css.planAnswer}>{planMeta.transition}</span>
                          )}
                          {(planMeta?.body ?? plan?.text ?? '') !== '' && (
                            <MarkdownText
                              text={planMeta?.body ?? plan?.text ?? ''}
                              labels={MARKDOWN_LABELS}
                              variant="compact"
                            />
                          )}
                          {planMeta === null && plan !== null && plan.answer !== '' && (
                            <span className={css.planAnswer}>{plan.answer}</span>
                          )}
                          {item.result !== undefined && (
                            <details className={css.planRaw}>
                              <summary>原始结果</summary>
                              <pre className={css.planRawBody}>{item.result}</pre>
                            </details>
                          )}
                        </div>
                      )
                      : delegation !== null
                      ? (
                        <div className={css.delegation} data-sub-agent-card>
                          {delegation.tasks.map(task => (
                            <div key={task.id} className={css.delegationTask}>
                              <span className={css.delegationHead}>
                                <span className={css.delegationId}>{task.id}</span>
                                <span className={css.delegationDescription}>{task.description}</span>
                              </span>
                              {task.prompt !== '' && <span className={css.delegationPrompt}>{task.prompt}</span>}
                            </div>
                          ))}
                        </div>
                      )
                      : (
                      <div className={css.ioCard}>
                        <div className={css.ioSection}>
                          <span className={css.ioLabel}>输入</span>
                          <span className={css.ioText}>{item.input}</span>
                        </div>
                        {card !== null && (
                          <>
                            <span className={css.ioDivider} aria-hidden />
                            <div className={css.ioSection}>
                              <span className={css.ioLabel}>输出</span>
                              <span className={css.ioText} data-error={state === 'error' || undefined}>{card}</span>
                            </div>
                          </>
                        )}
                      </div>
                    )}
          </div>
        )}
        {open && (filePath !== null || item.artifact !== undefined) && (
          <div className={css.entryRow}>
            {filePath !== null && onOpenFile !== undefined && (
              <button type="button" className={css.entry} onClick={() => { onOpenFile(filePath) }}>
                查看当前文件
              </button>
            )}
            {item.artifact !== undefined && onOpenArtifact !== undefined && (
              <button
                type="button"
                className={css.entry}
                data-tool-artifact={item.artifact}
                onClick={() => { onOpenArtifact(item.artifact as string) }}
              >
                {`artifact://${item.artifact}`}
              </button>
            )}
          </div>
        )}
      </DisclosureRow>
    </div>
  )
})

/** The raw result behind a disclosure: the card shows the reading, not the wire. */
function RawDisclosure({ result }: { result: string | undefined }) {
  if (result === undefined) return null
  return (
    <details className={css.planRaw}>
      <summary>原始结果</summary>
      <pre className={css.planRawBody}>{result}</pre>
    </details>
  )
}
