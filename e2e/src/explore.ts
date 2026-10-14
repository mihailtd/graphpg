/**
 * Exploratory testing of a running graphpg, driven by its own schema: every root field, relation,
 * ordering, filter and aggregate it advertises is requested ({@link crawl}), and every filter
 * operator is sent malformed and boundary values ({@link fuzz}). What it reports is the requests
 * the engine *failed* on (a 5xx, or an error that is not one of its typed rejections), not the
 * ones it refused as invalid input. Run by the end-to-end suite on the blog schema, and by hand
 * on any schema (`pnpm explore <graphql url>`).
 */

interface TypeRef {
  kind: string;
  name: string | null;
  ofType: TypeRef | null;
}
interface InputValue {
  name: string;
  type: TypeRef;
}
interface Field {
  name: string;
  args: InputValue[];
  type: TypeRef;
}
interface FullType {
  name: string;
  kind: string;
  fields: Field[] | null;
  inputFields: InputValue[] | null;
  enumValues: { name: string }[] | null;
}
interface GraphQLError {
  message?: string;
  extensions?: { code?: string };
}
interface Response {
  status: number;
  body: { data?: Record<string, unknown> | null; errors?: GraphQLError[] };
}

/** A request the engine failed on. */
export interface Failure {
  label: string;
  query: string;
  variables?: Record<string, unknown>;
  status: number;
  error: unknown;
}

export interface Options {
  /** Only root fields whose name matches. */
  only?: RegExp;
  /** Stop after this many requests. */
  maxRequests?: number;
}

/** Codes of requests refused, as opposed to failed. */
const REFUSED = new Set([
  "BAD_USER_INPUT",
  "GRAPHQL_VALIDATION_FAILED",
  "COST_LIMIT_EXCEEDED",
  "DEPTH_LIMIT_EXCEEDED",
  "PAGE_SIZE_EXCEEDED",
  "BAD_REQUEST",
  "PERMISSION_DENIED",
]);

const TYPE_REF = "kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } }";

class Explorer {
  readonly failures: Failure[] = [];
  requests = 0;
  types = new Map<string, FullType>();
  queryFields: Field[] = [];

  readonly url: string;
  readonly options: Options;

  constructor(url: string, options: Options) {
    this.url = url;
    this.options = options;
  }

  async post(query: string, variables: Record<string, unknown> = {}): Promise<Response> {
    const res = await fetch(this.url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ query, variables }),
    });
    const text = await res.text();
    try {
      return { status: res.status, body: JSON.parse(text) };
    } catch {
      return { status: res.status, body: { errors: [{ message: text.slice(0, 300) }] } };
    }
  }

  async load() {
    const r =
      await this.post(`{ __schema { queryType { fields { name args { name type { ${TYPE_REF} } } type { ${TYPE_REF} } } }
      types { name kind enumValues { name } inputFields { name type { ${TYPE_REF} } }
        fields { name args { name type { ${TYPE_REF} } } type { ${TYPE_REF} } } } } }`);
    const schema = r.body.data?.__schema as { queryType: { fields: Field[] }; types: FullType[] } | undefined;
    if (!schema) throw new Error(`introspection failed (graphql.introspection must be on): ${JSON.stringify(r.body)}`);
    this.types = new Map(schema.types.map((t) => [t.name, t]));
    this.queryFields = schema.queryType.fields.filter(
      (f) => !f.name.startsWith("_") && (!this.options.only || this.options.only.test(f.name)),
    );
  }

  get exhausted() {
    return this.options.maxRequests !== undefined && this.requests >= this.options.maxRequests;
  }

  /** Requests `query`, recording it when the engine fails on it. */
  async check(label: string, query: string, variables?: Record<string, unknown>): Promise<Response | undefined> {
    if (this.exhausted) return undefined;
    this.requests++;
    const r = await this.post(query, variables);
    const failed =
      r.status >= 500 ||
      (r.body.errors ?? []).some((e) => {
        const code = e.extensions?.code;
        if (code && REFUSED.has(code)) return false;
        // The document's validation (a query this explorer got wrong)
        if (!code && /^Error: /.test(e.message ?? "")) return false;
        return true;
      });
    if (failed) this.failures.push({ label, query, variables, status: r.status, error: r.body.errors?.[0] ?? r.body });
    return r;
  }

  type(ref: TypeRef): FullType | undefined {
    return this.types.get(named(ref).name ?? "");
  }

  /** The scalar and enum fields of an object type, and `depth` levels of objects. */
  selection(typeName: string, depth: number): string {
    const type = this.types.get(typeName);
    const parts = ["__typename"];
    for (const f of type?.fields ?? []) {
      if (f.args.some((a) => a.type.kind === "NON_NULL")) continue;
      const t = this.type(f.type);
      if (t?.kind === "SCALAR" || t?.kind === "ENUM") parts.push(f.name);
      else if (depth > 0 && (t?.kind === "OBJECT" || t?.kind === "INTERFACE")) {
        parts.push(`${f.name} { ${this.selection(t.name, depth - 1)} }`);
      }
    }
    return parts.join(" ");
  }

  connections(): { field: Field; connection: FullType; node: string }[] {
    return this.queryFields.flatMap((field) => {
      const connection = this.type(field.type);
      const nodes = connection?.name.endsWith("Connection") ? connection.fields?.find((f) => f.name === "nodes") : null;
      const node = nodes ? named(nodes.type).name : null;
      return connection && node && field.args.every((a) => a.type.kind !== "NON_NULL")
        ? [{ field, connection, node }]
        : [];
    });
  }
}

