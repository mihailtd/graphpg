// A row of every table created, updated and deleted, with inputs built from the schema (required
// fields, and references to rows that exist).
//   k6 run benchmarks/explore/writes.js
import { loadSchema, sampleKeys, scenario, Schema, stepsOf, writePlan, writeRound } from "./lib/graphpg.js";

export const options = {
  scenarios: { writes: scenario("writes") },
  thresholds: { engine_failures: ["count==0"] },
};

export function setup() {
  const raw = loadSchema();
  return { schema: raw, keys: sampleKeys(new Schema(raw)) };
}

let schema;
let plan;
export function writes(data) {
  schema = schema || new Schema(data.schema);
  plan = plan || writePlan(schema);
  for (const table of stepsOf(plan, __ITER)) writeRound(schema, table, data.keys, { op: "writes" });
}
