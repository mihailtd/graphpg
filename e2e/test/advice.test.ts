import { execFileSync } from "node:child_process";
import { writeFileSync, unlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { databaseUrl } from "../src/db.ts";

const binary = fileURLToPath(new URL("../../target/debug/graphpg", import.meta.url));

describe("graphpg's advice on the blog schema", () => {
  it("has no warning: the schema Drizzle creates is indexed as graphpg recommends", () => {
    // `advise` prints what graphpg would say at startup (missing foreign key indexes among it)
    const configPath = join(tmpdir(), `graphpg-e2e-advise-${Date.now()}.yaml`);
    writeFileSync(configPath, "database:\n  schemas: [blog]\n");
    try {
      const output = execFileSync(binary, ["--config", configPath, "advise"], {
        env: { ...process.env, DATABASE_URL: databaseUrl },
        encoding: "utf8",
      });
      const warnings = output.split("\n").filter((line) => line.startsWith("warning:"));
      expect(warnings, output).toEqual([]);
    } finally {
      try {
        unlinkSync(configPath);
      } catch {}
    }
  });
});
