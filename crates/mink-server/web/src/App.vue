<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import TopBar from "./components/TopBar.vue";
import SessionSidebar from "./components/SessionSidebar.vue";
import SessionView from "./components/session/SessionView.vue";
import EmptyState from "./components/EmptyState.vue";
import NewTaskDialog from "./components/NewTaskDialog.vue";
import SettingsDialog from "./components/SettingsDialog.vue";
import { appState, uiState } from "./lib/store";
import { openSession, navigationRevision } from "./lib/sessionController";
import { startCatalog, refreshCatalog } from "./lib/catalogController";
import { preferences, feedback, flash } from "./lib/workbench";
import { useMedia, useOverlay } from "./lib/useOverlay";
import { ChevronDown } from "@lucide/vue";
const initialParams = new URLSearchParams(location.search);
const initialSession = initialParams.get('session');
const initialProject = initialParams.get('project');
const starting = ref(!!initialSession);
const middle = useMedia("(min-width:1024px) and (max-width:1279px)");
const narrow = useMedia("(max-width:1023px)");
const sidebar = ref(window.innerWidth >= 1024);
const mobile = useMedia("(max-width:767px)");
const reading = ref(false);
const inputLocked = ref(false);
const chromeHidden = computed(() => mobile.value && !!appState.currentSessionId && reading.value && !inputLocked.value && !sidebar.value && !uiState.ctxOpen && !uiState.newOpen && !uiState.settingsOpen);
function showControls() { reading.value = false; }
function readingMode(value:boolean) {
  if (!value) { showControls(); return; }
  if (!mobile.value || inputLocked.value || sidebar.value || uiState.ctxOpen || uiState.newOpen || uiState.settingsOpen || document.querySelector('[role="menu"]')) return;
  if (document.activeElement?.closest('.composer input,.composer textarea,.composer select')) return;
  reading.value = true;
}
watch(() => [mobile.value, inputLocked.value, sidebar.value, uiState.ctxOpen, uiState.newOpen, uiState.settingsOpen], () => { if (!mobile.value || inputLocked.value || sidebar.value || uiState.ctxOpen || uiState.newOpen || uiState.settingsOpen) showControls(); });
const navigation = ref<HTMLElement | null>(null);
watch(narrow, value => { if (value) sidebar.value = false; });
const navOverlay = computed(() => sidebar.value && narrow.value);
useOverlay(navigation, navOverlay, () => sidebar.value = false);
const viewportHeight = ref(window.visualViewport?.height ?? window.innerHeight);
const updateViewport = () => { viewportHeight.value = window.visualViewport?.height ?? window.innerHeight; };
let stopCatalog: (() => void) | undefined;
const onKey = (event: KeyboardEvent) => {
  if ((event.target instanceof Element && event.target.closest('[role="menu"]')) || event.defaultPrevented || uiState.newOpen || uiState.settingsOpen) return;
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "b") { event.preventDefault(); if (narrow.value) uiState.ctxOpen = false; sidebar.value = !sidebar.value; }
  if (event.key === "Escape" && !event.isComposing && !uiState.newOpen && !uiState.settingsOpen) { if (uiState.ctxOpen) uiState.ctxOpen = false; else if (chromeHidden.value) showControls(); else if (window.innerWidth < 1024) sidebar.value = false; }
};
function syncAddress() {
  const url = new URL(location.href);
  if (appState.currentSessionId) {
    url.searchParams.set('session', appState.currentSessionId);
    url.searchParams.set('project', appState.currentProjectKey ?? '');
  } else { url.searchParams.delete('session'); url.searchParams.delete('project'); }
  history.replaceState(history.state, '', url);
}
function selectedView() { starting.value = false; syncAddress(); if (narrow.value) sidebar.value = false; }
watch(() => [appState.currentProjectKey, appState.currentSessionId], () => { showControls(); inputLocked.value = false; syncAddress(); if (window.innerWidth < 1024) sidebar.value = false; });
onMounted(async () => {
  window.addEventListener("keydown", onKey);
  window.visualViewport?.addEventListener("resize", updateViewport);
  window.addEventListener("resize", updateViewport);
  try {
    const revision = navigationRevision();
    await refreshCatalog(); stopCatalog = startCatalog();
    const found = appState.sessions.find(row => row.id === initialSession && (!initialProject || row.project_key === initialProject));
    // An explicit choice made during the catalog request wins over startup.
    if (revision !== navigationRevision()) return;
    if (found) { appState.currentWorkspace = found.cwd; await openSession(found); }
    else if (initialSession) { syncAddress(); flash('会话不存在，已返回首页'); }
  } catch (error) { flash(String(error)); }
  finally { starting.value = false; }
});
onUnmounted(() => { window.removeEventListener("keydown", onKey); window.visualViewport?.removeEventListener("resize", updateViewport); window.removeEventListener("resize", updateViewport); stopCatalog?.(); });
</script>
<template>
  <div class="app-shell" :class="{ 'nav-open': sidebar, 'detail-open': uiState.ctxOpen }" :style="{ '--viewport-height': viewportHeight + 'px', '--nav-width': preferences.navWidth + 'px', '--detail-width': preferences.detailWidth + 'px' }">
    <aside ref="navigation" id="session-navigation" class="navigation" :inert="!sidebar || (middle && uiState.ctxOpen)" :role="navOverlay ? 'dialog' : 'navigation'" :aria-modal="navOverlay ? true : undefined" aria-label="项目会话导航" tabindex="-1"><SessionSidebar @selected="selectedView" @close="sidebar = false" /></aside>
    <button v-if="navOverlay" class="nav-mask" tabindex="-1" aria-label="关闭导航遮罩" @click="sidebar = false"></button>
    <main class="content" :inert="navOverlay">
      <TopBar v-show="!chromeHidden" :sidebar-open="sidebar" :inert="narrow && uiState.ctxOpen" @toggle-sidebar="sidebar = !sidebar" @home="selectedView" />
      <button v-if="chromeHidden" class="topbar-reveal" aria-label="显示顶部栏" @click="showControls"><ChevronDown :size="16" aria-hidden="true" /></button>
      <SessionView v-if="appState.currentSessionId" :key="`${appState.currentProjectKey}:${appState.currentSessionId}`" :controls-hidden="chromeHidden" @reading-mode="readingMode" @show-controls="showControls" @input-lock="inputLocked = $event" />
      <div v-else-if="starting" class="startup" role="status">正在打开会话…</div>
      <EmptyState v-else @browse="sidebar = true" />
    </main>
    <NewTaskDialog />
    <SettingsDialog />
    <div v-if="feedback.message" class="app-toast" role="status">{{ feedback.message }}</div>
  </div>
