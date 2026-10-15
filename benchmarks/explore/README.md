# Exploratory load tests (k6)

[k6](https://k6.io) scripts that test a running graphpg with **what its own schema offers**: they read the schema by introspection and generate the requests, so they need no knowledge of the database and work on any one. Each counts `engine_failures`, the requests the engine *failed* on (a 5xx, or an error that is not one of its typed refusals), and fails when there is one. Requests refused as invalid input or over a limit are expected, and counted as `refusals`.

| Script | What it sends |
| :--- | :--- |
| `crawl.js` | Every root connection's rows, every ordering (and the pages after and before a cursor), every aggregate and grouping, every relation filter and relation |
| `fuzz.js` | Malformed and boundary values for every operator of every scalar filter, for cursors, page sizes and `node(id:)` |
| `writes.js` | A row of every table created, updated and deleted, with inputs built from the schema (required fields, references to rows that exist) |
| `soak.js` | All at once for `DURATION`: reads (the crawl's requests), writes, REST reads, and live queries over WebSocket, clients subscribing and leaving |
| `auth.js` | Access tokens of every shape against a server with `AUTH_JWT_SECRET`: valid ones with hostile claims (long, Unicode, nested, quoting, setting-like names) must be served, invalid ones (expired, another audience or issuer, unsigned, tampered, NUL characters) refused |

```bash
# Against a graphpg on localhost:4000 (graphql.introspection must be on)
k6 run benchmarks/explore/crawl.js
k6 run benchmarks/explore/fuzz.js
k6 run benchmarks/explore/writes.js        # writes: use a database you can write to
DURATION=10m k6 run benchmarks/explore/soak.js
# auth: the server's secret, issuer and audience, and what to read
AUTH_SECRET=... AUTH_ISSUER=https://issuer/ AUTH_AUDIENCE=api \
  QUERY='{ forumsConnection(first: 1) { nodes { name } } }' RESOURCE=forums k6 run benchmarks/explore/auth.js
```

Requests answered `503` with `SCHEMA_CHANGED` or `DATABASE_UNAVAILABLE` are retryable refusals, not failures: run the soak while migrations run or the database restarts, and only engine failures fail it.

| Variable | Default | |
| :--- | :--- | :--- |
| `BASE` | `http://localhost:4000` | The server |
| `MODE` | `cover` | `cover`: every request once, the plan split between the VUs; `load`: requests replayed for `DURATION` |
| `VUS`, `DURATION` | `10`, `1m` | Virtual users, and how long `load` and `soak.js` run |
| `ONLY` | | Only root fields (and mutations) whose name matches this regular expression |
| `READ_VUS`, `WRITE_VUS`, `REST_VUS`, `SUBSCRIPTION_VUS` | `40`, `5`, `10`, `50` | The mix of `soak.js` |

`soak.js` subscribes many times from one address: raise `subscriptions.max_per_subject` (or authenticate) so the per-client limit does not refuse them (refusals are not counted as failures). Watch the server's and the database's memory while it runs (`podman stats`, `/metrics`).

The same requests run without load in the end-to-end suite (`e2e/src/explore.ts`, `pnpm --filter @graphpg/e2e explore <url>`).

## What they found

A second round ran the soak under continuous DDL (columns renamed and dropped, tables created and dropped, column types changed, every 0.3 s) and across a database restart and a kill, and `auth.js`: they found the fixes of the auth, DDL and failover round (requests caught by a schema change or a lost connection failing as internal errors, the dev watcher reloading in a loop after a restart, a catalog panic during DDL, NUL claims, an unauthenticated `GET /rest`). The rounds now end with no engine failure.

The first round:

Run against [graphpg-fhir](https://github.com/mihailtd/graphpg-fhir) (FHIR R5, 177 tables), they found the bugs fixed in [#201](https://github.com/mihailtd/graphpg/pull/201): duplicated many-to-many rows, `min`/`max` failing on `uuid`, cursors bound for pages not read, unvalidated input reaching PostgreSQL, and a row with many relations taking gigabytes to plan (the soak got the database killed for lack of memory).
