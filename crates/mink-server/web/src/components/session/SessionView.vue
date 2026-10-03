<script setup lang="ts">
import { computed, ref } from "vue";
import { uiState } from "../../lib/store";
import { useMedia, useOverlay } from "../../lib/useOverlay";
import Transcript from "./Transcript.vue";
import InputBar from "./InputBar.vue";
import DetailPanel from "./DetailPanel.vue";
withDefaults(defineProps<{ controlsHidden?:boolean }>(), { controlsHidden:false });
defineEmits<{ readingMode:[value:boolean]; showControls:[]; inputLock:[value:boolean] }>();
const narrow = useMedia("(max-width:1023px)");
const detail = ref<InstanceType<typeof DetailPanel> | null>(null);
const detailElement = computed(() => detail.value?.$el as HTMLElement | null);
const overlay = computed(() => narrow.value && uiState.ctxOpen);
useOverlay(detailElement, overlay, () => uiState.ctxOpen = false);
</script>
<template><div class="session-page"><div class="chat" :inert="narrow && uiState.ctxOpen"><Transcript :controls-hidden="controlsHidden" @reading-mode="$emit('readingMode',$event)" @show-controls="$emit('showControls')" /><InputBar :collapsed="controlsHidden" @show-composer="$emit('showControls')" @reading-lock="$emit('inputLock',$event)" /></div><button v-if="narrow && uiState.ctxOpen" class="detail-mask" tabindex="-1" aria-label="关闭详情遮罩" @click="uiState.ctxOpen = false"></button><DetailPanel ref="detail" v-if="uiState.ctxOpen" /></div></template>
<style scoped>
.session-page { flex:1; min-height:0; display:flex; position:relative; }.chat { position:relative; display:flex; flex-direction:column; flex:1; min-width:0; min-height:0; }
.detail-mask { position:absolute; inset:0; background:#0003; border:0; border-radius:0; z-index:35; }
</style>
