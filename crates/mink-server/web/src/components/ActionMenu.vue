<script setup lang="ts">
import { DropdownMenuContent, DropdownMenuPortal, DropdownMenuRoot, DropdownMenuTrigger } from 'reka-ui';
import { Ellipsis } from '@lucide/vue';
withDefaults(defineProps<{ label: string; variant?: 'actions' | 'turns' }>(), { variant:'actions' });
</script>
<template>
  <DropdownMenuRoot :modal="false">
    <DropdownMenuTrigger as-child><slot name="trigger"><button class="menu-trigger" :aria-label="label"><Ellipsis :size="18" aria-hidden="true" /></button></slot></DropdownMenuTrigger>
    <DropdownMenuPortal><DropdownMenuContent class="action-menu" :class="{ 'turn-menu':variant==='turns' }" align="end" :side-offset="6" :collision-padding="8" :aria-label="label + '选项'" @escape-key-down="$event.stopImmediatePropagation()"><slot /></DropdownMenuContent></DropdownMenuPortal>
  </DropdownMenuRoot>
</template>
<style scoped>
.menu-trigger { display:grid; place-items:center; width:32px; height:32px; padding:0; border:0; background:transparent; border-radius:8px; }
.menu-trigger:hover,.menu-trigger[data-state="open"] { background:var(--panel-2); }
@media(pointer:coarse) { .menu-trigger { width:36px; height:36px; } }
</style>
<!-- The teleported third-party content does not inherit this component's scope ID. -->
<style>
.action-menu { display:flex; flex-direction:column; z-index:90; min-width:200px; max-width:calc(100vw - 16px); max-height:min(360px,var(--reka-dropdown-menu-content-available-height)); overflow:auto; padding:5px; border:1px solid var(--line); border-radius:12px; background:var(--bg-elevated); box-shadow:var(--shadow); outline:none; }
.action-menu.turn-menu { width:min(300px,calc(100vw - 16px)); min-width:0; max-height:min(260px,40dvh,var(--reka-dropdown-menu-content-available-height)); }
.action-menu [role="menuitem"],.action-menu [role="menuitemcheckbox"] { display:flex; flex-shrink:0; align-items:center; gap:8px; width:100%; min-height:32px; text-align:left; padding:7px 10px; border:0; background:transparent; border-radius:7px; outline:none; font-size:12px; }
.action-menu [data-highlighted] { background:var(--panel-2); }
.action-menu [data-disabled] { opacity:.45; cursor:default; }
.action-menu small { display:block; padding:6px 10px; color:var(--text-dim); font-size:11px; overflow-wrap:anywhere; }
@media(pointer:coarse) { .action-menu [role="menuitem"],.action-menu [role="menuitemcheckbox"] { min-height:36px; } }
</style>
