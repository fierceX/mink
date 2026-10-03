<script setup lang="ts">
import { computed } from 'vue';
import { decodeToolInput } from '../../../lib/toolInput';
import { renderMarkdown } from '../../../lib/markdown';
import ParameterValue from './ParameterValue.vue';
const props = withDefaults(defineProps<{ name:string; input:unknown; showBody?:boolean }>(), { showBody:true });
const args = computed(() => decodeToolInput(props.input));
const bodyKey = computed(() => props.name === 'Bash' ? 'command' : ['Python','PythonSandbox'].includes(props.name) ? 'script' : ['Write','PlanDraft'].includes(props.name) ? 'content' : props.name === 'SubAgent' ? 'prompt' : null);
const body = computed(() => bodyKey.value && typeof args.value?.[bodyKey.value] === 'string' ? String(args.value[bodyKey.value]) : null);
const markdown = computed(() => ['PlanDraft','SubAgent'].includes(props.name));
const fields = computed(() => Object.fromEntries(Object.entries(args.value ?? {}).filter(([key]) => body.value == null || key !== bodyKey.value)));
const raw = computed(() => typeof props.input === 'string' ? props.input : JSON.stringify(props.input,null,2));
</script>
<template>
  <section v-if="args" class="tool-input" aria-label="调用参数">
    <ParameterValue v-if="Object.keys(fields).length" :value="fields" />
    <div v-if="body != null && showBody" class="input-content"><small>{{ name==='Bash' ? '命令' : ['Python','PythonSandbox'].includes(name) ? 'Python 脚本' : name==='SubAgent' ? '子任务' : name==='PlanDraft' ? '计划草稿' : '写入内容' }}</small><div v-if="markdown" class="md-body" v-html="renderMarkdown(body)"></div><pre v-else>{{ body || '（空内容）' }}</pre></div>
    <p v-else-if="body == null && !Object.keys(fields).length" class="empty-parameters">无参数</p>
    <details class="raw-input"><summary>原始参数</summary><pre>{{ raw }}</pre></details>
  </section>
  <pre v-else class="legacy-input">{{ input }}</pre>
</template>
<style scoped>
.tool-input { display:grid; gap:10px; font-size:12px; }.input-content small,.empty-parameters { color:var(--text-dim); font-size:11px; }.input-content pre { padding:8px 10px; border-radius:7px; background:var(--panel-2); }pre { white-space:pre-wrap; overflow-wrap:anywhere; font:12px/1.65 var(--mono); }.raw-input { color:var(--text-dim); font-size:11px; }.raw-input summary { cursor:pointer; }.raw-input pre { color:var(--text-soft); padding-top:8px; }.legacy-input { color:var(--text-soft); }
</style>
