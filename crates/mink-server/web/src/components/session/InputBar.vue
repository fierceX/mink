<script setup lang="ts">
import { computed, ref, nextTick, watch, useId } from "vue";
import { openSession } from "../../lib/sessionController";
import { appState } from "../../lib/store";
import { api, sessionUrl } from "../../lib/api";
import { identity, viewFor, flash, type Upload } from "../../lib/workbench";
import type { HumanInput, InputReceipt } from "../../lib/types";
import { useMedia } from "../../lib/useOverlay";
import { Plus, ArrowUp, Square, LoaderCircle, RotateCw, X, Pencil } from "@lucide/vue";
import IconButton from "../IconButton.vue";
const mobileInput = useMedia("(pointer: coarse), (max-width:767px)");
const props = withDefaults(defineProps<{ collapsed?:boolean }>(), { collapsed:false });
const emit = defineEmits<{ readingLock:[value:boolean]; showComposer:[] }>();
const hintId = useId();
const stopping = ref(false);
const reopening = ref(false);
const changing = ref<string | null>(null);
const recovering = ref<string | null>(null);
const view = computed(() => viewFor());
const state = computed(() => appState.sessionState);
const picker = ref<HTMLInputElement | null>(null);
const textarea = ref<HTMLTextAreaElement | null>(null);
const errorHint = ref("");
const editing = ref<string | null>(null); const editText = ref("");
const pending = computed(() => state.value?.inputs?.filter(item => ["pending", "applying", "unapplied"].includes(item.status)) ?? []);
const running = computed(() => state.value?.running ?? false);
const readingLocked = computed(() => !!(pending.value.length || view.value.failures.length || view.value.uploads.length || errorHint.value || editing.value || changing.value || recovering.value || stopping.value || reopening.value || view.value.busy || state.value?.desynced || ['cancelling','closing','closed'].includes(state.value?.phase ?? '')));
const collapsed = computed(() => props.collapsed && !readingLocked.value);
watch(readingLocked, value => emit('readingLock', value), { immediate:true });
async function showComposer() { emit('showComposer'); await nextTick(); resize(); textarea.value?.focus({ preventScroll:true }); }
const limits = computed(() => state.value?.imageLimits);
const canSend = computed(() => !view.value.busy && !stopping.value && !state.value?.desynced && !["cancelling","closing","closed"].includes(state.value?.phase ?? "") && !!state.value && view.value.uploads.every(upload => upload.status === "ready") && (!!view.value.draft.trim() || !!view.value.uploads.length));
const newId = () => globalThis.crypto?.randomUUID?.() ?? `request-${Date.now()}-${Math.random().toString(36).slice(2)}`;
function follow(key: string) { viewFor(key).follow = true; window.dispatchEvent(new CustomEvent("mink-follow", { detail: key })); }
async function send() {
  if (!canSend.value || !state.value) return;
  const id = state.value.sessionId; const project = appState.currentProjectKey!; const key = identity(project, id); const origin = viewFor(key);
  const text = origin.draft.trim(); const uploads = [...origin.uploads];
  const input: HumanInput = { request_id: newId(), text, attachment_ids: uploads.flatMap(upload => upload.attachment ? [upload.attachment.id] : []), target_turn_id: running.value ? state.value.currentTurn ?? null : null };
  if (running.value && !input.target_turn_id) { errorHint.value = "当前会话尚未取得引导能力，请等待同步"; return; }
  origin.busy = true; origin.draft = ""; origin.uploads = []; errorHint.value = ""; follow(key);
  try {
    const response = await api.submitInput(id, input, project);
    if (response.code !== 200) throw new Error(response.message);
    // Receipt belongs to the originating session. Formal history owns the echo.
    if (identity() === key && appState.sessionState) {
      const receipts = appState.sessionState.inputs ?? [];
      if (!receipts.some(receipt => receipt.input_id === response.data.input_id)) receipts.push(response.data);
      appState.sessionState.inputs = receipts;
    }
  } catch(error) {
    // Resolve uncertain network reception before offering an explicit retry.
    let received = false;
    try { const response = await api.inputs(id, project, input.request_id); received = response.code === 200 && response.data.some(receipt => receipt.input_id === input.request_id); } catch { /* uncertain reception stays visible */ }
    if (!received) origin.failures.push({ id: input.request_id, text, attachments: input.attachment_ids, error: `提交失败或接收状态待确认：${String(error)}` });
    if (identity() === key) errorHint.value = received ? "server 已接收此输入" : "输入已保留。请确认接收状态后重试。";
  } finally { origin.busy = false; }
}
async function recoverFailure(failure: { id: string; text: string; attachments: string[]; error: string }) {
  const id = state.value?.sessionId; const project = appState.currentProjectKey!; if (!id) return;
  if (recovering.value) return; recovering.value = failure.id;
  const origin = viewFor(identity(project, id));
  try {
    const received = await api.inputs(id, project, failure.id); if (received.code !== 200) throw new Error(received.message);
    if (received.data.some(receipt => receipt.input_id === failure.id)) { origin.failures = origin.failures.filter(item => item.id !== failure.id); flash("已确认 server 接收"); return; }
    if (origin.draft.trim()) { flash("当前已有草稿，请先发送或清空后恢复失败输入"); return; }
    origin.draft = failure.text;
    origin.uploads = failure.attachments.map(attachmentId => ({ key: attachmentId, name: attachmentId, bytes: 0, preview: sessionUrl(id, `/attachments/${attachmentId}`, project), status: "ready", attachment: { id: attachmentId, bytes: 0, width: 0, height: 0, mime: "" } }));
    origin.failures = origin.failures.filter(item => item.id !== failure.id);
  } catch(error) { flash(`接收状态待确认：${String(error)}`); } finally { recovering.value = null; }
}
async function reopen() {
  const current=appState.sessions.find(row => row.id===state.value?.sessionId && row.project_key===appState.currentProjectKey);
  if (!current || reopening.value) return; reopening.value = true;
  try { await openSession(current); } catch(error) { flash(`重新打开失败：${String(error)}`); } finally { reopening.value = false; }
}
async function interrupt() {
  const id = state.value?.sessionId; const project = appState.currentProjectKey!; const key = identity(project, id); const generation = state.value?.generation; const turn = state.value?.currentTurn; if (!id || stopping.value) return; stopping.value = true;
  try { const response = await api.interrupt(id, project); if (response.code !== 200) throw new Error(response.message); if (identity() === key && appState.sessionState?.generation === generation && appState.sessionState?.currentTurn === turn && appState.sessionState?.running) appState.sessionState.phase = "cancelling"; } catch(error) { flash(`停止失败：${String(error)}`); } finally { stopping.value = false; }
}
async function change(receipt: InputReceipt, action: "edit" | "withdraw" | "resume") {
  const id = state.value?.sessionId; const project = appState.currentProjectKey!; const key = identity(project, id); if (!id || changing.value) return; changing.value = receipt.input_id;
  try {
    const response = action === "edit" ? await api.editInput(id, receipt, editText.value, project) : action === "withdraw" ? await api.withdrawInput(id, receipt, project) : await api.resumeInput(id, receipt, project);
    if (response.code !== 200) throw new Error(response.message);
    if (identity() === key) {
      const receipts = appState.sessionState?.inputs;
      const index = receipts?.findIndex(item => item.input_id === response.data.input_id) ?? -1;
      if (receipts && index >= 0 && receipts[index].revision <= response.data.revision && receipts[index].status !== "applied") receipts[index] = response.data;
      editing.value = null; if (action === "resume") follow(key);
    }
  } catch(error) { flash(String(error)); } finally { changing.value = null; }
}
async function uploadFiles(files: File[]) {
  const id = state.value?.sessionId; const project = appState.currentProjectKey!; const cap = limits.value;
  if (!id || !cap) { flash("此会话不支持图片输入"); return; }
  const origin = viewFor(identity(project, id));
  for (const file of files) {
    const maxCount = Math.min(8, cap.max_images_per_request);
    if (origin.uploads.length >= maxCount) { flash(`最多选择 ${maxCount} 张图片`); break; }
    if (file.size > Math.min(cap.max_image_bytes, 16 * 1024 * 1024) || origin.uploads.reduce((sum, upload) => sum + upload.bytes, 0) + file.size > Math.min(cap.max_image_bytes_per_request, 16 * 1024 * 1024)) { flash("图片超过会话字节上限"); continue; }
    const format = file.type.replace("image/", "").replace("jpg", "jpeg");
    if (!cap.allowed_mime.includes(format)) { flash(`不支持的图片格式：${file.type || file.name}`); continue; }
    const upload: Upload = { key: newId(), name: file.name, bytes: file.size, preview: URL.createObjectURL(file), status: "uploading" };
    origin.uploads.push(upload);
    // Operate on the reactive entry so late uploads update their original draft.
    const entry = origin.uploads[origin.uploads.length - 1];
    void api.upload(id, file, project).then(response => {
      if (response.code !== 200) throw new Error(response.message);
      entry.status = "ready"; entry.attachment = response.data; URL.revokeObjectURL(entry.preview); entry.preview = sessionUrl(id, `/attachments/${response.data.id}`, project);
    }).catch(error => { entry.status = "failed"; entry.error = String(error); });
  }
  if (picker.value) picker.value.value = "";
}
function removeUpload(upload: Upload) { const origin = view.value; origin.uploads = origin.uploads.filter(item => item.key !== upload.key); if (upload.preview.startsWith("blob:")) URL.revokeObjectURL(upload.preview); }
function paste(event: ClipboardEvent) { const files = [...event.clipboardData?.files ?? []].filter(file => file.type.startsWith("image/")); if (files.length) { event.preventDefault(); void uploadFiles(files); } }
function drop(event: DragEvent) { event.preventDefault(); void uploadFiles([...event.dataTransfer?.files ?? []]); }
function onKeydown(event: KeyboardEvent) {
  if (event.isComposing || event.keyCode === 229) return;
  if (event.key === "Enter" && (event.metaKey || event.ctrlKey || (!event.shiftKey && !mobileInput.value))) { event.preventDefault(); void send(); }
}
function resize() { if (textarea.value) { textarea.value.style.height = "auto"; textarea.value.style.height = `${Math.min(textarea.value.scrollHeight, 200)}px`; } }
watch(() => view.value.draft, () => { void nextTick(resize); }, { immediate: true });
const blockedHint = computed(() => state.value?.phase === "closed" ? "会话已关闭，重新打开后可继续" : state.value?.desynced ? "正在恢复连接，草稿可继续编辑" : stopping.value || ["cancelling", "closing"].includes(state.value?.phase ?? "") ? "正在停止，草稿已保留" : view.value.uploads.some(upload => upload.status !== "ready") ? "请等待上传完成，或移除失败的附件后提交" : "");
const label = (receipt: InputReceipt) => !running.value && receipt.status === "applying" ? "本轮已结束，接收状态待确认，请重新打开对账" : receipt.status === "unapplied" ? "本轮已结束，尚未应用" : receipt.status === "applying" ? "正在加入本轮上下文" : "已接收，等待当前步骤完成";
</script>
<template>
  <div class="composer" :class="{ collapsed }" @dragover.prevent @drop="drop"><div v-show="!collapsed" class="composer-card">
    <div v-if="pending.length || view.failures.length" class="input-receipts" aria-label="待处理输入">
    <div v-for="receipt in pending" :key="receipt.input_id" class="pending-input">
      <small>{{ label(receipt) }}</small><p>{{ receipt.input.text }}</p>
      <template v-if="receipt.status !== 'applying'"><button :disabled="!!changing || state?.desynced" @click="editing = receipt.input_id; editText = receipt.input.text">编辑</button><button :disabled="!!changing || state?.desynced" @click="change(receipt, 'withdraw')">移除</button><button v-if="receipt.status === 'unapplied'" :disabled="!!changing || running || state?.desynced || ['cancelling','closing','closed'].includes(state?.phase ?? '')" @click="change(receipt, 'resume')">用于下一轮</button></template>
      <div v-if="editing === receipt.input_id"><textarea v-model="editText" aria-label="编辑待处理输入"></textarea><button :disabled="!!changing || !editText.trim() || state?.desynced" @click="change(receipt, 'edit')">{{ changing === receipt.input_id ? '保存中…' : '保存' }}</button><button :disabled="!!changing" @click="editing = null">取消</button></div>
    </div>
    <div v-for="failure in view.failures" :key="failure.id" class="failed-input" role="alert"><p>{{ failure.error }}</p><span>{{ failure.text }}</span><button :disabled="!!recovering" @click="recoverFailure(failure)">{{ recovering === failure.id ? '正在确认…' : '确认接收状态并恢复' }}</button></div>
    </div>
    <div v-if="errorHint" class="error-hint" role="status">{{ errorHint }}</div>
    <div class="uploads"><div v-for="upload in view.uploads" :key="upload.key" class="upload"><img :src="upload.preview" :alt="upload.name" /><small>{{ upload.name }} · {{ Math.ceil(upload.bytes / 1024) }} KiB<br>{{ upload.status === 'uploading' ? '上传中…' : upload.status === 'failed' ? upload.error : '已上传' }}</small><button class="remove-upload" @click="removeUpload(upload)" aria-label="移除附件"><X :size="12" aria-hidden="true" /></button></div></div>
    <div class="input-bar"><textarea ref="textarea" v-model="view.draft" rows="1" aria-label="任务或补充指令" :aria-describedby="hintId" :placeholder="running ? '补充任务要求…' : '描述你想完成的任务…'" @keydown="onKeydown" @input="resize" @paste="paste"></textarea></div>
    <div class="composer-actions">
      <input ref="picker" type="file" multiple :accept="limits?.allowed_mime.map(format => `image/${format}`).join(',')" hidden @change="uploadFiles([...(($event.target as HTMLInputElement).files ?? [])])" />
      <IconButton label="选择图片" :disabled="!limits" :hint="limits ? '选择图片，也可粘贴或拖放' : '此会话不支持图片输入'" @click="picker?.click()"><Plus :size="20" aria-hidden="true" /></IconButton>
      <span class="composer-context">{{ running ? '运行中 · 可补充引导' : '' }}</span>
      <IconButton v-if="running" :label="stopping || state?.phase === 'cancelling' ? '正在停止…' : '停止'" tone="danger" @click="interrupt" :disabled="stopping || ['cancelling','closing'].includes(state?.phase ?? '')"><Square :size="13" fill="currentColor" aria-hidden="true" /></IconButton>
      <IconButton v-if="state?.phase === 'closed'" :label="reopening ? '正在打开…' : '重新打开'" tone="primary" :disabled="reopening" :busy="reopening" @click="reopen"><component :is="reopening ? LoaderCircle : RotateCw" :size="18" aria-hidden="true" /></IconButton>
      <IconButton v-else :label="view.busy ? '正在提交' : running ? '提交引导' : '发送'" tone="primary" :disabled="!canSend" :busy="view.busy" :hint="blockedHint || (running ? '提交引导，在下一安全边界加入上下文' : mobileInput ? '发送任务' : '发送 · Enter 提交，Shift+Enter 换行')" @click="send"><component :is="view.busy ? LoaderCircle : ArrowUp" :size="20" aria-hidden="true" /></IconButton>
    </div>
    <small v-if="blockedHint" class="composer-notice" role="status">{{ blockedHint }}</small>
    <small :id="hintId" class="sr-only">{{ blockedHint || (mobileInput ? 'Enter 换行 · 点击按钮提交' : 'Enter 提交 · Shift+Enter 换行') }}</small>
    </div>
    <div v-if="collapsed" class="composer-reading-actions"><button class="composer-reveal" aria-label="显示输入框" @click="showComposer"><Pencil :size="16" aria-hidden="true" /></button><IconButton v-if="running" label="停止" tone="danger" @click="interrupt"><Square :size="13" fill="currentColor" aria-hidden="true" /></IconButton></div>
  </div>
