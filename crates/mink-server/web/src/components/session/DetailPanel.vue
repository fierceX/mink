<script setup lang="ts">
import { computed, ref, watch, onBeforeUnmount, nextTick } from "vue";
import { appState, uiState } from "../../lib/store";
import { api, type FileItem } from "../../lib/api";
import { identity, viewFor, flash } from "../../lib/workbench";
import { renderMarkdown } from "../../lib/markdown";
import { mergeTodo } from "../../lib/workbenchReducer";
import { fmtK } from "../../lib/fmt";
const view = computed(() => viewFor()); const state = computed(() => appState.sessionState);
const plan = ref<{ plan: string | null; draft: string | null }>({ plan: null, draft: null });
const todos = ref<{ revision?: number; items?: { id: string; status: string; content: string }[] }>({});
const artifacts = ref<{ id: string; tool?: string }[]>([]); const files = ref<FileItem[]>([]);
const directory = ref(""); const content = ref(""); const title = ref(""); const markdown = ref(false); const error = ref("");
const verifiedFiles = ref<string[]>([]);
const scroll = ref<HTMLElement | null>(null); let revision = 0;
async function load(path?: string, artifact = false) {
  const id = state.value?.sessionId; const project = appState.currentProjectKey!; const key = identity(project,id); if (!id) return;
  const token = ++revision; error.value = "";
  try {
    const tab = view.value.detailTab;
    if (tab === "task" && state.value?.resources) { plan.value = state.value.resources.plan; todos.value = state.value.resources.todo; }
    if (tab === "task") {
      const [p,t] = await Promise.all([api.plan(id,project),api.todo(id,project)]);
      if (identity() !== key || token !== revision) return;
      if (p.code !== 200 || t.code !== 200) throw new Error(p.message || t.message);
      plan.value = p.data; todos.value = (t.data.todos ?? {}) as typeof todos.value;
    } else if (tab === "outputs") {
      const outputPath = path ?? view.value.detailPath;
      if (outputPath) { const response = await api.artifact(id,outputPath,project); if (identity() !== key || token !== revision) return; if (response.code !== 200) throw new Error(response.message); content.value = response.data.content; title.value = `工具输出 Artifact · ${outputPath}`; markdown.value = false; }
      else { const response = await api.artifacts(id,project); if (identity() !== key || token !== revision) return; if (response.code !== 200) throw new Error(response.message); artifacts.value = response.data.artifacts;
        const paths = [...new Set(outputFiles.value)].slice(-32);
        const checks = await Promise.allSettled(paths.map(file => api.files(id,file,true,project)));
        if (identity() !== key || token !== revision) return;
        verifiedFiles.value = paths.filter((_,index) => checks[index].status === "fulfilled" && (checks[index] as PromiseFulfilledResult<Awaited<ReturnType<typeof api.files>>>).value.code === 200);
      }
    } else if (tab === "files") {
      const target = path ?? view.value.detailPath;
      const response = await api.files(id,target,!!target && !target.endsWith('/'),project);
      if (identity() !== key || token !== revision) return;
      if (response.code !== 200) throw new Error(response.message);
      if (response.data.items) { files.value = response.data.items; directory.value = target.replace(/\/$/,''); }
      else { content.value = response.data.content ?? ""; title.value = target; markdown.value = /\.(md|markdown)$/i.test(target); }
    }
    await nextTick(); if (scroll.value) scroll.value.scrollTop = view.value.detailScroll;
    if (view.value.detailTab === "files" && view.value.detailLine > 1) { await nextTick(); scroll.value?.querySelector(`[data-line="${view.value.detailLine}"]`)?.scrollIntoView({ block: "center" }); }
  } catch(e) { if (identity() === key && token === revision) error.value = String(e); }
}
watch(() => [identity(),view.value.detailTab,view.value.detailPath], () => { content.value = ""; title.value = ""; void load(); }, { immediate: true });
watch(() => state.value?.running, (running,previous) => { if (!running && previous) void load(); });
watch(() => state.value?.items, items => {
  for (const item of items ?? []) {
    if (item.kind !== "tool" || !item.presentation) continue;
    const presentation = item.presentation as { kind?: string; data?: typeof todos.value & { plan?: string; content?: string; state?: string } };
    if (presentation.kind === "todo" && presentation.data) todos.value = mergeTodo(todos.value, presentation.data);
  }
});
watch(() => state.value?.resources, resources => { if (resources) { plan.value = resources.plan; todos.value = resources.todo; artifacts.value = resources.artifacts ?? []; } }, { immediate: true });
onBeforeUnmount(() => { ++revision; });
function selectFile(name: string, dir: boolean) { view.value.detailPath = [directory.value,name].filter(Boolean).join('/') + (dir ? '/' : ''); view.value.detailLine = 1; }
const outputFiles = computed(() => state.value?.items.filter(item => item.kind === "tool" && item.success === true && ["Write","Edit"].includes(item.name)).map(item => item.kind === "tool" ? item.summary : '') ?? []);
const turnUsage = computed(() => state.value?.outcomes?.[view.value.detailTurn]?.usage as { request_count?: number; reported_request_count?: number; unreported_request_count?: number; tokens?: {input_tokens:number;output_tokens:number;cache_read_tokens:number} } | undefined);
const copyPath = async () => { try { await navigator.clipboard.writeText(title.value); flash("已复制路径"); } catch(e) { flash(`复制失败：${String(e)}`); } };
</script>
<template>
  <aside class="detail-panel" aria-label="会话详情">
    <header><button v-for="tab in (['task','files','outputs','diagnostics'] as const)" :key="tab" :class="{ selected: view.detailTab === tab }" @click="view.detailPath = ''; view.detailTurn = ''; view.detailTab = tab">{{ { task:'任务',files:'文件',outputs:'输出',diagnostics:'诊断' }[tab] }}</button><button class="close" @click="uiState.ctxOpen = false" aria-label="关闭详情">×</button></header>
    <div ref="scroll" class="detail-body" @scroll="view.detailScroll = ($event.target as HTMLElement).scrollTop">
      <p v-if="error" role="alert" class="error">{{ error }}</p>
      <template v-if="view.detailTab === 'task'">
        <h3>当前计划 <small>{{ plan.plan ? '已确认' : plan.draft ? '草稿' : '' }}</small></h3><div class="md-body" v-if="plan.plan || plan.draft" v-html="renderMarkdown(plan.plan ?? plan.draft ?? '')"></div><p v-else class="hint">尚无计划</p>
        <h3>Todo <small v-if="todos.revision">revision {{ todos.revision }}</small></h3><ul class="todos"><li v-for="item in todos.items" :key="item.id"><span :class="item.status">{{ item.status === 'completed' ? '✓' : item.status === 'in_progress' ? '◉' : '○' }}</span>{{ item.content }}</li></ul><p v-if="!todos.items?.length" class="hint">尚无待办</p><p class="hint">计划与 Todo 为只读。调整要求可写入输入区。</p><button @click="load()">刷新</button>
      </template>
      <template v-else-if="view.detailTab === 'files'">
        <p class="hint">当前磁盘内容</p><div class="file-ops"><button @click="view.detailPath = ''; load('')">根目录</button><button @click="load()">刷新</button><button v-if="title" @click="copyPath">复制路径</button></div>
        <div class="file-tree"><button v-for="file in files" :key="file.name" @click="selectFile(file.name,file.dir)">{{ file.dir ? '▸' : '·' }} {{ file.name }}</button></div>
        <h4 v-if="title">{{ title }}</h4><label v-if="title && /\.(md|markdown)$/i.test(title)"><input type="checkbox" v-model="markdown" />Markdown 预览</label>
        <div v-if="title && markdown" class="md-body" v-html="renderMarkdown(content)"></div><pre v-else-if="title" class="file-content"><span v-for="(line,index) in content.split('\n')" :key="index" :data-line="index+1"><em>{{ index+1 }}</em>{{ line }}{{ '\n' }}</span></pre>
      </template>
      <template v-else-if="view.detailTab === 'outputs'">
        <p class="hint">Artifact 是当时执行的工具输出。</p><button v-for="artifact in artifacts" :key="artifact.id" class="output-link" @click="load(artifact.id,true)">artifact://{{ artifact.id }} · {{ artifact.tool }}</button>
        <h3 v-if="verifiedFiles.length">当前确认存在的文件引用</h3><button v-for="path in verifiedFiles" :key="path" @click="view.detailTab='files'; view.detailPath=path">{{ path }}</button><h4>{{ title }}</h4><pre v-if="content" class="artifact-content">{{ content }}</pre>
      </template>
      <template v-else>
        <section v-if="view.detailTurn" class="turn-usage"><h3>本轮用量</h3><p v-if="!turnUsage" class="hint">本轮用量未记录，或仍在运行。</p><template v-else><p>请求 {{ turnUsage.request_count }}</p><p v-if="turnUsage.reported_request_count && turnUsage.tokens">已报告输入 / 输出：{{ fmtK(turnUsage.tokens.input_tokens) }} / {{ fmtK(turnUsage.tokens.output_tokens) }} · 缓存 {{ fmtK(turnUsage.tokens.cache_read_tokens) }}</p><p v-if="turnUsage.unreported_request_count" class="hint">{{ turnUsage.unreported_request_count }} 次请求未报告 Token，用量仅计已报告部分。</p></template><hr /></section>
        <h3>会话诊断</h3><dl><dt>模型</dt><dd>{{ state?.model || '未记录' }}</dd><dt>输入 / 输出</dt><dd>{{ fmtK(state?.tokensIn ?? 0) }} / {{ fmtK(state?.tokensOut ?? 0) }}</dd><dt>缓存</dt><dd>{{ fmtK(state?.cacheReadTokens ?? 0) }}</dd><dt>上下文</dt><dd>{{ fmtK(state?.contextTokens ?? 0) }}</dd><dt>信念</dt><dd>{{ state?.belief || '未记录' }}</dd><dt>运行阶段</dt><dd>{{ state?.phase }}</dd></dl>
        <p v-for="(item,index) in state?.items.filter(item => item.kind === 'error' || item.kind === 'signal')" :key="index">{{ 'text' in item ? item.text : '' }}</p>
      </template>
    </div>
  </aside>
