<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from "vue";
import TopBar from "./components/TopBar.vue";
import SessionSidebar from "./components/SessionSidebar.vue";
import SessionView from "./components/session/SessionView.vue";
import EmptyState from "./components/EmptyState.vue";
import NewTaskDialog from "./components/NewTaskDialog.vue";
import { appState, savedSessionId, uiState } from "./lib/store";
import { openSession } from "./lib/sessionController";
import { startCatalog, refreshCatalog } from "./lib/catalogController";
import { preferences, feedback, flash } from "./lib/workbench";
const sidebar = ref(window.innerWidth >= 1024);
let stopCatalog: (() => void) | undefined;
const onKey = (event: KeyboardEvent) => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "b") { event.preventDefault(); sidebar.value = !sidebar.value; }
  if (event.key === "Escape" && !event.isComposing && !uiState.newOpen) { if (uiState.ctxOpen) uiState.ctxOpen = false; else if (window.innerWidth < 1024) sidebar.value = false; }
};
watch(() => appState.currentSessionId, () => { if (window.innerWidth < 1024) sidebar.value = false; });
onMounted(async () => {
  window.addEventListener("keydown", onKey);
  try {
    await refreshCatalog(); stopCatalog = startCatalog();
    const params = new URLSearchParams(location.search);
    const stored = savedSessionId();
    const [project, id] = stored?.includes("\n") ? stored.split("\n", 2) : [undefined, stored];
    const targetId = params.get("session") ?? id;
    const targetProject = params.get("project") ?? project;
    const found = appState.sessions.find(row => row.id === targetId && (!targetProject || row.project_key === targetProject));
    if (found) { appState.currentWorkspace = found.cwd; await openSession(found); }
  } catch (error) { flash(String(error)); }
});
onUnmounted(() => { window.removeEventListener("keydown", onKey); stopCatalog?.(); });
</script>
<template>
  <div class="app-shell" :class="{ 'nav-open': sidebar, 'detail-open': uiState.ctxOpen }" :style="{ '--nav-width': preferences.navWidth + 'px', '--detail-width': preferences.detailWidth + 'px' }">
    <aside class="navigation"><SessionSidebar /></aside>
    <div v-if="sidebar" class="nav-mask" @click="sidebar = false"></div>
    <main class="content">
      <TopBar @toggle-sidebar="sidebar = !sidebar" />
      <SessionView v-if="appState.currentSessionId" :key="`${appState.currentProjectKey}:${appState.currentSessionId}`" />
      <EmptyState v-else @browse="sidebar = true" />
    </main>
    <NewTaskDialog />
    <div v-if="feedback.message" class="app-toast" role="status">{{ feedback.message }}</div>
  </div>
</template>
<style scoped>
.app-shell { display:flex; height:100dvh; overflow:hidden; }
.navigation { width:0; flex-shrink:0; overflow:hidden; background:var(--bg-elevated); border-right:1px solid var(--line); }
.nav-open .navigation { width:var(--nav-width); }
.content { flex:1; display:flex; flex-direction:column; min-width:0; min-height:0; }
.nav-mask { display:none; }
.app-toast { position:fixed; bottom:24px; left:50%; transform:translateX(-50%); padding:10px 18px; border:1px solid var(--line); border-radius:10px; background:var(--bg-elevated); box-shadow:var(--shadow); z-index:100; max-width:90vw; }
@media (min-width:1024px) and (max-width:1279px) { .detail-open .navigation { width:0; } }
@media (max-width:1023px) {
  .navigation { position:fixed; z-index:65; inset:0 auto 0 0; width:min(var(--nav-width),86vw); transform:translateX(-101%); }
  .nav-open .navigation { transform:none; }
  .nav-open .nav-mask { display:block; position:fixed; inset:0; background:#0005; z-index:60; }
}
</style>
