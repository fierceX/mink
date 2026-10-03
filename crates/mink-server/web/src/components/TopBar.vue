<script setup lang="ts">
import { computed, ref } from "vue";
import { appState, uiState } from "../lib/store";
import { closeSessionView, openSession } from "../lib/sessionController";
import { api } from "../lib/api";
import { identity, flash, showSettings } from "../lib/workbench";
import { DropdownMenuItem } from "reka-ui";
import { Menu, RotateCw, PanelRight, LoaderCircle } from "@lucide/vue";
import IconButton from "./IconButton.vue";
import ActionMenu from "./ActionMenu.vue";
defineProps<{ sidebarOpen: boolean }>();
const emit = defineEmits<{ toggleSidebar: []; home: [] }>();
const home = () => { closeSessionView(); emit("home"); };
const busy = ref(false);
const session = computed(() => appState.sessions.find(row => row.id === appState.currentSessionId && row.project_key === appState.currentProjectKey));
const reconnect = async () => { if (!session.value || busy.value) return; busy.value = true; try { await openSession(session.value); } catch (error) { flash(String(error)); } finally { busy.value = false; } };
async function release() {
  const current=session.value; if (!current || busy.value) return; busy.value = true; const key=identity(current.project_key,current.id); const generation=appState.sessionState?.generation;
  try { const response=await api.closeSession(current.id,current.project_key); if (response.code!==200) throw new Error(response.message);
    if (identity()===key && appState.sessionState?.generation===generation && appState.sessionState) { appState.sessionState.phase="closed";appState.sessionState.desynced=true;appState.sessionState.running=false; }
    flash("运行时已释放，草稿与会话保留");
  } catch(error) { flash(`释放失败：${String(error)}`); } finally { busy.value = false; }
}
</script>
<template>
  <header class="topbar">
    <IconButton label="切换导航" :aria-expanded="sidebarOpen" aria-controls="session-navigation" @click="$emit('toggleSidebar')"><Menu :size="18" aria-hidden="true" /></IconButton>
    <button class="brand" title="返回首页（任务继续运行）" @click="home">Mink</button>
    <div class="crumb" :title="session?.cwd"><span>{{ session?.cwd.split('/').filter(Boolean).pop() ?? '开发工作台' }}</span><span v-if="session"> / {{ session.title ?? session.alias ?? session.id }}</span></div>
    <span class="state" role="status">{{ appState.sessionState?.phase === 'closed' ? '已关闭' : appState.sessionState?.desynced ? '正在恢复连接' : ['cancelling','closing'].includes(appState.sessionState?.phase ?? '') ? '正在停止' : appState.sessionState?.running ? '运行中' : '空闲' }}</span>
    <IconButton v-if="session" label="重新打开并同步" :disabled="busy" :busy="busy" @click="reconnect"><component :is="busy ? LoaderCircle : RotateCw" :size="16" aria-hidden="true" /></IconButton>
    <IconButton v-if="session" label="详情" :aria-pressed="uiState.ctxOpen" @click="uiState.ctxOpen = !uiState.ctxOpen"><PanelRight :size="18" aria-hidden="true" /></IconButton>
    <ActionMenu :key="identity()" :label="session ? '会话操作菜单' : '工作台菜单'">
      <DropdownMenuItem as-child><button @click="showSettings">设置</button></DropdownMenuItem>
      <DropdownMenuItem v-if="session" as-child :disabled="busy"><button :disabled="busy" @click="release">释放运行时（停止当前任务）</button></DropdownMenuItem>
    </ActionMenu>
    <button class="primary new-task" @click="uiState.newOpen = true">＋ 新建任务</button>
  </header>
</template>
<style scoped>
.topbar { height:48px; min-height:48px; display:flex; align-items:center; padding:0 12px; gap:8px; border-bottom:1px solid var(--line); background:var(--bg-elevated); }
.brand { border:0; font-weight:750; font-size:15px; padding:3px; }
.crumb { flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; font-size:12px; color:var(--text-soft); }
.state { font-size:11px; white-space:nowrap; color:var(--text-dim); }
.brand,.new-task { padding:4px 10px; min-height:30px; }
:deep(button[aria-pressed="true"]) { background:var(--blue-soft); color:var(--blue); border-color:var(--blue); }
@media(max-width:767px) { .brand,.new-task { display:none; } .topbar { padding:0 10px; gap:4px; } }

</style>
