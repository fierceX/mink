<script setup lang="ts">
import { computed, ref } from "vue";
import { appState, uiState, workspaces } from "../lib/store";
import { api, type SessionSummary } from "../lib/api";
import { openSession, closeSessionView } from "../lib/sessionController";
import { flash } from "../lib/workbench";
import { DropdownMenuItem } from "reka-ui";
import { Home, X } from "@lucide/vue";
import { matchesSession, projectName, type FilterScope } from "../lib/catalogFilter";
import ActionMenu from "./ActionMenu.vue";
const emit = defineEmits<{ close: []; selected: [] }>();
const removing = ref<string | null>(null);
const query = ref("");
const searchInput = ref<HTMLInputElement | null>(null);
const clearSearch = () => { query.value = ''; searchInput.value?.focus({ preventScroll: true }); };
const scope = ref<FilterScope>('all');
const placeholder = computed(() => ({ all: '搜索项目或会话…', project: '搜索项目名称…', session: '标题、别名或 ID…', path: '搜索工作目录…' })[scope.value]);
const deleting = ref<string | null>(null);
const errors = ref<Record<string, string>>({});
const key = (row: SessionSummary) => JSON.stringify([row.project_key, row.id]);
const groups = computed(() => workspaces().map(project => ({ ...project, sessions: project.sessions.filter(row => matchesSession(row, query.value, scope.value)) })).filter(project => project.sessions.length));
const home = () => { closeSessionView(); emit("selected"); };
const open = async (row: SessionSummary) => { appState.currentWorkspace = row.cwd; try { await openSession(row); emit("selected"); } catch(error) { flash(String(error)); } };
const remove = async (row: SessionSummary) => {
  const identity = key(row); if (removing.value) return; removing.value = identity; delete errors.value[identity];
  try { const response = await api.deleteSession(row.id, row.project_key); if (response.code !== 200) throw new Error(response.message);
    appState.sessions = appState.sessions.filter(item => key(item) !== identity); deleting.value = null;
    if (appState.currentSessionId === row.id && appState.currentProjectKey === row.project_key) closeSessionView();
  } catch(error) { errors.value[identity] = String(error); } finally { removing.value = null; }
};
</script>
<template>
  <div class="sessions-sidebar">
    <div class="nav-head"><b>Mink</b><button class="primary" @click="uiState.newOpen = true">＋ 新建任务</button><button class="nav-close" aria-label="关闭导航" data-overlay-focus @click="$emit('close')">×</button></div>
    <button class="home-link" :class="{ active: !appState.currentSessionId }" :aria-current="!appState.currentSessionId ? 'page' : undefined" @click="home"><Home :size="16" aria-hidden="true" />首页</button>
    <div class="search-row">
      <select v-model="scope" aria-label="筛选范围"><option value="all">全部</option><option value="project">项目名称</option><option value="session">会话名称</option><option value="path">项目路径</option></select>
      <div class="search-field"><input ref="searchInput" class="search" v-model="query" aria-label="搜索会话" :placeholder="placeholder" /><button v-if="query" class="clear-search" aria-label="清空筛选" @click="clearSearch"><X :size="14" aria-hidden="true" /></button></div>
    </div>
    <div class="session-list">
      <section v-for="project in groups" :key="project.cwd">
        <h3 :title="project.cwd">{{ projectName(project.cwd) }}<span>{{ project.sessions.filter(row => row.status === 'running').length || '' }}</span></h3>
        <div v-for="row in project.sessions" :key="key(row)" class="sess-row" :class="{ active: row.id === appState.currentSessionId && row.project_key === appState.currentProjectKey }" :data-id="row.id">
          <button :aria-current="row.id === appState.currentSessionId && row.project_key === appState.currentProjectKey ? 'page' : undefined" class="session-select" @click="open(row)"><span class="dot" :class="row.status"></span><span class="sess-title">{{ row.title ?? row.alias ?? row.id }}</span><small v-if="row.pending_input_count">{{ row.pending_input_count }}</small></button>
          <div class="session-menu"><ActionMenu label="会话操作"><DropdownMenuItem as-child><button @click="deleting = key(row)">删除会话</button></DropdownMenuItem><small>{{ row.updated_at || '时间未记录' }}</small></ActionMenu></div>
          <div v-if="deleting === key(row)" class="delete-confirm"><span>删除会话及全部文件？</span><button class="danger" :disabled="removing === key(row)" @click="remove(row)">{{ removing === key(row) ? '删除中…' : '删除' }}</button><button :disabled="removing === key(row)" @click="deleting = null">取消</button></div>
          <p v-if="errors[key(row)]" class="row-error" role="alert">{{ errors[key(row)] }}</p>
        </div>
      </section>
      <p v-if="!groups.length" class="hint">没有匹配的会话</p>
    </div>

  </div>
</template>
<style scoped>
.sessions-sidebar { display:flex; flex-direction:column; height:100%; min-width:240px; background:var(--bg-elevated); }
.nav-head { display:flex; align-items:center; justify-content:space-between; padding:18px 14px; }
.home-link { margin:0 12px 12px; display:flex; align-items:center; gap:8px; border:0; background:transparent; font-size:13px; min-height:36px; text-align:left; } .home-link:hover { background:var(--panel-2); } .home-link.active { background:var(--blue-soft); color:var(--blue); }
.search-row { display:flex; align-items:center; gap:6px; margin:0 12px 10px; min-width:0; }
.search-row select { width:88px; flex-shrink:0; font-size:12px; padding:7px 4px; }
.search-field { position:relative; flex:1; min-width:0; }
.search { width:100%; font-size:12px; padding-right:28px; }
.clear-search { position:absolute; right:2px; top:50%; transform:translateY(-50%); width:26px; height:28px; border:0; padding:0; display:grid; place-items:center; background:transparent; color:var(--text-dim); }
.session-list { flex:1; min-height:0; overflow:auto; padding:0 8px; }
h3 { padding:12px 8px 6px; font-size:12px; color:var(--text-dim); display:flex; justify-content:space-between; }
.sess-row { position:relative; border-radius:8px; margin:3px 0; }
.sess-row.active { background:var(--blue-soft); }
.session-select { width:100%; display:flex; align-items:center; gap:8px; border:0; background:transparent; text-align:left; min-height:42px; padding:9px 42px 9px 10px; }
.sess-title { flex:1; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; font-size:13px; }
.dot { height:7px; width:7px; background:var(--text-dim); border-radius:50%; flex-shrink:0; }
.dot.running { background:var(--green); }.dot.active { background:var(--blue); }
.session-menu { position:absolute; top:5px; right:4px; }
.nav-close { display:none; }
.delete-confirm { padding:8px; font-size:12px; display:flex; flex-wrap:wrap; gap:5px; }.row-error { font-size:11px; color:var(--red); padding:8px; }
small { color:var(--text-dim); }
@media(max-width:1023px) { .nav-head { padding:10px 12px; gap:8px; }.nav-head b { margin-right:auto; }.nav-close { display:block; padding:0; min-width:32px; }.sessions-sidebar { padding-top:env(safe-area-inset-top); padding-bottom:env(safe-area-inset-bottom); } }
@media(max-width:767px), (pointer:coarse) { .search-row select { font-size:16px; width:106px; } .search { font-size:16px; } .session-select { min-height:44px; padding-right:46px; }.session-menu { top:4px; }.nav-close { width:34px; height:34px; } }
</style>
