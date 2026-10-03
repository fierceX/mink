<script setup lang="ts">
import { computed, ref, watch, nextTick } from "vue";
import { appState, uiState, workspaces } from "../lib/store";
import { api } from "../lib/api";
import { flash } from "../lib/workbench";
import { openSession } from "../lib/sessionController";
const dialog = ref<HTMLDialogElement | null>(null);
const cwd = ref(""); const name = ref(""); const error = ref(""); const busy = ref(false);
const projects = computed(() => workspaces());
watch(() => uiState.newOpen, async open => { await nextTick(); if (open) { cwd.value = appState.currentWorkspace ?? projects.value[0]?.cwd ?? ""; name.value = ""; error.value = ""; dialog.value?.showModal(); } else dialog.value?.close(); });
async function create() {
  if (!cwd.value.trim() || busy.value) return;
  busy.value = true; error.value = "";
  try {
    const alias = `${name.value.trim() || 'task'}-${(globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(36).slice(2)}`)}`;
    const response = await api.createSession(alias, cwd.value.trim());
    if (response.code !== 200) throw new Error(response.message);
    appState.sessions.unshift(response.data); appState.currentWorkspace = response.data.cwd;
    uiState.newOpen = false;
    try { await openSession(response.data); } catch(e) { flash(`任务已创建，打开失败：${String(e)}`); }
  } catch(e) { error.value = String(e); } finally { busy.value = false; }
}
</script>
<template>
  <dialog ref="dialog" @cancel="busy ? $event.preventDefault() : uiState.newOpen = false" @close="uiState.newOpen = false" aria-labelledby="new-title">
    <form @submit.prevent="create"><h2 id="new-title">新建任务</h2><p>工作目录位于 server 所在机器。</p>
      <label>工作目录<input v-model="cwd" list="projects" placeholder="/path/to/project" required autofocus /></label>
      <datalist id="projects"><option v-for="project in projects" :key="project.cwd" :value="project.cwd" /></datalist>
      <label>任务名称（可选）<input v-model="name" placeholder="例如：调整恢复策略" /></label>
      <p v-if="error" role="alert" class="error">{{ error }}</p><div class="actions"><button type="button" :disabled="busy" @click="uiState.newOpen = false">取消</button><button class="primary" :disabled="busy || !cwd.trim()">{{ busy ? '正在创建…' : '创建' }}</button></div>
    </form>
  </dialog>
</template>
<style scoped>
dialog { margin:auto; width:min(480px,calc(100vw - 32px)); border:1px solid var(--line); border-radius:14px; padding:24px; max-height:calc(100dvh - 32px); overflow:auto; background:var(--bg-elevated); color:var(--text); } dialog::backdrop { background:#0005; } form { display:grid; gap:16px; } h2 { font-size:20px; } p { font-size:13px; color:var(--text-dim); } label { display:grid; gap:5px; } .actions { display:flex; justify-content:flex-end; gap:8px; }.error { color:var(--red); }
</style>
