<script setup lang="ts">
import { computed } from "vue";
import { renderMarkdown } from "../../../lib/markdown";

const props = defineProps<{ content: string; presentation?: unknown }>();
const plan = computed(() => {
  const value = props.presentation as {kind?:string;data?:{transition?:string;content?:unknown}} | undefined;
  return value?.kind === 'plan' && value.data && ['draft_saved','draft_cancelled','confirmed','cleared'].includes(value.data.transition ?? '') ? value.data : null;
});
const transitionLabels: Record<string,string> = {draft_saved:'计划草稿已保存',draft_cancelled:'计划草稿已取消',confirmed:'计划已确认',cleared:'计划已清除'};
const html = computed(() => renderMarkdown(typeof plan.value?.content === 'string' ? plan.value.content : plan.value ? '' : props.content));
</script>

<template>
  <p v-if="plan" class="plan-transition">{{ transitionLabels[plan.transition!] }}</p>
  <div v-if="html" class="md-body plan-body" v-html="html"></div>
  <details v-if="plan" class="plan-original"><summary>原始结果</summary><pre>{{ content }}</pre></details>
</template>

<style scoped>
.plan-body { font-size: 13px; }
.plan-transition { font-size:12px; color:var(--text-soft); margin-bottom:8px; }.plan-original { margin-top:8px; font-size:11px; color:var(--text-dim); }.plan-original summary { cursor:pointer; }.plan-original pre { white-space:pre-wrap; overflow-wrap:anywhere; font:12px/1.6 var(--mono); }
</style>
