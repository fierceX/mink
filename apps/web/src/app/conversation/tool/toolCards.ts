/**
 * mink tool call → copied card props. Every parser is a pure function over the
 * transcript item, and every one falls back to `null` rather than guessing: a
 * result the parser cannot recognize renders as the generic in/out card.
 */

import { languageForPath } from '../../../util/code-language.ts'
import { formatChanges, parseChanges, parseTodoContent, type TodoBlock } from '../../../data/toolFormat.ts'
import type { DiffBlockProps, ReadBlockProps, SearchBlockProps } from '../../../ui/primitives/index.ts'
import type { ToolItem } from '../../../data/types.ts'

/** The primitive's own props minus the chrome the render site supplies. */
type CardProps<T> = T extends unknown ? Omit<T, 'labels' | 'maxLines' | 'className'> : never

/** File-card payload the render site completes with labels and caps. */
export type ReadCard = CardProps<ReadBlockProps>
/** Search-card payload (one of the two shapes) with the same omissions. */
export type SearchCard = CardProps<SearchBlockProps>

/** Row variant, mirroring the copied tool row's per-tool presentation axis. */
export type ToolVariant = 'search' | 'read' | 'bash' | 'write' | 'edit' | 'code' | 'others'

/** Row lifecycle state the summary and body styling read. */
export type ToolState = 'running' | 'ok' | 'error' | 'stopped'

/** Pick the variant for a mink tool name. */
export function toolVariant(name: string): ToolVariant {
  if (/^Bash$|^Python/.test(name)) return 'bash'
  if (name === 'Read') return 'read'
  if (name === 'Write') return 'write'
  if (name === 'Edit') return 'edit'
  if (/^Glob$|^Grep$/.test(name)) return 'search'
  if (/^SubAgent$/.test(name)) return 'code'
  return 'others'
}

/** Lifecycle state: a missing result is still running, a failure settles red. */
export function toolState(item: ToolItem): ToolState {
  if (item.result === undefined) return 'running'
  return item.failed === true || item.success === false ? 'error' : 'ok'
}

/** Parse the tool's argument JSON, or null when it is absent or malformed. */
export function toolArgs(item: ToolItem): Record<string, unknown> | null {
  if (item.input === '') return null
  try {
    const parsed: unknown = JSON.parse(item.input)
    return typeof parsed === 'object' && parsed !== null ? parsed as Record<string, unknown> : null
  } catch {
    return null
  }
}

/** A string field of the argument object. */
function argString(args: Record<string, unknown> | null, key: string): string | undefined {
  const value = args?.[key]
  return typeof value === 'string' ? value : undefined
}

/**
 * File card for a Read result: mink numbers every returned line `N:text` and
 * prefixes hashline windows with `[path#tag]`.
 * @param item - the read item.
 * @returns the card props, or null when the result is not a numbered window.
 */