function named(t: TypeRef): TypeRef {
  let r = t;
  while (r.ofType && (r.kind === "NON_NULL" || r.kind === "LIST")) r = r.ofType;
  return r;
}

function isList(t: TypeRef): boolean {
  let r: TypeRef | null = t;
  while (r && r.kind === "NON_NULL") r = r.ofType;
  return r?.kind === "LIST";
}

/**
 * Requests everything the schema offers: each connection's rows, every ordering (and the pages
 * after and before), every aggregate, grouping, relation and relation filter.
 */
export async function crawl(url: string, options: Options = {}): Promise<{ requests: number; failures: Failure[] }> {
  const x = new Explorer(url, options);
  await x.load();
  for (const { field, connection, node } of x.connections()) {
    const name = field.name;
    await x.check(`${name}`, `{ ${name}(first: 3) { totalCount nodes { ${x.selection(node, 1)} } } }`);
    const orderArg = field.args.find((a) => a.name === "orderBy");
    for (const v of (orderArg && x.type(orderArg.type)?.enumValues) || []) {
      const r = await x.check(
        `${name} orderBy ${v.name}`,
        `{ ${name}(first: 2, orderBy: [${v.name}]) { pageInfo { endCursor } nodes { __typename } } }`,
      );
      const cursor = (r?.body.data?.[name] as { pageInfo?: { endCursor?: string } } | undefined)?.pageInfo?.endCursor;
      if (cursor) {
        for (const page of ["first: 2, after: $c", "last: 2, before: $c", "first: 2, after: $c, offset: 1"]) {
          await x.check(
            `${name} orderBy ${v.name} ${page}`,
            `query($c: String) { ${name}(${page}, orderBy: [${v.name}]) { totalCount nodes { __typename } } }`,
            { c: cursor },
          );
        }
        await x.check(
          `${name} totalCount after a cursor`,
          `query($c: String) { ${name}(first: 2, after: $c) { totalCount } }`,
          { c: cursor },
        );
      }
    }
    const aggregates = connection.fields?.find((f) => f.name === "aggregates");
    const aggregatesType = aggregates && x.type(aggregates.type);
    if (aggregatesType?.fields) {
      const parts = aggregatesType.fields
        .filter((f) => f.args.every((a) => a.type.kind !== "NON_NULL"))
        .map((f) => `${f.name} { ${x.selection(named(f.type).name ?? "", 0)} }`)
        .join(" ");
      await x.check(`${name} aggregates`, `{ ${name} { aggregates { ${parts} } } }`);
      const grouped = connection.fields?.find((f) => f.name === "groupedAggregates");
      const groupBy = grouped?.args.find((a) => a.name === "groupBy");
      for (const g of (groupBy && x.type(groupBy.type)?.enumValues) || []) {
        await x.check(
          `${name} groupedAggregates ${g.name}`,
          `{ ${name} { groupedAggregates(groupBy: [${g.name}]) { keys ${parts} } } }`,
        );
      }
    }
    const filterArg = field.args.find((a) => a.name === "filter");
    for (const f of (filterArg && x.type(filterArg.type)?.inputFields) || []) {
      const ops = (x.type(f.type)?.inputFields ?? []).map((i) => i.name);
      if (ops.includes("isNull")) {
        await x.check(
          `${name} ${f.name} isNull`,
          `{ ${name}(filter: { ${f.name}: { isNull: false } }) { totalCount } }`,
        );
      } else if (ops.includes("some")) {
        for (const q of [
          "some: {}",
          "none: {}",
          "every: {}",
          ...(ops.includes("count") ? ["count: { greaterThan: 0 }"] : []),
        ]) {
          await x.check(`${name} ${f.name} ${q}`, `{ ${name}(filter: { ${f.name}: { ${q} } }) { totalCount } }`);
        }
      }
    }
    for (const rel of x.types.get(node)?.fields ?? []) {
      const t = x.type(rel.type);
      if ((t?.kind !== "OBJECT" && t?.kind !== "INTERFACE") || rel.args.some((a) => a.type.kind === "NON_NULL"))
        continue;
      const inner = t.name.endsWith("Connection")
        ? `${rel.name}(first: 3) { totalCount nodes { __typename } }`
        : `${rel.name} { ${x.selection(t.name, 1)} }`;
      await x.check(`${name}.${rel.name}`, `{ ${name}(first: 3) { nodes { ${inner} } } }`);
    }
  }
  return { requests: x.requests, failures: x.failures };
}