</template>
<style scoped>
.detail-panel { width:var(--detail-width,360px); flex-shrink:0; border-left:1px solid var(--line); background:var(--bg-elevated); min-height:0; display:flex; flex-direction:column; }
header { display:flex; gap:4px; padding:8px; border-bottom:1px solid var(--line); }header button { padding:4px 8px; border:0; }.selected { color:var(--blue); background:var(--blue-soft); }.close { margin-left:auto; }
.detail-body { overflow:auto; flex:1; padding:16px; font-size:13px; min-width:0; }h3 { font-size:13px; margin:10px 0; }h4 { font-size:11px; overflow-wrap:anywhere; margin:12px 0; }small,.hint { color:var(--text-dim); font-size:11px; }.hint { margin:8px 0; }.error { color:var(--red); }.todos { list-style:none; }.todos li { display:flex; gap:8px; margin:8px 0; }.completed { color:var(--green); }.in_progress { color:var(--blue); }
.file-ops { display:flex; gap:5px; }.file-tree { display:grid; margin:10px 0; max-height:180px; overflow:auto; }.file-tree button,.output-link { text-align:left; border:0; font-size:12px; overflow-wrap:anywhere; }.output-link { display:block; width:100%; }
pre { font:11px/1.7 var(--mono); white-space:pre-wrap; overflow-wrap:anywhere; }.file-content em { display:inline-block; width:32px; color:var(--text-dim); font-style:normal; user-select:none; }dl { display:grid; grid-template-columns:80px 1fr; gap:10px; }dt { color:var(--text-dim); }
@media(max-width:1023px) { .detail-panel { position:absolute; inset:0 0 0 auto; z-index:40; box-shadow:var(--shadow); max-width:100%; } }
@media(max-width:767px) { .detail-panel { width:100%; }.detail-body { padding-bottom:max(16px,env(safe-area-inset-bottom)); } }
</style>
