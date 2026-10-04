import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { basename, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";
import esbuild from "esbuild";

const require = createRequire(import.meta.url);
const __dirname = dirname(fileURLToPath(import.meta.url));
const outDir = resolve(__dirname, "../../crates/graphpg-ui/assets/graphiql");

const isWatch = process.argv.includes("--watch");

const workerPaths = {
  "editor.worker": require.resolve("monaco-editor/esm/vs/editor/editor.worker.js"),
  "json.worker": require.resolve("monaco-editor/esm/vs/language/json/json.worker.js"),
  "graphql.worker": require.resolve("monaco-graphql/esm/graphql.worker.js"),
};

const versions = {
  graphiql: require("graphiql/package.json").version,
  react: require("react/package.json").version,
  monaco: require("monaco-editor/package.json").version,
};

function getContentType(filename) {
  const ext = extname(filename);
  switch (ext) {
    case ".html":
      return "text/html; charset=utf-8";
    case ".js":
    case ".mjs":
      return "application/javascript; charset=utf-8";
    case ".css":
      return "text/css; charset=utf-8";
    case ".json":
      return "application/json; charset=utf-8";
    case ".txt":
      return "text/plain; charset=utf-8";
    default:
      return "application/octet-stream";
  }
}

async function runBuild() {
  if (existsSync(outDir)) {
    rmSync(outDir, { recursive: true, force: true });
  }
  mkdirSync(outDir, { recursive: true });

  console.log("Building GraphiQL assets to:", outDir);

  // 1. Build main application and bundle styles
  const appResult = await esbuild.build({
    entryPoints: {
      graphiql: resolve(__dirname, "src/app.jsx"),
    },
    entryNames: "graphiql.[hash]",
    bundle: true,
    minify: true,
    format: "esm",
    target: "es2022",
    outdir: outDir,
    metafile: true,
    legalComments: "linked",
    loader: {
      ".ttf": "dataurl",
      ".woff": "dataurl",
      ".woff2": "dataurl",
      ".eot": "dataurl",
    },
    define: {
      "process.env.NODE_ENV": '"production"',
    },
  });

  // 2. Build Monaco workers
  await esbuild.build({
    entryPoints: workerPaths,
    entryNames: "[name]",
    bundle: true,
    minify: true,
    format: "iife",
    target: "es2022",
    outdir: outDir,
    legalComments: "none",
  });

  // 3. Identify hashed JS and CSS files
  let hashedJs = null;
  let hashedCss = null;

  for (const outputFile of Object.keys(appResult.metafile.outputs)) {
    const name = basename(outputFile);
    if (name.startsWith("graphiql.") && name.endsWith(".js") && !name.endsWith(".worker.js")) {
      hashedJs = name;
    } else if (name.startsWith("graphiql.") && name.endsWith(".css")) {
      hashedCss = name;
    }
  }

  if (!hashedJs || !hashedCss) {
    throw new Error(`Failed to find hashed JS/CSS output in metafile: js=${hashedJs}, css=${hashedCss}`);
  }

  // 4. Generate index.html
  const htmlContent = `<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>graphpg · GraphiQL</title>
    <link rel="stylesheet" href="/graphiql/${hashedCss}" />
  </head>
  <body>
    <div id="graphiql">
      <div class="loading">Loading…</div>
    </div>
    <script type="module" src="/graphiql/${hashedJs}"></script>
  </body>
</html>
`;
  writeFileSync(join(outDir, "index.html"), htmlContent, "utf-8");

  // 5. Build manifest.json and pre-gzip each file
  const files = {};
  const entries = readdirSync(outDir).filter((file) => !file.endsWith(".gz") && file !== "manifest.json");

  for (const filename of entries) {
    const fullPath = join(outDir, filename);
    const content = readFileSync(fullPath);
    const sha256 = createHash("sha256").update(content).digest("hex");
    files[filename] = {
      size: content.length,
      sha256,
      contentType: getContentType(filename),
    };
  }

  const manifest = {
    versions,
    files,
  };

  const manifestContent = JSON.stringify(manifest, null, 2);
  writeFileSync(join(outDir, "manifest.json"), manifestContent, "utf-8");

  // Also record manifest.json in manifest entry
  const manifestSha256 = createHash("sha256").update(manifestContent).digest("hex");
  manifest.files["manifest.json"] = {
    size: Buffer.byteLength(manifestContent),
    sha256: manifestSha256,
    contentType: "application/json; charset=utf-8",
  };
  writeFileSync(join(outDir, "manifest.json"), JSON.stringify(manifest, null, 2), "utf-8");

  // 6. Generate .gz copies for all files in outDir
  const allFiles = readdirSync(outDir).filter((file) => !file.endsWith(".gz"));
  for (const filename of allFiles) {
    const fullPath = join(outDir, filename);
    const content = readFileSync(fullPath);
    const compressed = zlib.gzipSync(content, { level: 9 });
    writeFileSync(`${fullPath}.gz`, compressed);
  }

  console.log(`Successfully built ${allFiles.length} assets with gzip copies to ${outDir}`);
}

if (isWatch) {
  console.log("Watching packages/graphiql-ui/src for changes...");
  // Watch mode: run initial build, then poll/watch
  await runBuild();
  const ctx = await esbuild.context({
    entryPoints: { graphiql: resolve(__dirname, "src/app.jsx") },
    bundle: true,
    plugins: [
      {
        name: "rebuild-notify",
        setup(build) {
          build.onEnd(async () => {
            console.log("Source changed, rebuilding assets...");
            try {
              await runBuild();
            } catch (err) {
              console.error("Rebuild failed:", err);
            }
          });
        },
      },
    ],
  });
  await ctx.watch();
} else {
  await runBuild();
}
