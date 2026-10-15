<script setup lang="ts">
// The latest release's headline items, read from CHANGELOG.md by sync.mjs.
import { withBase } from "vitepress";
import { computed, ref } from "vue";
import release from "../../release.json";

const latest = release.releases[0];
const shown = 18;
const expanded = ref(false);
const items = computed(() => (expanded.value ? latest.highlights : latest.highlights.slice(0, shown)));
const date = new Date(`${latest.date}T00:00:00Z`).toLocaleDateString("en", {
  year: "numeric",
  month: "long",
  day: "numeric",
  timeZone: "UTC",
});
</script>

<template>
  <div class="release">
    <div class="release-head">
      <span class="release-badge">v{{ latest.version }}</span>
      <span class="release-date">{{ date }}</span>
    </div>
    <ul class="release-items">
      <li v-for="item in items" :key="item">{{ item }}</li>
    </ul>
    <div class="release-actions">
      <button v-if="latest.highlights.length > shown" type="button" class="release-more" @click="expanded = !expanded">
        {{ expanded ? "Show fewer" : `+ ${latest.highlights.length - shown} more` }}
      </button>
      <a :href="withBase('/changelog')">Read the changelog →</a>
    </div>
  </div>
</template>

<style scoped>
.release {
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  padding: 24px;
  background: var(--vp-c-bg-soft);
}
.release-head {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 16px;
}
.release-badge {
  font-family: var(--vp-font-family-mono);
  font-weight: 600;
  font-size: 14px;
  padding: 2px 10px;
  border-radius: 999px;
  background: var(--vp-c-brand-soft);
  color: var(--vp-c-brand-1);
}
.release-date {
  color: var(--vp-c-text-2);
  font-size: 14px;
}
.release-items {
  list-style: none;
  padding: 0;
  margin: 0;
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
.release-items li {
  margin: 0;
  font-size: 13px;
  line-height: 1.4;
  padding: 4px 10px;
  border-radius: 6px;
  border: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg);
}
.release-actions {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  margin-top: 16px;
  font-size: 14px;
}
.release-more {
  color: var(--vp-c-text-2);
  font-size: 14px;
}
.release-more:hover {
  color: var(--vp-c-brand-1);
}
</style>
