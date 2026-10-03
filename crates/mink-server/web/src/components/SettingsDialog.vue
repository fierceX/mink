<script setup lang="ts">
import { nextTick, ref, watch } from 'vue';
import { X } from '@lucide/vue';
import { uiState } from '../lib/store';
import { preferences, settingsOpener } from '../lib/workbench';
import { cycleFocus } from '../lib/useOverlay';
import IconButton from './IconButton.vue';
const dialog = ref<HTMLDialogElement | null>(null);
watch(() => uiState.settingsOpen, async open => {
  await nextTick();
  // Let the menu restore its trigger before the native dialog captures focus.
  if (open) {
    await new Promise<void>(resolve => requestAnimationFrame(() => resolve()));
    if (uiState.settingsOpen && !dialog.value?.open) dialog.value?.showModal();
  } else {
    dialog.value?.close();
    if (settingsOpener.value?.isConnected) settingsOpener.value.focus({ preventScroll: true });
    settingsOpener.value = null;
  }
});
</script>
<template>
  <dialog ref="dialog" @keydown="cycleFocus(dialog, $event)" aria-labelledby="settings-title" @cancel="uiState.settingsOpen = false" @close="uiState.settingsOpen = false">
    <header><h2 id="settings-title">设置</h2><IconButton label="关闭设置" autofocus @click="uiState.settingsOpen = false"><X :size="18" aria-hidden="true" /></IconButton></header>
    <section aria-label="阅读偏好">
      <label>外观<select v-model="preferences.theme"><option value="light">浅色</option><option value="dark">深色</option></select></label>
      <label>过程展示<select v-model="preferences.process"><option value="concise">简洁</option><option value="standard">标准</option><option value="detailed">详细</option></select></label>
      <label>自动换行<input type="checkbox" v-model="preferences.wrapText" /></label>
    </section>
    <details><summary>布局与快捷键</summary>
      <label>导航宽度 <span>{{ preferences.navWidth }} px</span><input type="range" min="240" max="320" v-model.number="preferences.navWidth" /></label>
      <label>详情宽度 <span>{{ preferences.detailWidth }} px</span><input type="range" min="320" max="560" v-model.number="preferences.detailWidth" /></label>
      <p>⌘/Ctrl+B 切换导航<br />Enter 提交 · Shift+Enter 换行<br />手机 Enter 换行，通过按钮提交</p>
    </details>
    <p class="saved">偏好自动保存在当前浏览器。</p>
  </dialog>
</template>
<style scoped>
dialog { margin:auto; width:min(420px,calc(100vw - 32px)); max-height:calc(100dvh - 32px); overflow:auto; border:1px solid var(--line); border-radius:14px; padding:20px; background:var(--bg-elevated); color:var(--text); }
dialog::backdrop { background:#0005; }
header { display:flex; align-items:center; justify-content:space-between; margin-bottom:16px; } h2 { font-size:18px; }
section { display:grid; gap:16px; } label { display:flex; align-items:center; justify-content:space-between; gap:12px; font-size:13px; } select { width:112px; }
details { border-top:1px solid var(--line); margin-top:20px; padding-top:16px; font-size:13px; } summary { cursor:pointer; color:var(--text-soft); } details label { flex-wrap:wrap; margin-top:16px; } details input { width:100%; } details span { color:var(--text-dim); }
p { color:var(--text-dim); font-size:12px; line-height:1.8; margin-top:16px; } .saved { margin-top:20px; }
</style>
