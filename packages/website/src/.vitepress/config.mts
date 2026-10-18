import { defineConfig } from "vitepress";
import release from "./release.json" with { type: "json" };
import { author } from "./theme/site";

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
// the public address of the site, for canonical links, the sitemap and social cards (absolute URLs)
const siteUrl = (process.env.SITE_URL ?? `https://mihailtd.github.io${base}`).replace(/\/?$/, "/");

const description =
  "graphpg turns a PostgreSQL schema into type-safe GraphQL and REST APIs, with the GraphQL SDL and OpenAPI 3.1 files your TypeScript types are generated from.";

// structured data: what graphpg is and who builds it, linking the project to its author's sites
const structuredData = {
  "@context": "https://schema.org",
  "@type": "SoftwareSourceCode",
  name: "graphpg",
  description,
  url: siteUrl,
  codeRepository: release.repo,
  programmingLanguage: ["Rust", "SQL"],
  runtimePlatform: "PostgreSQL",
  license: ["https://opensource.org/licenses/MIT", "https://www.apache.org/licenses/LICENSE-2.0"],
  version: release.version,
  author: {
    "@type": "Person",
    name: author.name,
    url: author.site,
    sameAs: [author.site, author.github, author.company.url],
    worksFor: { "@type": "Organization", name: author.company.name, url: author.company.url },
  },
};

export default defineConfig({
  title: "graphpg",
  description,
  base,
  cleanUrls: true,
  lastUpdated: true,
  appearance: true,
  // the docs link to a local server (localhost:4000/graphiql) as a place to go once it runs
  ignoreDeadLinks: "localhostLinks",
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: `${base}logo.svg` }],
    ["meta", { name: "theme-color", content: "#336791" }],
    ["meta", { name: "author", content: author.name }],
    ["link", { rel: "author", href: author.site }],
    ["meta", { property: "og:type", content: "website" }],
    ["meta", { property: "og:site_name", content: "graphpg" }],
    ["meta", { property: "og:image", content: `${siteUrl}og.png` }],
    ["meta", { property: "og:image:alt", content: "graphpg: GraphQL and REST, generated from PostgreSQL" }],
    ["meta", { name: "twitter:card", content: "summary_large_image" }],
    ["meta", { name: "twitter:image", content: `${siteUrl}og.png` }],
    ["script", { type: "application/ld+json" }, JSON.stringify(structuredData)],
  ],
  sitemap: { hostname: siteUrl },
  // per page: the canonical address, and a social card titled and described like the page
  transformHead({ pageData, title, description: pageDescription }) {
    const path = pageData.relativePath.replace(/(^|\/)index\.md$/, "$1").replace(/\.md$/, "");
    const url = `${siteUrl}${path}`;
    return [
      ["link", { rel: "canonical", href: url }],
      ["meta", { property: "og:url", content: url }],
      ["meta", { property: "og:title", content: title }],
      ["meta", { property: "og:description", content: pageDescription }],
      ["meta", { name: "twitter:title", content: title }],
      ["meta", { name: "twitter:description", content: pageDescription }],
    ];
  },
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
            { text: "End-to-end type safety", link: "/docs/type-safety" },
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
  },
});
