# Comparison

graphpg belongs to a family of tools that expose a PostgreSQL database as a GraphQL API. This page lists what each one does so you can choose; it is not a ranking. The other tools are mature and, in several areas, broader than graphpg.

> The columns for other tools reflect their documented behaviour at the time of writing and may be out of date. Check their documentation for the version you would use.

## At a glance

| | graphpg | PostGraphile v5 (Grafast) | Hasura (v2 / v3 DDN) |
| :--- | :--- | :--- | :--- |
| Runtime | Rust, single binary | Node.js library and CLI | Haskell (v2); Rust (v3) |
| Data sources | One PostgreSQL database | PostgreSQL, plus other sources through plugins | Many databases and remote schemas |
| Schema comes from | Database catalogs; optional YAML for GraphQL-level changes | Database catalogs; smart comments and plugins | Tracked tables and relationships in metadata |
| Extension model | Configuration only | Plugin and inflection system | Metadata, actions, remote schemas |

## Features

| Feature | graphpg | PostGraphile v5 | Hasura |
| :--- | :--- | :--- | :--- |
| Queries compile to one SQL statement | Yes (lateral joins, JSON aggregation) | Plan-based; batches steps | Yes |
| Relay connections, keyset pagination, `node(id:)` | Yes | Yes | Relay support varies by version |
| Filtering, ordering, aggregates | Built in, including relation filters (`some` / `every` / `none`) | Built in or through plugins | Built in |
| Mutations: CRUD, upsert | Yes | Yes | Yes |
| Nested writes (create / connect / update / sync) | Yes | Plugin | Yes (nested inserts) |
| Functions as fields (queries and mutations) | Yes, by volatility | Yes | Yes |
| Views, materialized views, declared keys | Yes (keys and relations in YAML) | Yes (smart comments) | Yes |
| Composite types, enums, ranges, domains | Yes | Mostly | Partial |
| Row-Level Security | JWT claims set as session settings; policies in PostgreSQL | Through `pgSettings`; policies in PostgreSQL | Role permissions in metadata |
| Schema pruned by database privileges | Yes (`GRANT`s) | Yes | Per role from metadata |
| Authentication | OAuth 2.0 resource server (JWKS, discovery) | Bring your own (middleware) | JWT or webhook |
| Apollo Federation subgraph | Yes (v2) | Plugin | Yes (v3) |
| `@defer` / `@stream` | Yes | Yes | No |
| Subscriptions | No | Live queries and subscriptions through plugins | Yes |
| PostgreSQL 17/18 features (`JSON_TABLE` views, temporal keys, `RETURNING OLD`, `MERGE`) | Yes | Manual (custom SQL) | Manual |
| Hierarchies (recursive, `ltree`) | Yes | Custom SQL or plugin | Custom SQL |
| Search (trigram, pgvector, full-text, PostGIS, BM25) | Root fields from the indexes present | Plugins | Partial |
| Bulk export (NDJSON / CSV) and `COPY` import | Yes | No | No |
| Read replicas | Yes (WAL-position aware) | Configure sources | Yes |
| Multi-database joins | No | Yes, across sources | Yes |
| Event triggers / webhooks | No | No (use a job queue) | Yes |
| REST endpoints | No | No | Yes |
| Admin console / schema editor | GraphiQL only | Ruru IDE | Full web console |
| Live schema reload on DDL changes | Yes (`--dev`) | Watch mode | Console and metadata API |
| Telemetry | tracing spans over OTLP, sqlcommenter | Plugins | Built in |

## When something else fits better

- You need data from **more than one database or API** in a single query: Hasura or PostGraphile.
- You need **subscriptions**, **event triggers**, **REST endpoints** or an **admin console**: Hasura.
- You want to **rewrite how the schema is generated** (naming, field shapes, custom plugins): PostGraphile.
- You are happy with a plain REST API over PostgreSQL: [PostgREST](https://postgrest.org) is a good choice.

## Where graphpg is a fit

- You want a **small, self-contained service** that you configure rather than program, deployed as a binary or container.
- Your authorization already lives in **PostgreSQL** (roles, grants, Row-Level Security) and you want the API to follow it.
- You use recent PostgreSQL features (temporal keys, `JSON_TABLE`, composite and range types) and want them in the schema without hand-written SQL.
- You run it as a **subgraph** behind a federation router.

For benchmarking graphpg on your own schema, see [../benchmarks/](../benchmarks/README.md).