export function readCard(item: ToolItem): ReadCard | null {
  if (item.result === undefined) return null
  const rows = item.result.split('\n')
  const lines: { number: number; text: string }[] = []
  let label: string | undefined
  for (const row of rows) {
    const header = /^\[(.+)#[0-9A-Fa-f]+\]$/.exec(row) ?? /^\[read-only virtual file: (.+)\]$/.exec(row)
    if (header !== null) {
      label = header[1]
      continue
    }
    const numbered = /^(\d+):(.*)$/.exec(row)
    if (numbered !== null) lines.push({ number: Number(numbered[1]), text: numbered[2] ?? '' })
  }
  if (lines.length === 0) return null
  const path = label ?? argString(toolArgs(item), 'path') ?? item.summary
  const lang = languageForPath(path)
  return {
    label: path,
    lines,
    totalLines: lines[lines.length - 1]?.number ?? lines.length,
    ...(lang === undefined ? {} : { lang }),
  }
}

/** A unified diff body, when the result itself carries the patch text. */
function unifiedHunks(path: string, text: string): DiffBlockProps['diffs'] | null {
  const rows = text.split('\n')
  const oldLines: string[] = []
  const newLines: string[] = []
  let sawMarker = false
  for (const row of rows) {
    if (row.startsWith('--- ') || row.startsWith('+++ ') || row.startsWith('diff --git') || row.startsWith('@@')) {
      sawMarker = true
      continue
    }
    if (row.startsWith('-')) {
      oldLines.push(row.slice(1))
      continue
    }
    if (row.startsWith('+')) {
      newLines.push(row.slice(1))
      continue
    }
    if (row.startsWith(' ')) {
      oldLines.push(row.slice(1))
      newLines.push(row.slice(1))
    }
  }
  if (!sawMarker || (oldLines.length === 0 && newLines.length === 0)) return null
  return [{ path, oldText: oldLines.join('\n'), newText: newLines.join('\n') }]
}

/**
 * Diff card for an Edit: the Replace input carries the old/new text pairs, and
 * a hashline patch or unified diff in the result is used when it is present.
 * @param item - the edit item.
 * @returns the card props, or null when neither side yields hunks.
 */
export function diffCard(item: ToolItem): Pick<DiffBlockProps, 'diffs'> | null {
  const args = toolArgs(item)
  const path = argString(args, 'path') ?? item.summary
  const edits = args?.['edits']
  if (Array.isArray(edits) && edits.length > 0) {
    const diffs: DiffBlockProps['diffs'] = []
    for (const entry of edits) {
      if (typeof entry !== 'object' || entry === null) return null
      const oldText = (entry as { old_text?: unknown }).old_text
      const newText = (entry as { new_text?: unknown }).new_text
      if (typeof oldText !== 'string' || typeof newText !== 'string') return null
      diffs.push({ path, oldText, newText })
    }
    return { diffs }
  }
  if (item.result === undefined) return null
  const hunks = unifiedHunks(path, item.result)
  return hunks === null ? null : { diffs: hunks }
}

/**
 * Search card: Glob answers a path list, Grep a `path:line:text` list grouped by
 * file.
 * @param item - the search item.
 * @returns the card props, or null when the result is not a search listing.
 */
export function searchCard(item: ToolItem): SearchCard | null {
  if (item.result === undefined) return null
  const rows = item.result.split('\n').filter(row => row.trim() !== '')
  if (rows.length === 0) return null
  const matches = rows.map(row => /^(.*?):(\d+):(.*)$/.exec(row)).filter(entry => entry !== null)
  if (item.name === 'Grep' && matches.length > 0) {
    const files: { path: string; matches: { lineNumber: number; line: string }[] }[] = []
    const byPath = new Map<string, { path: string; matches: { lineNumber: number; line: string }[] }>()
    for (const match of matches) {
      const path = match[1] ?? ''
      let group = byPath.get(path)
      if (group === undefined) {
        group = { path, matches: [] }
        byPath.set(path, group)
        files.push(group)
      }
      group.matches.push({ lineNumber: Number(match[2]), line: match[3] ?? '' })
    }
    const shown = files.reduce((sum, file) => sum + file.matches.length, 0)
    return { kind: 'matches', files, truncated: false, total: shown } as SearchCard
  }
  if (item.name === 'Glob') return { kind: 'paths', paths: rows, truncated: false, total: rows.length } as SearchCard
  return null
}

/** One delegated task, as the call's arguments describe it. */
export interface SubAgentTask {
  id: string
  description: string
  prompt: string
}

/**
 * Delegation card: mink's SubAgent call carries one entry per dispatched task,
 * which reads far better as rows than as the raw argument JSON the generic card
 * would show. A call whose arguments do not parse keeps the generic card.
 * @param item - the transcript tool item.
 * @returns one entry per task, or null when the call is not a delegation.
 */
export function subAgentCard(item: ToolItem): { tasks: SubAgentTask[] } | null {
  if (item.name !== 'SubAgent') return null
  const args = toolArgs(item)
  // Current schema: one child per call. Keep the old `tasks[]` shape as a
  // compatibility fallback for historical transcript rows.
  const prompt = argString(args, 'prompt')
  if (prompt !== undefined && prompt !== '') {
    const description = argString(args, 'description') ?? prompt.split('\n')[0] ?? ''
    return {
      tasks: [{
        id: argString(args, 'description') ?? 'task-1',
        description: description === '' ? 'task-1' : description,
        prompt,
      }],
    }
  }
  const tasks = args?.['tasks']
  if (!Array.isArray(tasks)) return null
  const rows = tasks.flatMap((task, index) => {
    if (typeof task !== 'object' || task === null) return []
    const entry = task as Record<string, unknown>
    const description = typeof entry['description'] === 'string' ? entry['description'] : ''
    const prompt = typeof entry['prompt'] === 'string' ? entry['prompt'] : ''
    const id = typeof entry['id'] === 'string' && entry['id'] !== '' ? entry['id'] : `task-${index + 1}`
    if (description === '' && prompt === '') return []
    return [{ id, description: description === '' ? id : description, prompt }]
  })
  return rows.length === 0 ? null : { tasks: rows }
}

/** One todo entry, as the tool result records it. */
export interface TodoEntry {
  id: string
  content: string
  status: string
}

/** Status wording the todo card prints. */
const TODO_STATUS: Record<string, string> = {
  pending: '待办',
  in_progress: '进行中',
  completed: '已完成',
}

/** Wording for one todo status; unknown states stay as the server wrote them. */
export function todoStatusLabel(status: string): string {
  return TODO_STATUS[status] ?? status
}

/**
 * Todo card: mink's todo tools answer with the whole list, revision and all,
 * which reads as rows — the same list the terminal shows — rather than as the
 * argument JSON. Anything unreadable keeps the generic card.
 * @param item - the transcript tool item.
 * @returns the list the result recorded, or null when it is not a todo answer.
 */
export function todoCard(item: ToolItem): {
  revision: number | null
  counts: { pending: number; in_progress: number; completed: number } | null
  entries: TodoEntry[]
} | null {
  if (!/^TodoRead|^TodoWrite|^TodoAdvance/.test(item.name)) return null
  const result = item.result
  if (result === undefined || result.trim() === '') return null
  let parsed: unknown
  try {
    parsed = JSON.parse(result)
  } catch {
    return null
  }
  if (typeof parsed !== 'object' || parsed === null) return null
  const record = parsed as Record<string, unknown>
  const todos = record['todos']
  if (!Array.isArray(todos)) return null
  const entries = todos.flatMap(entry => {
    if (typeof entry !== 'object' || entry === null) return []
    const row = entry as Record<string, unknown>
    const content = typeof row['content'] === 'string' ? row['content'] : ''
    if (content === '') return []
    return [{
      id: typeof row['id'] === 'string' ? row['id'] : '',
      content,
      status: typeof row['status'] === 'string' ? row['status'] : 'pending',
    }]
  })
  const revision = typeof record['revision'] === 'number' ? record['revision'] : null
  if (entries.length === 0 && revision === null) return null
  // The head the legacy workbench drew: how many of each state the snapshot holds,
  // counted from the rows when the payload itself carries no tally.
  const tally = record['counts']
  const counts = typeof tally === 'object' && tally !== null
    ? {
      pending: numeric((tally as Record<string, unknown>)['pending']),
      in_progress: numeric((tally as Record<string, unknown>)['in_progress']),
      completed: numeric((tally as Record<string, unknown>)['completed']),
    }
    : countStatuses(entries)
  return { revision, counts, entries }
}

/** Read a count the payload wrote, defaulting to zero. */
function numeric(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : 0
}

/** Tally the rows by state, in the three the store knows. */
function countStatuses(entries: readonly TodoEntry[]): { pending: number; in_progress: number; completed: number } {
  const counts = { pending: 0, in_progress: 0, completed: 0 }
  for (const entry of entries) {
    if (entry.status === 'in_progress') counts.in_progress += 1
    else if (entry.status === 'completed') counts.completed += 1
    else counts.pending += 1
  }
  return counts
}

/**
 * Plan card: a plan tool's body is the plan itself, so the card shows that text
 * (and the tool's own answer line) instead of the JSON that carried it.
 * @param item - the transcript tool item.
 * @returns the plan text, or null when the call carries none.
 */
export function planCard(item: ToolItem): { text: string; answer: string } | null {
  if (!item.name.startsWith('Plan')) return null
  const args = toolArgs(item)
  const text = argString(args, 'content') ?? argString(args, 'plan') ?? ''
  const answer = (item.result ?? '').trim()
  if (text.trim() === '' && answer === '') return null
  return { text, answer }
}

/** Wording for the three kinds of goal report. */
const GOAL_KIND: Record<string, string> = {
  continue: '继续执行',
  complete: '报告完成',
  blocked: '报告阻塞',
}

/**
 * Goal-report card: mink's goal reports carry the summary, the next step and the
 * evidence a reader checks, so the card shows those instead of the JSON that
 * carried them.
 * @param item - the transcript tool item.
 * @returns the report's parts, or null when the call is not a goal report.
 */
export function goalReportCard(item: ToolItem): {
  kind: string
  iteration: number | null
  summary: string
  nextStep: string
  criteria: string[]
  evidence: string[]
} | null {
  if (item.name !== 'GoalReport') return null
  const args = toolArgs(item)
  if (args === null) return null
  const strings = (key: string): string[] => {
    const value = args[key]
    return Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === 'string') : []
  }
  const rawKind = typeof args['kind'] === 'string' ? args['kind'] : ''
  const summary = typeof args['summary'] === 'string' ? args['summary'] : ''
  const nextStep = typeof args['next_step'] === 'string' ? args['next_step'] : ''
  const criteria = strings('criteria')
  const evidence = strings('evidence')
  if (summary === '' && nextStep === '' && criteria.length === 0 && evidence.length === 0) return null
  return {
    kind: GOAL_KIND[rawKind] ?? rawKind,
    iteration: typeof args['iteration'] === 'number' ? args['iteration'] : null,
    summary,
    nextStep,
    criteria,
    evidence,
  }
}

