<script setup lang="ts">
// ToolCard：容器（着色/头部/折叠）+ 按 view 分发结果组件
import { computed } from "vue";
import { viewFor, openDetail } from "../../lib/workbench";
import CommandResult from "./results/CommandResult.vue";
import FileResult from "./results/FileResult.vue";
import SearchResult from "./results/SearchResult.vue";
import TodoResult from "./results/TodoResult.vue";
import PlanResult from "./results/PlanResult.vue";
import DiffResult from "./results/DiffResult.vue";
import TextResult from "./results/TextResult.vue";
import EditCall from "./results/EditCall.vue";
const props = defineProps<{ item: any }>();
// 工具卡片始终折叠（头部 summary 展示核心参数），仅手动点击展开详情
const open = computed(() => viewFor().expanded[`tool:${String(props.item.key ?? props.item.id)}`] ?? false);
const onToggle = () => { viewFor().expanded[`tool:${String(props.item.key ?? props.item.id)}`] = !open.value; };
const openArtifact = () => { if (props.item.artifact) openDetail("outputs", props.item.artifact); };
const openFile = () => {
  try { const input = JSON.parse(props.item.input); const path = input.path ?? (input.input ?? input.patch)?.match(/^\[(.*?)(?:#|\])/m)?.[1]; if (path) { const match = String(path).match(/^(.*?)(?::(\d+)(?:[-+].*)?)?$/); if (match) openDetail("files", match[1], Number(match[2] ?? 1)); } } catch { /* no verified path in legacy inputs */ }
};

const resultComp = computed(() => {
  const v = props.item.view;
  if (v === "todo") return TodoResult;
  if (v === "plan") return PlanResult;
  if (v === "diff") return DiffResult;
  if (v === "command") return CommandResult;
  if (v === "file") return FileResult;
  if (v === "search") return SearchResult;
  return TextResult;
});
</script>
<template>
  <details class="tool-card" :class="[`tc-${item.color}`, { 'tc-failed': item.failed }]" :open="open" >
    <summary class="t-head" @click.prevent="onToggle">
      <span class="t-name">{{ item.name }}</span>
      <span v-if="item.summary" class="t-summary" :title="item.summary">{{ item.summary }}</span>
      <span class="t-status">{{ item.result === undefined ? "执行中" : item.success === undefined ? "状态未记录" : item.success ? "完成" : "失败" }}</span>
      <span v-if="item.resultKind" class="t-kind">{{ item.resultKind }}</span>
    </summary>
    <div class="t-body">
      <button v-if="['Read','Write','Edit'].includes(item.name)" @click="openFile">查看当前文件</button>
      <!-- Edit/diff：结构化 patch 优先（input 解析 hunk + path/tag），result 作为执行结果附后 -->
      <template v-if="item.view === 'diff'">
        <EditCall :input="item.input" />
        <div v-if="item.result !== undefined" class="t-result" :class="{ ok: !item.failed, err: item.failed }">
          <component :is="resultComp" :content="item.result" :exit-code="item.exitCode" :summary="item.summary" :presentation="item.presentation" />
          <button v-if="item.artifact" class="t-artifact" @click="openArtifact">artifact://{{ item.artifact }}</button>
        </div>
      </template>
      <template v-else-if="item.result !== undefined">
        <div class="t-result" :class="{ ok: !item.failed, err: item.failed }">
          <component :is="resultComp" :content="item.result" :exit-code="item.exitCode" :summary="item.summary" :presentation="item.presentation" />
          <button v-if="item.artifact" class="t-artifact" @click="openArtifact">artifact://{{ item.artifact }}</button>
        </div>
      </template>
      <pre v-else class="t-call">{{ item.input }}</pre>
    </div>
  </details>
</template>

<style scoped>
.tool-card {
  align-self: stretch;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  background: var(--panel);
  overflow: hidden;
  border-left-width: 3px;
  animation: rise-in 0.14s ease;
  box-shadow: var(--shadow);
}
.tool-card.tc-failed { border-color: rgba(214, 69, 93, 0.4); }
.tc-exec { border-left-color: var(--green); }
.tc-file { border-left-color: var(--blue); }
.tc-search { border-left-color: var(--cyan); }
.tc-todo { border-left-color: var(--purple); }
.tc-plan { border-left-color: var(--orange); }
.tc-delegate { border-left-color: var(--pink); }
.tc-tool { border-left-color: var(--yellow); }

.t-head {
  display: flex; align-items: center; justify-content: flex-start; gap: 9px;
  width: 100%; padding: 8px 14px;
  background: none; border: none; cursor: pointer;
  font-family: var(--mono); font-size: 12px; text-align: left; list-style: none;
}
.t-head::-webkit-details-marker { display: none; }
.t-head::before { content: "▾"; margin-right: 2px; opacity: 0.5; }
.tool-card:not([open]) .t-head::before { content: "▸"; }
.t-head:hover { background: var(--panel-2); }
.t-name { font-weight: 600; flex-shrink: 0; margin-right: auto; }
.tc-exec .t-name { color: var(--green); }
.tc-file .t-name { color: var(--blue); }
.tc-search .t-name { color: var(--cyan); }
.tc-todo .t-name { color: var(--purple); }
.tc-plan .t-name { color: var(--orange); }
.tc-delegate .t-name { color: var(--pink); }
.tc-tool .t-name { color: var(--yellow); }
.t-summary { color: var(--text-dim); font-size: 11px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; }
.t-kind { font-size: 9px; letter-spacing: 0.08em; background: var(--panel-3); border: 1px solid var(--line); border-radius: 4px; padding: 1px 5px; color: var(--text-dim); flex-shrink: 0; }
.t-body { border-top: 1px solid var(--line); padding: 10px 14px; }
.t-call { color: var(--text-soft); white-space: pre-wrap; font-family: var(--mono); font-size: 12px; margin: 0; }
.t-result { font-size: 12.5px; }
.t-result.ok { border-left: 3px solid rgba(23, 154, 97, 0.5); padding-left: 11px; }
.t-result.err { border-left: 3px solid rgba(214, 69, 93, 0.6); padding-left: 11px; color: var(--red); }
.t-artifact { margin-top: 8px; background: none; border: none; color: var(--blue); font-family: var(--mono); font-size: 12px; padding: 0; }
</style>

<style scoped>
.tool-card { border:0; border-left:0; box-shadow:none; background:transparent; border-radius:4px; }.t-head { padding:6px 8px; font-size:11px; }.t-body { max-height:320px; overflow:auto; }.t-status { margin-left:auto; font-size:10px; color:var(--text-dim); white-space:nowrap; }
</style>
