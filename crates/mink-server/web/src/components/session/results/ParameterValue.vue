<script setup lang="ts">
import { computed } from 'vue';
import { parameterLabels } from '../../../lib/toolInput';
const props = withDefaults(defineProps<{ value: unknown; depth?: number }>(), { depth:0 });
const array = computed(() => Array.isArray(props.value) ? props.value : null);
const object = computed(() => props.value && typeof props.value === 'object' && !array.value ? Object.entries(props.value) : null);
</script>
<template>
  <span v-if="depth >= 5 && (array || object)" class="limited">嵌套内容请查看原始参数</span>
  <ul v-else-if="array?.length" class="parameter-list"><li v-for="(entry,index) in array.slice(0,100)" :key="index"><ParameterValue :value="entry" :depth="depth+1" /></li><li v-if="array.length>100" class="limited">其余 {{ array.length-100 }} 项请查看原始参数</li></ul>
  <dl v-else-if="object?.length" class="parameter-fields"><template v-for="[key,entry] in object.slice(0,100)" :key="key"><dt>{{ parameterLabels[key] ?? key }}</dt><dd><ParameterValue :value="entry" :depth="depth+1" /></dd></template><dt v-if="object.length>100" class="limited">其余字段请查看原始参数</dt></dl>
  <span v-else-if="array">空列表</span><span v-else-if="object">无参数</span>
  <span v-else class="parameter-text">{{ value === true ? '是' : value === false ? '否' : value == null ? '—' : String(value) }}</span>
</template>
<style scoped>
.parameter-fields { display:grid; grid-template-columns:minmax(64px,100px) minmax(0,1fr); gap:5px 12px; }.parameter-fields dt,.limited { color:var(--text-dim); font-size:11px; overflow-wrap:anywhere; }.parameter-fields dd { min-width:0; }.parameter-list { list-style:none; display:grid; gap:6px; }.parameter-list>li { border-left:2px solid var(--line); padding-left:8px; }.parameter-text { white-space:pre-wrap; overflow-wrap:anywhere; }.limited { font-style:italic; }
@media(max-width:767px) { .parameter-fields { grid-template-columns:minmax(0,1fr); gap:3px; }.parameter-fields dd:not(:last-child) { margin-bottom:5px; } }
</style>
