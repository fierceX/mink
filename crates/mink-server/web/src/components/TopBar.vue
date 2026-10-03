<script setup lang="ts">
import { computed } from "vue";
import { appState, uiState } from "../lib/store";
import { closeSessionView, openSession } from "../lib/sessionController";
import { api } from "../lib/api";
import { identity, flash } from "../lib/workbench";
defineEmits<{ toggleSidebar: [] }>();
const session = computed(() => appState.sessions.find(row => row.id === appState.currentSessionId && row.project_key === appState.currentProjectKey));
const reconnect = async () => { if (session.value) { try { await openSession(session.value); } catch (error) { flash(String(error)); } } };
async function release() {
  const current=session.value; if (!current) return; const key=identity(current.project_key,current.id);
  try { const response=await api.closeSession(current.id,current.project_key); if (response.code!==200) throw new Error(response.message);
    if (identity()===key && appState.sessionState) { appState.sessionState.phase="closed";appState.sessionState.desynced=true;appState.sessionState.running=false; }
    flash("运行时已释放，草稿与会话保留");
  } catch(error) { flash(`释放失败：${String(error)}`); }
}
</script>
<template>
  <header class="topbar">
    <button aria-label="切换导航" class="hamburger" @click="$emit('toggleSidebar')">☰</button>
    <button class="brand" title="返回首页（任务继续运行）" @click="closeSessionView()">Mink</button>
    <div class="crumb" :title="session?.cwd"><span>{{ session?.cwd.split('/').filter(Boolean).pop() ?? '开发工作台' }}</span><span v-if="session"> / {{ session.title ?? session.alias ?? session.id }}</span></div>
    <span class="state" role="status">{{ appState.sessionState?.phase === 'closed' ? '已关闭' : appState.sessionState?.desynced ? '正在恢复连接' : ['cancelling','closing'].includes(appState.sessionState?.phase ?? '') ? '正在停止' : appState.sessionState?.running ? '运行中' : '空闲' }}</span>
    <button v-if="session" aria-label="重新打开并同步" title="重新打开并同步" @click="reconnect">⟳</button>
    <button v-if="session" @click="uiState.ctxOpen = !uiState.ctxOpen">详情</button>
    <details v-if="session" class="session-menu"><summary aria-label="会话操作菜单">⋯</summary><div><button @click="release">释放运行时（停止当前任务）</button></div></details>
    <button class="primary new-task" @click="uiState.newOpen = true">＋ 新建任务</button>
  </header>
</template>
<style scoped>
.topbar { height:48px; min-height:48px; display:flex; align-items:center; padding:0 16px; gap:10px; border-bottom:1px solid var(--line); background:var(--bg-elevated); }
.brand { border:0; font-weight:750; font-size:15px; padding:3px; }
.crumb { flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; font-size:12px; color:var(--text-soft); }
.session-menu { position:relative; }.session-menu summary { list-style:none; cursor:pointer; padding:4px 6px; }.session-menu div { position:absolute; z-index:70; right:0; top:28px; background:var(--bg-elevated); border:1px solid var(--line); border-radius:8px; padding:8px; box-shadow:var(--shadow); white-space:nowrap; }
.state { font-size:11px; white-space:nowrap; color:var(--text-dim); }
button { padding:3px 8px; }
@media(max-width:767px) { .brand,.new-task { display:none; } .topbar { padding:0 10px; gap:7px; } }
</style>
