// Copies the repository's documentation into the site, so the docs keep one source: docs/*.md,
// README.md (the introduction) and CHANGELOG.md. Links are rewritten for the site: links between
// the copied pages stay relative, links to the rest of the repository go to GitHub. It also writes
// the release data the home page shows (src/.vitepress/release.json). Run before every build.
import { mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, posix, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../..");
const src = join(here, "src");
const repo = "https://github.com/mihailtd/graphpg";

// repository path of a source → site path of the page made from it
const pages = new Map([
  ["README.md", "docs/index.md"],
  ["CHANGELOG.md", "changelog.md"],
]);
for (const name of readdirSync(join(root, "docs"))) {
  if (name.endsWith(".md")) pages.set(`docs/${name}`, `docs/${name}`);
}

function rewriteTarget(target, from) {
  if (/^([a-z]+:|#|\/)/i.test(target)) return target;
  const [path, hash = ""] = target.split(/(?=#)/);
  const repoPath = posix.normalize(posix.join(posix.dirname(from), path)).replace(/\/$/, "");
  // the site's own assets (images the docs and README show) are served from its root
  const publicDir = "packages/website/src/public/";
  if (repoPath.startsWith(publicDir)) return `/${repoPath.slice(publicDir.length)}${hash}`;
  const page = pages.get(repoPath);
  if (page) {
    const link = posix.relative(posix.dirname(pages.get(from)), page) || posix.basename(page);
    return (link.startsWith(".") ? link : `./${link}`) + hash;
  }
  let kind = "blob";
  try {
    if (statSync(join(root, repoPath)).isDirectory()) kind = "tree";
  } catch {
    console.warn(`sync: ${from} links to ${target}, which does not exist`);
  }
  return `${repo}/${kind}/main/${repoPath}${hash}`;
}

// Rewrites `[text](target)` outside code: fenced blocks and inline code spans are left alone.
function rewriteLinks(markdown, from) {
  let fenced = false;
  return markdown
    .split("\n")
    .map((line) => {
      if (/^\s*(```|~~~)/.test(line)) fenced = !fenced;
      if (fenced) return line;
      return line
        .split(/(`[^`]*`)/)
        .map((part, i) =>
          i % 2 ? part : part.replace(/\]\(([^)\s]+)\)/g, (_, target) => `](${rewriteTarget(target, from)})`),
        )
        .join("");
    })
    .join("\n");
}

// The page's first paragraph as plain text, cut at a word near 160 characters: its search and
// social card description.
function firstParagraph(markdown) {
  const paragraph = markdown
    .replace(/^---\n[\s\S]*?\n---\n/, "")
    .split(/\n{2,}/)
    .map((block) => block.trim())
    .find((block) => block && !/^(#|>|\||```|~~~|!\[|<|-|\*|\d+\.)/.test(block));
  if (!paragraph) return undefined;
  const text = paragraph
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/[`*_]/g, "")
    .replace(/\s+/g, " ")
    .trim();
  if (text.length <= 160) return text;
  return `${text.slice(0, text.lastIndexOf(" ", 157))}…`;
}

function frontmatter(fields) {
  const lines = Object.entries(fields).map(([k, v]) => `${k}: ${JSON.stringify(v)}`);
  return `---\n${lines.join("\n")}\n---\n\n`;
}

rmSync(join(src, "docs"), { recursive: true, force: true });
mkdirSync(join(src, "docs"), { recursive: true });

for (const [from, to] of pages) {
  let markdown = readFileSync(join(root, from), "utf8");
  const fields = { source: from };
  const description = firstParagraph(from === "README.md" ? markdown.replace(/^[\s\S]*?^# graphpg\n/m, "") : markdown);
  if (description) fields.description = description;
  if (from === "README.md") {
    // The introduction: the README without its title and cover image, which the site's home
    // page already shows.
    markdown = markdown.replace(/^[\s\S]*?^# graphpg\n/m, "# Introduction\n");
    fields.title = "Introduction";
  }
  writeFileSync(join(src, to), frontmatter(fields) + rewriteLinks(markdown, from));
}

// Release data for the home page: the version, and the latest release's headline items.
const cargo = readFileSync(join(root, "Cargo.toml"), "utf8");
const version = cargo.match(/\[workspace\.package\][^[]*?version\s*=\s*"([^"]+)"/)?.[1];
const changelog = readFileSync(join(root, "CHANGELOG.md"), "utf8");
const releases = [...changelog.matchAll(/^## (\S+) - (\d{4}-\d{2}-\d{2})\n([\s\S]*?)(?=^## |(?![\s\S]))/gm)].map(
  ([, release, date, body]) => ({
    version: release,
    date,
    highlights: [...body.matchAll(/^- \*\*([^*]+?)\*\*/gm)]
      .map(([, title]) => title.replace(/[.:]$/, "").replace(/`/g, ""))
      .filter((title) => !/^(fix|fixed|breaking|docs)\b/i.test(title)),
  }),
);
mkdirSync(join(src, ".vitepress"), { recursive: true });
writeFileSync(join(src, ".vitepress", "release.json"), `${JSON.stringify({ version, repo, releases }, null, 2)}\n`);

console.log(`sync: ${pages.size} pages, version ${version}, ${releases.length} releases`);
console.log(`sync: wrote ${relative(root, src)}`);
