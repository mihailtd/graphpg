// k6 load test for graphpg against the blog example (examples/blog).
//
//   k6 run benchmarks/queries.js
//   VUS=50 DURATION=30s URL=http://localhost:4000/graphql k6 run benchmarks/queries.js
//
// Start graphpg (and PostgreSQL with examples/blog/schema.sql loaded) first.
import http from 'k6/http';
import { check } from 'k6';

const url = __ENV.URL || 'http://localhost:4000/graphql';

export const options = {
  vus: parseInt(__ENV.VUS || '20', 10),
  duration: __ENV.DURATION || '15s',
  thresholds: { http_req_failed: ['rate<0.01'] },
};

const QUERIES = {
  baseline: '{ __typename }',
  list: '{ postsConnection(first: 20, orderBy: [CREATED_AT_DESC]) { nodes { id title status } } }',
  filter: '{ postsConnection(first: 20, filter: { status: { equalTo: PUBLISHED } }) { nodes { id title } } }',
  relations: `{
    authors {
      username
      posts { title tags { name } commentsConnection { totalCount } }
    }
  }`,
};

export default function () {
  for (const [name, query] of Object.entries(QUERIES)) {
    const res = http.post(url, JSON.stringify({ query }), {
      headers: { 'Content-Type': 'application/json' },
      tags: { query: name },
    });
    check(res, {
      [`${name}: 200`]: (r) => r.status === 200,
      [`${name}: no errors`]: (r) => !r.json('errors'),
    });
  }
}
