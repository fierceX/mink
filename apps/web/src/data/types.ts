// 事件与状态类型：与核心 events.jsonl 协议对齐。

export interface RawEvent {
  type: string;
  seq?: number;
  /** Core turn-local diagnostic sequence. */
  sequence?: number;
  /** Server-wide live stream sequence; the authoritative realtime UI key. */
  stream_sequence?: number;
  [key: string]: unknown;
}

export type TranscriptKey = number | `live:${number}` | `live:${string}:${number}` | `message:${number}:${number}` | `input:${string}`;

export type ToolColor = "exec" | "file" | "search" | "todo" | "plan" | "delegate" | "tool";
export type ResultView = "command" | "file" | "search" | "todo" | "plan" | "diff" | "text";

export interface ThinkingItem { key?: TranscriptKey; kind: "thinking"; text: string; }
export interface TextItem { key?: TranscriptKey; kind: "text"; text: string; }
/** A goal boundary published by core; rendered as a card, never as prose. */
export interface GoalMessageItem {
  key?: TranscriptKey;
  kind: "goal_message";
  goal: {
    kind: "draft" | "start" | "continuation";
    goal_id: string;
    contract_revision: number;
    iteration: number;
    objective: string;
    acceptance_criteria?: string[];
    constraints?: string[];
    prior_decision?: string | null;
    /** Only the draft discussion carries it. */
    feedback?: string | null;
  };
}
export interface UserItem { key?: TranscriptKey; kind: "user"; text: string; inputId?: string; turnId?: string; guidance?: boolean; attachmentIds?: string[]; }
export interface ErrorItem { key?: TranscriptKey; kind: "error"; text: string; }
export interface SignalItem { key?: TranscriptKey; kind: "signal"; text: string; }
export interface SystemItem { key?: TranscriptKey; kind: "system"; text: string; separator?: boolean; }

export interface ToolItem {
  key?: TranscriptKey;
  kind: "tool";
  id: string;
  name: string;
  color: ToolColor;
  view: ResultView;
  summary: string;
  input: string;
  result?: string;
  resultKind?: string;
  rawResultKind?: string;
  success?: boolean;
  exitCode?: number | null;
  artifact?: string | null;
  failed?: boolean;
  presentation?: unknown;
  artifacts?: unknown[];
}

export interface SubAgentItem {
  key?: TranscriptKey;
  kind: "sub_agent";
  sessionId: string;
  status: string;
  thinking: string;
  text: string;
  inTokens: number;
  outTokens: number;
}

export type TranscriptItem = ThinkingItem | TextItem | UserItem | ToolItem | SubAgentItem | ErrorItem | SignalItem | SystemItem | GoalMessageItem;

export interface SessionState {
  sessionId: string;
  generation?: string;
  currentTurn?: string | null;
  phase?: string;
  messages?: Record<string, unknown>[];
  overlays?: RawEvent[];
  outcomes?: Record<string,Record<string,unknown>>;
  resources?: { plan: { plan: string | null; draft: string | null }; todo: TodoResource; artifacts?: {id:string;tool?:string}[] };
  inputs?: InputReceipt[];
  imageLimits?: ImageLimits | null;
  title: string;
  running: boolean;
  /** Live stream may have missed events; input stays disabled until reload succeeds. */
  desynced: boolean;
  lastSeq: number;
  /** Last core turn-local envelope coordinates, for diagnostics. */
  lastTurnId?: string;
  lastCoreSequence?: number;
  /** Bounded turn_id+sequence keys used to reject duplicate live delivery. */
  seenTurnEvents: string[];
  model: string;
  tokensIn: number;
  tokensOut: number;
  belief: number;
  /** Agent 工作状态（由事件推导）：idle/waiting/thinking/generating/tool/sub-agent/compacting/error */
  workState: string;
  /** 缓存命中 tokens（usage 事件累计）与当前上下文估计 */
  cacheReadTokens: number;
  cacheCreationTokens: number | null;
  turnCount: number | null;
  requestCount: number | null;
  contextLimitKnown: boolean;
  waitElapsedSecs: number | null;
  activeSubAgents: string[];
  contextTokens: number;
  maxContextTokens: number;
  items: TranscriptItem[];
  /** Live mirror of the authoritative goal. `null` when there is none. */
  goal?: GoalSnapshot | null;
}

