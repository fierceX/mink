import type { TranscriptItem } from "./types";
export interface ProcessSegment { kind: "process"; key: string; items: TranscriptItem[] }
export interface MessageSegment { kind: "message"; key: string; item: TranscriptItem }
export interface Turn { key: string; user?: TranscriptItem; segments: (ProcessSegment | MessageSegment)[] }
export function projectTurns(items: TranscriptItem[]): Turn[] {
  const turns: Turn[] = [];
  for (const [index,item] of items.entries()) {
    const key = String(item.key ?? `legacy:${index}`);
    if (!turns.length || (item.kind === "user" && !item.guidance)) turns.push({ key, segments: [] });
    const turn = turns[turns.length-1];
    if (item.kind === "user" && !item.guidance) { turn.user = item; continue; }
    const process = ["thinking","tool","sub_agent","system"].includes(item.kind) && !(item.kind === "system" && item.separator);
    const last = turn.segments.at(-1);
    if (process) {
      if (last?.kind === "process") last.items.push(item);
      else turn.segments.push({ kind: "process", key, items: [item] });
    } else turn.segments.push({ kind: "message", key, item });
  }
  return turns;
}
export function processSummary(items: TranscriptItem[], active: boolean): string {
  const tools = items.filter(item => item.kind === "tool");
  const last = tools.at(-1);
  if (active && last?.result === undefined && last) {
    const label = last.name === "Read" ? "正在读取文件" : ["Edit","Write"].includes(last.name) ? "正在修改文件" : last.name === "SubAgent" ? "等待子代理" : last.color === "exec" ? "正在执行命令" : "正在执行工具";
    return `${label} · ${last.summary}`;
  }
  if (active && items.at(-1)?.kind === "thinking") return "正在思考";
  const read = tools.filter(item => item.name === "Read").length;
  const edit = tools.filter(item => ["Edit","Write"].includes(item.name)).length;
  const exec = tools.filter(item => item.color === "exec").length;
  const counts = [read && `读取 ${read} 次`, edit && `编辑 ${edit} 次`, exec && `执行 ${exec} 次`].filter(Boolean);
  return `${active ? '执行中' : '执行过程'}${counts.length ? ' · ' + counts.join('，') : ''}`;
}
