<script setup lang="ts">
import { computed } from "vue";
import { renderMarkdown } from "../../../lib/markdown";

const props = defineProps<{ item?: { text: string }; content?: string }>();
const html = computed(() => renderMarkdown(props.item?.text ?? props.content ?? ""));
</script>

<template>
  <div class="md-body" v-html="html"></div>
</template>
