<script setup lang="ts">
import { ref, computed, watch, nextTick, onMounted, onBeforeUnmount } from "vue";
import { appState, prependOlder } from "../../lib/store";
import { api, sessionUrl } from "../../lib/api";
import { conversationToEvents } from "../../lib/toolFormat";
import { identity, viewFor, flash, openDetail, preferences } from "../../lib/workbench";
import { projectTurns } from "../../lib/transcriptProjection";
import { copyText } from "../../lib/clipboard";
import { projectMessages } from "../../lib/workbenchReducer";
import type { RawEvent } from "../../lib/types";
import ProcessGroup from "./ProcessGroup.vue";
import TextOutput from "./results/TextResult.vue";
import { DropdownMenuItem, DropdownMenuCheckboxItem } from "reka-ui";
import { List, ArrowDown, Check } from "@lucide/vue";
import ActionMenu from "../ActionMenu.vue";
import { readingGesture } from "../../lib/readingGesture";
const props = withDefaults(defineProps<{ controlsHidden?:boolean }>(), { controlsHidden:false });
const emit = defineEmits<{ readingMode:[value:boolean]; showControls:[] }>();
const gesture = readingGesture();
let userScrollUntil = 0; let lastScrollTop = 0;
const scrollEl = ref<HTMLElement | null>(null); const body = ref<HTMLElement | null>(null);
const items = computed(() => appState.sessionState?.items ?? []);
const turns = computed(() => projectTurns(items.value));
const following = ref(viewFor().follow); const loading = ref(false); const hasOlder = ref(true);
const loadedKey = identity(); let observer: ResizeObserver | undefined; let raf = 0; let restoringLayout = false;
const copied = ref(""); let copyTimer: ReturnType<typeof setTimeout> | undefined;
async function copy(text: string) { try { await copyText(text); copied.value = text; clearTimeout(copyTimer); copyTimer = setTimeout(() => copied.value = "",1500); } catch(e) { flash(`复制失败：${String(e)}`); } }
function capture() {
  const el=scrollEl.value; if (!el) return;
  const top=el.getBoundingClientRect().top;
  const first=[...el.querySelectorAll<HTMLElement>("[data-item-key]")].find(node => node.getBoundingClientRect().bottom > top + 1);
  if (first) viewFor(loadedKey).anchor={key:first.dataset.itemKey!,offset:first.getBoundingClientRect().top-top};
}
function innerKey(el: HTMLElement) {
  const item = el.closest<HTMLElement>("[data-item-key]");
  return item ? JSON.stringify([item.dataset.itemKey,el.className]) : null;
}
function innerScroll(event: Event) {
  const target = event.target as HTMLElement; if (target === scrollEl.value) return;
  const key = innerKey(target); if (key) viewFor(loadedKey).innerScroll[key] = target.scrollTop;
}
function restore() {
  const el=scrollEl.value; if (!el) return;
  const view=viewFor(loadedKey); following.value=view.follow;
  for (const target of el.querySelectorAll<HTMLElement>(".process-content,.t-body,.tp-body,.artifact-content")) {
    const key=innerKey(target); if (key && view.innerScroll[key] !== undefined) target.scrollTop=view.innerScroll[key];
  }
  if (view.follow) el.scrollTop=el.scrollHeight;
  else {
    const anchor=view.anchor;
    const node=anchor && [...el.querySelectorAll<HTMLElement>("[data-item-key]")].find(node=>node.dataset.itemKey===anchor.key);
    if (node) el.scrollTop+=node.getBoundingClientRect().top-el.getBoundingClientRect().top-anchor!.offset;
  }
  restoringLayout = false;
  lastScrollTop = el.scrollTop;
}
function scheduleRestore() { cancelAnimationFrame(raf); raf=requestAnimationFrame(restore); }
function onScroll() {
  const top=scrollEl.value?.scrollTop ?? 0; const delta=top-lastScrollTop; lastScrollTop=top;
  if (restoringLayout) return;
  capture();
  if (performance.now() < userScrollUntil) { const reading=gesture.consume(delta); if (reading !== undefined) emit('readingMode',reading); }
}
function armScroll(event: Event) {
  if ((event.target as Element).closest('.process-content,.tp-body,.t-body,.file-content')) return;
  userScrollUntil = performance.now()+700;
}
function onWheel(event: WheelEvent) { intent(event); if (!event.ctrlKey && !event.shiftKey && Math.abs(event.deltaY)>Math.abs(event.deltaX)) armScroll(event); }
function onTouchStart(event: TouchEvent) { gesture.reset(); lastScrollTop=scrollEl.value?.scrollTop ?? 0; armScroll(event); }
function onTouchMove(event: TouchEvent) { intent(event); armScroll(event); }
function intent(event?: Event) { if (event && (event.target as HTMLElement).closest(".process-content,.tp-body,.t-body,.file-content")) return; viewFor(loadedKey).follow=false; following.value=false; }
function latest() { viewFor(loadedKey).follow=true; following.value=true; scheduleRestore(); }
function onFollow(event: Event) { if ((event as CustomEvent).detail===loadedKey) latest(); }
async function loadOlder() {
  if (loading.value || !hasOlder.value) return;
  const state=appState.sessionState; const project=appState.currentProjectKey!; if (!state) return;
  const firstRow=state.messages?.[0]?.seq ?? items.value.map(item=>String(item.key).match(/^message:(\d+):/)).find(Boolean)?.[1];
  if (!firstRow || Number(firstRow)<=1) { hasOlder.value=false; return; }
  capture(); loading.value=true;
  try {
    const response=await api.conversation(state.sessionId,{limit:20,beforeSeq:Number(firstRow),project});
    if (identity()!==loadedKey || state.generation!==appState.sessionState?.generation) return;
    if (response.code!==200) throw new Error(response.message);
    if (!response.data.length) hasOlder.value=false;
    else if (appState.sessionState?.messages) {
      const current=appState.sessionState;
      const live=current.items.filter(item=>String(item.key).startsWith('live:') && ['text','thinking'].includes(item.kind));
      appState.sessionState=projectMessages(current,[...(response.data as Record<string,unknown>[]),...current.messages!]);
      appState.sessionState.items.push(...live);
    } else prependOlder(response.data.flatMap(row=>conversationToEvents(row as Record<string,unknown>)) as RawEvent[]);
    await nextTick(); restore();
  } catch(error) { flash(String(error)); } finally { loading.value=false; }
}
function reference(event: MouseEvent) {
  const anchor=(event.target as HTMLElement).closest('a'); if (!anchor) return;
  const href=anchor.getAttribute('href'); if (!href || /^(https?:|mailto:|#)/.test(href)) return;
  if (href.startsWith('artifact://')) { event.preventDefault(); openDetail('outputs',href.slice(11)); return; }
  if (/^[a-z]+:/i.test(href) && !/^\/.+/.test(href)) return;
  event.preventDefault(); const match=href.match(/^(.*?)(?::(\d+)|#L(\d+))?$/);
  if (match) openDetail('files',decodeURIComponent(match[1]),Number(match[2]??match[3]??1));
}
function contentClick(event: MouseEvent) {
  reference(event);
  if (props.controlsHidden && !(event.target as Element).closest('button,a,summary,input,textarea,select,pre,code') && !window.getSelection()?.toString()) emit('showControls');
}
function showProcess(turn: typeof turns.value[number]) { for (const segment of turn.segments) if (segment.kind === "process") viewFor(loadedKey).expanded[`process:${segment.key}`] = true; }
function showUsage(turn: typeof turns.value[number]) { openDetail("diagnostics"); viewFor(loadedKey).detailTurn = turn.user?.kind === "user" ? turn.user.turnId ?? `legacy:${turn.key}` : `legacy:${turn.key}`; }
function jump(key: string) { intent(); const node=[...scrollEl.value?.querySelectorAll<HTMLElement>('[data-turn-key]') ?? []].find(node=>node.dataset.turnKey===key); node?.scrollIntoView({block:'start'}); capture(); }
watch(() => [items.value.length,items.value.at(-1)], async () => { await nextTick(); scheduleRestore(); });
watch(() => preferences.wrapText, () => { capture(); restoringLayout = true; void nextTick(scheduleRestore); }, { flush:'sync' });
watch(() => props.controlsHidden, () => { restoringLayout = true; void nextTick(scheduleRestore); }, { flush:'sync' });
onMounted(() => { observer=new ResizeObserver(scheduleRestore); if (body.value) observer.observe(body.value); if (scrollEl.value) observer.observe(scrollEl.value); window.addEventListener('mink-follow',onFollow); scheduleRestore(); });
onBeforeUnmount(() => { clearTimeout(copyTimer); capture(); observer?.disconnect(); cancelAnimationFrame(raf); window.removeEventListener('mink-follow',onFollow); });
const attachmentUrl = (id:string) => sessionUrl(appState.currentSessionId!,`/attachments/${id}`,appState.currentProjectKey!);
</script>
<template>
  <div class="transcript-shell" :class="{ 'chrome-hidden':controlsHidden }">
    <div class="transcript" ref="scrollEl" @scroll.self.passive="onScroll" @scroll.capture.passive="innerScroll" @wheel.passive="onWheel" @touchstart.passive="onTouchStart" @touchmove.passive="onTouchMove" @touchend.passive="armScroll" @pointerdown.self="intent" @click="contentClick">
      <div class="transcript-body" ref="body">
        <button v-if="hasOlder && items.length" class="load-older" :disabled="loading" @click="loadOlder">{{ loading ? '加载更早…' : '加载更早的轮次' }}</button>
        <section v-for="(turn,index) in turns" :key="turn.key" :data-turn-key="turn.key" class="turn">
          <div v-if="turn.user && turn.user.kind === 'user'" class="msg user" :data-item-key="turn.user.key"><span class="bubble">{{ turn.user.text }}</span><div class="history-images"><img v-for="id in turn.user.attachmentIds" :key="id" :src="attachmentUrl(id)" alt="已提交图片" loading="lazy" @load="scheduleRestore" /></div><button class="copy-btn" @click="copy(turn.user.text)">{{ copied === turn.user.text ? '已复制' : '复制' }}</button></div>
          <template v-for="(segment,segmentIndex) in turn.segments" :key="segment.key">
            <ProcessGroup v-if="segment.kind === 'process'" :data-item-key="segment.key" :items="segment.items" :group-key="segment.key" :active="!!appState.sessionState?.running && index===turns.length-1 && segmentIndex===turn.segments.length-1" />
            <div v-else-if="segment.item.kind === 'text'" class="msg agent" :data-item-key="segment.item.key"><TextOutput :item="segment.item" /><button class="copy-btn" @click="copy(segment.item.text)">{{ copied === segment.item.text ? '已复制' : '复制' }}</button></div>
            <div v-else-if="segment.item.kind === 'user'" class="msg guidance" :data-item-key="segment.item.key"><small>已加入本轮上下文</small><p>{{ segment.item.text }}</p><div class="history-images"><img v-for="id in segment.item.attachmentIds" :key="id" :src="attachmentUrl(id)" alt="补充图片" loading="lazy" @load="scheduleRestore" /></div></div>
            <div v-else-if="'text' in segment.item" class="msg" :class="segment.item.kind" :data-item-key="segment.item.key">{{ segment.item.text }}</div>
          </template>
          <footer class="turn-actions"><button @click="showProcess(turn)">查看过程</button><button @click="showUsage(turn)">本轮用量</button></footer>
        </section>
      </div>
    </div>
    <div class="reading-ops"><ActionMenu v-if="turns.length" variant="turns" label="跳转已加载轮次"><template #trigger><button class="jump-trigger" aria-label="跳转已加载轮次"><List :size="16" aria-hidden="true" /></button></template>
      <DropdownMenuCheckboxItem v-model="preferences.wrapText" @select.prevent><span class="wrap-check" aria-hidden="true"><Check v-if="preferences.wrapText" :size="12" /></span>自动换行</DropdownMenuCheckboxItem>
      <div class="turn-menu-list"><DropdownMenuItem v-for="(turn,index) in turns" :key="turn.key" as-child><button class="turn-option" @click="jump(turn.key)"><span class="turn-number">{{ index+1 }}</span><span class="turn-title">{{ turn.user && 'text' in turn.user ? turn.user.text.slice(0,80) : '历史过程' }}</span></button></DropdownMenuItem></div>
    </ActionMenu><button v-if="!following" class="latest" aria-label="↓ 返回最新内容" @click="latest"><ArrowDown :size="14" aria-hidden="true" />返回最新内容</button></div>
  </div>
</template>
<style scoped>
.transcript-shell { flex:1; min-height:0; position:relative; display:flex; flex-direction:column; }.transcript { flex:1; min-height:0; overflow:auto; overscroll-behavior:contain; overflow-anchor:none; }.transcript-body { padding:24px max(16px,calc((100% - 840px)/2)); }.turn-actions { display:flex; gap:12px; }.turn-actions button { padding:2px 4px; border:0; font-size:11px; color:var(--text-dim); background:none; }
.turn { display:grid; gap:13px; margin-bottom:28px; }.msg { min-width:0; overflow-wrap:anywhere; }.user { justify-self:end; max-width:85%; }.bubble { display:block; background:var(--blue-soft); border:1px solid var(--line); padding:10px 14px; border-radius:10px; white-space:pre-wrap; }.agent { line-height:1.8; }.copy-btn { font-size:10px; border:0; background:none; color:var(--text-dim); padding:3px 0; }.guidance { border-left:2px solid var(--blue); padding:8px 12px; background:var(--blue-soft); font-size:12px; white-space:pre-wrap; }.guidance small { color:var(--blue); }.error { color:var(--red); background:var(--panel); padding:10px; border:1px solid var(--red); border-radius:8px; white-space:pre-wrap; }.signal { font-size:11px; color:var(--yellow); }.history-images { display:flex; flex-wrap:wrap; gap:6px; }.history-images img { width:96px; height:96px; object-fit:contain; margin-top:8px; border:1px solid var(--line); border-radius:6px; }.load-older { display:block; margin:0 auto 20px; font-size:11px; border:0; color:var(--text-dim); }.reading-ops { position:absolute; right:max(20px,calc((100% - 840px)/2)); bottom:8px; display:flex; align-items:center; gap:6px; pointer-events:none; }.reading-ops>* { pointer-events:auto; }.jump-trigger { width:28px; height:28px; display:grid; place-items:center; padding:0; background:var(--bg-elevated); border-radius:50%; box-shadow:0 1px 3px #00008; }.latest { display:flex; align-items:center; gap:4px; padding:4px 9px; font-size:11px; border-radius:16px; box-shadow:var(--shadow); background:var(--bg-elevated); }
@media(max-width:767px) { .transcript-body { padding:16px 12px; }.user { max-width:92%; }.msg { font-size:15px; }.turn-actions { gap:8px; }.turn-actions button,.copy-btn { font-size:12px; }.reading-ops { right:12px; bottom:6px; }.copy-btn { padding:4px 8px; } }
.turn-number { flex:0 0 20px; color:var(--text-dim); font-variant-numeric:tabular-nums; }.turn-title { flex:1; min-width:0; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
.turn-menu-list { min-height:0; overflow:auto; overscroll-behavior:contain; border-top:1px solid var(--line); margin-top:4px; padding-top:4px; }.wrap-check { width:14px; height:14px; display:grid; place-items:center; border:1px solid var(--text-dim); border-radius:3px; }
.transcript-shell,.transcript-body,.turn { min-width:0; max-width:100%; }.turn { grid-template-columns:minmax(0,1fr); }.transcript { overflow-x:hidden; }
@media(max-width:767px) { .chrome-hidden .transcript-body { padding-top:max(16px,env(safe-area-inset-top)); padding-bottom:calc(56px + env(safe-area-inset-bottom)); }.chrome-hidden .reading-ops { bottom:max(10px,env(safe-area-inset-bottom)); } }
</style>
