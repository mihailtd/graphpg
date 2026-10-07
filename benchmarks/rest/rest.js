// k6 load test of the REST APIs of graphpg and PostgREST on the same database
// (benchmarks/rest/schema.sql): the same logical requests, each in the grammar of its server.
//
//   TARGET=graphpg   URL=http://localhost:4000 k6 run benchmarks/rest/rest.js
//   TARGET=postgrest URL=http://localhost:3000 k6 run benchmarks/rest/rest.js
//
// VUS (default 20), DURATION per scenario (default 10s), SCENARIOS (comma-separated names,
// default all). Each scenario runs alone, one after the other. `compare.sh` runs both.
import http from 'k6/http';
import { check } from 'k6';

const target = __ENV.TARGET || 'graphpg';
const base = (__ENV.URL || (target === 'graphpg' ? 'http://localhost:4000' : 'http://localhost:3000')).replace(/\/$/, '');
const vus = parseInt(__ENV.VUS || '20', 10);
const duration = parseInt(__ENV.DURATION || '10', 10);
const POSTS = parseInt(__ENV.POSTS || '100000', 10);
const NOTES = 1000;

const json = { 'Content-Type': 'application/json' };
const rand = (min, max) => min + Math.floor(Math.random() * (max - min + 1));
// Start rows far into the table, so offset and keyset pagination have something to skip
const deep = () => rand(Math.floor(POSTS * 0.1), Math.floor(POSTS * 0.9));

// name -> what to send, per server. `rows`: the answer is a non-empty list.
const SCENARIOS = {
  simple: {
    describe: 'list: 20 rows, 3 columns, ordered by key',
    rows: true,
    graphpg: () => ['GET', '/rest/posts?fields=id,title,status&order=id&first=20'],
    postgrest: () => ['GET', '/posts?select=id,title,status&order=id&limit=20'],
  },
  filtered: {
    describe: 'list: filter on an enum, newest first (indexed)',
    rows: true,
    graphpg: () => ['GET', '/rest/posts?filter[status][eq]=PUBLISHED&order=-createdAt&first=20&fields=id,title'],
    postgrest: () => ['GET', '/posts?status=eq.published&order=created_at.desc&limit=20&select=id,title'],
  },
  by_key: {
    describe: 'one row by primary key',
    graphpg: () => ['GET', `/rest/posts/${rand(1, POSTS)}`],
    postgrest: () => ['GET', `/posts?id=eq.${rand(1, POSTS)}`, null, { Accept: 'application/vnd.pgrst.object+json' }],
  },
  embedded: {
    describe: 'list of 20 posts with their author and their comments embedded',
    rows: true,
    graphpg: () => ['GET', '/rest/posts?include=author,comments&fields=id,title&fields[author]=username&fields[comments]=id,body&order=id&first=20'],
    postgrest: () => ['GET', '/posts?select=id,title,author:authors(username),comments(id,body)&order=id&limit=20'],
  },
  offset_deep: {
    describe: 'pagination: offset deep into the table',
    rows: true,
    graphpg: () => ['GET', `/rest/posts?order=id&first=20&offset=${deep()}&fields=id,title`],
    postgrest: () => ['GET', `/posts?order=id&limit=20&offset=${deep()}&select=id,title`],
  },
  keyset_deep: {
    describe: 'pagination: keyset (rows after a key) deep into the table',
    rows: true,
    graphpg: () => ['GET', `/rest/posts?filter[id][gt]=${deep()}&order=id&first=20&fields=id,title`],
    postgrest: () => ['GET', `/posts?id=gt.${deep()}&order=id&limit=20&select=id,title`],
  },
  insert: {
    describe: 'write: insert one row',
    write: true,
    graphpg: () => ['POST', '/rest/notes', JSON.stringify({ body: 'benchmark' }), { Prefer: 'return=minimal' }],
    postgrest: () => ['POST', '/notes', JSON.stringify({ body: 'benchmark' }), { Prefer: 'return=minimal' }],
  },
  update: {
    describe: 'write: update one row by key',
    write: true,
    graphpg: () => ['PATCH', `/rest/notes/${rand(1, NOTES)}`, JSON.stringify({ hits: rand(1, 1e6) }), { Prefer: 'return=minimal' }],
    postgrest: () => ['PATCH', `/notes?id=eq.${rand(1, NOTES)}`, JSON.stringify({ hits: rand(1, 1e6) }), { Prefer: 'return=minimal' }],
  },
  update_many: {
    describe: 'write: update 10 rows by filter',
    write: true,
    graphpg: () => {
      const from = rand(1, NOTES - 10);
      return ['PATCH', `/rest/notes?filter[id][gte]=${from}&filter[id][lt]=${from + 10}`, JSON.stringify({ hits: rand(1, 1e6) }), { Prefer: 'return=minimal' }];
    },
    postgrest: () => {
      const from = rand(1, NOTES - 10);
      return ['PATCH', `/notes?id=gte.${from}&id=lt.${from + 10}`, JSON.stringify({ hits: rand(1, 1e6) }), { Prefer: 'return=minimal' }];
    },
  },
};

const names = (__ENV.SCENARIOS || Object.keys(SCENARIOS).join(',')).split(',').filter(Boolean);

export const options = {
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(90)', 'p(95)', 'p(99)'],
  scenarios: Object.fromEntries(
    names.map((name, i) => [name, {
      executor: 'constant-vus',
      exec: 'run',
      env: { SCENARIO: name },
      vus,
      duration: `${duration}s`,
      startTime: `${i * (duration + 2)}s`,
      gracefulStop: '1s',
    }]),
  ),
  // Thresholds that always hold: they make k6 keep a summary per scenario
  thresholds: Object.fromEntries(names.flatMap((name) => [
    [`http_req_duration{scenario:${name}}`, ['max>=0']],
    [`http_reqs{scenario:${name}}`, ['count>=0']],
    [`http_req_failed{scenario:${name}}`, ['rate>=0']],
  ])),
};

export function run() {
  const scenario = SCENARIOS[__ENV.SCENARIO];
  const [method, path, body, headers] = scenario[target]();
  const res = http.request(method, base + path, body || null, { headers: { ...json, ...headers }, tags: { name: __ENV.SCENARIO } });
  check(res, {
    'status is 2xx': (r) => r.status >= 200 && r.status < 300,
    ...(scenario.rows ? { 'rows came': (r) => Array.isArray(r.json()) && r.json().length > 0 } : {}),
  });
}

export function handleSummary(data) {
  const lines = [
    `| scenario | requests/s | p50 ms | p95 ms | p99 ms | failed |`,
    `| :--- | ---: | ---: | ---: | ---: | ---: |`,
  ];
  for (const name of names) {
    const t = data.metrics[`http_req_duration{scenario:${name}}`]?.values;
    const n = data.metrics[`http_reqs{scenario:${name}}`]?.values;
    const f = data.metrics[`http_req_failed{scenario:${name}}`]?.values;
    if (!t || !n) continue;
    lines.push(`| ${name} | ${(n.count / duration).toFixed(0)} | ${t.med.toFixed(2)} | ${t['p(95)'].toFixed(2)} | ${t['p(99)'].toFixed(2)} | ${(((f.rate ?? f.value ?? 0)) * 100).toFixed(1)}% |`);
  }
  const out = `\n${target} (${vus} VUs, ${duration}s per scenario)\n\n${lines.join('\n')}\n\n`;
  const file = __ENV.OUT || `benchmarks/rest/results/${target}.md`;
  return { stdout: out, [file]: out };
}
