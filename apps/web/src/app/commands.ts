/**
 * Slash-command execution for the workbench. Parsing is the shared parser the
 * terminal is tested against; this module only decides what each parsed command
 * does against the server, and reports a message for the composer to show. A
 * command either succeeds (the draft clears) or refuses with copy (the draft
 * stays), never silently.
 */

import { api, type SessionSummary } from '../data/api.ts'
import { goalBudget } from '../data/goalBudget.ts'
import { SlashCommandError, isTerminalOnly, parseSlashCommand, type SimpleCommand, type SlashCommand } from '../data/commands.ts'
import type { InputReceipt, SessionState } from '../data/types.ts'

/** Detail tabs a command can elect. */
export type CommandTab = 'task' | 'goal' | 'files' | 'outputs' | 'diagnostics'

/** What the workbench should do after a command ran. */
export interface CommandEffects {
  /** Open the detail panel, optionally on a tab. */
  openDetails?: CommandTab
  /** Target row inside the opened tab (artifact id / sub-agent id). */
  path?: string
  /** Return the transcript to its tail. */
  follow?: boolean
  /** Whether the session's goal snapshot should be refetched. */
  refreshGoal?: boolean
  /** Whether the session's model vocabulary should be re-read. */
  refreshModel?: boolean
}

/** Result of one command: message plus the effects the shell applies. */
export interface CommandResult {
  message: string
  /** Refusals keep the draft; successes clear it. */
  ok: boolean
  effects?: CommandEffects
}

export interface CommandContext {
  session: SessionSummary
  state: SessionState | undefined
  /** Creates a goal draft; host confirmation starts execution. */
  createGoal: (body: Parameters<typeof api.createGoal>[1]) => Promise<CommandResult>
}

const SIMPLE_EFFECTS: Record<string, CommandEffects & { message: string }> = {
  goals: { message: '已打开目标详情', openDetails: 'goal' },
  plan: { message: '已打开计划', openDetails: 'task' },
  todos: { message: '已打开待办', openDetails: 'task' },
  details: { message: '已打开详情', openDetails: 'task' },
  latest: { message: '已回到最新输出', follow: true },
  inputs: { message: '已打开详情，待处理输入显示在输入框上方', openDetails: 'task' },
  skills: { message: '已打开诊断页签', openDetails: 'diagnostics' },
  help: {
    message: '可用命令：/goal（正文/confirm/refine/human/verified/pause/resume/cancel/clear/approve/reconcile/budget）、/model、/flash、/pro、/compact、/plan、/todos、/details、/latest、/status、/resume、/withdraw',
  },
  status: { message: '已打开诊断页签', openDetails: 'diagnostics' },
}

/** Message describing a successful send-style command. */
function ok(message: string, effects?: CommandEffects): CommandResult {
  return effects === undefined ? { message, ok: true } : { message, ok: true, effects }
}

/** Message describing a refusal (the draft survives). */
function refuse(message: string): CommandResult {
  return { message, ok: false }
}

/** Failure text from a response envelope, without the success branch. */
function failure(response: { code: number; message: string }): string | undefined {
  return response.code === 200 ? undefined : (response.message || `code ${response.code}`)
}

/** Find the pending receipt a /resume or /withdraw names. */
function receiptFor(state: SessionState | undefined, inputId: string): InputReceipt | undefined {
  return state?.inputs?.find(receipt => receipt.input_id === inputId)
}

async function runSimple(command: SimpleCommand, context: CommandContext): Promise<CommandResult> {
  if (command === 'compact') {
    const response = await api.compact(context.session.id, context.session.project_key)
    const error = failure(response)
    return error === undefined ? ok('已手动压缩上下文') : refuse(`压缩失败：${error}`)
  }
  if (command === 'flash' || command === 'pro') {
    const response = await api.setModel(context.session.id, command, context.session.project_key)
    const error = failure(response)
    // The seat's label is the runtime's own report: re-read it rather than
    // guessing from the request.
    return error === undefined
      ? ok(`已切换到 ${response.data.label}`, { refreshModel: true })
      : refuse(`切换模型失败：${error}`)
  }
  const effect = SIMPLE_EFFECTS[command]
  return effect === undefined
    ? refuse(`/${command} 在工作台不可用`)
    : ok(effect.message, {
      ...(effect.openDetails === undefined ? {} : { openDetails: effect.openDetails }),
      ...(effect.follow === undefined ? {} : { follow: effect.follow }),
      ...(command === 'goals' ? { refreshGoal: true } : {}),
    })
}

