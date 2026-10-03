<script setup lang="ts">
import { uiState } from "../../lib/store";
import Transcript from "./Transcript.vue";
import InputBar from "./InputBar.vue";
import DetailPanel from "./DetailPanel.vue";
</script>
<template><div class="session-page"><div class="chat"><Transcript /><InputBar /></div><DetailPanel v-if="uiState.ctxOpen" /></div></template>
<style scoped>
.session-page { flex:1; min-height:0; display:flex; position:relative; }.chat { display:flex; flex-direction:column; flex:1; min-width:0; min-height:0; }
</style>
