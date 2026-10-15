import { type ChildProcess, spawn } from "node:child_process";
import { unlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { databaseUrl, resetDatabase } from "../src/db.ts";

const binary = fileURLToPath(new URL("../../target/debug/graphpg", import.meta.url));
let server: ChildProcess | undefined;
let tempConfigPath: string | undefined;

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.once("error", reject);
    s.listen(0, () => {
      const { port } = s.address() as { port: number };
      s.close(() => resolve(port));
    });
  });
}

/** Migrate and seed with Drizzle, then start graphpg against the result. */
export async function setup(project: { provide: (key: "graphqlUrl", value: string) => void }) {
  await resetDatabase();
  const port = await freePort();
  tempConfigPath = join(tmpdir(), `graphpg-e2e-${port}.yaml`);
  writeFileSync(
    tempConfigPath,
    `server:\n  port: ${port}\ndatabase:\n  schemas: [blog]\ngraphql:\n  introspection: true\nsubscriptions:\n  enabled: true\n  interval: 200ms\n`,
  );

  server = spawn(binary, ["--config", tempConfigPath], {
    env: { ...process.env, DATABASE_URL: databaseUrl },
    stdio: ["ignore", "inherit", "inherit"],
  });
  server.once("exit", (code) => code && console.error(`graphpg exited with ${code}`));
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`http://localhost:${port}/health`)).ok) break;
    } catch {}
    if (i === 99) throw new Error("graphpg did not become healthy");
    await new Promise((r) => setTimeout(r, 200));
  }
  project.provide("graphqlUrl", `http://localhost:${port}/graphql`);
}

export async function teardown() {
  server?.kill("SIGTERM");
  if (tempConfigPath) {
    try {
      unlinkSync(tempConfigPath);
    } catch {}
  }
}