/** Malformed and boundary values: what clients send by mistake, or on purpose. */
const HOSTILE: unknown[] = [
  "",
  " ",
  "'",
  "''; DROP TABLE x; --",
  "\\",
  "%",
  "_",
  "\u0000",
  "a\u0000b",
  "😀",
  "a".repeat(5000),
  "-1",
  "NaN",
  "Infinity",
  "1e309",
  "99999999999999999999999999",
  "2026-13-45",
  "0000-00-00",
  "24:00:00",
  "not-a-uuid",
  "00000000-0000-0000-0000-000000000000",
  "P1Y2M",
  "[1,2)",
  "(,)",
  "{a,b}",
  "$.a",
  "$$",
  "(",
  "[",
  "*",
  "a{99999}",
];
const NUMBERS: unknown[] = [0, -1, 1.5, 2147483647, 2147483648, -2147483649, 1e308];
const CURSORS = ["", "x", "%%%", "bm90LWpzb24=", "WzFd", "AAAA", "MTsgRFJPUCBUQUJMRSB4Ow==", "a".repeat(3000)];

/**
 * Sends hostile values to every operator of every scalar filter (each filter input type a few
 * times, not once per column), to pagination arguments and cursors, and to `node(id:)`.
 */
export async function fuzz(url: string, options: Options = {}): Promise<{ requests: number; failures: Failure[] }> {
  const x = new Explorer(url, options);
  await x.load();
  const seen = new Map<string, number>();
  const connections = x.connections();
  for (const { field } of connections) {
    const filterArg = field.args.find((a) => a.name === "filter");
    for (const column of (filterArg && x.type(filterArg.type)?.inputFields) || []) {
      const filter = x.type(column.type);
      if (!filter?.inputFields || filter.name.endsWith("ToManyFilter") || filter.name === filterArg?.type.name)
        continue;
      const times = seen.get(filter.name) ?? 0;
      if (times >= 3) continue;
      seen.set(filter.name, times + 1);
      for (const op of filter.inputFields) {
        const opType = named(op.type);
        if (x.types.get(opType.name ?? "")?.kind === "INPUT_OBJECT") continue;
        const list = isList(op.type);
        const values =
          opType.name === "Int" || opType.name === "Float" ? NUMBERS : opType.name === "Boolean" ? [true] : HOSTILE;
        for (const v of values) {
          await x.check(
            `${field.name} ${column.name}.${op.name} ${JSON.stringify(v).slice(0, 30)}`,
            `query($v: ${list ? `[${opType.name}!]` : opType.name}) { ${field.name}(first: 1, filter: { ${column.name}: { ${op.name}: $v } }) { totalCount nodes { __typename } } }`,
            { v: list ? [v, v] : v },
          );
        }
      }
    }
  }
  for (const { field } of connections.slice(0, 5)) {
    for (const c of CURSORS) {
      for (const [page, sel] of [
        ["first: 1, after: $c", "nodes { __typename }"],
        ["last: 1, before: $c", "nodes { __typename }"],
        ["first: 1, after: $c", "totalCount"],
      ]) {
        await x.check(
          `${field.name}(${page}) ${c.slice(0, 12)}`,
          `query($c: String) { ${field.name}(${page}) { ${sel} } }`,
          {
            c,
          },
        );
      }
    }
    for (const n of [-1, 0, 2147483647]) {
      for (const arg of ["first", "last", "offset"]) {
        await x.check(`${field.name}(${arg}: ${n})`, `query($n: Int) { ${field.name}(${arg}: $n) { totalCount } }`, {
          n,
        });
      }
    }
  }
  for (const id of ["", "x", "%%", "Tm9wZTox", "a".repeat(3000), "\u0000"]) {
    await x.check(`node(${id.slice(0, 12)})`, `query($id: ID!) { node(id: $id) { id } }`, { id });
  }
  return { requests: x.requests, failures: x.failures };
}
