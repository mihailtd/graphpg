<script setup lang="ts">
// Articles about graphpg on the author's blog (theme/site.ts), shown on the home page.
import { articles, author } from "../site";
</script>

<template>
  <div class="writing">
    <a v-for="article in articles" :key="article.url" class="writing-card" :href="article.url">
      <span class="writing-source">mihai.ltd</span>
      <h3>{{ article.title }}</h3>
      <p>{{ article.description }}</p>
      <span class="writing-more">Read the article →</span>
    </a>
  </div>
  <p class="writing-by">
    Written by <a :href="author.site" rel="author">{{ author.name }}</a>, who builds graphpg. Follow along on
    <a :href="author.github">GitHub</a>.
  </p>
</template>

<style scoped>
.writing {
  display: grid;
  gap: 16px;
  grid-template-columns: 1fr;
}

@media (min-width: 768px) {
  .writing {
    grid-template-columns: repeat(2, 1fr);
  }
}

.vp-doc a.writing-card {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 24px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  background: var(--vp-c-bg-soft);
  color: inherit;
  text-decoration: none;
  transition: border-color 0.25s;
}

.vp-doc a.writing-card:hover,
.vp-doc a.writing-card:focus-visible {
  border-color: var(--vp-c-brand-1);
}

.writing-source {
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--vp-c-text-2);
}

.writing-card h3 {
  margin: 0;
  font-size: 18px;
  line-height: 1.35;
}

.writing-card p {
  margin: 0;
  font-size: 14px;
  color: var(--vp-c-text-2);
}

.writing-more {
  margin-top: auto;
  padding-top: 8px;
  font-size: 14px;
  font-weight: 600;
  color: var(--vp-c-brand-1);
}

.writing-by {
  margin-top: 16px;
  font-size: 14px;
  color: var(--vp-c-text-2);
}
</style>
