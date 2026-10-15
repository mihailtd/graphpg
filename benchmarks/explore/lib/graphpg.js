// What every script here shares: requests, the rule telling a failure from a refusal, and the
// requests generated from a server's own schema (read by introspection). Nothing is specific to
// one schema: point BASE at any graphpg (with `graphql.introspection` on).
import http from "k6/http";
import { Counter } from "k6/metrics";

export const BASE = __ENV.BASE || "http://localhost:4000";
export const GRAPHQL = `${BASE}/graphql`;
/** Only root fields whose name matches (a regular expression). */
export const ONLY = __ENV.ONLY ? new RegExp(__ENV.ONLY) : null;

/** Requests the engine failed on: a 5xx, or an error that is not one of its typed refusals. */
export const engineFailures = new Counter("engine_failures");
/** Requests it refused (invalid input, a limit): expected, counted for the record. */
export const refusals = new Counter("refusals");

const REFUSED = new Set([
  "BAD_USER_INPUT",
  "GRAPHQL_VALIDATION_FAILED",
  "COST_LIMIT_EXCEEDED",
  "DEPTH_LIMIT_EXCEEDED",
  "PAGE_SIZE_EXCEEDED",
  "BAD_REQUEST",
  "PERMISSION_DENIED",
  "TOO_MANY_SUBSCRIPTIONS",
]);
/** Refusals a client retries (503): the schema changed under the request, the database is down. */
export const RETRYABLE = new Set(["SCHEMA_CHANGED", "DATABASE_UNAVAILABLE"]);
// Typed constraint violations (`EMAIL_TAKEN`, `CHK_..._XOR`, `..._NOT_FOUND`): refusals of writes
const TYPED = /^[A-Z][A-Z0-9_]*_(TAKEN|NOT_FOUND|REQUIRED|INVALID|TOO_LONG|OUT_OF_RANGE|CONFLICT|REFERENCED|VIOLATION|XOR)$|^CHK_|^STALE_VERSION$|^BULK_LIMIT_EXCEEDED$|^FILTER_REQUIRED$/;

let reported = 0;

/** POSTs an operation; counts and logs (the first few) engine failures. */
export function gql(label, query, variables = {}, tags = {}) {
  const res = http.post(GRAPHQL, JSON.stringify({ query, variables }), {
    headers: { "content-type": "application/json" },
    tags: { op: tags.op || "graphql", ...tags },
  });
  let body = null;
  try {
    body = res.json();
  } catch (_) {
    body = null;
  }
  const errors = (body && body.errors) || [];
  const retryable = errors.length > 0 && errors.every((e) => RETRYABLE.has((e.extensions && e.extensions.code) || ""));
  const failed =
    (res.status >= 500 && !retryable) ||
    !body ||
    errors.some((e) => {
      const code = (e.extensions && e.extensions.code) || "";
      if (REFUSED.has(code) || RETRYABLE.has(code) || TYPED.test(code)) return false;
      // The document's own validation: a request these scripts got wrong, not the engine
      return !(code === "" && /^Error: /.test(e.message || ""));
    });
  if (failed) {
    engineFailures.add(1);
    if (reported++ < 20) console.error(`FAIL ${label} (${res.status}): ${String(res.body).slice(0, 400)}\n  ${query.slice(0, 300)}`);
  } else if (errors.length) {
    refusals.add(1);
  }
  return { status: res.status, body: body || {} };
}

const T = "kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } }";

/** The server's schema, as introspection tells it (call in `setup()`). */
export function loadSchema() {
  const r = gql(
    "introspection",
    `{ __schema { queryType { fields { name args { name type { ${T} } } type { ${T} } } }
      mutationType { fields { name args { name type { ${T} } } type { ${T} } } }
      types { name kind enumValues { name } inputFields { name type { ${T} } } fields { name args { name type { ${T} } } type { ${T} } } } } }`,
  );
  const schema = r.body.data && r.body.data.__schema;
  if (!schema) throw new Error(`introspection failed (is graphql.introspection on?): ${JSON.stringify(r.body).slice(0, 300)}`);
  return schema;
}

