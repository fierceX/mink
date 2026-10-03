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

export type TranscriptItem = ThinkingItem | TextItem | UserItem | ToolItem | SubAgentItem | ErrorItem | SignalItem | SystemItem;

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
  contextTokens: number;
  maxContextTokens: number;
  items: TranscriptItem[];
}

export function emptySession(sessionId: string, title: string): SessionState {
  return {
    sessionId, title, running: false, desynced: false, lastSeq: 0, seenTurnEvents: [], model: "",
    tokensIn: 0, tokensOut: 0, belief: 0, workState: "idle", cacheReadTokens: 0, contextTokens: 0, maxContextTokens: 0, items: [],
  };
}

export interface HumanInput { request_id: string; text: string; target_turn_id: string | null; attachment_ids: string[] }
export interface InputReceipt { input_id: string; revision: number; turn_id: string; status: "pending" | "applying" | "applied" | "unapplied" | "withdrawn"; guidance: boolean; input: HumanInput }
export interface Attachment { id: string; mime: string; width: number; height: number; bytes: number }
export interface ImageLimits { kind: string; allowed_mime: string[]; max_image_bytes: number; max_image_bytes_per_request: number; max_images_per_request: number }

export interface TodoResource { revision?: number; items?: { id: string; status: string; content: string }[] }
