// k6 stress-test suite comparing PostgreSQL 18 vs PostgreSQL 19 under graphpg.
// Normally driven by compare.sh (pinned jit, interleaved rounds, medians).
//
//   LABEL=pg18 URL=http://localhost:4009 k6 run benchmarks/pg19/pg19.js
//   LABEL=pg19 URL=http://localhost:4009 k6 run benchmarks/pg19/pg19.js
//
// Env vars:
//   URL       base URL of graphpg (default http://localhost:4009)
//   LABEL     label for this run (default pg18)
//   VUS       virtual users per scenario (default 20; export_ndjson caps at 4)
//   DURATION  seconds per scenario (default 10)
//   SCENARIOS comma-separated subset of scenario names (default all)
//   OUT       optional JSON summary path (default benchmarks/pg19/results/<LABEL>.json)
import { check } from 'k6';
import http from 'k6/http';

const base = (__ENV.URL || 'http://localhost:4009').replace(/\/$/, '');
const label = __ENV.LABEL || 'pg18';
const vus = parseInt(__ENV.VUS || '20', 10);
const duration = parseInt(__ENV.DURATION || '10', 10);
const AUTHORS = parseInt(__ENV.AUTHORS || '1000', 10);
const POSTS = parseInt(__ENV.POSTS || '100000', 10);
const TAGS = parseInt(__ENV.TAGS || '5000', 10);
const CATEGORIES = parseInt(__ENV.CATEGORIES || '2000', 10);

const jsonHeaders = { 'Content-Type': 'application/json' };
const rand = (min, max) => min + Math.floor(Math.random() * (max - min + 1));

const COC_MUTATION = `
  mutation($authorId: Int!, $slug: String!, $name: String!) {
    createNote(input: {
      authorId: $authorId
      body: "coc"
      tag: { connectOrCreate: { where: { slug: $slug }, create: { slug: $slug, name: $name } } }
    }) {
      note { databaseId tag { slug } }
    }
  }
`;

const EXPORT_BODY = JSON.stringify({
  query: '{ postsConnection(filter: { priority: { equalTo: 1 }, authorId: { lessThanOrEqualTo: 400 } }, orderBy: [PRIMARY_KEY_ASC]) { nodes { databaseId title status authorId } } }',
});

const SCENARIOS = {
  // --- Read paths: same SQL on both versions; any difference is the PostgreSQL engine ---
  simple: {
    describe: 'REST list: 20 rows, 3 columns, ordered by id',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => ['GET', '/rest/posts?fields=id,title,status&order=id&first=20'],
  },
  embedded: {
    describe: 'REST embedded: 20 posts + author + comments (lateral joins)',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => [
      'GET',
      '/rest/posts?include=author,comments&fields=id,title&fields[author]=username&fields[comments]=id,body&order=id&first=20',
    ],
  },
  not_in: {
    describe: 'REST filter: NOT IN 8 values on NOT NULL priority',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => ['GET', '/rest/posts?filter[priority][nin]=1,2,3,4,5,6,7,8&order=id&first=20&fields=id,title,priority'],
  },
  anti_join: {
    describe: 'REST anti=comments: posts with zero comments (LEFT JOIN -> ANTI JOIN + Memoize)',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => ['GET', '/rest/posts?anti=comments&order=id&first=20&fields=id,title'],
  },
  distinct_from: {
    describe: 'REST filter: priority IS DISTINCT FROM 1 on NOT NULL column',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => ['GET', '/rest/posts?filter[priority][isDistinctFrom]=1&order=id&first=20&fields=id,title,priority'],
  },
  distinct_null: {
    describe: 'REST filter: nullable summary IS DISTINCT FROM null',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => ['GET', '/rest/posts?filter[summary][isDistinctFrom]=null&order=id&first=20&fields=id,title'],
  },
  ancestors: {
    describe: 'REST hierarchy: 5-level category ancestors (recursive CTE IS DISTINCT FROM guard)',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => [
      'GET',
      `/rest/categories?include=ancestors&fields=id,name&fields[ancestors]=id,name&order=id&first=20&offset=${Math.floor(CATEGORIES * 0.8)}`,
    ],
  },
  aggregate_group: {
    describe: 'REST grouped aggregate over 100k posts (group=authorId,status)',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => ['GET', '/rest/posts/aggregate?group=authorId,status&sum=priority'],
  },
  toast_read: {
    describe: 'REST list reading TOAST-compressed ~2.6KB body (pglz on PG18 vs lz4 on PG19)',
    verify: (r) => Array.isArray(r.json()) && r.json().length > 0,
    request: () => [
      'GET',
      `/rest/posts?fields=id,title,body&order=id&first=50&offset=${rand(Math.floor(POSTS * 0.1), Math.floor(POSTS * 0.8))}`,
    ],
  },
  insert_fk: {
    describe: 'REST write: insert row with 2 FK checks (authorId, tagId)',
    request: () => [
      'POST',
      '/rest/notes',
      JSON.stringify({ authorId: rand(1, AUTHORS), tagId: rand(1, TAGS), body: 'bench' }),
      { Prefer: 'return=minimal' },
    ],
  },

  // --- Write paths: on PostgreSQL 19 graphpg compiles connectOrCreate to ON CONFLICT DO SELECT ---
  connect_or_create_hit: {
    describe: 'GraphQL connectOrCreate hitting an existing unique slug (2-CTE on PG 18, ON CONFLICT DO SELECT on PG 19)',
    verify: (r) => {
      const body = r.json();
      return !body.errors && body.data?.createNote?.note?.tag?.slug != null;
    },
    request: () => {
      const slug = `tag-${rand(1, TAGS)}`;
      return [
        'POST',
        '/graphql',
        JSON.stringify({
          query: COC_MUTATION,
          variables: { authorId: rand(1, AUTHORS), slug, name: slug },
        }),
      ];
    },
  },
  connect_or_create_race: {
    describe: 'GraphQL connectOrCreate under concurrent new-slug contention (fail % = 23505 races surfaced to clients)',
    verify: (r) => {
      const body = r.json();
      return !body.errors && body.data?.createNote?.note?.tag?.slug != null;
    },
    request: () => {
      const bucket = Math.floor(Date.now() / 50);
      const slug = `race-${label}-${bucket}`;
      return [
        'POST',
        '/graphql',
        JSON.stringify({
          query: COC_MUTATION,
          variables: { authorId: rand(1, AUTHORS), slug, name: slug },
        }),
      ];
    },
  },
  export_ndjson: {
    describe: 'POST /export streaming ~2000 NDJSON rows (portal fetch on PG 18, COPY ... FORMAT json on PG 19)',
    vus: Math.min(vus, 4),
    verify: (r) => typeof r.body === 'string' && r.body.includes('"databaseId"'),
    request: () => ['POST', '/export', EXPORT_BODY],
  },
};

