import type { RawEvent, SessionState, ImageLimits, InputReceipt, TodoResource } from "./types";
import { reduceEvent } from "./reducer";
import { conversationToEvents } from "./toolFormat";
export function projectMessages(state: SessionState, messages: Record<string, unknown>[]): SessionState {
  let output: SessionState = { ...state, items: [], seenTurnEvents: [], messages };
  const overlays = [...state.overlays ?? []];
  for (const row of messages) {
    while (overlays.length && Number(overlays[0].after_conversation_seq ?? 0) < Number(row.seq)) output = reduceEvent(output, overlays.shift()!);
    for (const event of conversationToEvents(row)) output = reduceEvent(output, event);
  }
  for (const event of overlays) output = reduceEvent(output, event);
  return { ...output, running: state.running, workState: state.workState, lastSeq: state.lastSeq, seenTurnEvents: state.seenTurnEvents, lastTurnId: state.lastTurnId };
}
export function mergeTodo(previous: TodoResource, data: TodoResource & { changes?: Record<string, unknown>[] }): TodoResource {
  if ((data.revision ?? 0) <= (previous.revision ?? 0)) return previous;
  let items = (previous.items ?? []).map(item => ({...item}));
  for (const change of data.changes ?? []) {
    const id = change.id ?? (change.item as { id?: string })?.id;
    if (change.change === "removed") { items = items.filter(item => item.id !== id); continue; }
    if (change.change === "added") { items.push(change.item as typeof items[number]); continue; }
    const item = items.find(item => item.id === id); if (!item) continue;
    if (change.change === "updated") item.content = String(change.content);
    else if (change.change === "completed") item.status = "completed";
    else if (change.change === "activated") item.status = "in_progress";
    else if (["paused", "reopened"].includes(String(change.change))) item.status = "pending";
  }
  for (const current of data.items ?? []) { const index = items.findIndex(item => item.id === current.id); if (index < 0) items.push(current); else items[index] = current; }
  return { revision: data.revision, items };
}
function recordOutcome(state: SessionState, event: RawEvent): SessionState {
  const outcome = event.outcome as Record<string,unknown> | undefined;
  const id = event.turn_id ?? outcome?.turn_id;
  return outcome && typeof id === "string" ? {...state,outcomes:{...state.outcomes,[id]:outcome}} : state;
}
export function reduceWorkbench(state: SessionState, event: RawEvent): SessionState {
  if (event.type === "session_snapshot") {
    const caps = (event.capabilities as { image_input?: ImageLimits })?.image_input;
    let next = projectMessages({ ...state, lastSeq: Number(event.stream_sequence), generation: String(event.generation), currentTurn: event.current_turn as string | null,
      seenTurnEvents: [], overlays: [], resources: event.resources as SessionState["resources"], running: event.running === true, phase: String(event.phase), desynced: false, inputs: event.inputs as InputReceipt[], imageLimits: caps?.kind === "unsupported" ? null : caps,
      workState: event.running ? "waiting" : "idle" }, event.conversation as Record<string, unknown>[]);
    next.overlays = (event.progress as RawEvent[] ?? []).filter(progress => !["text","thinking"].includes(progress.type));
    next = projectMessages(next, next.messages ?? []);
    for (const progress of (event.progress as RawEvent[] ?? [])) if (["text","thinking"].includes(progress.type)) next = reduceEvent(next, progress);
    if (event.diagnostics) next = reduceEvent(next,event.diagnostics as RawEvent);
    if (event.last_final && !event.running) next = recordOutcome(reduceEvent(next,event.last_final as RawEvent),event.last_final as RawEvent);
    return next;
  }
  if (event.generation !== state.generation || Number(event.stream_sequence) <= state.lastSeq) return state;
  if (event.type === "conversation_committed") {
    const message: Record<string, unknown> = { ...(event.message as Record<string, unknown>), seq: event.conversation_seq };
    const rows = [...state.messages ?? []];
    if (!rows.some(row => row.seq === message.seq)) rows.push(message);
    const next = projectMessages(state, rows);
    next.lastSeq = Number(event.stream_sequence);
    if (next.resources && Array.isArray(message.content)) {
      next.resources = { ...next.resources, plan: {...next.resources.plan}, todo: {...next.resources.todo} };
      for (const block of message.content) {
        if (block._mink?.artifacts) {
          const existing = [...next.resources.artifacts ?? []];
          for (const artifact of block._mink.artifacts) if (!existing.some(row => row.id === artifact.id)) existing.push(artifact);
          next.resources.artifacts = existing;
        }
        const presentation = block._mink?.presentation;
        if (presentation?.kind === "todo") next.resources.todo = mergeTodo(next.resources.todo, presentation.data);
        if (presentation?.kind === "plan") {
          const data = presentation.data;
          if (data.transition === "confirmed") next.resources.plan = {plan:data.content,draft:null};
          if (data.transition === "cleared") next.resources.plan = {plan:null,draft:null};
          if (data.transition === "draft_saved") next.resources.plan.draft = data.content;
          if (data.transition === "draft_cancelled") next.resources.plan.draft = null;
        }
      }
    }
    if (message.role === "user" && typeof message.content === "string") {
      const inputId = (message._mink as { input_id?: string })?.input_id;
      next.inputs = state.inputs?.map(input => input.input_id === inputId ? { ...input, status: "applied" } : input);
    }
    return next;
  }
  if (event.type === "inputs_updated") {
    const applied = new Set((state.messages ?? []).map(row => (row._mink as { input_id?: string })?.input_id));
    return { ...state, inputs: (event.inputs as InputReceipt[]).map(receipt => applied.has(receipt.input_id) ? {...receipt,status:"applied"} : receipt), lastSeq: Number(event.stream_sequence) };
  }
  if (event.type === "phase_updated") return {...state,phase:String(event.phase),desynced:event.phase === "closed" || state.desynced,lastSeq:Number(event.stream_sequence)};
  // Tool records are already in the formal history; live tool events only update activity.
  if (event.type === "tool_call" || event.type === "tool_result") return { ...state, workState: "tool", lastSeq: Number(event.stream_sequence) };
  const next = event.type === "turn_final" ? recordOutcome(reduceEvent(state,event),event) : reduceEvent(state,event);
  if (["error","signal","sub_agent_status","sub_agent_output","retry","compact","stop","turn_error","turn_final"].includes(event.type)) next.overlays = [...state.overlays ?? [], event];
  if (event.type === "turn_started") { next.currentTurn = String(event.turn_id); next.phase = "running"; }
  if (event.type === "turn_final") { next.currentTurn = null; if (!["closing","closed"].includes(state.phase ?? "")) next.phase = "idle"; }
  return next;
}
