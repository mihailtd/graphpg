<script setup lang="ts">
// The footer on every page: where to go next in the docs, the project, the writing about it and
// who builds it. Replaces the default theme's one-line footer (themeConfig.footer is unset).
import { useData, withBase } from "vitepress";
import release from "../../release.json";
import { articles, author } from "../site";

const { page } = useData();
const year = new Date().getUTCFullYear();

const columns = [
  {
    title: "Docs",
    links: [
      { text: "Introduction", href: withBase("/docs/") },
      { text: "End-to-end type safety", href: withBase("/docs/type-safety") },
      { text: "AI agents", href: withBase("/docs/ai-agents") },
      { text: "GraphQL API", href: withBase("/docs/graphql-api") },
      { text: "REST API", href: withBase("/docs/rest-api") },
      { text: "Configuration", href: withBase("/docs/configuration") },
    ],
  },
  {
    title: "Project",
    links: [
      { text: "GitHub", href: release.repo },
      { text: "Releases", href: `${release.repo}/releases` },
      { text: "Changelog", href: withBase("/changelog") },
      { text: "Container image", href: `${release.repo}/pkgs/container/graphpg` },
      { text: "Comparison", href: withBase("/docs/comparison") },
      { text: "Security policy", href: `${release.repo}/blob/main/SECURITY.md` },
    ],
  },
  {
    title: "Writing",
    links: [
      ...articles.map((a) => ({ text: a.title, href: a.url })),
      { text: "More on mihai.ltd", href: `${author.site}/blog` },
    ],
  },
];
</script>

<template>
  <footer v-if="page.frontmatter.footer !== false" class="site-footer">
    <div class="site-footer-inner">
      <div class="site-footer-about">
        <a class="site-footer-brand" :href="withBase('/')">
          <img :src="withBase('/logo.svg')" alt="" width="28" height="28" />
          <span>graphpg</span>
        </a>
        <p>
          Type-safe GraphQL and REST APIs, generated from your PostgreSQL schema. One Rust binary, open source under MIT
          or Apache-2.0.
        </p>
        <p class="site-footer-author">
          Built by
          <a :href="author.site" rel="author">{{ author.name }}</a>, software architect.
          Need help with PostgreSQL APIs or AI agents in production?
          <a :href="author.company.url">{{ author.company.name }}</a> takes on consulting work.
        </p>
        <ul class="site-footer-social">
          <li><a :href="author.github">GitHub @{{ author.githubHandle }}</a></li>
          <li><a :href="author.site">mihai.ltd</a></li>
          <li><a :href="author.company.url">innovi.pro</a></li>
        </ul>
      </div>

      <nav v-for="column in columns" :key="column.title" class="site-footer-col" :aria-label="column.title">
        <h2>{{ column.title }}</h2>
        <ul>
          <li v-for="link in column.links" :key="link.href">
            <a :href="link.href">{{ link.text }}</a>
          </li>
        </ul>
      </nav>
    </div>

    <div class="site-footer-base">
      <span>© {{ year }} {{ author.name }} · graphpg {{ release.version }}</span>
      <span>Dual-licensed under MIT or Apache-2.0</span>
    </div>
  </footer>
</template>

<style scoped>
.site-footer {
  border-top: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg-alt);
  padding: 48px 24px 32px;
  font-size: 14px;
  line-height: 1.6;
}

/* the docs layout has a fixed sidebar on wide screens: keep the footer clear of it */
@media (min-width: 960px) {
  :global(.Layout:has(.VPSidebar)) .site-footer {
    padding-left: calc(var(--vp-sidebar-width) + 32px);
  }
}

.site-footer-inner {
  display: grid;
  gap: 32px;
  grid-template-columns: 1fr;
  max-width: 1152px;
  margin: 0 auto;
}

@media (min-width: 640px) {
  .site-footer-inner {
    grid-template-columns: repeat(3, 1fr);
  }
  .site-footer-about {
    grid-column: 1 / -1;
  }
}

@media (min-width: 1100px) {
  .site-footer-inner {
    grid-template-columns: 1.6fr repeat(3, 1fr);
  }
  .site-footer-about {
    grid-column: auto;
  }
}

.site-footer-brand {
  display: inline-flex;
  align-items: center;
  gap: 10px;
  font-weight: 700;
  font-size: 18px;
  color: var(--vp-c-text-1);
  text-decoration: none;
}

.site-footer-about p {
  margin: 12px 0 0;
  max-width: 420px;
  color: var(--vp-c-text-2);
}

.site-footer a {
  color: var(--vp-c-text-2);
  text-decoration: none;
  transition: color 0.2s;
}

.site-footer a:hover,
.site-footer a:focus-visible {
  color: var(--vp-c-brand-1);
  text-decoration: underline;
}

.site-footer-author a {
  color: var(--vp-c-brand-1);
  font-weight: 600;
}

.site-footer h2 {
  margin: 0 0 12px;
  padding: 0;
  border: 0;
  font-size: 13px;
  font-weight: 600;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--vp-c-text-1);
}

.site-footer ul {
  list-style: none;
  margin: 0;
  padding: 0;
}

.site-footer-col li {
  margin: 0;
}

/* 24px rows: tap targets that are easy to hit on a phone */
.site-footer-col li a {
  display: inline-block;
  padding: 4px 0;
}

.site-footer-social {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  margin-top: 16px !important;
}

.site-footer-social a {
  display: inline-block;
  padding: 4px 0;
  font-weight: 600;
}

.site-footer-base {
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  gap: 8px 24px;
  max-width: 1152px;
  margin: 40px auto 0;
  padding-top: 20px;
  border-top: 1px solid var(--vp-c-divider);
  font-size: 13px;
  color: var(--vp-c-text-2);
}
</style>
