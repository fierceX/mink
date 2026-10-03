<script setup lang="ts">
import { computed } from "vue";
import { parsePatch } from "../../../lib/toolFormat";
import { decodeToolInput } from "../../../lib/toolInput";

const props = defineProps<{ input: string }>();

type ResolvedInput = { patch?: string; path?: string; input?: string; edits?: unknown };
const resolved = computed<ResolvedInput>(() => {
  const obj = decodeToolInput(props.input);
  if (obj) return obj;
  // 新协议：input 是 "[PATH#TAG]\nPUT..."；旧协议：patch 是 "@path#tag\nreplace..."
  return { patch: String(props.input) };
});
const patchText = computed(() => resolved.value.input ?? resolved.value.patch ?? "");
const patch = computed(() => parsePatch(patchText.value));
const displayPath = computed(() => patch.value?.path || resolved.value.path || "");
const replacements = computed(() => {
  const edits = resolved.value.edits;
  return Array.isArray(edits) && edits.length && edits.every(edit => edit && typeof edit.old_text === 'string' && typeof edit.new_text === 'string' && (edit.all == null || typeof edit.all === 'boolean')) ? edits as {old_text:string;new_text:string;all?:boolean}[] : null;
});
</script>

<template>
  <div v-if="replacements" class="e-replacements">
    <div class="e-path">{{ displayPath }}</div>
    <section v-for="(edit,index) in replacements" :key="index" class="e-replacement"><div class="e-operation">替换 {{ index+1 }} · {{ edit.all ? '全部匹配' : '单处匹配' }}</div><div class="e-pair"><div><small>旧文本</small><pre class="e-before">{{ edit.old_text || '（空文本）' }}</pre></div><div><small>新文本</small><pre class="e-after">{{ edit.new_text || '（空文本）' }}</pre></div></div></section>
  </div>
  <div v-else-if="patch" class="e-lines">
    <div v-if="displayPath" class="e-path">{{ displayPath }}<span v-if="patch.tag" class="e-tag"> #{{ patch.tag }}</span></div>
    <div v-for="(line, i) in patch.lines" :key="i" class="e-line" :class="`e-${line.op}`">
      <span v-if="['replace', 'insert', 'delete', 'append', 'hunk', 'put', 'cut'].includes(line.op)" class="e-hunk" :class="`e-hunk-${line.op}`">@@ {{ line.op === 'put' ? 'PUT' : line.op === 'cut' ? 'CUT' : line.op }} {{ line.range }} @@</span>
      <span v-else-if="line.op === 'head'" class="e-head">{{ line.content }}</span>
      <template v-else-if="line.op === 'add'"><span class="e-sign add">+</span><span class="e-content">{{ line.content }}</span></template>
      <template v-else-if="line.op === 'del'"><span class="e-sign del">-</span><span class="e-content">{{ line.content }}</span></template>
      <span v-else class="e-content">{{ line.content }}</span>
    </div>
  </div>
  <pre v-else class="e-raw">{{ input || "（等待输入同步）" }}</pre>
  <details v-if="replacements || patch" class="e-original"><summary>原始参数</summary><pre>{{ input }}</pre></details>
</template>

<style scoped>
.e-path { font-family: var(--mono); font-size: 12px; color: var(--blue); margin-bottom: 6px; word-break: break-all; }
.e-tag { color: var(--text-dim); }
.e-lines { display: flex; flex-direction: column; gap: 1px; }
.e-line { font-family: var(--mono); font-size: 12px; line-height: 1.5; }
.e-hunk { display: inline-block; font-family: var(--mono); font-size: 11.5px; font-weight: 700; color: var(--blue); background: rgba(47, 111, 237, 0.07); border-radius: 4px; padding: 0 7px; margin: 2px 0; }
.e-hunk-insert { color: var(--green); background: rgba(23, 154, 97, 0.07); }
.e-hunk-append { color: var(--green); background: rgba(23, 154, 97, 0.07); }
.e-hunk-delete { color: var(--red); background: rgba(214, 69, 93, 0.07); }
.e-head { color: var(--text-dim); font-size: 11px; }
.e-content { color: var(--text-soft); }
.e-sign { font-weight: 700; margin-right: 4px; display: inline-block; width: 10px; }
.e-sign.add { color: var(--green); }
.e-sign.del { color: var(--red); }
.e-add { background: rgba(23, 154, 97, 0.05); border-radius: 3px; padding: 0 4px; }
.e-del { background: rgba(214, 69, 93, 0.06); border-radius: 3px; padding: 0 4px; }
.e-raw { margin: 0; white-space: pre-wrap; font-family: var(--mono); font-size: 12px; color: var(--text-soft); }
.e-replacements { display:grid; gap:10px; }.e-operation,.e-pair small,.e-original { font-size:11px; color:var(--text-dim); }.e-pair { display:grid; grid-template-columns:minmax(0,1fr) minmax(0,1fr); gap:8px; }.e-pair pre,.e-original pre { white-space:pre-wrap; overflow-wrap:anywhere; font:12px/1.6 var(--mono); padding:8px; border-radius:6px; }.e-before { background:color-mix(in srgb,var(--red) 8%,transparent); }.e-after { background:color-mix(in srgb,var(--green) 8%,transparent); }.e-original { margin-top:10px; }.e-original summary { cursor:pointer; }@media(max-width:767px) { .e-pair { grid-template-columns:minmax(0,1fr); } }
</style>
