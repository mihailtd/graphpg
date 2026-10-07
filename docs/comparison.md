# Comparison

graphpg serves a **GraphQL** API and a **REST** API (with an OpenAPI 3.1 description) from the same PostgreSQL schema. This page compares each side with the tools you would otherwise choose: [PostGraphile](#compared-with-other-graphql-tools) and Hasura for GraphQL, [PostgREST](#compared-with-postgrest) for REST. It lists what each one does so you can choose; it is not a ranking. The other tools are mature and, in several areas, broader than graphpg.

> The columns for other tools reflect their documented behaviour at the time of writing (October 2026; PostgREST from [docs.postgrest.org](https://docs.postgrest.org/en/stable/), see [sources](#sources)) and may be out of date. Check their documentation for the version you would use. Where graphpg has no equivalent, the table says so.

## Compared with other GraphQL tools

### At a glance

| | graphpg | PostGraphile v5 (Grafast) | Hasura (v2 / v3 DDN) |
| :--- | :--- | :--- | :--- |
| Runtime | Rust, single binary | Node.js library and CLI | Haskell (v2); Rust (v3) |
| Data sources | One PostgreSQL database | PostgreSQL, plus other sources through plugins | Many databases and remote schemas |
| Schema comes from | Database catalogs; optional YAML for API-level changes | Database catalogs; smart comments and plugins | Tracked tables and relationships in metadata |
| Extension model | Configuration only | Plugin and inflection system | Metadata, actions, remote schemas |

### Features

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
| Depth, cost and page-size limits | Yes (`limits:`, enforced on the plan) | Plugins / custom | Yes (depth, node limit) |
| Multi-database joins | No | Yes, across sources | Yes |
| Event triggers / webhooks | No | No (use a job queue) | Yes |
| REST endpoints | Yes ([see below](#compared-with-postgrest)) | No | Yes (REST endpoints from saved queries) |
| Admin console / schema editor | GraphiQL and Scalar only | Ruru IDE | Full web console |
| Live schema reload on DDL changes | Yes (`--mode dev`) | Watch mode | Console and metadata API |
| Telemetry | tracing spans over OTLP, sqlcommenter | Plugins | Built in |

### When something else fits better

- You need data from **more than one database or API** in a single query: Hasura or PostGraphile.
- You need **subscriptions**, **event triggers** or an **admin console**: Hasura.
- You want to **rewrite how the schema is generated** (naming, field shapes, custom plugins): PostGraphile.

## Compared with PostgREST

[PostgREST](https://postgrest.org) is the established way to serve a PostgreSQL schema as a REST API: a single Haskell binary, in production since 2014, whose design principle is that "the structural constraints and permissions in the database determine the API endpoints and operations". graphpg shares that principle and the same architecture (one statement and one transaction per request, authorization in PostgreSQL), but it is a **GraphQL server first** whose REST API is planned onto the same engine, so the two APIs share filters, pagination, nested writes, limits and error codes. PostgREST has no GraphQL API (the separate `pg_graphql` extension is another project), and it has had a decade to grow the HTTP surface graphpg's REST API does not yet have.

### At a glance

| | graphpg REST | PostgREST |
| :--- | :--- | :--- |
| Runtime | Rust, single binary (and a GraphQL API on the same engine) | Haskell, single binary; REST only |
| Maturity | 1.0, young; behaviour covered by an integration suite against PostgreSQL 18 | Mature, widely deployed, large ecosystem and client libraries |
| API shape | Resources `/rest/<table>`, `/rest/rpc/<function>`; typed query parameters | Resources `/<table>`, `/rpc/<function>`; operator grammar in the values (`?age=gt.18`) |
| Description | OpenAPI **3.1**, typed parameters and nested-write bodies, one per container, pinnable | OpenAPI 2.0 (Swagger), depends on the requesting role; 3.x rewrite not shipped at the time of writing |
| Authorization | OAuth 2.0 resource server; claims reach RLS policies; the role a request runs as is fixed by configuration | JWT; the `role` claim selects the PostgreSQL role (`SET ROLE`); claims reach policies; pre-request function |
| Pagination | Keyset cursors (`first` / `after`), offset, `Link` headers | `limit` / `offset`, `Range` headers, `Content-Range`; no keyset |
| Configuration | `graphpg.yaml` with `dev:` / `prod:` blocks; secrets in the environment | File, environment (`PGRST_*`) or in-database settings |
| Typical fit | You want GraphQL and REST from one service, nested writes, typed limits and a typed spec | You want the most complete, battle-tested REST surface and its HTTP features |

### Reading

| Feature | graphpg REST | PostgREST |
| :--- | :--- | :--- |
| Filter operators | `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in`, `nin`, `like`, `ilike`, `startsWith`, `endsWith`, `regex`, `iregex`, `isNull`, `isDistinctFrom`, `…Any` / `…All` of a list, array (`contains`, `containedBy`, `overlaps`), range (`contains`, `strictlyLeftOf`, `strictlyRightOf`, `adjacentTo`, `overlaps`, `containedBy`), JSON key and path operators, and the rest of the GraphQL filter inputs (`ltree`, PostGIS) through a JSON filter | `eq`, `gt`, `gte`, `lt`, `lte`, `neq`, `like`, `ilike`, `match`, `imatch`, `in`, `is`, `isdistinct`, `cs`, `cd`, `ov`, `sl`, `sr`, `nxr`, `nxl`, `adj`, `fts`, `plfts`, `phfts`, `wfts`; `any` / `all` modifiers |
| POSIX regular expressions | Yes (`regex`, `iregex`; `match` and `imatch` too) | Yes (`match`, `imatch`) |
| `and` / `or` / `not` | In the query string (`filter[or][0][a][eq]=1&filter[or][1][b][gt]=2`, `filter[not]…`, nested), and in the JSON filter (`?filter={…}`, `POST …/search`) | In the query string (`or=(a.eq.1,b.gt.2)`, `not.`) |
| Filtering through relations | `filter[author][name][eq]=…`, `some` / `every` / `none` / `count` on to-many | Embedded filters with `!inner`; `is.null` anti-joins |
| Complex filters in a body | `POST /rest/<table>/search` with the same JSON filter GraphQL takes | No (the query string only; or an RPC function) |
| Ordering | `order=name,-created_at`; also by related columns | `order=name.desc.nullslast`; also by embedded to-one columns |
| `NULLS FIRST` / `LAST` | PostgreSQL's default (last ascending, first descending) | Yes |
| Column selection | `fields=a,b` | `select=a,b`, with renaming and casts (`total:amount::text`) |
| Embedding relations | `include=posts,posts.author` (to-one, to-many, many-to-many); all columns of the embedded rows | `select=*,posts(*)`, nested, with column lists, hints (`!fk`), `!inner` joins |
| Filter, order and limit *inside* an embedding | No in REST (yes in GraphQL, per field) | Yes (`posts.order=…`, `posts.limit=…`, `posts.status=eq.x`) |
| Spread (`...author(name)`) | No | Yes |
| Computed relationships | Functions mounted on tables (GraphQL fields); not selectable through REST | Yes (set-returning functions) |
| Pagination | `first` / `after` / `last` / `before` (keyset, stable under writes), `offset`, `Range: items=A-B` (`206`, `Content-Range`, `416`); `Link` headers | `limit` / `offset`, `Range` header; keyset not supported |
| Total count | `Prefer: count=exact` (`X-Total-Count`, and the total in `Content-Range`); opt-in; no `planned` / `estimated` | `Prefer: count=exact`, `planned`, `estimated` (`Content-Range`) |
| Server-side maximum | `limits.page_size.max` (rejects), default page size | `db-max-rows` (silently truncates) |
| Aggregates | `min`, `max`, `sum`, `avg`, `distinctCount`, `stddev*`, `variance*`, `median`, `percentile`; `group` by columns, truncated dates and bins; `having`; time zones; gap filling | `avg`, `count`, `max`, `min`, `sum`; grouping by the other selected columns; **disabled by default**; no `HAVING`; no ordering by an aggregate |
| JSON column access | The whole value; JSON-key filters; a `path` argument in GraphQL | `->` / `->>` in `select`, filters and `order` |
| Full-text search | `POST …/search/full-text` (ranked, `tsvector`) and the filter inputs | `fts` / `plfts` / `phfts` / `wfts` operators |
| Other search | Fuzzy (`pg_trgm`), semantic (`pgvector`), BM25, hybrid, nearest (PostGIS), and `GET /rest/search` across tables; scored, best first | No (build with functions) |
| Views and materialized views | Yes (writable where PostgreSQL can update them; keys declared in YAML) | Yes |
| Spatial data | `GeoJSON` values, filters, nearest search | Through PostGIS and GeoJSON media-type handlers |

### Writing

| Feature | graphpg REST | PostgREST |
| :--- | :--- | :--- |
| Create one row | `POST` JSON | `POST` JSON |
| Bulk create | `POST` JSON array or **CSV**, one statement, bounded by `limits.bulk_rows` | `POST` JSON array or **CSV**; `?columns=` to pick keys; form-encoded bodies |
| **Nested writes** (create / connect / update / sync children in one request) | **Yes**: the GraphQL input shape, validated to any depth, atomic | **No** (multi-table writes need an RPC function) |
| Update | `PATCH /rest/<table>/<key>` | `PATCH /<table>?<filters>` (any filter; many rows) |
| Delete | `DELETE /rest/<table>/<key>` | `DELETE /<table>?<filters>` (any filter; many rows) |
| Update or delete many rows by filter | Not over REST (GraphQL `update<Types>(where:)`) | Yes; `Prefer: max-affected=N` guards the count |
| Upsert | `PUT /rest/<table>/<key>` (primary key) | `POST` with `resolution=merge-duplicates` / `ignore-duplicates` and `on_conflict=<unique columns>`; `PUT` by primary key |
| Missing columns | The column default (or null) | `Prefer: missing=default` chooses the default over null |
| Response shape | `Prefer: return=minimal` (204) or the row (default); `Location` and `ETag` | `return=minimal` (default), `headers-only`, `representation`; `Preference-Applied` |
| Row as it was before the write | `Prefer: previous` (`RETURNING OLD`, PostgreSQL 18) | No |
| Optimistic concurrency | Row version as `ETag` / `If-Match` (412 `STALE_VERSION`); conditional `GET` / `HEAD` with `If-None-Match` (304 `Not Modified`) | No row versions |
| Transactions | One per request; automatic retry on serialization failures and deadlocks | One per request; `Prefer: tx=rollback` (with `db-tx-end`) for dry runs |
| Unknown input members | **400**: validated against the GraphQL input types, at any depth | Unknown columns are an error from PostgreSQL |

### Functions

| Feature | graphpg REST | PostgREST |
| :--- | :--- | :--- |
| Endpoint | `/rest/rpc/<function>` | `/rpc/<function>` |
| `GET` vs `POST` | `GET` and `POST` for `STABLE` / `IMMUTABLE`; `POST` only for `VOLATILE` (405 otherwise) | Same rule |
| Arguments | Query parameters (`GET`) or `{"args": {…}}` (`POST`), camelCase names, typed and validated | Query parameters or a JSON object; PostgreSQL parameter names |
| Set-returning functions | A list with the table's own filters, ordering, pagination and embedding | The same |
| Single JSON body parameter (`json` / `jsonb`) | Yes (raw JSON body or `{"args": {…}}`) | Yes |
| Variadic functions | Not exposed | Yes |
| Overloads | The first one created is exposed | By argument names |
| Procedures | Not exposed | Not supported |
| Functions of a row (computed columns) | GraphQL fields only | Selectable like columns |

### Content, HTTP and routing

| Feature | graphpg REST | PostgREST |
| :--- | :--- | :--- |
| Media types | JSON (and `application/problem+json` errors); CSV lists and CSV bulk creates; `multipart/form-data` writes; the bytes of a `bytea` column as their own media type; CSV and NDJSON streaming through the GraphQL `/export` endpoint | JSON, CSV, GeoJSON, plain text, XML, binary; custom handlers (domains, aggregates); `application/vnd.pgrst.object` for one row |
| `HEAD` and `OPTIONS` | `HEAD` answers like `GET` without a body; `OPTIONS` lists the methods (`Allow`, `Accept-Post`, `Accept-Patch`) and CORS preflights are answered by the CORS layer; both are in the OpenAPI document | Yes |
| Several schemas in one deployment | All `database.schemas` merged into one API; if two schemas have a same-named table, REST serves the default schema's | One schema per request, chosen by `Accept-Profile` / `Content-Profile` |
| CORS | Configurable (`server.cors`): any origin in dev, none in prod unless listed | Configurable allowed origins |
| Request ID / `Server-Timing` / `EXPLAIN` by header | `X-Request-Id` echo/generation, dev `Server-Timing`, tracing spans, GraphQL `debug.explain` in dev | `X-Request-Id` echo, `Server-Timing`, `Accept: application/vnd.pgrst.plan` |

### Authentication and authorization

| Feature | graphpg | PostgREST |
| :--- | :--- | :--- |
| Token | OAuth 2.0 access token (RFC 9068), `Authorization: Bearer` | JWT, `Authorization: Bearer` |
| Key material | Issuer's JWKS found by OpenID Connect discovery or RFC 8414, cached and refreshed on key rotation | A static secret or a literal JWK / JWKS in the configuration |
| Algorithms | RS\*, PS\*, ES256/384, EdDSA (configurable) | HS256 and RSA family |
| Which PostgreSQL role a request runs as | Fixed by configuration (`authenticated` / `anonymous`); **tokens never choose it** | The token's `role` claim (`SET ROLE`); anonymous role without a token |
| Claims in policies | Mapped to session settings you name | `request.jwt.claims` (and headers, cookies) settings |
| Custom per-request check | Row-Level Security policies | `db-pre-request` function, and policies |
| Token validation cache | Verified keys cached | Validation results cached (SIEVE) |
| Row-Level Security | Yes | Yes |
| Schema / spec pruned by privileges | Yes, with `rbac.enabled` (the connecting role's `GRANT`s; one per container) | Yes (per request role) |

### API description

| Feature | graphpg | PostgREST |
| :--- | :--- | :--- |
| Format | OpenAPI **3.1** with JSON Schema 2020-12, JSON or YAML | OpenAPI 2.0 (Swagger) |
| Typed query parameters | Yes: `filter` (`deepObject`), `order`, `fields`, `include` and `group` are enumerated from the schema | Operators are part of the value, so the grammar is not described as typed parameters |
| Write bodies, nested writes | Described from the GraphQL input types (`Input.<Type>`) | Row schemas only |
| Role-specific | One document per container (run another container for another role) | One per request role (`openapi-mode`) |
| Pinning and CI | `graphpg schema export --check`; startup refuses drift in prod; snapshot-tested | No |
| Interactive reference | Scalar, bundled in the binary (offline) | Bring your own (Swagger UI) |
| Customization | SQL comments and `graphpg.yaml` descriptions | SQL comments; a function can replace the whole document (`db-root-spec`) |

### Errors and limits

| Feature | graphpg | PostgREST |
| :--- | :--- | :--- |
| Error format | RFC 9457 `application/problem+json` with a stable `code` | JSON `code`, `message`, `details`, `hint` |
| PostgreSQL's own messages | Masked in prod behind a request ID that is logged; typed constraint codes (`EMAIL_TAKEN`, `AUTHOR_NOT_FOUND`) instead | Forwarded as they are |
| Custom HTTP status from SQL | No | `RAISE … SQLSTATE 'PT402'` |
| Page size | Default and maximum (`limits.page_size`) | `db-max-rows` |
| Depth and cost | `limits.depth`, `limits.cost`, enforced on the plan before SQL is compiled | Not built in (the docs suggest `pg_plan_filter`) |
| Bulk, body size, groups | `limits.bulk_rows`, `limits.request_body_bytes`, `limits.groups` | Not built in |
| `statement_timeout` | `limits.statement_timeout` (default 30s) on every connection | Set it on the role or database |
| Aggregates | On, bounded by the limits above | Off unless `db-aggregates-enabled` |

### Operations

| Feature | graphpg | PostgREST |
| :--- | :--- | :--- |
| Schema reload | Dev: automatic on DDL (event trigger). Prod: restart | `NOTIFY pgrst, 'reload schema'` or a signal, no restart |
| Configuration reload | Restart | `NOTIFY pgrst, 'reload config'`, in-database settings, per-role settings |
| Health and metrics | `GET /health` | `/live`, `/ready` and Prometheus `/metrics` on an admin port |
| Tracing and logs | OpenTelemetry spans (OTLP), sqlcommenter tags, structured logs | Logs, `log-query`, `Server-Timing`, trace-header echo |
| Read replicas | Built in (WAL-position aware routing) | Not built in (use a load balancer or pooler) |
| Connection pooling | Built-in pool; replicas separately | Built-in pool (`db-pool`, acquisition timeout, lifetime) |
| Graceful shutdown | Yes (`SIGTERM`) | Yes |
| Packaging | Static binary, signed distroless image (about 27 MB) | Static binaries and Docker image |

### Moving from PostgREST

The grammar differs but maps one to one for the common cases:

| PostgREST | graphpg REST |
| :--- | :--- |
| `GET /people?age=gte.18&name=ilike.*ann*` | `GET /rest/people?filter[age][gte]=18&filter[name][ilike]=%25ann%25` |
| `GET /people?select=id,name` | `GET /rest/people?fields=id,name` |
| `GET /people?order=age.desc,name` | `GET /rest/people?order=-age,name` |
| `GET /people?select=*,posts(*)` | `GET /rest/people?include=posts` |
| `GET /people?or=(age.lt.18,age.gt.65)` | `POST /rest/people/search` with `{"filter": {"or": [{"age": {"lt": 18}}, {"age": {"gt": 65}}]}}` |
| `GET /people?limit=20&offset=40` | `GET /rest/people?first=20&offset=40`, or follow the `Link: rel="next"` header (keyset) |
| `Range: 0-19` / `Content-Range` | `Range: items=0-19` → `206` with `Content-Range: items 0-19/*` (the total with `Prefer: count=exact`) |
| `Prefer: count=exact` | `Prefer: count=exact` → `X-Total-Count` |
| `POST /people` + `Prefer: return=representation` | `POST /rest/people` (the representation is the default; `return=minimal` for none) |
| `POST` + `resolution=merge-duplicates` | `PUT /rest/people/<key>` |
| `PATCH /people?id=eq.1` | `PATCH /rest/people/1` |
| `DELETE /people?id=eq.1` | `DELETE /rest/people/1` |
| `GET /rpc/fn?a=1` | `GET /rest/rpc/fn?a=1` (names in camelCase) |
| `POST /rpc/fn` with `{"a": 1}` | `POST /rest/rpc/fn` with `{"args": {"a": 1}}` |
| `Accept-Profile: other` | Not supported: schemas are merged into one API |
| `GET /` (OpenAPI 2.0) | `GET /openapi.json` (OpenAPI 3.1; `openapi.spec`) |

Rows differ in three ways worth knowing: field names are **camelCase** (`createdAt`), enums are the GraphQL values (`PAID`), and 64-bit integers and `numeric` are **strings** (so no precision is lost). The [REST API guide](rest-api.md) has the details.

### When PostgREST fits better

- You need **update or delete many rows by a filter**.
- You need **GeoJSON, XML, plain-text or custom media types** (domains, aggregates) from the REST API itself.
- You rely on **embedded filters, ordering and limits**, spread, or `!inner` joins in one URL.
- You need JSON arrow operators in the URL (`->`, `->>`), or `NULLS FIRST` / `LAST`.
- You want the token to **choose the PostgreSQL role**, or `Accept-Profile` to switch **schemas per request**.
- You need **config and schema reload without a restart**, Prometheus metrics, or a custom status from SQL.
- You value a **decade of production use**, a large ecosystem and client libraries (`postgrest-js`, ...).

### When graphpg fits better

- You want **GraphQL and REST from one service**, with the same filters, pagination, errors and limits in both.
- You write **related rows together**: nested writes, `connectOrCreate`, `MERGE`-based child syncing, optimistic `If-Match`, the row as it was (`Prefer: previous`).
- You paginate with **stable keyset cursors**, or need richer **aggregates** (`HAVING`, percentiles, time-bucketed groups).
- You want a **typed OpenAPI 3.1 document** that is pinned in CI and describes parameters and nested bodies, and an offline reference UI.
- You want **safety limits** (depth, cost, bulk, timeouts) and **masked errors** built in.
- You want **search** over trigram, vector, BM25 and full-text indexes without writing functions.
- You run it as an **OAuth 2.0 resource server** with JWKS discovery, and want tokens to never pick a database role.

### Known gaps

What graphpg's REST API does not do today, from the tables above, in the order we would expect people to hit them: update and delete by filter (GraphQL has it), embedded filters, ordering and limits, and column lists, GeoJSON, XML and custom media types, `planned` / `estimated` counts, schema reload in production without a restart, a Prometheus metrics endpoint, and `Accept-Profile`-style schema selection.

## Where graphpg is a fit

- You want a **small, self-contained service** that you configure rather than program, deployed as a binary or container.
- Your authorization already lives in **PostgreSQL** (roles, grants, Row-Level Security) and you want the APIs to follow it.
- You use recent PostgreSQL features (temporal keys, `JSON_TABLE`, composite and range types) and want them in the schema without hand-written SQL.
- You run it as a **subgraph** behind a federation router.

## Performance

Throughput depends on your schema, indexes and hardware far more than on the server. graphpg compiles each request to one statement, as PostgREST does, so the difference is usually small next to the database's own work; no comparative numbers are published here. For benchmarking graphpg on your own schema, see [../benchmarks/](../benchmarks/README.md), and run the other tool against the same database, queries and hardware.

## Sources

- PostgREST: [documentation](https://docs.postgrest.org/en/stable/), in particular [tables and views](https://docs.postgrest.org/en/stable/references/api/tables_views.html), [resource embedding](https://docs.postgrest.org/en/stable/references/api/resource_embedding.html), [pagination and count](https://docs.postgrest.org/en/stable/references/api/pagination_count.html), [aggregate functions](https://docs.postgrest.org/en/stable/references/api/aggregate_functions.html), [functions](https://docs.postgrest.org/en/stable/references/api/functions.html), [preferences](https://docs.postgrest.org/en/stable/references/api/preferences.html), [media type handlers](https://docs.postgrest.org/en/stable/references/api/media_type_handlers.html), [OpenAPI](https://docs.postgrest.org/en/stable/references/api/openapi.html), [authentication](https://docs.postgrest.org/en/stable/references/auth.html), [errors](https://docs.postgrest.org/en/stable/references/errors.html), [schemas](https://docs.postgrest.org/en/stable/references/api/schemas.html), [observability](https://docs.postgrest.org/en/stable/references/observability.html) and [configuration](https://docs.postgrest.org/en/stable/references/configuration.html).
- graphpg: the [REST API guide](rest-api.md), the [GraphQL API guide](graphql-api.md) and [configuration](configuration.md); the REST behaviour described there is covered by the integration test suite.
