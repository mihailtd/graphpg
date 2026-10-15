import { defineConfig } from "vitepress";
import release from "./release.json" with { type: "json" };

// GitHub's heading slugs: lower case, punctuation dropped (dots included), each space a hyphen
function githubSlug(heading: string): string {
  return heading
    .trim()
    .toLowerCase()
    .replace(/<[^>]*>/g, "")
    .replace(/[^\p{L}\p{N}\p{M}\- _]/gu, "")
    .replace(/ /g, "-");
}

// GitHub Pages serves a project site under /<repository>/; SITE_BASE overrides it (a custom domain: "/")
const base = process.env.SITE_BASE ?? "/graphpg/";

export default defineConfig({
  title: "graphpg",
  description:
    "A GraphQL and REST server for PostgreSQL: point it at a database and it serves both APIs, generated from the schema.",
  base,
  cleanUrls: true,
  lastUpdated: true,
  appearance: true,
  // the docs link to a local server (localhost:4000/graphiql) as a place to go once it runs
  ignoreDeadLinks: "localhostLinks",
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: `${base}logo.svg` }],
    ["meta", { name: "theme-color", content: "#336791" }],
    ["meta", { property: "og:type", content: "website" }],
    ["meta", { property: "og:title", content: "graphpg: GraphQL and REST for PostgreSQL" }],
    ["meta", { property: "og:image", content: `${base}og.png` }],
    ["meta", { name: "twitter:card", content: "summary_large_image" }],
    ["meta", { name: "twitter:image", content: `${base}og.png` }],

    [
      "meta",
      {
        property: "og:description",
        content: "One Rust binary reads your PostgreSQL schema and serves a GraphQL API and a REST API from it.",
      },
    ],
  ],
  // pnpm-workspace.yaml overrides esbuild to a version that no longer lowers syntax for the
  // browser list Vite 5 defaults to; every browser the site needs supports ES2022
  vite: { build: { target: "es2022" } },
  markdown: {
    // heading ids as GitHub makes them, which is what the docs' #anchors were written against
    anchor: { slugify: githubSlug },
    theme: { light: "github-light", dark: "github-dark" },
    config(md) {
      // ```mermaid blocks render as diagrams in the browser (see theme/components/Mermaid.vue)
      const fence = md.renderer.rules.fence;
      md.renderer.rules.fence = (tokens, idx, options, env, self) => {
        const token = tokens[idx];
        if (token.info.trim() === "mermaid") {
          return `<Mermaid code="${encodeURIComponent(token.content)}" />`;
        }
        return fence ? fence(tokens, idx, options, env, self) : self.renderToken(tokens, idx, options);
      };
    },
  },
  themeConfig: {
    logo: "/logo.svg",
    nav: [
      { text: "Docs", link: "/docs/", activeMatch: "^/docs/" },
      { text: "GraphQL", link: "/docs/graphql-api" },
      { text: "REST", link: "/docs/rest-api" },
      { text: "Changelog", link: "/changelog" },
      {
        text: `v${release.version}`,
        items: [
          { text: "Changelog", link: "/changelog" },
          { text: "Releases", link: `${release.repo}/releases` },
          { text: "Versioning", link: "/docs/versioning" },
          { text: "Container image", link: `${release.repo}/pkgs/container/graphpg` },
        ],
      },
    ],
    sidebar: {
      "/": [
        {
          text: "Getting started",
          items: [
            { text: "Introduction", link: "/docs/" },
            { text: "AI agents", link: "/docs/ai-agents" },
            { text: "Configuration", link: "/docs/configuration" },
            { text: "Schema mapping", link: "/docs/schema-mapping" },
          ],
        },
        {
          text: "APIs",
          items: [
            { text: "GraphQL API", link: "/docs/graphql-api" },
            { text: "REST API", link: "/docs/rest-api" },
            { text: "GraphQL and OpenAPI parity", link: "/docs/graphql-openapi-parity" },
            { text: "Subscriptions", link: "/docs/subscriptions" },
          ],
        },
        {
          text: "Security",
          items: [
            { text: "Authentication and RLS", link: "/docs/authentication" },
            { text: "Federation and RBAC", link: "/docs/federation-and-rbac" },
          ],
        },
        {
          text: "Beyond the schema",
          items: [
            { text: "Event triggers", link: "/docs/events" },
            { text: "PostgreSQL extensions", link: "/docs/extensions" },
            { text: "Other databases and files", link: "/docs/foreign-data" },
            { text: "Extending graphpg", link: "/docs/extending" },
          ],
        },
        {
          text: "Operations",
          items: [
            { text: "Performance", link: "/docs/performance" },
            { text: "Observability", link: "/docs/observability" },
          ],
        },
        {
          text: "Project",
          items: [
            { text: "Comparison", link: "/docs/comparison" },
            { text: "Versioning", link: "/docs/versioning" },
            { text: "Pipelines", link: "/docs/pipelines" },
            { text: "Changelog", link: "/changelog" },
          ],
        },
      ],
    },
    outline: { level: [2, 3] },
    search: { provider: "local" },
    socialLinks: [{ icon: "github", link: release.repo }],
    editLink: {
      // Pages are copies (sync.mjs): edit the source each one names. The function is sent to the
      // browser as source text, so it cannot use anything from this module.
      pattern: ({ frontmatter, filePath }) =>
        `https://github.com/mihailtd/graphpg/edit/main/${frontmatter.source ?? `packages/website/src/${filePath}`}`,
      text: "Edit this page on GitHub",
    },
    footer: {
      message: "Dual-licensed under MIT or Apache-2.0.",
      copyright: `graphpg ${release.version}`,
    },
  },
});
