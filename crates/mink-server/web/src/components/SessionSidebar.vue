<script setup lang="ts">
import { computed, ref } from "vue";
import { appState, uiState, workspaces } from "../lib/store";
import { api, type SessionSummary } from "../lib/api";
import { openSession, closeSessionView } from "../lib/sessionController";
import { preferences, flash } from "../lib/workbench";
const query = ref("");
const deleting = ref<string | null>(null);
const errors = ref<Record<string, string>>({});
const key = (row: SessionSummary) => JSON.stringify([row.project_key, row.id]);
const groups = computed(() => workspaces().map(project => ({ ...project, sessions: project.sessions.filter(row => `${row.title ?? ''} ${row.alias ?? ''} ${row.cwd}`.toLowerCase().includes(query.value.toLowerCase())) })).filter(project => project.sessions.length));
const open = async (row: SessionSummary) => { appState.currentWorkspace = row.cwd; try { await openSession(row); } catch(error) { flash(String(error)); } };
const remove = async (row: SessionSummary) => {
  const identity = key(row);
  try { const response = await api.deleteSession(row.id, row.project_key); if (response.code !== 200) throw new Error(response.message);
    appState.sessions = appState.sessions.filter(item => key(item) !== identity); deleting.value = null;
    if (appState.currentSessionId === row.id && appState.currentProjectKey === row.project_key) closeSessionView();
  } catch(error) { errors.value[identity] = String(error); }
};
</script>
<template>
  <div class="sessions-sidebar">
    <div class="nav-head"><b>Mink</b><button class="primary" @click="uiState.newOpen = true">＋ 新建任务</button></div>
    <input class="search" v-model="query" aria-label="搜索会话" placeholder="搜索标题、别名或项目…" />
    <div class="session-list">
      <section v-for="project in groups" :key="project.cwd">
        <h3 :title="project.cwd">{{ project.cwd.split('/').filter(Boolean).pop() || project.cwd }}<span>{{ project.sessions.filter(row => row.status === 'running').length || '' }}</span></h3>
        <div v-for="row in project.sessions" :key="key(row)" class="sess-row" :class="{ active: row.id === appState.currentSessionId && row.project_key === appState.currentProjectKey }" :data-id="row.id">
          <button class="session-select" @click="open(row)"><span class="dot" :class="row.status"></span><span class="sess-title">{{ row.title ?? row.alias ?? row.id }}</span><small v-if="row.pending_input_count">{{ row.pending_input_count }}</small></button>
          <details class="session-menu"><summary aria-label="会话操作">⋯</summary><button @click="deleting = key(row)">删除会话</button><small>{{ row.updated_at || '时间未记录' }}</small></details>
          <div v-if="deleting === key(row)" class="delete-confirm"><span>删除会话及全部文件？</span><button class="danger" @click="remove(row)">删除</button><button @click="deleting = null">取消</button></div>
          <p v-if="errors[key(row)]" class="row-error" role="alert">{{ errors[key(row)] }}</p>
        </div>
      </section>
      <p v-if="!groups.length" class="hint">没有匹配的会话</p>
    </div>
    <footer>
      <label>外观<select v-model="preferences.theme"><option value="light">浅色</option><option value="dark">深色</option></select></label>
      <label>过程<select v-model="preferences.process"><option value="concise">简洁</option><option value="standard">标准</option><option value="detailed">详细</option></select></label>
      <details><summary>布局与快捷键</summary><label>导航宽度<input type="range" min="240" max="320" v-model.number="preferences.navWidth" /></label><label>详情宽度<input type="range" min="320" max="560" v-model.number="preferences.detailWidth" /></label><small>⌘/Ctrl+B 导航 · Enter 提交 · Shift+Enter 换行 · 手机 Enter 换行</small></details>
    </footer>
  </div>
</template>
<style scoped>
.sessions-sidebar { display:flex; flex-direction:column; height:100%; min-width:240px; background:var(--bg-elevated); }
.nav-head { display:flex; align-items:center; justify-content:space-between; padding:18px 14px; }
.search { margin:0 12px 10px; width:calc(100% - 24px); font-size:12px; }
.session-list { flex:1; min-height:0; overflow:auto; padding:0 8px; }
h3 { padding:12px 8px 6px; font-size:12px; color:var(--text-dim); display:flex; justify-content:space-between; }
.sess-row { position:relative; border-radius:8px; margin:3px 0; }
.sess-row.active { background:var(--blue-soft); }
.session-select { width:100%; display:flex; align-items:center; gap:8px; border:0; background:transparent; text-align:left; padding:9px 30px 9px 10px; }
.sess-title { flex:1; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; font-size:13px; }
.dot { height:7px; width:7px; background:var(--text-dim); border-radius:50%; flex-shrink:0; }
.dot.running { background:var(--green); }.dot.active { background:var(--blue); }
.session-menu { position:absolute; top:8px; right:5px; z-index:1; font-size:12px; }.session-menu summary { cursor:pointer; list-style:none; }.session-menu[open] { padding:6px; background:var(--bg-elevated); box-shadow:var(--shadow); }.session-menu small { display:block; max-width:180px; }
.delete-confirm { padding:8px; font-size:12px; display:flex; flex-wrap:wrap; gap:5px; }.row-error { font-size:11px; color:var(--red); padding:8px; }
footer { border-top:1px solid var(--line); padding:12px; display:grid; gap:8px; font-size:12px; } label { display:flex; align-items:center; justify-content:space-between; gap:10px; } select { max-width:100px; } small { color:var(--text-dim); }
</style>
