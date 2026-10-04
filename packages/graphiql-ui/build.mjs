// Builds the GraphiQL UI into crates/graphpg-ui/assets/graphiql, which that crate embeds.
//
//   node build.mjs           one build
//   node build.mjs --watch   rebuild when src/ changes (a debug graphpg reads the files from
//                            disk, so reloading the page is enough)
//
// Every file is written gzip-compressed only (`<name>.gz`): the server sends the stored bytes
// to clients that accept gzip and inflates them for the rest, so nothing is embedded twice.
// `manifest.json.gz` is written last, so its presence means the directory is complete.
import { createHash } from "node:crypto";
import { mkdirSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";
import esbuild from "esbuild";

const require = createRequire(import.meta.url);
const root = dirname(fileURLToPath(import.meta.url));
const outDir = resolve(root, "../../crates/graphpg-ui/assets/graphiql");
const watch = process.argv.includes("--watch");

const workers = {
  "editor.worker": require.resolve("monaco-editor/esm/vs/editor/editor.worker.js"),
  "json.worker": require.resolve("monaco-editor/esm/vs/language/json/json.worker.js"),
  "graphql.worker": require.resolve("monaco-graphql/esm/graphql.worker.js"),
};

const versions = {
  graphiql: require("graphiql/package.json").version,
  react: require("react/package.json").version,
  monaco: require("monaco-editor/package.json").version,
};

// Nothing is written by esbuild itself: the outputs come back in memory and `publish` stores them
const app = {
  entryPoints: { graphiql: resolve(root, "src/app.jsx") },
  entryNames: "graphiql.[hash]",
  bundle: true,
  minify: true,
  format: "esm",
  target: "es2022",
  outdir: outDir,
  write: false,
  legalComments: "linked",
  loader: { ".ttf": "dataurl", ".woff": "dataurl", ".woff2": "dataurl", ".eot": "dataurl" },
  define: { "process.env.NODE_ENV": '"production"' },
};

// The Monaco workers do not depend on src/, so they are built once
const workerFiles = (
  await esbuild.build({
    entryPoints: workers,
    entryNames: "[name]",
    bundle: true,
    minify: true,
    format: "iife",
    target: "es2022",
    outdir: outDir,
    write: false,
    legalComments: "none",
  })
).outputFiles;

const name = (file) => file.path.slice(outDir.length + 1);

/** Stores the build: every file as `.gz`, the page, and the manifest last; drops what is stale. */
function publish({ outputFiles }) {
  const files = new Map([...outputFiles, ...workerFiles].map((f) => [name(f), Buffer.from(f.contents)]));

  const script = [...files.keys()].find((n) => /^graphiql\.[A-Z0-9]+\.js$/.test(n));
  const style = [...files.keys()].find((n) => /^graphiql\.[A-Z0-9]+\.css$/.test(n));
  if (!script || !style) {
    throw new Error(`the build has no script or no style: ${[...files.keys()].join(", ")}`);
  }
  files.set(
    "index.html",
    Buffer.from(`<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>graphpg · GraphiQL</title>
    <link rel="stylesheet" href="/graphiql/${style}" />
  </head>
  <body>
    <div id="graphiql">
      <div class="loading">Loading…</div>
    </div>
    <script type="module" src="/graphiql/${script}"></script>
  </body>
</html>
`),
  );

  const manifest = {
    versions,
    files: Object.fromEntries(
      [...files].map(([n, content]) => [
        n,
        { size: content.length, sha256: createHash("sha256").update(content).digest("hex") },
      ]),
    ),
  };

  mkdirSync(outDir, { recursive: true });
  const stored = new Set();
  for (const [n, content] of files) {
    writeFileSync(resolve(outDir, `${n}.gz`), gzipSync(content, { level: 9 }));
    stored.add(`${n}.gz`);
  }
  for (const stale of readdirSync(outDir).filter((f) => !stored.has(f) && f !== "manifest.json.gz")) {
    rmSync(resolve(outDir, stale), { force: true });
  }
  writeFileSync(resolve(outDir, "manifest.json.gz"), gzipSync(JSON.stringify(manifest, null, 2), { level: 9 }));
  console.log(`graphiql-ui: ${files.size} files (gzip) in ${outDir}`);
}

if (watch) {
  const context = await esbuild.context({
    ...app,
    plugins: [
      {
        name: "publish",
        setup: (build) =>
          build.onEnd((result) => {
            if (result.errors.length === 0) publish(result);
          }),
      },
    ],
  });
  await context.watch();
  console.log("graphiql-ui: watching src/ for changes");
} else {
  publish(await esbuild.build(app));
}
