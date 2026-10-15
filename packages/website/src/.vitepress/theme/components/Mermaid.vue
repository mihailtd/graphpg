<script setup lang="ts">
// Renders a ```mermaid block in the browser, again when the color scheme changes. mermaid is
// loaded only on pages that have a diagram.
import { useData } from "vitepress";
import { onMounted, ref, watch } from "vue";

const props = defineProps<{ code: string }>();
const { isDark } = useData();
const svg = ref("");
let count = 0;

async function render() {
  const { default: mermaid } = await import("mermaid");
  // SVG text labels: the page's styles reach into HTML labels and clip them. Sized once fonts are in.
  mermaid.initialize({
    startOnLoad: false,
    theme: isDark.value ? "dark" : "default",
    securityLevel: "strict",
    htmlLabels: false,
    flowchart: { htmlLabels: false },
  });
  await document.fonts.ready;
  const id = `mermaid-${Math.random().toString(36).slice(2)}-${count++}`;
  svg.value = (await mermaid.render(id, decodeURIComponent(props.code))).svg;
}

onMounted(render);
watch(isDark, render);
</script>

<template>
  <div class="mermaid-diagram" v-html="svg" />
</template>

<style scoped>
.mermaid-diagram {
  display: flex;
  justify-content: center;
  margin: 16px 0;
  overflow-x: auto;
}
</style>