export function emptySession(sessionId: string, title: string): SessionState {
  return {
    sessionId, title, running: false, desynced: false, lastSeq: 0, seenTurnEvents: [], model: "",
    tokensIn: 0, tokensOut: 0, belief: 0, workState: "idle", cacheReadTokens: 0, cacheCreationTokens: null, turnCount: null, requestCount: null, contextLimitKnown: false, waitElapsedSecs: null, activeSubAgents: [], contextTokens: 0, maxContextTokens: 0, items: [],
  };
}

export interface HumanInput { request_id: string; text: string; target_turn_id: string | null; attachment_ids: string[] }
export interface InputReceipt { input_id: string; revision: number; turn_id: string; status: "pending" | "applying" | "applied" | "unapplied" | "withdrawn"; guidance: boolean; input: HumanInput }
export interface Attachment { id: string; mime: string; width: number; height: number; bytes: number }
export interface ImageLimits { kind: string; allowed_mime: string[]; max_image_bytes: number; max_image_bytes_per_request: number; max_images_per_request: number }

export interface TodoResource { revision?: number; items?: { id: string; status: string; content: string }[] }

export interface GoalReviewCandidate {
  goal_id: string
  contract_revision: number
  generation?: number
  iteration?: number
  conversation_seq?: number
  reviewer_id?: string
}

export interface GoalReview {
  candidate?: GoalReviewCandidate
  report?: { artifact: string }
  original_request?: { status: string; reason: string; evidence: string[] }
  verdict: string
  summary: string
  next_step: string
  criteria: { criterion_id: string; status: string; reason: string; evidence: string[] }[]
  constraints: { constraint: string; status: string; reason: string; evidence: string[] }[]
}

/**
 * Read-only mirror of the authoritative goal snapshot. The runtime owns the
 * contract, budget and lifecycle; the workbench only renders this. A goal the
 * user cleared carries `hidden: true` and must not be shown as a marker.
 */
export interface GoalSnapshot {
  id: string;
  contract_revision?: number;
  /** Compare-and-set token; every control call must send the current value. */
  revision: number;
  needs_confirmation?: boolean;
  draft_ready?: boolean;
  draft_duration_secs?: number | null;
  review_candidate?: GoalReviewCandidate | null;
  /** Reviewer infrastructure failure; the candidate can be retried by resume. */
  review_failure_reason?: string | null;
  review_failure_stage?: 'screening' | 'inspecting' | 'adjudicating' | null;
  reviews?: GoalReview[];
  review_stage?: 'screening' | 'inspecting' | 'adjudicating' | null;
  review_requires_full?: boolean;
  review_escalation_reason?: string | null;
  continuations?: {
    proposal: { candidate: GoalReviewCandidate; summary: string; remaining_requirements: string[]; next_step: string };
    report: string;
    fingerprint: string;
    consecutive_rounds: number;
    conversation_seq: number;
  }[];
  /** Optional precomputed projection; raw core snapshots carry the source limits under `spec`. */
  budget?: {
    max_turns: number;
    turns_used: number;
    remaining_turns: number;
    deadline_unix_secs: number;
    remaining_secs: number;
    blocked_after_consecutive_rounds: number;
    blocked_rounds: number;
    soft_token_limit: number | null;
  };
  status: string;
  phase: string;
  spec?: {
    original_request?: string;
    subgoals?: { id: string; objective: string }[];
    criteria?: { id: string; subgoal_id: string; outcome: string; required_evidence: string }[];
    suggested_actions?: string[];
    objective?: string;
    acceptance_criteria?: string[];
    scope?: string;
    constraints?: string[];
    acceptance?: unknown;
    limits?: { max_turns?: number; deadline_unix_secs?: number; soft_token_limit?: number | null; blocked_after_consecutive_rounds?: number };
  };
  completion_basis?: string | null;
  iterations?: unknown[];
  reason?: string | null;
  hidden?: boolean;
  repeated_failures?: number;
  blocked_rounds?: number;
  /** Consecutive same-blocker rounds — the value the block gate counts. */
  blocker_rounds?: number;
  pending_effects?: string[];
  /** Token bookkeeping the runtime settled for the goal, all request kinds. */
  usage?: {
    tokens?: {
      input_tokens?: number;
      cache_read_tokens?: number;
      cache_creation_tokens?: number;
      output_tokens?: number;
    };
  };
}
