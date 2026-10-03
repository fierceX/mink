<script setup lang="ts">
import { TooltipContent, TooltipPortal, TooltipProvider, TooltipRoot, TooltipTrigger } from 'reka-ui';
defineOptions({ inheritAttrs: false });
withDefaults(defineProps<{ label: string; hint?: string; disabled?: boolean; tone?: 'default' | 'primary' | 'danger'; busy?: boolean }>(), { tone:'default' });
</script>
<template>
  <TooltipProvider :delay-duration="550" :ignore-non-keyboard-focus="true"><TooltipRoot>
    <TooltipTrigger as-child><button v-bind="$attrs" type="button" class="icon-button" :class="[tone,{ busy }]" :aria-label="label" :disabled="disabled" :aria-busy="busy || undefined"><slot /></button></TooltipTrigger>
    <TooltipPortal><TooltipContent class="control-tooltip" side="top" :side-offset="8" :collision-padding="8">{{ hint || label }}</TooltipContent></TooltipPortal>
  </TooltipRoot></TooltipProvider>
</template>
<style scoped>
.icon-button { display:inline-flex; align-items:center; justify-content:center; flex-shrink:0; width:32px; height:32px; padding:0; border:0; border-radius:9px; color:var(--text-soft); background:transparent; box-shadow:none; }
.icon-button:hover { color:var(--text); background:var(--panel-2); }.icon-button.primary { border-radius:50%; color:var(--bg-elevated); background:var(--text); box-shadow:none; }.icon-button.primary:hover { background:var(--text-soft); }.icon-button.primary:disabled { opacity:.25; }.icon-button.danger { border:1px solid var(--line); color:var(--text); }.icon-button.danger:hover { color:var(--red); background:var(--panel-2); }
.control-tooltip { z-index:110; max-width:280px; border-radius:7px; padding:6px 10px; color:var(--bg-elevated); background:var(--text); font-size:11px; line-height:1.6; box-shadow:var(--shadow); }
.busy :deep(svg) { animation:spin 1s linear infinite; }@keyframes spin { to { transform:rotate(360deg); } }
@media(pointer:coarse) { .icon-button { width:36px; height:36px; } }
</style>