/** The todo blocks a result carries, when it is a formatted todo answer. */
export function todoBlocks(item: ToolItem): TodoBlock[] {
  if (!/^TodoRead|^TodoWrite|^TodoAdvance/.test(item.name)) return []
  const result = item.result
  if (result === undefined) return []
  return parseTodoContent(result)
}

/** Change summary the tool's presentation recorded, when it has one. */
export function todoChangeSummary(item: ToolItem): string {
  return formatChanges(parseChanges(item.presentation))
}

/** Plan transitions the card names, keyed as the core reports them. */
const PLAN_TRANSITION: Record<string, string> = {
  draft_saved: '计划草稿已保存',
  draft_cancelled: '计划草稿已取消',
  confirmed: '计划已确认',
  cleared: '计划已清除',
}

/**
 * Plan presentation: the core reports the transition and the plan body it wrote,
 * which is what the card shows (the raw result stays behind a disclosure).
 * @param item - the transcript tool item.
 * @returns the transition label and body, or null when the call carries neither.
 */
export function planPresentation(item: ToolItem): { transition: string; body: string } | null {
  const value = item.presentation as { kind?: string; data?: { transition?: string; content?: unknown } } | undefined
  if (value?.kind !== 'plan' || value.data === undefined) return null
  const transition = typeof value.data.transition === 'string' ? PLAN_TRANSITION[value.data.transition] ?? value.data.transition : ''
  const body = typeof value.data.content === 'string' ? value.data.content : ''
  if (transition === '' && body === '') return null
  return { transition, body }
}

/**
 * The file a call names, when it names one.
 * @param item - the transcript tool item.
 * @returns the path, or null when the call is not about one file.
 */
export function filePathOf(item: ToolItem): string | null {
  if (!['Read', 'Write', 'Edit'].includes(item.name)) return null
  const args = toolArgs(item)
  const patch = eitherString(args, 'input') ?? eitherString(args, 'patch') ?? eitherString(args, 'file_path') ?? eitherString(args, 'path')
  if (patch === null) return null
  // Edit reports `path:line[-other]`: the panel wants the path itself.
  const match = patch.match(/^(.*?)(?::\d+(?:[-+].*)?)?$/)
  const path = (match?.[1] ?? patch).trim()
  return path === '' ? null : path
}

/** A string argument under either of two keys. */
function eitherString(args: Record<string, unknown> | null, key: string): string | null {
  const value = args?.[key]
  return typeof value === 'string' && value !== '' ? value : null
}
