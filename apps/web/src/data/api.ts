/**
 * REST client for mink-server. Endpoints and envelope mirror the Vue workbench's
 * `lib/api.ts`, so both frontends speak the same protocol.
 */

import type { Attachment, GoalSnapshot, HumanInput, InputReceipt } from './types.ts'

// The goal mirror has one authoritative definition (`data/types.ts`); the REST
// module re-exports it so every caller names the same shape.
export type { GoalSnapshot }

/** Uniform envelope every mink-server REST route answers with. */
export interface ApiResponse<T = unknown> {
  code: number
  message: string
  data: T
}

/** One session row as the workspace list reports it. */
export interface SessionSummary {
  project_key: string
  corrupt: boolean
  id: string
  alias: string | null
  title: string | null
  /** First human input, used as the row label when the session has no title. */
  first_prompt?: string | null
  /** Created without a title; the row reads as a new conversation. */
  untitled?: boolean
  cwd: string
  created_at: string
  updated_at: string
  modified_secs: number | null
  status: 'free' | 'active' | 'running'
  path: string
  phase?: string
  pending_input_count?: number
  /** usage.jsonl rollup; 0/absent when the session has no recorded usage. */
  tokens_in?: number
  tokens_out?: number
  cache_read_tokens?: number
  last_context_tokens?: number
}

/** Live session state as `GET /api/sessions/{id}` reports it. */
export interface SessionLiveState {
  id: string
  open: boolean
  running: boolean
  generation?: string
  current_turn?: string
  phase?: string
}

/** One entry of a directory listing. */
export interface FileItem {
  name: string
  dir: boolean
}

/** Todo resource as `GET /api/sessions/{id}/todo` reports it. */
export interface TodoSnapshot {
  revision?: number
  items?: { id: string; status: string; content: string }[]
}

async function request<T>(path: string, options: RequestInit = {}): Promise<ApiResponse<T>> {
  const resp = await fetch(path, {
    headers: { 'Content-Type': 'application/json' },
    ...options,
  })
  const body = (await resp.json().catch(() => ({}))) as ApiResponse<T>
  if (resp.status >= 400 && !body.code) {
    throw new Error(`HTTP ${resp.status}`)
  }
  return body
}

/** Session-scoped URL, carrying the project key when the caller knows it. */
export function sessionUrl(id: string, suffix = '', project?: string): string {
  const base = `/api/sessions/${encodeURIComponent(id)}${suffix}`
  return project === undefined ? base : `${base}?project=${encodeURIComponent(project)}`
}

