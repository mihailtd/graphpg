# Versioning

graphpg follows [semantic versioning](https://semver.org) from **1.1.0**, the first published release. Releases are tagged `vX.Y.Z`; each publishes the container image (`ghcr.io/mihailtd/graphpg:X.Y.Z`, `:X.Y`, `:X`, `:latest`) and a GitHub Release. The [changelog](../CHANGELOG.md) lists what each one changes.

- **Patch** (`1.1.x`): fixes. Nothing that worked as documented stops working.
- **Minor** (`1.x.0`): additions. New fields, arguments, routes, error codes, config keys and features, with defaults that leave existing deployments as they were.
- **Major** (`x.0.0`): anything that can break a client, a deployment or a database that worked with the previous version.

## What the version covers

For the same database and the same `graphpg.yaml`, a minor or patch upgrade serves the same API:

- **The GraphQL schema**: type, field, argument, enum value and input names (and the rules deriving them from database names: singulars and plurals, camelCase, clash resolution), nullability, and the root fields each table, view and function gets.
- **The REST API**: routes, query parameters, headers, status codes, the shape of bodies and of the OpenAPI document.
- **Errors**: `extensions.code` values (`BAD_USER_INPUT`, `SCHEMA_CHANGED`, `<Type>ErrorCode` values, ...) and the HTTP status of each.
- **Configuration**: `graphpg.yaml` keys, their meaning and defaults; command-line commands and flags; environment variables.
- **Behaviour the database relies on**: the session settings authenticated requests set (`role`, `request.jwt.claims`, the prefix and mappings), the transaction each request runs in, and what graphpg creates in the database (only in dev mode: its DDL event trigger).
- **Telemetry names**: metric names and attributes, and span names, which dashboards and alerts depend on.

New things may appear in a minor version: a new root field, argument, enum value or error code. Clients must tolerate them (an unknown enum value or error code, a new field in a REST body), as GraphQL and HTTP clients generally do.

## What it does not cover

- **The SQL graphpg generates**, its plans and performance: they improve in any release.
- **Log messages**, the text of error messages (the codes are covered), and the content of dev-mode-only output (`X-Debug-SQL`, `extensions.sql`).
- **The Rust crate's API** (`graphpg` as a library): it is the server's implementation, not a supported interface.
- **Behaviour that was a vulnerability**: a security fix may refuse what a request could do before (for example the global ID of another type in a key argument), in a patch release.
- **A database that changed**: renaming a table renames its type. Pin the API with `export.pin` (see [configuration](configuration.md)) to make such a change fail at startup instead of reaching clients.

## Deprecation

Something the next major version removes or changes is deprecated first, in a minor version: marked `@deprecated` in the GraphQL schema and `deprecated: true` in the OpenAPI document, logged as a warning at startup when a configuration uses it, and listed in the changelog. It keeps working until the next major version.

## Supported PostgreSQL

One compiled `graphpg` binary supports **PostgreSQL 18** and **PostgreSQL 19**. It reads the server's `server_version_num` each time it inspects the catalog, and each read replica's every 10 seconds, and it uses PostgreSQL 19 features only on a server that has them ([how](postgresql-19.md#how-graphpg-detects-the-version)). On PostgreSQL 19 the GraphQL schema and OpenAPI document gain fields and enum values and lose nothing, so re-export pinned schemas after upgrading. Dropping a PostgreSQL major version is a major change.

### Automatic PostgreSQL adaptations

| Capability | PostgreSQL 18 | PostgreSQL 19+ |
| :--- | :--- | :--- |
| **`connectOrCreate` by a unique key** | `WITH _found ... _new` with `ON CONFLICT (<key>) DO NOTHING`, run again if a concurrent insert won: no `23505` under contention | One `INSERT ... ON CONFLICT (<key>) DO SELECT RETURNING` |
| **Upsert conflict action `SELECT`** | Not exposed | GraphQL `ConflictAction.SELECT` and REST `resolution=select` |
| **Read-your-writes on standbys (`X-Min-WAL-Position`)** | Immediate `pg_last_wal_replay_lsn() >= $1` check, else the primary | `WAIT FOR LSN '<lsn>' WITH (MODE 'standby_replay', TIMEOUT '...', NO_THROW)` when `database.replica_wait_timeout` is set, else the primary |
| **Streaming `POST /export`** | Portal fetch of each row's JSON as text | `COPY (SELECT ...) TO STDOUT (FORMAT json[, FORCE_ARRAY])` for `ndjson` and `"format": "json"` |
| **Window value functions** | `rowNumber`, `rank`, `denseRank`, `percentRank`, `runningSum` | Also `lag`, `lead`, `firstValue`, `lastValue`, `nthValue` with `ignoreNulls` (`IGNORE NULLS` / `RESPECT NULLS`) |
| **Diagnostics & statement advice** | `EXPLAIN (ANALYZE, BUFFERS, SERIALIZE, MEMORY)` | Adds `IO`, `pg_stat_statements` generic vs custom plan counts, and `postgres_fdw` `import_stats` advice |
| **JSONPath** (`matchesPath`, `existsPath`, `path`) | PostgreSQL 18's JSONPath | Also the string methods `.lower()`, `.upper()`, `.initcap()`, `.replace()`, `.split_part()`, `.btrim()`, `.ltrim()`, `.rtrim()` |

### Extension compatibility

The whole test suite runs against PostgreSQL 18 with every extension below (`docker/test-pg/Dockerfile`), and against PostgreSQL 19.0 with the extensions that publish PostgreSQL 19 packages (`docker/test-pg/Dockerfile.19`, from the PGDG repository). Where an extension has no PostgreSQL 19 package yet, its tests are skipped on 19. Stay on PostgreSQL 18 if your database relies on it.

| Component / Extension | PostgreSQL 18 | PostgreSQL 19 | Notes |
| :--- | :--- | :--- | :--- |
| **PostgreSQL core** | Tested | Tested | 18.6 and 19.0 |
| **Contrib** (`pg_stat_statements`, `postgres_fdw`, `file_fdw`, `pg_trgm`, `ltree`, `citext`, `btree_gist`, `pgcrypto`, `uuid-ossp`) | Tested | Tested | Ship with PostgreSQL |
| **pgvector** | Tested | Tested | `postgresql-19-pgvector` 0.8.7 |
| **PostGIS** | Tested | Tested | `postgresql-19-postgis-3` 3.7.0 |
| **hypopg** | Tested | Tested | `postgresql-19-hypopg` 1.4.5 |
| **pg_cron** | Tested | Tested | `postgresql-19-cron` 1.6.8 |
| **pg_partman** | Tested | Tested | `postgresql-19-partman` 5.5.1 |
| **pgmq** | Tested | Tested | SQL only, installed from source (1.10.0) |
| **pg_duckdb** | Tested | Not tested | On a server of its own (`pgduckdb/pgduckdb:18-v1.1.1`) |
| **TimescaleDB** | Tested | No package yet | |
| **PGroonga** | Tested | No package yet | |
| **pg_jsonschema** | Tested | No package yet | |
| **VectorChord** | Tested | No package yet | |
| **pgvectorscale** | Tested | No package yet | |

