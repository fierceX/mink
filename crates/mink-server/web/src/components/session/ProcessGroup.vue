<script setup lang="ts">
import { computed } from "vue";
import type { TranscriptItem } from "../../lib/types";
import { preferences, viewFor } from "../../lib/workbench";
import { processSummary } from "../../lib/transcriptProjection";
import ToolCard from "./ToolCard.vue";
import ThinkingBlock from "./ThinkingBlock.vue";
const props = defineProps<{ items: TranscriptItem[]; groupKey: string; active: boolean }>();
const manual = computed(() => viewFor().expanded[`process:${props.groupKey}`] !== undefined);
const open = computed(() => manual.value ? viewFor().expanded[`process:${props.groupKey}`] : preferences.process === "detailed" || (props.active && preferences.process === "standard"));
const visible = computed(() => props.active && preferences.process === "standard" && !manual.value ? props.items.slice(-3) : props.items);
const toggle = () => { viewFor().expanded[`process:${props.groupKey}`] = !open.value; };
</script>
<template>
  <details class="process-group" :open="open">
    <summary @click.prevent="toggle"><span>{{ open ? '▾' : '▸' }}</span>{{ processSummary(items, active) }}<small>{{ items.length }} 步</small></summary>
    <div class="process-content"><button v-if="visible.length < items.length" @click="viewFor().expanded[`process:${groupKey}`] = true">查看全部步骤</button>
      <div v-for="(item,index) in visible" :key="item.key ?? index" :data-item-key="item.key">
        <ToolCard v-if="item.kind === 'tool'" :item="item" />
        <ThinkingBlock v-else-if="item.kind === 'thinking'" :item="item" />
        <p v-else-if="item.kind === 'sub_agent'">子代理 {{ item.sessionId }} · {{ item.status }}<br>{{ item.text }}</p>
        <details v-else-if="'text' in item"><summary>上下文记录</summary><pre>{{ item.text }}</pre></details>
      </div>
    </div>
  </details>
</template>
<style scoped>
.process-group { border:1px solid var(--line); border-radius:8px; background:var(--bg-elevated); font-size:12px; }summary { display:flex; align-items:center; gap:8px; list-style:none; cursor:pointer; padding:9px 12px; color:var(--text-soft); overflow-wrap:anywhere; }summary small { margin-left:auto; white-space:nowrap; color:var(--text-dim); }.process-content { padding:4px 10px 10px; display:grid; gap:6px; max-height:440px; overflow:auto; overscroll-behavior:contain; }.process-content p { font-size:12px; color:var(--text-dim); overflow-wrap:anywhere; }pre { white-space:pre-wrap; overflow-wrap:anywhere; font-size:11px; }
</style>
