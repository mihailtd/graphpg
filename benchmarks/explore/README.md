# Exploratory load tests (k6)

[k6](https://k6.io) scripts that test a running graphpg with **what its own schema offers**: they read the schema by introspection and generate the requests, so they need no knowledge of the database and work on any one. Each counts `engine_failures`, the requests the engine *failed* on (a 5xx, or an error that is not one of its typed refusals), and fails when there is one. Requests refused as invalid input or over a limit are expected, and counted as `refusals`.

| Script | What it sends |
| :--- | :--- |
| `crawl.js` | Every root connection's rows, every ordering (and the pages after and before a cursor), every aggregate and grouping, every relation filter and relation |
| `fuzz.js` | Malformed and boundary values for every operator of every scalar filter, for cursors, page sizes and `node(id:)` |
| `writes.js` | A row of every table created, updated and deleted, with inputs built from the schema (required fields, references to rows that exist) |
| `soak.js` | All at once for `DURATION`: reads (the crawl's requests), writes, REST reads, and live queries over WebSocket, clients subscribing and leaving |

```bash
# Against a graphpg on localhost:4000 (graphql.introspection must be on)
k6 run benchmarks/explore/crawl.js
k6 run benchmarks/explore/fuzz.js
k6 run benchmarks/explore/writes.js        # writes: use a database you can write to
DURATION=10m k6 run benchmarks/explore/soak.js
```

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

Run against [graphpg-fhir](https://github.com/mihailtd/graphpg-fhir) (FHIR R5, 177 tables), they found the bugs fixed in [#201](https://github.com/mihailtd/graphpg/pull/201): duplicated many-to-many rows, `min`/`max` failing on `uuid`, cursors bound for pages not read, unvalidated input reaching PostgreSQL, and a row with many relations taking gigabytes to plan (the soak got the database killed for lack of memory).