</template>
<style scoped>
.app-shell { display:flex; height:var(--viewport-height,100dvh); overflow:hidden; }
.navigation { width:0; flex-shrink:0; overflow:hidden; background:var(--bg-elevated); border-right:1px solid var(--line); }
.nav-open .navigation { width:var(--nav-width); }
.content { position:relative; flex:1; display:flex; flex-direction:column; min-width:0; min-height:0; }
.startup { flex:1; display:grid; place-items:center; font-size:13px; color:var(--text-dim); }
.topbar-reveal { position:absolute; z-index:25; top:max(6px,env(safe-area-inset-top)); right:12px; width:32px; height:32px; padding:0; display:grid; place-items:center; border-radius:50%; background:var(--bg-elevated); box-shadow:var(--shadow); }
@media(pointer:coarse) { .topbar-reveal { width:36px; height:36px; } }
.nav-mask { display:none; }
.app-toast { position:fixed; top:68px; left:50%; transform:translateX(-50%); padding:10px 18px; border:1px solid var(--line); border-radius:10px; background:var(--bg-elevated); box-shadow:var(--shadow); z-index:100; max-width:min(90vw,560px); pointer-events:none; font-size:13px; overflow-wrap:anywhere; }
@media (min-width:1024px) and (max-width:1279px) { .detail-open .navigation { width:0; } }
@media (max-width:1023px) {
  .navigation { position:fixed; z-index:65; inset:0 auto 0 0; width:min(var(--nav-width),86vw); transform:translateX(-101%); }
  .nav-open .navigation { transform:none; }
  .nav-open .nav-mask { display:block; position:fixed; inset:0; background:#0005; z-index:60; border:0; border-radius:0; padding:0; width:100%; height:100%; }
}
</style>
