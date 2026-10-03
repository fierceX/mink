<script setup lang="ts">
import { computed } from "vue";
import { parseTodoContent, classifyTodoLine, parseChanges, formatChanges } from "../../../lib/toolFormat";
import { renderMarkdown } from "../../../lib/markdown";

const props = defineProps<{ content: string; presentation?: unknown }>();
const blocks = computed(() => parseTodoContent(props.content));
const changes = computed(() => formatChanges(parseChanges(props.presentation)));
const hasBlocks = computed(() => blocks.value.length > 0);
type TodoItem = {id:string;content:string;status:string};
const todo = computed(() => {
  const value = props.presentation as {kind?:string;data?:{revision?:number;counts?:{pending:number;in_progress:number;completed:number};items?:TodoItem[];changes?:{change:string;id?:string;content?:string;item?:TodoItem}[]}} | undefined;
  const data = value?.data;
  if (value?.kind !== 'todo' || !data || typeof data.revision !== 'number' || !Array.isArray(data.items) || !data.items.every(item => typeof item?.id === 'string' && typeof item.content === 'string' && ['pending','in_progress','completed'].includes(item.status))) return null;
  return { ...data, counts: data.counts && ['pending','in_progress','completed'].every(key => typeof (data.counts as Record<string,unknown>)[key] === 'number') ? data.counts : null, changes: Array.isArray(data.changes) ? data.changes.filter(change => typeof change?.change === 'string') : [] };
});
const statusLabels: Record<string,string> = {pending:'待办',in_progress:'进行中',completed:'已完成'};
const changeLabels: Record<string,string> = {added:'新增',updated:'修改',removed:'移除',completed:'完成',activated:'激活',paused:'暂停',reopened:'重开'};

const lineSymbol = (line: string) =>
  line
    .replace(/^- added /, "＋ added ")
    .replace(/^- updated /, "～ updated ")
    .replace(/^- removed /, "－ removed ")
    .replace(/^Completed:/, "✓ Completed:")
    .replace(/^Activated:/, "◉ Activated:")
    .replace(/^Paused:/, "○ Paused:")
    .replace(/^Reopened:/, "↻ Reopened:");
</script>

<template>
  <div v-if="changes" class="t-changes">{{ changes }}</div>
  <div v-if="todo" class="todo-presentation">
    <div class="t-head-block"><span class="t-meta">revision {{ todo.revision }}</span><span v-if="todo.counts" class="t-meta">待办 {{ todo.counts.pending }} · 进行中 {{ todo.counts.in_progress }} · 已完成 {{ todo.counts.completed }}</span></div>
    <div class="t-tasks"><div v-for="task in todo.items" :key="task.id" class="t-task" :class="{done:task.status==='completed'}"><span class="t-status" :class="task.status">{{ statusLabels[task.status] }}</span><span class="t-task-text">{{ task.id }}: {{ task.content }}</span></div></div>
    <p v-if="!todo.items?.length && !todo.changes?.length" class="t-note">没有待办项</p>
    <div v-for="(change,index) in todo.changes" :key="index" class="todo-change">{{ changeLabels[change.change] ?? change.change }} {{ change.item?.id ?? change.id }}<span v-if="change.item?.content || change.content"> · {{ change.item?.content ?? change.content }}</span></div>
    <details class="todo-original"><summary>原始结果</summary><pre>{{ content }}</pre></details>
  </div>
  <template v-else-if="hasBlocks">
    <div v-for="(block, bi) in blocks" :key="bi">
      <!-- snapshot / current：revision + counts 头 -->
      <div v-if="block.kind === 'snapshot' || block.kind === 'current'" class="t-head-block">
        <span v-if="block.revision !== undefined" class="t-meta">revision {{ block.revision }}</span>
        <span v-if="block.counts" class="t-meta">{{ block.counts.pending }} pending · {{ block.counts.in_progress }} in_progress · {{ block.counts.completed }} completed</span>
      </div>
      <!-- event：变更行（着色） -->
      <div v-if="block.kind === 'event'" class="t-event">
        <div v-for="(line, i) in block.lines" :key="i" class="t-event-line" :class="`t-ev-${classifyTodoLine(line)}`">{{ lineSymbol(line) }}</div>
      </div>
      <!-- note（current-todos 提示） -->
      <div v-if="block.note" class="t-note">{{ block.note }}</div>
      <!-- 任务列表 -->
      <div v-if="block.tasks.length > 0" class="t-tasks">
        <div v-for="task in block.tasks" :key="task.id" class="t-task" :class="{ done: task.status === 'completed' }">
          <span class="t-status" :class="task.status">{{ task.status }}</span>
          <span class="t-task-text">{{ task.id }}: {{ task.text }}</span>
        </div>
      </div>
    </div>
  </template>
  <div v-else class="md-body" v-html="renderMarkdown(content)"></div>
</template>

<style scoped>
.t-changes { font-size: 11px; font-family: var(--mono); color: var(--text-dim); margin-bottom: 6px; }
.t-head-block { display: flex; gap: 12px; flex-wrap: wrap; margin-bottom: 6px; }
.t-meta { font-size: 11px; font-family: var(--mono); color: var(--text-dim); }
.t-tasks { display: flex; flex-direction: column; gap: 3px; }
.t-task { display: flex; gap: 8px; align-items: baseline; font-size: 12.5px; }
.t-task.done { opacity: 0.55; }
.t-task.done .t-task-text { text-decoration: line-through; }
.t-status { font-family: var(--mono); font-size: 10.5px; min-width: 48px; text-align: center; border-radius: 999px; padding: 1px 6px; flex-shrink: 0; }
.t-status.pending { background: rgba(181, 122, 28, 0.12); color: var(--yellow); }
.t-status.in_progress { background: var(--blue-soft); color: var(--blue); }
.t-status.completed { background: rgba(23, 154, 97, 0.1); color: var(--green); }
.t-status.active { background: var(--blue-soft); color: var(--blue); }
.t-event { display: flex; flex-direction: column; gap: 2px; }
.t-event-line { font-family: var(--mono); font-size: 12px; white-space: pre-wrap; }
.t-ev-add { color: var(--green); }
.t-ev-update { color: var(--yellow); }
.t-ev-remove { color: var(--red); }
.t-ev-label { color: var(--text-soft); font-weight: 600; }
.t-note { font-size: 12px; color: var(--text-dim); font-style: italic; margin: 4px 0; }
.t-raw { margin: 0; white-space: pre-wrap; font-family: var(--mono); font-size: 12px; color: var(--text-soft); }
.t-task-text { overflow-wrap:anywhere; min-width:0; }.todo-change { font-size:12px; color:var(--text-soft); margin-top:5px; overflow-wrap:anywhere; }.todo-original { margin-top:8px; font-size:11px; color:var(--text-dim); }.todo-original summary { cursor:pointer; }.todo-original pre { white-space:pre-wrap; overflow-wrap:anywhere; font:12px/1.6 var(--mono); }
</style>