// ---- reading the schema -----------------------------------------------------------------------

export function named(t) {
  let r = t;
  while (r && (r.kind === "NON_NULL" || r.kind === "LIST")) r = r.ofType;
  return r;
}
export const isList = (t) => {
  let r = t;
  while (r && r.kind === "NON_NULL") r = r.ofType;
  return !!r && r.kind === "LIST";
};
const required = (a) => a.type.kind === "NON_NULL";

export class Schema {
  constructor(raw) {
    this.raw = raw;
    this.types = new Map(raw.types.map((t) => [t.name, t]));
    this.queryFields = raw.queryType.fields.filter((f) => !f.name.startsWith("_") && (!ONLY || ONLY.test(f.name)));
    this.mutationFields = raw.mutationType ? raw.mutationType.fields : [];
  }
  type(ref) {
    const n = named(ref);
    return n ? this.types.get(n.name) : undefined;
  }
  /** Scalar and enum fields of an object type, and `depth` levels of objects. */
  selection(typeName, depth) {
    const t = this.types.get(typeName);
    const parts = ["__typename"];
    for (const f of (t && t.fields) || []) {
      if (f.args.some(required)) continue;
      const ft = this.type(f.type);
      if (!ft) continue;
      if (ft.kind === "SCALAR" || ft.kind === "ENUM") parts.push(f.name);
      else if (depth > 0 && (ft.kind === "OBJECT" || ft.kind === "INTERFACE")) parts.push(`${f.name} { ${this.selection(ft.name, depth - 1)} }`);
    }
    return parts.join(" ");
  }
  /** Root connection fields that need no argument: `{ field, connection, node }`. */
  connections() {
    const out = [];
    for (const field of this.queryFields) {
      const c = this.type(field.type);
      if (!c || !c.name.endsWith("Connection") || field.args.some(required)) continue;
      const nodes = (c.fields || []).find((f) => f.name === "nodes");
      if (nodes) out.push({ field, connection: c, node: named(nodes.type).name });
    }
    return out;
  }
}

// ---- crawl: everything the schema offers ------------------------------------------------------

/**
 * Requests of every connection: its rows, every ordering (and the pages after and before its
 * cursor, which `then` asks for with the cursor of `cursorOf`), aggregates, groupings, relation
 * filters and relations. Each step is `{ label, query, variables?, cursorOf?, then? }`.
 */
export function crawlPlan(schema) {
  const steps = [];
  for (const { field, connection, node } of schema.connections()) {
    const name = field.name;
    steps.push({ label: `${name} rows`, query: `{ ${name}(first: 3) { totalCount nodes { ${schema.selection(node, 1)} } } }` });
    const order = field.args.find((a) => a.name === "orderBy");
    for (const v of ((order && schema.type(order.type)) || {}).enumValues || []) {
      steps.push({
        label: `${name} orderBy ${v.name}`,
        query: `{ ${name}(first: 2, orderBy: [${v.name}]) { pageInfo { endCursor } nodes { __typename } } }`,
        cursorOf: name,
        then: ["first: 2, after: $c", "last: 2, before: $c", "first: 2, after: $c, offset: 1"].map((page) => ({
          label: `${name} orderBy ${v.name} ${page}`,
          query: `query($c: String) { ${name}(${page}, orderBy: [${v.name}]) { totalCount nodes { __typename } } }`,
        })),
      });
    }
    const aggregates = (connection.fields || []).find((f) => f.name === "aggregates");
    const aggType = aggregates && schema.type(aggregates.type);
    if (aggType && aggType.fields) {
      const parts = aggType.fields
        .filter((f) => !f.args.some(required))
        .map((f) => `${f.name} { ${schema.selection(named(f.type).name, 1)} }`)
        .join(" ");
      steps.push({ label: `${name} aggregates`, query: `{ ${name} { aggregates { ${parts} } } }` });
      const grouped = (connection.fields || []).find((f) => f.name === "groupedAggregates");
      const groupBy = grouped && grouped.args.find((a) => a.name === "groupBy");
      for (const g of ((groupBy && schema.type(groupBy.type)) || {}).enumValues || []) {
        steps.push({ label: `${name} groupedAggregates ${g.name}`, query: `{ ${name} { groupedAggregates(groupBy: [${g.name}]) { keys ${parts} } } }` });
      }
    }
    const filter = field.args.find((a) => a.name === "filter");
    for (const f of ((filter && schema.type(filter.type)) || {}).inputFields || []) {
      const ops = (((schema.type(f.type) || {}).inputFields) || []).map((i) => i.name);
      if (ops.includes("isNull")) {
        steps.push({ label: `${name} ${f.name} isNull`, query: `{ ${name}(filter: { ${f.name}: { isNull: false } }) { totalCount } }` });
      } else if (ops.includes("some")) {
        for (const q of ["some: {}", "none: {}", "every: {}"].concat(ops.includes("count") ? ["count: { greaterThan: 0 }"] : [])) {
          steps.push({ label: `${name} ${f.name} ${q}`, query: `{ ${name}(filter: { ${f.name}: { ${q} } }) { totalCount } }` });
        }
      }
    }
    for (const rel of (schema.types.get(node) || {}).fields || []) {
      const t = schema.type(rel.type);
      if (!t || (t.kind !== "OBJECT" && t.kind !== "INTERFACE") || rel.args.some(required)) continue;
      const inner = t.name.endsWith("Connection")
        ? `${rel.name}(first: 3) { totalCount nodes { __typename } }`
        : `${rel.name} { ${schema.selection(t.name, 1)} }`;
      steps.push({ label: `${name}.${rel.name}`, query: `{ ${name}(first: 3) { nodes { ${inner} } } }` });
    }
  }
  return steps;
}