const names = (__ENV.SCENARIOS || Object.keys(SCENARIOS).join(','))
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean);

let offsetSeconds = 0;
const scenarioConfigs = {};
for (const name of names) {
  const spec = SCENARIOS[name];
  if (!spec) {
    throw new Error(`Unknown scenario: ${name}`);
  }
  scenarioConfigs[name] = {
    executor: 'constant-vus',
    exec: 'run',
    env: { SCENARIO: name },
    vus: spec.vus || vus,
    duration: `${duration}s`,
    startTime: `${offsetSeconds}s`,
    gracefulStop: '1s',
  };
  offsetSeconds += duration + 2;
}

export const options = {
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(90)', 'p(95)', 'p(99)'],
  scenarios: scenarioConfigs,
  thresholds: Object.fromEntries(
    names.flatMap((name) => [
      [`http_req_duration{scenario:${name}}`, ['max>=0']],
      [`http_reqs{scenario:${name}}`, ['count>=0']],
      [`http_req_failed{scenario:${name}}`, ['rate>=0']],
      [`checks{scenario:${name}}`, ['rate>=0']],
    ]),
  ),
};

export function run() {
  const scenario = SCENARIOS[__ENV.SCENARIO];
  const [method, path, body, extraHeaders] = scenario.request();
  const res = http.request(method, base + path, body || null, {
    headers: { ...jsonHeaders, ...extraHeaders },
    tags: { name: __ENV.SCENARIO },
  });
  check(res, {
    'status is 2xx': (r) => r.status >= 200 && r.status < 300,
    ...(scenario.verify ? { 'payload ok': scenario.verify } : {}),
  });
}

export function handleSummary(data) {
  const rows = [];
  const lines = [
    '| scenario | VUs | requests/s | p50 ms | p95 ms | p99 ms | http err % | check fail % |',
    '| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |',
  ];
  for (const name of names) {
    const t = data.metrics[`http_req_duration{scenario:${name}}`]?.values;
    const n = data.metrics[`http_reqs{scenario:${name}}`]?.values;
    const f = data.metrics[`http_req_failed{scenario:${name}}`]?.values;
    const c = data.metrics[`checks{scenario:${name}}`]?.values;
    if (!t || !n) continue;
    const scenarioVus = scenarioConfigs[name].vus;
    const rps = n.count / duration;
    const httpFailedRate = (f?.rate ?? f?.value ?? 0) * 100;
    const checkFailRate = c ? (1 - (c.rate ?? 1)) * 100 : 0;
    rows.push({
      scenario: name,
      vus: scenarioVus,
      count: n.count,
      rps,
      p50: t.med,
      p95: t['p(95)'],
      p99: t['p(99)'],
      httpFailedPct: httpFailedRate,
      checkFailPct: checkFailRate,
    });
    lines.push(
      `| ${name} | ${scenarioVus} | ${rps.toFixed(0)} | ${t.med.toFixed(2)} | ${t['p(95)'].toFixed(2)} | ${t['p(99)'].toFixed(2)} | ${httpFailedRate.toFixed(1)}% | ${checkFailRate.toFixed(1)}% |`,
    );
  }
  const md = `\n${label} (${vus} VUs default, ${duration}s per scenario)\n\n${lines.join('\n')}\n\n`;
  const outPath = __ENV.OUT || `benchmarks/pg19/results/${label}.json`;
  return {
    stdout: md,
    [outPath]: JSON.stringify({ label, vus, duration, scenarios: rows }, null, 2),
  };
}
