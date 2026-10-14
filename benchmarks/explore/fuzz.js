// Malformed and boundary values for every filter operator, cursor and id: the engine may refuse
// them (typed errors), never fail on them (engine_failures).
//   k6 run benchmarks/explore/fuzz.js
import { fuzzPlan, loadSchema, runStep, scenario, Schema, stepsOf } from "./lib/graphpg.js";

export const options = {
  scenarios: { fuzz: scenario("fuzz") },
  thresholds: { engine_failures: ["count==0"] },
};

export function setup() {
  return { schema: loadSchema() };
}

let plan;
export function fuzz(data) {
  plan = plan || fuzzPlan(new Schema(data.schema));
  for (const step of stepsOf(plan, __ITER)) runStep(step, { op: "fuzz" });
}