/** Runs one step (and the pages after its cursor). */
export function runStep(step, tags) {
  const r = gql(step.label, step.query, step.variables || {}, tags);
  if (step.then && step.cursorOf) {
    const page = r.body.data && r.body.data[step.cursorOf];
    const cursor = page && page.pageInfo && page.pageInfo.endCursor;
    if (cursor) for (const next of step.then) gql(next.label, next.query, { c: cursor }, tags);
  }
  return r;
}

// ---- fuzz: what clients send by mistake, or on purpose ----------------------------------------

const HOSTILE = [
  "", " ", "'", "''; DROP TABLE x; --", "\\", "%", "_", "\u0000", "a\u0000b", "😀", "a".repeat(5000), "-1", "NaN",
  "Infinity", "1e309", "99999999999999999999999999", "2026-13-45", "0000-00-00", "24:00:00", "not-a-uuid",
  "00000000-0000-0000-0000-000000000000", "P1Y2M", "[1,2)", "(,)", "{a,b}", "$.a", "$$", "(", "[", "*", "a{99999}",
];
const NUMBERS = [0, -1, 1.5, 2147483647, 2147483648, -2147483649, 1e308];
const CURSORS = ["", "x", "%%%", "bm90LWpzb24=", "WzFd", "AAAA", "MTsgRFJPUCBUQUJMRSB4Ow==", "a".repeat(3000)];

