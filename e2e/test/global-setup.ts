import { type ChildProcess, spawn } from "node:child_process";
import { createServer } from "node:net";
import { fileURLToPath } from "node:url";
import { databaseUrl, resetDatabase } from "../src/db.ts";

const binary = fileURLToPath(new URL("../../target/debug/graphpg", import.meta.url));
let server: ChildProcess | undefined;

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
  server = spawn(binary, [], {
    env: { ...process.env, DATABASE_URL: databaseUrl, DATABASE_SCHEMA: "blog", PORT: String(port) },
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
}
