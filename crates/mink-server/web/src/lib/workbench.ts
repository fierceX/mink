import { reactive, watch } from "vue";
import type { Attachment, TranscriptItem, RawEvent } from "./types";
import { appState, uiState } from "./store";
export interface Upload { key: string; name: string; bytes: number; preview: string; status: "uploading" | "ready" | "failed"; attachment?: Attachment; error?: string }
export interface ViewState {
  draft: string; uploads: Upload[]; failures: { id: string; text: string; attachments: string[]; error: string }[];
  busy: boolean; follow: boolean; anchor?: { key: string; offset: number }; expanded: Record<string, boolean>;
  detailTab: "task" | "files" | "outputs" | "diagnostics"; detailPath: string; detailLine: number; detailScroll: number; innerScroll: Record<string,number>; detailTurn: string;
}
const views = reactive<Record<string, ViewState>>({});
export function identity(project = appState.currentProjectKey ?? "", id = appState.currentSessionId ?? "") { return JSON.stringify([project, id]); }
export function viewFor(key = identity()): ViewState {
  if (!views[key]) {
    let draft = "";
    let persisted: Partial<ViewState> = {};
    try { draft = localStorage.getItem(`mink.draft:${key}`) ?? ""; } catch { /* storage optional */ }
    try { persisted = JSON.parse(localStorage.getItem(`mink.view:${key}`) ?? '{}'); } catch { /* invalid saved state ignored */ }
    views[key] = { draft, uploads: [], failures: [], busy: false, follow: true, expanded: {}, detailTab: "task", detailPath: "", detailLine: 1, detailScroll: 0, innerScroll: {}, detailTurn: "", ...persisted };
    views[key].uploads = (persisted.uploads ?? []).map(upload => upload.status === "uploading" ? { ...upload, status: "failed", preview: "", error: "上传被页面刷新中断，请重新选择图片" } : upload);
  }
  return views[key];
}
watch(views, () => { for (const [key, view] of Object.entries(views)) { try { localStorage.setItem(`mink.draft:${key}`, view.draft); localStorage.setItem(`mink.view:${key}`, JSON.stringify({ uploads: view.uploads, failures: view.failures, expanded: view.expanded, follow: view.follow, anchor: view.anchor, detailTab: view.detailTab, detailPath: view.detailPath, detailLine: view.detailLine, detailScroll: view.detailScroll, innerScroll: view.innerScroll, detailTurn: view.detailTurn })); } catch { /* storage optional */ } } }, { deep: true });
const saved = (name: string, fallback: string) => { try { return localStorage.getItem(name) ?? fallback; } catch { return fallback; } };
export const preferences = reactive({ theme: saved("mink.theme", "light"), process: saved("mink.process", "standard"), navWidth: Number(saved("mink.navWidth", "260")), detailWidth: Number(saved("mink.detailWidth", "360")) });
watch(preferences, () => {
  document.documentElement.dataset.theme = preferences.theme;
  for (const [key, value] of Object.entries(preferences)) { try { localStorage.setItem(`mink.${key}`, String(value)); } catch { /* storage optional */ } }
}, { immediate: true });
export const feedback = reactive({ message: "" });
let toastTimer: ReturnType<typeof setTimeout> | undefined;
export function flash(message: string) { feedback.message = message; clearTimeout(toastTimer); toastTimer = setTimeout(() => feedback.message = "", 4500); }
export function openDetail(tab: ViewState["detailTab"], path = "", line = 1) {
  const view = viewFor(); view.detailTurn = ""; view.detailTab = tab; view.detailPath = path; view.detailLine = line;
  uiState.ctxOpen = true;
}

/** Preserve explicit reading choices when an accepted response takes its formal identity. */
export function handoffCommittedView(key: string, items: TranscriptItem[], event: RawEvent) {
  if (event.type !== "conversation_committed") return;
  const message = event.message as { role?: string; content?: Record<string, unknown>[] };
  if (message?.role !== "assistant" || !Array.isArray(message.content)) return;
  const view = viewFor(key);
  message.content.forEach((block,index) => {
    if (!["thinking","text"].includes(String(block.type))) return;
    const previous = [...items].reverse().find(item => item.kind === block.type && String(item.key).startsWith("live:"));
    if (!previous?.key) return;
    const oldKey = String(previous.key), nextKey = `message:${event.conversation_seq}:${index}`;
    for (const prefix of ["process:","thinking:"]) {
      if (view.expanded[prefix+oldKey] !== undefined && view.expanded[prefix+nextKey] === undefined) view.expanded[prefix+nextKey] = view.expanded[prefix+oldKey];
    }
    if (view.anchor?.key === oldKey) view.anchor.key = nextKey;
    for (const [saved,position] of Object.entries(view.innerScroll)) {
      const [identity,className] = JSON.parse(saved);
      if (identity === oldKey) view.innerScroll[JSON.stringify([nextKey,className])] = position;
    }
  });
}

export function handoffSnapshotView(key: string, items: TranscriptItem[], messages: Record<string,unknown>[], previousSeq: number) {
  for (const message of messages) {
    if (Number(message.seq) <= previousSeq || message.role !== "assistant" || !Array.isArray(message.content)) continue;
    const blocks = message.content;
    const matches = items.filter(item => (item.kind === "thinking" || item.kind === "text") && String(item.key).startsWith("live:") && blocks.some((block: Record<string,unknown>) => block.type === item.kind && String(block.thinking ?? block.text ?? '').startsWith(item.text)));
    handoffCommittedView(key,matches,{type:"conversation_committed",conversation_seq:message.seq,message});
  }
}