/** Hostile values for every operator of every scalar filter (each filter type a few times). */
export function fuzzPlan(schema) {
  const steps = [];
  const seen = new Map();
  const connections = schema.connections();
  for (const { field } of connections) {
    const filterArg = field.args.find((a) => a.name === "filter");
    const filterType = filterArg && schema.type(filterArg.type);
    for (const column of (filterType && filterType.inputFields) || []) {
      const filter = schema.type(column.type);
      if (!filter || !filter.inputFields || filter.name.endsWith("ToManyFilter") || filter.name === filterType.name) continue;
      const times = seen.get(filter.name) || 0;
      if (times >= 3) continue;
      seen.set(filter.name, times + 1);
      for (const op of filter.inputFields) {
        const opType = named(op.type);
        if ((schema.types.get(opType.name) || {}).kind === "INPUT_OBJECT") continue;
        const list = isList(op.type);
        const values = opType.name === "Int" || opType.name === "Float" ? NUMBERS : opType.name === "Boolean" ? [true] : HOSTILE;
        for (const v of values) {
          steps.push({
            label: `${field.name} ${column.name}.${op.name} ${JSON.stringify(v).slice(0, 30)}`,
            query: `query($v: ${list ? `[${opType.name}!]` : opType.name}) { ${field.name}(first: 1, filter: { ${column.name}: { ${op.name}: $v } }) { totalCount nodes { __typename } } }`,
            variables: { v: list ? [v, v] : v },
          });
        }
      }
    }
  }
  for (const { field } of connections.slice(0, 5)) {
    for (const c of CURSORS) {
      for (const [page, sel] of [["first: 1, after: $c", "nodes { __typename }"], ["last: 1, before: $c", "nodes { __typename }"], ["first: 1, after: $c", "totalCount"]]) {
        steps.push({ label: `${field.name}(${page}) ${c.slice(0, 12)}`, query: `query($c: String) { ${field.name}(${page}) { ${sel} } }`, variables: { c } });
      }
    }
    for (const n of [-1, 0, 2147483647]) {
      for (const arg of ["first", "last", "offset"]) {
        steps.push({ label: `${field.name}(${arg}: ${n})`, query: `query($n: Int) { ${field.name}(${arg}: $n) { totalCount } }`, variables: { n } });
      }
    }
  }
  for (const id of ["", "x", "%%", "Tm9wZTox", "a".repeat(3000), "\u0000"]) {
    steps.push({ label: `node(${id.slice(0, 12)})`, query: `query($id: ID!) { node(id: $id) { id } }`, variables: { id } });
  }
  return steps;
}

// ---- writes: create, update and delete a row of every table -----------------------------------

/** A key of an existing row of every type that has a root connection (call in `setup()`). */
export function sampleKeys(schema) {
  const keys = {};
  for (const { field, node } of schema.connections()) {
    const nodeType = schema.types.get(node);
    if (!nodeType || !(nodeType.fields || []).some((f) => f.name === "databaseId")) continue;
    const r = gql(`${field.name} keys`, `{ ${field.name}(first: 5) { nodes { databaseId } } }`);
    const nodes = (r.body.data && r.body.data[field.name] && r.body.data[field.name].nodes) || [];
    if (nodes.length) keys[node] = nodes.map((n) => n.databaseId);
  }
  return keys;
}

let seq = 0;
function scalarValue(name) {
  seq++;
  switch (name) {
    case "Int": return 1;
    case "Float": return 1.5;
    case "Boolean": return true;
    case "String": return `k6-${__VU}-${seq}`;
    case "BigInt": return "1";
    case "BigFloat": case "Decimal": return "1.5";
    case "UUID": return null; // keys and references: from `keys`, else left to the database
    case "JSON": return { k6: seq };
    case "Datetime": return "2026-10-14T12:00:00Z";
    case "Date": return "2026-10-14";
    case "Time": return "12:00:00";
    case "Interval": return "P1D";
    default: return undefined;
  }
}

/** An input of `typeName`: its required fields, and references set to rows that exist. */
export function buildInput(schema, typeName, keys, depth = 0) {
  const t = schema.types.get(typeName);
  const out = {};
  for (const f of (t && t.inputFields) || []) {
    const ft = named(f.type);
    const kind = (schema.types.get(ft.name) || {}).kind;
    const req = f.type.kind === "NON_NULL";
    if (kind === "ENUM") {
      if (req) out[f.name] = schema.types.get(ft.name).enumValues[0].name;
      continue;
    }
    if (kind === "INPUT_OBJECT") {
      if (req && depth < 2) out[f.name] = isList(f.type) ? [] : buildInput(schema, ft.name, keys, depth + 1);
      continue;
    }
    const ref = f.name.match(/^(.+)Id$/);
    if (ref) {
      const target = ref[1].charAt(0).toUpperCase() + ref[1].slice(1);
      const candidates = Object.keys(keys).filter((k) => k === target || k.endsWith(target));
      if (candidates.length) {
        const ids = keys[candidates[0]];
        out[f.name] = ids[Math.floor(Math.random() * ids.length)];
        continue;
      }
    }
    if (req) {
      const v = isList(f.type) ? [] : scalarValue(ft.name);
      if (v !== undefined && v !== null) out[f.name] = v;
    }
  }
  return out;
}