async function runGoal(
  goal: Extract<SlashCommand, { kind: 'goal' }>['goal'],
  context: CommandContext,
): Promise<CommandResult> {
  const { session, state } = context
  const snapshot = state?.goal ?? null
  switch (goal.action) {
    case 'status':
      return ok(snapshot === null
        ? '本会话没有目标'
        : `目标 ${snapshot.status} · ${snapshot.phase} · 剩余 ${goalBudget(snapshot)?.remaining_turns ?? '—'} 轮`,
      { openDetails: 'goal', refreshGoal: true })
    case 'create':
      return context.createGoal({
        protocol_version: 2,
        objective: goal.objective,
        acceptance: goal.acceptance,
        ...(goal.acceptance === 'verifier' ? { acceptance_criteria: [goal.objective] } : {}),
      })
    case 'confirm': {
      const response = await api.goalAction(session.id, 'confirm', {
        goal_id: goal.id,
        revision: goal.revision,
      }, session.project_key)
      const error = failure(response)
      return error === undefined
        ? ok('已确认目标草案', { refreshGoal: true, openDetails: 'goal' })
        : refuse(`确认目标失败：${error}`)
    }
    case 'refine': {
      if (snapshot === null) return refuse('本会话没有待确认目标草案')
      const response = await api.goalAction(session.id, 'refine', {
        goal_id: snapshot.id,
        revision: snapshot.revision,
        feedback: goal.feedback,
      }, session.project_key)
      const error = failure(response)
      return error === undefined
        ? ok('已提交草案修改意见', { refreshGoal: true, openDetails: 'goal' })
        : refuse(`修改目标草案失败：${error}`)
    }
    case 'pause':
    case 'resume':
    case 'cancel':
    case 'clear':
    case 'approve':
    case 'reconcile': {
      if (snapshot === null) return refuse('本会话没有目标')
      const body = goal.action === 'approve' || goal.action === 'reconcile'
        ? { goal_id: snapshot.id, revision: snapshot.revision, evidence: goal.evidence }
        : { goal_id: snapshot.id, revision: snapshot.revision }
      const response = await api.goalAction(session.id, goal.action, body, session.project_key)
      const error = failure(response)
      return error === undefined
        ? ok(`已执行 /goal ${goal.action}`, { refreshGoal: true, openDetails: 'goal' })
        : refuse(`/goal ${goal.action} 失败：${error}`)
    }
    case 'budget': {
      if (snapshot === null) return refuse('本会话没有目标')
      const response = await api.goalAction(session.id, 'budget', {
        goal_id: snapshot.id,
        revision: snapshot.revision,
        max_turns: goal.turns,
        max_minutes: goal.minutes,
        ...(goal.tokens === null ? {} : { soft_token_limit: goal.tokens }),
      }, session.project_key)
      const error = failure(response)
      return error === undefined
        ? ok(`已调整额度：${goal.turns} 轮 / ${goal.minutes} 分钟`, { refreshGoal: true })
        : refuse(`调整额度失败：${error}`)
    }
  }
}

async function runReceiptCommand(
  kind: 'resume' | 'withdraw',
  inputId: string,
  context: CommandContext,
): Promise<CommandResult> {
  const receipt = receiptFor(context.state, inputId)
  if (receipt === undefined) return refuse(`未找到待处理输入 ${inputId}`)
  const response = kind === 'resume'
    ? await api.resumeInput(context.session.id, receipt, context.session.project_key)
    : await api.withdrawInput(context.session.id, receipt, context.session.project_key)
  const error = failure(response)
  if (error !== undefined) return refuse(`${kind === 'resume' ? '续发' : '撤回'}失败：${error}`)
  return ok(kind === 'resume' ? '已续发' : '已撤回', { follow: kind === 'resume' })
}

/**
 * Parse and run one command line.
 * @param input - the composer's draft, which starts with `/`.
 * @param context - session, projection and the goal driver hook.
 * @returns the message and the effects to apply.
 */
export async function runCommand(input: string, context: CommandContext): Promise<CommandResult> {
  let command: SlashCommand | null
  try {
    command = parseSlashCommand(input)
  } catch (error) {
    // The parser's message is the usage hint the reader needs.
    return refuse(error instanceof SlashCommandError ? error.message : String(error))
  }
  if (command === null) return refuse('未知命令')
  if (isTerminalOnly(command)) return refuse(`/${command.kind} 只在终端可用`)

  try {
    switch (command.kind) {
      case 'goal':
        return await runGoal(command.goal, context)
      case 'model': {
        const response = await api.setModel(context.session.id, command.model, context.session.project_key)
        const error = failure(response)
        return error === undefined
          ? ok(`已切换到 ${response.data.label}`, { refreshModel: true })
          : refuse(`切换模型失败：${error}`)
      }
      case 'resume':
      case 'withdraw':
        return await runReceiptCommand(command.kind, command.id, context)
      case 'subagent':
        return ok(`已打开 ${command.id}`, { openDetails: 'outputs' })
      case 'artifact':
        return ok(`已打开 ${command.id}`, { openDetails: 'outputs', path: command.id })
      case 'quit':
        return refuse('/quit 只在终端可用')
      default:
        return await runSimple(command.kind, context)
    }
  } catch (cause) {
    return refuse(cause instanceof Error ? cause.message : String(cause))
  }
}
