// Every request the schema offers: each connection's rows, orderings and pages, aggregates,
// groupings, relation filters and relations. MODE=cover runs each once; MODE=load replays them.
//   k6 run benchmarks/explore/crawl.js                       # BASE=http://localhost:4000
//   MODE=load VUS=40 DURATION=5m k6 run benchmarks/explore/crawl.js
import { crawlPlan, loadSchema, runStep, scenario, Schema, stepsOf } from "./lib/graphpg.js";

export const options = {
  scenarios: { crawl: scenario("crawl") },
  thresholds: { engine_failures: ["count==0"] },
};

export function setup() {
  return { schema: loadSchema() };
}

let plan;
export function crawl(data) {
  plan = plan || crawlPlan(new Schema(data.schema));
  for (const step of stepsOf(plan, __ITER)) runStep(step, { op: "crawl" });
}