/** The tables that can be written: `{ base, create, inputType, row, update, delete }` per table. */
export function writePlan(schema) {
  const plans = [];
  const byName = new Map(schema.mutationFields.map((m) => [m.name, m]));
  for (const m of schema.mutationFields) {
    if (!m.name.startsWith("create") || (ONLY && !ONLY.test(m.name))) continue;
    const input = m.args.find((a) => a.name === "input");
    if (!input || isList(input.type)) continue;
    const payload = schema.type(m.type);
    const rowField = ((payload && payload.fields) || []).find((f) => ((schema.type(f.type) || {}).fields || []).some((x) => x.name === "databaseId"));
    if (!rowField) continue;
    const base = m.name.slice("create".length);
    plans.push({
      base,
      create: m.name,
      inputType: named(input.type).name,
      row: rowField.name,
      // Updated by global ID where there is one (`updateXByNodeId`), deleted by key
      update: byName.get(`update${base}ByNodeId`) || byName.get(`update${base}`),
      delete: byName.get(`delete${base}`),
    });
  }
  return plans;
}

/** Creates a row with `plan`, updates one of its fields, and deletes it. */
export function writeRound(schema, plan, keys, tags) {
  const created = gql(
    plan.create,
    `mutation($i: ${plan.inputType}!) { ${plan.create}(input: $i) { ${plan.row} { id databaseId } } }`,
    { i: buildInput(schema, plan.inputType, keys) },
    tags,
  );
  const row = created.body.data && created.body.data[plan.create] && created.body.data[plan.create][plan.row];
  if (!row) return;
  // The row's one key argument (`id`, `nodeId`); keys of several columns are not addressed
  const keyArg = (m) => {
    const keys = m.args.filter((a) => a.name !== "input" && a.name !== "expectedVersion");
    return keys.length === 1 ? keys[0] : undefined;
  };
  if (plan.update) {
    const inputArg = plan.update.args.find((a) => a.name === "input");
    const id = keyArg(plan.update);
    const updInput = inputArg && schema.types.get(named(inputArg.type).name);
    const field = ((updInput && updInput.inputFields) || []).find(
      (f) => ["String", "Int", "Float", "Boolean", "JSON", "Datetime"].includes(named(f.type).name) && !f.name.endsWith("Id"),
    );
    if (id && field) {
      gql(
        plan.update.name,
        `mutation($id: ${named(id.type).name}!, $i: ${named(inputArg.type).name}!) { ${plan.update.name}(${id.name}: $id, input: $i) { __typename } }`,
        { id: named(id.type).name === "ID" ? row.id : row.databaseId, i: { [field.name]: scalarValue(named(field.type).name) } },
        tags,
      );
    }
  }
  if (plan.delete) {
    const id = keyArg(plan.delete);
    if (id) {
      gql(
        `delete${plan.base}`,
        `mutation($id: ${named(id.type).name}!) { delete${plan.base}(${id.name}: $id) { __typename } }`,
        { id: named(id.type).name === "ID" ? row.id : row.databaseId },
        tags,
      );
    }
  }
}

export const VUS = Number(__ENV.VUS || 10);
export const DURATION = __ENV.DURATION || "1m";
/** `cover` (each step once, the plan split between the VUs) or `load` (for DURATION). */
export const MODE = __ENV.MODE || "cover";

/** The scenario of `exec`: every step once (`cover`), or steps for a duration (`load`). */
export function scenario(exec, vus = VUS) {
  return MODE === "load"
    ? { executor: "constant-vus", vus, duration: DURATION, exec }
    : { executor: "per-vu-iterations", vus, iterations: 1, maxDuration: "2h", exec };
}

/** The steps of this VU: its share of the plan (`cover`), or the next one (`load`). */
export function stepsOf(plan, iteration, vus = VUS) {
  if (MODE === "load") return [plan[((__VU - 1) * Math.floor(plan.length / vus) + iteration) % plan.length]];
  return plan.filter((_, i) => i % vus === __VU - 1);
}
