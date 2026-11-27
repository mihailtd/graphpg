# PostgreSQL 19

**PostgreSQL 19** was released on November 19, 2026 ([release notes](https://www.postgresql.org/docs/19/release-19.html)). One compiled `graphpg` binary supports both **PostgreSQL 18** and **PostgreSQL 19**: it reads the server's version and turns on what PostgreSQL 19 adds only when the server has it. Upgrading PostgreSQL needs no new graphpg build and no configuration, and a deployment that stays on PostgreSQL 18 keeps working as before.

- [How graphpg detects the version](#how-graphpg-detects-the-version)
- [What PostgreSQL 18 deployments get too](#what-postgresql-18-deployments-get-too)
- [Benchmark](#benchmark)

## At a glance

| PostgreSQL 19 capability | How `graphpg` uses it | Fallback on PostgreSQL 18 |
| :--- | :--- | :--- |
| [`INSERT ... ON CONFLICT DO SELECT`](https://www.postgresql.org/docs/19/sql-insert.html) | Atomic `connectOrCreate`, GraphQL `ConflictAction.SELECT`, and REST `resolution=select` in one probe with zero dead tuples | Two-CTE `WITH _found ... _new` (`connectOrCreate`); `SELECT` conflict action omitted from SDL / OpenAPI |
| [`WAIT FOR LSN`](https://www.postgresql.org/docs/19/sql-wait.html) | Bounded standby wait via `database.replica_wait_timeout` for `X-Min-WAL-Position` read-your-writes | Immediate `pg_last_wal_replay_lsn() >= $1` check, falling back to primary if behind |
| [`COPY ... (FORMAT json)` & `FORCE_ARRAY`](https://www.postgresql.org/docs/19/sql-copy.html) | Native server-side streaming for `POST /export` (`ndjson` and `"format": "json"`) | Zero-copy `::text` batched portal fetch (skips `serde_json::Value` round-trips) |
| [`IGNORE NULLS` / `RESPECT NULLS`](https://www.postgresql.org/docs/19/sql-expressions.html) | `lag`, `lead`, `firstValue`, `lastValue`, and `nthValue` with `ignoreNulls` in GraphQL and REST | Ranking and `runningSum` windows only; value window fields omitted from SDL / OpenAPI |
| [`EXPLAIN (..., IO)`](https://www.postgresql.org/docs/19/sql-explain.html) & [`pg_stat_statements`](https://www.postgresql.org/docs/19/pgstatstatements.html) | Adds `IO` to `--debug-explain` and surfaces `genericPlanCalls` / `customPlanCalls` in `graphpg advise --statements` | `EXPLAIN (ANALYZE, BUFFERS, SERIALIZE, MEMORY)` and execution stats without plan-type split |
| [`postgres_fdw`](https://www.postgresql.org/docs/19/postgres-fdw.html) & [`pg_stash_advice`](https://www.postgresql.org/docs/19/pgstashadvice.html) | `import_stats 'true'` advice for unanalyzed foreign tables; automatic array pushdown on prepared statements; external plan pinning | Manual foreign-table `ANALYZE`; local array filtering on foreign scans |
| [JSONPath string methods](https://www.postgresql.org/docs/19/functions-json.html#FUNCTIONS-SQLJSON-PATH) | `matchesPath`, `existsPath` and `path` compile to PostgreSQL's own `@@`, `@?` and `jsonb_path_query_first` with the expression as a bound `jsonpath` parameter, so `.lower()`, `.upper()`, `.initcap()`, `.replace()`, `.split_part()` and `.btrim()` / `.ltrim()` / `.rtrim()` work in filters and projections, evaluated inside the one SQL statement | PostgreSQL 18's JSONPath; a string method is rejected by PostgreSQL's parser and returned as `BAD_USER_INPUT` (HTTP 400 in REST) |
| `lz4` default TOAST compression | Nothing to change in graphpg: PostgreSQL compresses new large values with `lz4` | `pglz` |

## How graphpg detects the version

graphpg never assumes a version. It asks the server:

- **The primary**: `current_setting('server_version_num')` is read each time graphpg inspects the catalog, at startup and on every schema reload (`NOTIFY graphpg_reload`, `SIGHUP`, or the schema watcher in dev mode). After an in-place `pg_upgrade`, graphpg's connections are new ones to the upgraded server, and the next reload or restart turns the PostgreSQL 19 paths on.
- **Each read replica**: checked every 10 seconds together with `pg_is_in_recovery()`. During a rolling upgrade a replica already on PostgreSQL 19 waits with `WAIT FOR LSN`, while one still on 18 keeps the immediate LSN check. The two can serve side by side.
- **Gating**: every PostgreSQL 19 code path is behind `server_version >= 190000`. Below that, graphpg emits no PostgreSQL 19 syntax, and the GraphQL schema and OpenAPI document leave out what needs it (`ConflictAction.SELECT`, `resolution=select`, the `lag` / `lead` / `firstValue` / `lastValue` / `nthValue` window fields). Asking for them on PostgreSQL 18 is a validation error, not a failed query.

What this means for an upgrade:

- **The API only grows.** On PostgreSQL 19 the schema gains the fields and enum values above, and nothing is removed or renamed, so existing clients keep working.
- **Pinned schemas see that growth as drift.** If you pin the SDL or OpenAPI document ([pinned schemas](configuration.md#pinned-schemas)), export them again after upgrading (`graphpg schema export`). With `export.pin: fail` (the production default), a reload that finds the grown schema is refused and the current one keeps serving, and a restart refuses to start. Pin against the version you run in production. A pin exported on 19 does not match an instance still on 18.
- **No flag is needed to go back.** Pointing graphpg at a PostgreSQL 18 server again simply leaves the PostgreSQL 19 paths off.

## 1. Race-free `connectOrCreate` with `ON CONFLICT DO SELECT`

`connectOrCreate` returns the row with a key, or else creates it. The hard case is two requests creating the same new key at once: both look, both find nothing, both insert, and the slower one used to fail with `23505 unique_violation`. Under contention in the [benchmark](#benchmark), that was 2–8% of the requests on PostgreSQL 18 before graphpg 1.4.

On PostgreSQL 19+, when `where` targets a unique key and `create` sets the same key values, `graphpg` compiles `connectOrCreate` into one statement:

```sql
INSERT INTO "app"."tags" AS _n ("slug", "name")
VALUES ($1::text, $2::text)
ON CONFLICT ("slug") DO SELECT
RETURNING _n.*
```

- **No race errors**: a request that loses the race waits for the winning transaction and gets back the committed row. In the benchmark, `connect_or_create_race` failed 0.0% of the time on PostgreSQL 19. Since graphpg 1.4, PostgreSQL 18 gets no race errors either ([below](#what-postgresql-18-deployments-get-too)).
- **One statement for found and created rows**: when the row exists, PostgreSQL finds it with the unique index and writes nothing.
- **Same least-privilege model**: `graphpg` intentionally emits plain `DO SELECT` rather than `DO SELECT FOR KEY SHARE` because PostgreSQL 19 requires `UPDATE` table privilege for `FOR KEY SHARE`, whereas plain `DO SELECT` needs only `SELECT, INSERT` (PostgreSQL still acquires `FOR KEY SHARE` internally when inserting referencing child foreign keys).
- **Explicit `SELECT` conflict action on upserts**: on PostgreSQL 19+, GraphQL exposes `ConflictAction.SELECT` and REST exposes `PUT /rest/<table>?conflict=<cols>&resolution=select` to return both newly inserted and pre-existing rows unchanged:

```graphql
mutation {
  createTags(
    inputs: [{ slug: "existing", label: "Ignored" }, { slug: "new", label: "New" }]
    onConflict: { key: SLUG, action: SELECT }
  ) {
    tags { slug label }
  }
}
```

```bash
curl -X PUT 'localhost:4000/rest/tags?conflict=slug&resolution=select' \
  -H 'content-type: application/json' \
  -d '[{"slug": "existing", "label": "Ignored"}, {"slug": "new", "label": "New"}]'
```

See [GraphQL mutations](graphql-api.md#upsert-and-bulk-writes) and [REST writes](rest-api.md#creating-updating-and-deleting).

On PostgreSQL 18, the same `connectOrCreate` keeps the two-CTE form (`WITH _found AS (SELECT ...), _new AS (INSERT ... WHERE NOT EXISTS (SELECT 1 FROM _found) ...)`), but it now names the same unique key in `ON CONFLICT ("slug") DO NOTHING`. See [what PostgreSQL 18 deployments get too](#what-postgresql-18-deployments-get-too).

## 2. Read-your-writes on standbys with `WAIT FOR LSN`

Before PostgreSQL 19, reading from a standby after sending `X-Min-WAL-Position` could only compare `pg_last_wal_replay_lsn() >= $1` once; even 1 ms of replication lag sent the read back to the primary.

With PostgreSQL 19's [`WAIT FOR LSN`](https://www.postgresql.org/docs/19/sql-wait.html), configure a short bounded wait in [configuration](configuration.md):

```yaml
database:
  replica_wait_timeout: 50ms
```

When a client sends `X-Min-WAL-Position: <lsn>` and the chosen replica reports `server_version >= 190000`, `graphpg` validates the LSN and runs:

```sql
WAIT FOR LSN '<lsn>' WITH (MODE 'standby_replay', TIMEOUT '50ms', NO_THROW);
```

- If the standby replays up to `<lsn>` within the budget (`success`), the query executes on the replica.
- If the timeout expires or the target returns `not in recovery`, `graphpg` marks/skips appropriately and falls back cleanly to the primary without surfacing an error.
- Replicas still on PostgreSQL 18 continue to use the non-blocking LSN check automatically during rolling upgrades.

## 3. Streaming exports with `COPY ... (FORMAT json)`

`POST /export` benefits on both major versions:

- **PostgreSQL 19+**: homogeneous single-root connection exports compile into server-side `COPY (SELECT ...) TO STDOUT (FORMAT json)` for NDJSON (or `FORMAT json, FORCE_ARRAY` when `"format": "json"` is requested), with `$1..$N` parameters safely inlined via collision-free dollar-quoting ([`SqlCompiler::compile_rows_copy`](../crates/graphpg/src/compiler/sql.rs)). Empty `"format": "json"` exports emit `[]\n` so JSON consumers always receive valid JSON.
- **PostgreSQL 18+ fallback**: parameterized or polymorphic exports that stay on portals now use `compile_rows_text` (`::text`), streaming UTF-8 JSON strings straight from PostgreSQL without parsing and re-serializing `serde_json::Value` per row in Rust.

```bash
curl -X POST localhost:4000/export \
  -H 'content-type: application/json' \
  -d '{"query": "{ postsConnection(orderBy: [PRIMARY_KEY_ASC]) { nodes { databaseId title } } }", "format": "json"}'
```

## 4. `IGNORE NULLS` value window functions

Analytics queries over sparse sensor or ledger rows often need forward-fill ("last observation carried forward") without self-joins or subqueries. When connected to PostgreSQL 19+, `graphpg` adds `<Type>WindowValues` and five edge fields in GraphQL — `lag`, `lead`, `firstValue`, `lastValue`, and `nthValue` — with an `ignoreNulls` argument (`IGNORE NULLS` / `RESPECT NULLS`), alongside matching REST query parameters:

::: code-group

```graphql [GraphQL]
{
  readingsConnection(orderBy: [SEQ_ASC, PRIMARY_KEY_ASC]) {
    edges {
      node { grp seq }
      lastValue(partitionBy: [GRP], ignoreNulls: true) { val label }
      lag(partitionBy: [GRP], offset: 1, ignoreNulls: true) { val }
      nthValue(n: 2, partitionBy: [GRP], ignoreNulls: true) { val }
    }
  }
}
```

```bash [REST]
curl -g 'localhost:4000/rest/readings?filter[grp][eq]=a&order=seq&partition=grp&lastValue=val,label&nthValue=val&windowNth=2&ignoreNulls=true'
```

:::

## 5. Diagnostics, plan stability, and `postgres_fdw`

- **`EXPLAIN (ANALYZE, ..., IO)`**: with `debug.explain: true`, requests against PostgreSQL 19+ include the `IO` option automatically so async I/O wait counts appear in `extensions.explain`.
- **`pg_stat_statements` plan mode counts**: on PostgreSQL 19+ (after `ALTER EXTENSION pg_stat_statements UPDATE` if upgraded in place), `graphpg advise --statements` and `GET /debug/statements` display `genericPlanCalls` and `customPlanCalls` per operation and flag slow parameterized operations stuck on generic plans so operators can tune `plan_cache_mode` or pin plans externally via PostgreSQL 19's core [`pg_plan_advice`](https://www.postgresql.org/docs/19/pgplanadvice.html) / [`pg_stash_advice`](https://www.postgresql.org/docs/19/pgstashadvice.html) modules without editing SQL.
- **`postgres_fdw`**: PG 19 pushes parameterized array comparisons (`IN` / `= ANY($1)`) inside prepared statements to remote servers, and `graphpg advise` suggests `ALTER SERVER ... OPTIONS (ADD import_stats 'true')` for a `postgres_fdw` foreign table that has never been analyzed. See [foreign data](foreign-data.md).

## 6. JSONPath string methods, natively

graphpg has no JSONPath parser of its own and no list of allowed methods. `matchesPath` and `existsPath` compile to PostgreSQL's `@@` and `@?` operators, and `path` to `jsonb_path_query_first`, with the expression bound as a typed `jsonpath` parameter. The JSONPath language of the API is therefore exactly the server's, and on PostgreSQL 19 it includes the new string methods `.lower()`, `.upper()`, `.initcap()`, `.replace()`, `.split_part()`, `.btrim()`, `.ltrim()` and `.rtrim()`, with no graphpg release needed:

```graphql
{
  docsConnection(filter: { doc: { matchesPath: "$.code.split_part(\":\", 2) == \"pg19\"" } }) {
    nodes {
      name: doc(path: "$.name.btrim().lower()")
    }
  }
}
```

```bash
curl -g 'localhost:4000/rest/docs?filter[doc][matchesPath]=$.email.lower() == "ada@example.com"&path[doc]=$.name.btrim()'
```

Case-insensitive matching and normalisation run in PostgreSQL, inside the same single statement as the rest of the request, so only the matching rows and the selected values leave the database. There is no fetch-then-filter step in the API, and nothing to keep in sync between graphpg's validation and PostgreSQL's evaluation. A `jsonb_path_ops` GIN index cannot narrow a predicate on a method's result, so for a hot case-insensitive lookup on a large table, store the normalised value: the methods are immutable, so they work in a generated column such as `email_lower text GENERATED ALWAYS AS (jsonb_path_query_first(doc, '$.email.lower()') #>> '{}') STORED`, which graphpg exposes as an ordinary field that a B-tree index serves.

On PostgreSQL 18 the same expression is rejected by PostgreSQL's JSONPath parser, and graphpg returns it as a client error (`BAD_USER_INPUT`, HTTP 400 in REST), never a 500. `crates/graphpg/tests/it/pg19.rs` covers both versions.

## 7. `lz4` TOAST compression by default

PostgreSQL 19 compresses new large `text` and `jsonb` values with `lz4` instead of `pglz`. graphpg needs no change for it. `lz4` compresses much faster, which helps writes of large values. Reads are not automatically faster: on the benchmark's highly repetitive 2.6 KB bodies, PostgreSQL 18 read 50-row pages 10–15% faster with `pglz` than with `lz4`. Measure on your own data before changing `default_toast_compression` on PostgreSQL 18.

## What PostgreSQL 18 deployments get too

Measuring PostgreSQL 19 also showed where PostgreSQL 18 could do better. These changes apply to every version:

- **`connectOrCreate` no longer fails under contention.** The 2-CTE form now names the unique key in `ON CONFLICT (<key>) DO NOTHING`. An insert that loses the race waits for the winning transaction instead of failing with `23505`, and returns no row. graphpg then runs the statement once more, and under `READ COMMITTED` its new snapshot finds the committed row. Under `REPEATABLE READ` or `SERIALIZABLE`, PostgreSQL raises `40001` instead, and the operation is retried as usual. It needs no extra privilege (`SELECT`, `INSERT`), and only `connectOrCreate` whose `where` and `create` agree on a unique key uses it. Any other `connectOrCreate` compiles as before. The difference that remains on PostgreSQL 19 is that `DO SELECT` resolves the race inside one statement, with no second run.
- **Less work per exported row.** Exports that are not eligible for `COPY`, which means every export on PostgreSQL 18, stream each row's JSON as text from PostgreSQL, with no parse and re-serialize step in graphpg.
- **`jit = off`, a PostgreSQL 19 default that PostgreSQL 18 can use today.** graphpg's short statements rarely gain from JIT compilation, and a grouped aggregate over a whole table was 3–11% faster on PostgreSQL 18 without it. See [server settings](performance.md#server-settings).

## Benchmark

[`benchmarks/pg19/`](../benchmarks/pg19/README.md) runs the same k6 scenarios through graphpg against PostgreSQL 18.6 and 19.0 on one machine: reads (lists, embedded relations, `NOT IN`, anti-joins, `IS DISTINCT FROM`, recursive ancestors, a grouped aggregate, large TOASTed bodies), writes (inserts with foreign keys, `connectOrCreate` that finds its row, and `connectOrCreate` racing on new keys), and an NDJSON export. Both servers get the same sizing and dataset, and `jit` is pinned to the same value. Rounds alternate between the versions, and the report gives medians and ranges together with each run's commit rate and host load.

What it showed:

- **Correctness under contention is the clear difference.** On PostgreSQL 18 before graphpg 1.4, 2–8% of the racing `connectOrCreate` requests failed with `23505`. On PostgreSQL 19 none did. graphpg 1.4 removes those errors on PostgreSQL 18 too.
- **Throughput is about the same.** For reads, writes and exports alike, the versions differ by less than the spread between rounds on the same version. Upgrading PostgreSQL is not a speed-up for this workload on its own, and staying on 18 costs nothing.
- **Writes follow the disk, the grouped aggregate the CPU.** Every write scenario waits for the WAL flush at commit, and the aggregate uses parallel workers. Those scenarios moved the most with other load on the host, so compare them only within a run.

## Extension compatibility before you upgrade

The test suite runs on PostgreSQL 19.0 with contrib, pgvector, PostGIS, hypopg, pg_cron, pg_partman and pgmq. TimescaleDB, PGroonga, pg_jsonschema, VectorChord and pgvectorscale publish no PostgreSQL 19 packages yet: stay on PostgreSQL 18 if your database uses one of them.

Check the full per-extension table in the [extension compatibility matrix](versioning.md#extension-compatibility) before upgrading production clusters.
