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

function frontmatter(fields) {
  const lines = Object.entries(fields).map(([k, v]) => `${k}: ${JSON.stringify(v)}`);
  return `---\n${lines.join("\n")}\n---\n\n`;
}

rmSync(join(src, "docs"), { recursive: true, force: true });
mkdirSync(join(src, "docs"), { recursive: true });

for (const [from, to] of pages) {
  let markdown = readFileSync(join(root, from), "utf8");
  const fields = { source: from };
  if (from === "README.md") {
    // The introduction: the README without its title, which the site's home page already shows.
    markdown = markdown.replace(/^# graphpg\n/, "# Introduction\n");
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