</template>
<style scoped>
.composer { flex-shrink:0; padding:8px max(20px,calc((100% - 840px)/2)) max(18px,env(safe-area-inset-bottom)); background:var(--bg); max-height:calc(var(--viewport-height,100dvh) - 64px); overflow:auto; }
.composer-card { border:1px solid var(--line); border-radius:22px; background:var(--bg-elevated); padding:10px 12px 8px; box-shadow:0 2px 8px #10182804; transition:border-color .15s; }.composer-card:focus-within { border-color:var(--text-dim); }
.input-bar { display:flex; }.input-bar textarea { width:100%; min-height:44px; max-height:200px; resize:none; border:0; border-radius:0; font:inherit; line-height:1.65; padding:6px 4px 8px; background:transparent; box-shadow:none; outline:none; }
.composer-actions { display:flex; align-items:center; gap:8px; }.composer-context { flex:1; min-width:0; color:var(--text-dim); font-size:11px; padding-left:4px; }.composer-notice { display:block; padding:6px 4px 2px; font-size:11px; color:var(--yellow); }
.input-receipts { max-height:min(140px,20dvh); overflow:auto; overscroll-behavior:contain; margin-bottom:8px; }.pending-input,.failed-input { border:1px solid var(--line); border-radius:10px; padding:8px 10px; margin-bottom:8px; font-size:12px; background:var(--panel-2); }.pending-input p { max-height:64px; overflow:auto; white-space:pre-wrap; }.pending-input small { color:var(--blue); }.failed-input,.error-hint { color:var(--red); font-size:12px; }.pending-input textarea { width:100%; }.pending-input button { padding:3px 8px; margin:4px 4px 0 0; }
.uploads { display:flex; flex-wrap:wrap; gap:8px; }.upload { position:relative; display:flex; align-items:center; gap:8px; padding:6px 30px 6px 6px; border:1px solid var(--line); border-radius:12px; max-width:100%; margin-bottom:8px; }.upload img { width:44px; height:44px; object-fit:cover; border-radius:7px; }.upload small { font-size:11px; overflow-wrap:anywhere; max-width:180px; }.remove-upload { position:absolute; top:4px; right:4px; display:grid; place-items:center; width:24px; height:24px; padding:0; border:0; border-radius:50%; background:var(--panel-2); }
@media(max-width:767px) { textarea { font-size:16px!important; }.composer { padding:6px 10px max(10px,env(safe-area-inset-bottom)); }.composer-card { padding:8px 10px 7px; border-radius:20px; }.input-bar textarea { min-height:40px; }.composer-context { font-size:10px; } }
.composer.collapsed { height:0; padding:0; overflow:visible; }.composer-reading-actions { position:absolute; bottom:max(8px,env(safe-area-inset-bottom)); left:12px; z-index:25; display:flex; gap:8px; align-items:center; }.composer-reveal { width:36px; height:36px; display:grid; place-items:center; padding:0; border-radius:50%; box-shadow:var(--shadow); background:var(--bg-elevated); }
.composer-reading-actions :deep(.icon-button) { width:36px; height:36px; border-radius:50%; background:var(--bg-elevated); box-shadow:var(--shadow); }
</style>
