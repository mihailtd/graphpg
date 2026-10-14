// Everything at once, for a while: reads (the crawl's requests), writes, REST reads and live
// queries over WebSocket (graphql-transport-ws), clients subscribing and leaving. Watch the
// server's memory and connections alongside; engine_failures must stay 0.
//   DURATION=10m k6 run benchmarks/explore/soak.js
import http from "k6/http";
import ws from "k6/ws";
import { Counter, Trend } from "k6/metrics";
import {
  BASE,
  crawlPlan,
  DURATION,
  engineFailures,
  GRAPHQL,
  loadSchema,
  runStep,
  sampleKeys,
  Schema,
  writePlan,
  writeRound,
} from "./lib/graphpg.js";

const subscriptionResults = new Counter("subscription_results");
const subscriptionErrors = new Counter("subscription_errors");
const firstResult = new Trend("subscription_first_result_ms");

const n = (name, fallback) => Number(__ENV[name] || fallback);
export const options = {
  scenarios: {
    reads: { executor: "constant-vus", vus: n("READ_VUS", 40), duration: DURATION, exec: "reads" },
    writes: { executor: "constant-vus", vus: n("WRITE_VUS", 5), duration: DURATION, exec: "writes" },
    rest: { executor: "constant-vus", vus: n("REST_VUS", 10), duration: DURATION, exec: "rest" },
    subscriptions: { executor: "constant-vus", vus: n("SUBSCRIPTION_VUS", 50), duration: DURATION, exec: "subscriptions" },
  },
  thresholds: { engine_failures: ["count==0"], subscription_errors: ["count==0"] },
};

export function setup() {
  const raw = loadSchema();
  const schema = new Schema(raw);
  const index = http.get(`${BASE}/rest`);
  const resources = index.status === 200 ? index.json().resources.map((r) => r.path) : [];
  // Live queries: the first rows of the connections
  const live = schema.connections().slice(0, 20).map(({ field }) => `subscription { ${field.name}(first: 5) { totalCount nodes { __typename } } }`);
  return { schema: raw, keys: sampleKeys(schema), resources, live };
}

let schema;
let readPlan;
let tables;

export function reads(data) {
  schema = schema || new Schema(data.schema);
  readPlan = readPlan || crawlPlan(schema).filter((s) => !s.label.includes("groupedAggregates"));
  runStep(readPlan[Math.floor(Math.random() * readPlan.length)], { op: "read" });
}

export function writes(data) {
  schema = schema || new Schema(data.schema);
  tables = tables || writePlan(schema);
  if (tables.length) writeRound(schema, tables[Math.floor(Math.random() * tables.length)], data.keys, { op: "write" });
}

export function rest(data) {
  if (!data.resources.length) return;
  const path = data.resources[Math.floor(Math.random() * data.resources.length)];
  const res = http.get(`${BASE}${path}?first=20`, { tags: { op: "rest" } });
  if (res.status >= 500) {
    engineFailures.add(1);
    console.error(`FAIL GET ${path} (${res.status}): ${String(res.body).slice(0, 300)}`);
  }
}

export function subscriptions(data) {
  if (!data.live.length) return;
  const query = data.live[Math.floor(Math.random() * data.live.length)];
  const started = Date.now();
  let first = true;
  ws.connect(GRAPHQL.replace(/^http/, "ws"), { headers: { "Sec-WebSocket-Protocol": "graphql-transport-ws" } }, (socket) => {
    socket.on("open", () => socket.send(JSON.stringify({ type: "connection_init" })));
    socket.on("message", (raw) => {
      const msg = JSON.parse(raw);
      if (msg.type === "connection_ack") {
        socket.send(JSON.stringify({ id: "1", type: "subscribe", payload: { query } }));
      } else if (msg.type === "next") {
        subscriptionResults.add(1);
        if (first) {
          firstResult.add(Date.now() - started);
          first = false;
        }
        if (msg.payload.errors) {
          subscriptionErrors.add(1);
          console.error(`subscription error: ${JSON.stringify(msg.payload.errors).slice(0, 300)}`);
        }
      } else if (msg.type === "error") {
        const codes = (msg.payload || []).map((e) => e.extensions && e.extensions.code);
        // Over the per-client limit: a refusal, not a failure
        if (!codes.includes("TOO_MANY_SUBSCRIPTIONS")) {
          subscriptionErrors.add(1);
          console.error(`subscription refused: ${JSON.stringify(msg.payload).slice(0, 300)}`);
        }
      } else if (msg.type === "ping") {
        socket.send(JSON.stringify({ type: "pong" }));
      }
    });
    socket.on("error", (e) => {
      subscriptionErrors.add(1);
      console.error(`websocket error: ${e.error()}`);
    });
    // Clients stay a while, then leave
    socket.setTimeout(() => {
      socket.send(JSON.stringify({ id: "1", type: "complete" }));
      socket.close();
    }, 3000 + Math.random() * 12000);
  });
}