export const api = {
  /** The bundle this server serves, for telling a stale page from a fresh one. */
  version: () => request<{ bundle: string }>('/api/version'),
  listSessions: () => request<SessionSummary[]>('/api/sessions'),
  /** Create a session in one working directory; the server names it when none is given. */
  createSession: (input: {
    cwd: string
    /** Stable client identity for the first-send creation; retries resolve it. */
    draftId?: string
    /** The first human input, which becomes the session's display title. */
    firstPrompt?: string
    name?: string
  }) =>
    request<SessionSummary>('/api/sessions', {
      method: 'POST',
      body: JSON.stringify({
        cwd: input.cwd,
        ...(input.draftId === undefined ? {} : { draft_id: input.draftId }),
        ...(input.firstPrompt === undefined ? {} : { first_prompt: input.firstPrompt }),
        ...(input.name === undefined ? {} : { name: input.name }),
      }),
    }),
  /** Validate a working directory without creating a session or project. */
  workspace: (path: string) =>
    request<{ cwd: string; project_key: string; name: string }>(
      `/api/workspace?path=${encodeURIComponent(path)}`,
    ),
  getSession: (id: string, project?: string, signal?: AbortSignal) =>
    request<SessionLiveState>(sessionUrl(id, '', project), signal === undefined ? {} : { signal }),
  openSession: (id: string, project?: string) =>
    request<SessionLiveState>(sessionUrl(id, '/open', project), { method: 'POST' }),
  deleteSession: (id: string, project?: string) =>
    request<unknown>(sessionUrl(id, '', project), { method: 'DELETE' }),
  interrupt: (id: string, project?: string) =>
    request<unknown>(sessionUrl(id, '/interrupt', project), { method: 'POST' }),
  /** Pending inputs (guidance) for the session; `requestId` looks one up. */
  inputs: (id: string, project?: string, requestId?: string) =>
    request<InputReceipt[]>(
      `${sessionUrl(id, '/inputs', project)}${requestId === undefined ? '' : `${project === undefined ? '?' : '&'}request_id=${encodeURIComponent(requestId)}`}`,
    ),
  submitInput: (id: string, input: HumanInput, project?: string) =>
    request<InputReceipt>(sessionUrl(id, '/inputs', project), {
      method: 'POST',
      body: JSON.stringify(input),
    }),
  editInput: (id: string, receipt: InputReceipt, text: string, project?: string) =>
    request<InputReceipt>(sessionUrl(id, `/inputs/${encodeURIComponent(receipt.input_id)}`, project), {
      method: 'PATCH',
      body: JSON.stringify({ revision: receipt.revision, text }),
    }),
  withdrawInput: (id: string, receipt: InputReceipt, project?: string) =>
    request<InputReceipt>(sessionUrl(id, `/inputs/${encodeURIComponent(receipt.input_id)}`, project), {
      method: 'DELETE',
      body: JSON.stringify({ revision: receipt.revision }),
    }),
  resumeInput: (id: string, receipt: InputReceipt, project?: string) =>
    request<InputReceipt>(sessionUrl(id, `/inputs/${encodeURIComponent(receipt.input_id)}/resume`, project), {
      method: 'POST',
      body: JSON.stringify({ revision: receipt.revision }),
    }),
  /**
   * Upload one attachment: the server stores the raw bytes under the session's
   * attachment store and answers with the id the input references. Images still
   * reach the model only when a Read captures them.
   */
  upload: (id: string, file: File, project?: string) =>
    request<Attachment>(sessionUrl(id, '/attachments', project), {
      method: 'POST',
      headers: { 'Content-Type': file.type || 'application/octet-stream' },
      body: file,
    }),
  /** The model in force and the alias choices this runtime honours. */
  modelChoices: (id: string, project?: string) =>
    request<{
      model: string
      resolved: string
      choices: { alias: string; model: string }[]
    }>(sessionUrl(id, '/model', project)),
  /** Switch the session's active model; aliases resolve in the same layer the CLI uses. */
  setModel: (id: string, model: string, project?: string) =>
    request<{ model: string; label: string }>(sessionUrl(id, '/model', project), {
      method: 'POST',
      body: JSON.stringify({ model }),
    }),
  /** Manual context compaction, mirroring the terminal's `/compact`. */
  compact: (id: string, project?: string) =>
    request<{ compacted: boolean; outcome: unknown }>(sessionUrl(id, '/compact', project), { method: 'POST' }),
  /** Create a goal draft; execution starts only after the host confirms it. */
  createGoal: (
    id: string,
    body: {
      objective: string
      protocol_version: 2
      acceptance?: 'agent_reviewed' | 'verifier' | 'human'
      max_turns?: number
      max_minutes?: number
      acceptance_criteria?: string[]
      constraints?: string[]
    },
    project?: string,
  ) => request<{ goal: GoalSnapshot }>(sessionUrl(id, '/goal', project), {
    method: 'POST',
    body: JSON.stringify(body),
  }),
  /** Full conversation turns, newest page first unless `tail` is set. */
  conversation: (
    id: string,
    opts: { limit?: number; tail?: boolean; beforeSeq?: number; project?: string; signal?: AbortSignal } = {},
  ) => {
    const params = new URLSearchParams({ limit: String(opts.limit ?? 20), turns: 'true' })
    if (opts.project !== undefined) params.set('project', opts.project)
    if (opts.tail === true) params.set('tail', 'true')
    if (opts.beforeSeq !== undefined) params.set('before_seq', String(opts.beforeSeq))
    return request<unknown[]>(`/api/sessions/${encodeURIComponent(id)}/conversation?${params}`, {
      ...(opts.signal === undefined ? {} : { signal: opts.signal }),
    })
  },
  plan: (id: string, project?: string) =>
    request<{ plan: string | null; draft: string | null }>(sessionUrl(id, '/plan', project)),
  todo: (id: string, project?: string) =>
    request<{ todos: TodoSnapshot }>(sessionUrl(id, '/todo', project)),
  /** Current goal, preferring the live runtime over the persisted file. */
  goal: (id: string, project?: string, full = false) =>
    request<{ goal: GoalSnapshot | null; driven: boolean; verifier?: boolean }>(
      `${sessionUrl(id, '/goal', project)}${project === undefined ? '?' : '&'}full=${full ? 'true' : 'false'}`,
    ),
  /**
   * Confirm or refine a draft, or pause, resume, cancel, clear, re-budget,
   * approve or reconcile. Both
   * `goal_id` and `revision` are required: every mutation is a compare-and-set
   * on the exact goal the caller means.
   */
  goalAction: (
    id: string,
    action: 'pause' | 'resume' | 'cancel' | 'clear' | 'budget' | 'approve' | 'reconcile' | 'confirm' | 'refine',
    body: { goal_id: string; revision: number; evidence?: string; feedback?: string; max_turns?: number; max_minutes?: number; soft_token_limit?: number | null },
    project?: string,
  ) => request<{ goal: GoalSnapshot }>(sessionUrl(id, `/goal/${action}`, project), {
    method: 'POST',
    body: JSON.stringify(body),
  }),
  artifacts: (id: string, project?: string) =>
    request<{ artifacts: { id: string; tool?: string }[] }>(sessionUrl(id, '/artifacts', project)),
  artifact: (id: string, name: string, project?: string) =>
    request<{ name: string; content: string }>(
      sessionUrl(id, `/artifacts/${encodeURIComponent(name)}`, project),
    ),
  files: (id: string, path: string, raw = false, project?: string) =>
    request<{ items?: FileItem[]; content?: string; path?: string }>(
      `${sessionUrl(id, '/files', project)}${project === undefined ? '?' : '&'}path=${encodeURIComponent(path)}${raw ? '&raw=true' : ''}`,
    ),
}
