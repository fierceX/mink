<script setup lang="ts">
// 思考正文默认折叠，显式选择与内层阅读位置由会话 UI 状态保留。
import { computed } from "vue";
import { viewFor } from "../../lib/workbench";
import { renderMarkdown } from "../../lib/markdown";
import type { ThinkingItem } from "../../lib/types";

const props = defineProps<{ item: ThinkingItem }>();
const html = computed(() => renderMarkdown(props.item?.text ?? ""));
const open = computed(() => viewFor().expanded[`thinking:${String(props.item.key)}`] ?? false);
const onToggle = () => { viewFor().expanded[`thinking:${String(props.item.key)}`] = !open.value; };
</script>

<template>
  <details class="thinking-panel" :open="open" >
    <summary @click.prevent="onToggle">思考过程</summary>
    <div class="tp-body md-body" v-html="html"></div>
  </details>
</template>

<style scoped>
.thinking-panel { align-self: stretch; border: 1px solid var(--line); border-radius: var(--radius); background: var(--panel); overflow: hidden; animation: rise-in 0.14s ease; }
.thinking-panel summary { padding: 8px 14px; cursor: pointer; color: var(--text-dim); font-size: 12px; font-family: var(--mono); list-style: none; user-select: none; }
.thinking-panel summary::before { content: "▾"; margin-right: 8px; opacity: 0.6; }
.thinking-panel:not([open]) summary::before { content: "▸"; }
.thinking-panel summary:hover { color: var(--text-soft); }
.tp-body { border-top: 1px solid var(--line); padding: 10px 14px; color: var(--text-soft); font-size: 13px; max-height: 360px; overflow-y: auto; }
</style>
