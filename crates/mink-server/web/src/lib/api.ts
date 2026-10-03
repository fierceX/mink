// REST 客户端：统一 ApiResponse 信封。
import type { HumanInput, InputReceipt, Attachment } from "./types";

export interface ApiResponse<T = unknown> {
  code: number;
  message: string;
  data: T;
}

export interface SessionSummary {
  project_key: string;
  corrupt: boolean;
  id: string;
  alias: string | null;
  title: string | null;
  cwd: string;
  created_at: string;
  updated_at: string;
  modified_secs: number | null;
  status: "free" | "active" | "running";
  path: string;
  phase?: string;
  pending_input_count?: number;
  /** usage.jsonl 汇总（无记录时为 0/缺省） */
  tokens_in?: number;
  tokens_out?: number;
  cache_read_tokens?: number;
  last_context_tokens?: number;
}

export interface FileItem {
  name: string;
  dir: boolean;
}

async function request<T>(path: string, options: RequestInit = {}): Promise<ApiResponse<T>> {
  const resp = await fetch(path, {
    headers: { "Content-Type": "application/json" },
    ...options,
  });
  const body = (await resp.json().catch(() => ({}))) as ApiResponse<T>;
  if (resp.status >= 400 && !body.code) {
    throw new Error(`HTTP ${resp.status}`);
  }
  return body;
}

export const api = {
  listSessions: () => request<SessionSummary[]>("/api/sessions"),
  createSession: (name: string, cwd: string) =>
    request<SessionSummary>("/api/sessions", {
      method: "POST",
      body: JSON.stringify({ name, cwd }),
    }),
  getSession: (id: string, project?: string, signal?: AbortSignal) =>
    request<{ id: string; open: boolean; running: boolean; generation?: string; current_turn?: string; phase?: string }>(
      sessionUrl(id, "", project),
      { signal },
    ),
  openSession: (id: string, project?: string) =>
    request(sessionUrl(id, "/open", project), { method: "POST" }),
  closeSession: (id: string, project?: string) => request(sessionUrl(id,"/close",project),{method:"POST"}),
  deleteSession: (id: string, project?: string) =>
    request(sessionUrl(id, "", project), { method: "DELETE" }),
  sendTurn: (id: string, input: string, project?: string) =>
    request(sessionUrl(id, "/turn", project), {
      method: "POST",
      body: JSON.stringify({ input }),
    }),
  inputs: (id: string, project?: string, requestId?: string) => request<InputReceipt[]>(sessionUrl(id, "/inputs", project) + (requestId ? `${project ? "&" : "?"}request_id=${encodeURIComponent(requestId)}` : "")),
  submitInput: (id: string, input: HumanInput, project?: string) => request<InputReceipt>(sessionUrl(id, "/inputs", project), { method: "POST", body: JSON.stringify(input) }),
  editInput: (id: string, receipt: InputReceipt, text: string, project?: string) => request<InputReceipt>(sessionUrl(id, `/inputs/${encodeURIComponent(receipt.input_id)}`, project), { method: "PATCH", body: JSON.stringify({ revision: receipt.revision, text }) }),
  withdrawInput: (id: string, receipt: InputReceipt, project?: string) => request<InputReceipt>(sessionUrl(id, `/inputs/${encodeURIComponent(receipt.input_id)}`, project), { method: "DELETE", body: JSON.stringify({ revision: receipt.revision }) }),
  resumeInput: (id: string, receipt: InputReceipt, project?: string) => request<InputReceipt>(sessionUrl(id, `/inputs/${encodeURIComponent(receipt.input_id)}/resume`, project), { method: "POST", body: JSON.stringify({ revision: receipt.revision }) }),
  upload: (id: string, file: File, project?: string) => request<Attachment>(sessionUrl(id, "/attachments", project), { method: "POST", headers: { "Content-Type": file.type || "application/octet-stream" }, body: file }),
  interrupt: (id: string, project?: string) =>
    request(sessionUrl(id, "/interrupt", project), { method: "POST" }),
  /** conversation.jsonl 完整轮次分页（历史展示主源） */
  conversation: (id: string, opts: { limit?: number; tail?: boolean; beforeSeq?: number; project?: string; signal?: AbortSignal } = {}) => {
    const params = new URLSearchParams({ limit: String(opts.limit ?? 20), turns: "true" });
    if (opts.project) params.set("project", opts.project);
    if (opts.tail) params.set("tail", "true");
    if (opts.beforeSeq) params.set("before_seq", String(opts.beforeSeq));
    return request<unknown[]>(`/api/sessions/${encodeURIComponent(id)}/conversation?${params}`, {
      signal: opts.signal,
    });
  },
  plan: (id: string, project?: string) =>
    request<{ plan: string | null; draft: string | null }>(
      sessionUrl(id, "/plan", project),
    ),
  todo: (id: string, project?: string) =>
    request<{ todos: unknown }>(sessionUrl(id, "/todo", project)),
  artifacts: (id: string, project?: string) =>
    request<{ artifacts: { id: string; tool?: string }[] }>(
      sessionUrl(id, "/artifacts", project),
    ),
  artifact: (id: string, name: string, project?: string) =>
    request<{ name: string; content: string }>(
      sessionUrl(id, `/artifacts/${encodeURIComponent(name)}`, project),
    ),
  files: (id: string, path: string, raw = false, project?: string) =>
    request<{ items?: FileItem[]; content?: string; path?: string }>(
      `${sessionUrl(id, "/files", project)}${project ? "&" : "?"}path=${encodeURIComponent(path)}${raw ? "&raw=true" : ""}`,
    ),
};

export function sessionUrl(id: string, suffix = "", project?: string): string {
  const base = `/api/sessions/${encodeURIComponent(id)}${suffix}`;
  return project ? `${base}?project=${encodeURIComponent(project)}` : base;
}
