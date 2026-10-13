# Configuration

graphpg is configured by one optional file, `graphpg.yaml`, which serves every environment (shared settings, then `dev:` and `prod:` blocks for what differs). Secrets (`DATABASE_URL`, `DATABASE_REPLICA_URLS`, `AUTH_JWT_SECRET`) are environment-only. The file's `${NAME}` references read non-secret environment variables, so settings can also be parameterized per environment.

## Running

```bash
graphpg                              # serve in prod mode on :4000
graphpg --mode dev                   # dev mode (watch DDL, auto-export, GraphiQL/Scalar on, OpenAPI document served)
graphpg --config path/to/file.yaml   # explicit config file
graphpg advise                       # print schema advice and exit
graphpg schema export                # write GraphQL SDL + OpenAPI (+ subgraph) to the `export:` paths
graphpg schema export --check        # CI: fail if the exported files are out of date
graphpg export-graphql --out schema.graphql
graphpg export-openapi --out openapi.json               # YAML for a .yaml path
graphpg export-subgraph --out subgraph.graphql          # Apollo Federation SDL
graphpg export-config-schema --out graphpg.schema.json
```

`DATABASE_URL` is required and must be provided via the environment or `.env` file in the working directory.

### Running in a container

```bash
docker run -p 4000:4000 \
  -e DATABASE_URL=postgres://user:pass@db/mydb \
  -v "$PWD/graphpg.yaml:/app/graphpg.yaml:ro" \
  ghcr.io/mihailtd/graphpg:1.0.1
```

The image is a statically linked binary on `gcr.io/distroless/static`: no shell, no package manager, CA certificates, and a non-root user (UID 65532). graphpg reads `graphpg.yaml` from the working directory, `/app`. Because there is no shell or `curl` in the image, probe `GET /health` from outside (a Kubernetes `httpGet` probe, or your load balancer) rather than with a `HEALTHCHECK` command. The container stops cleanly on `SIGTERM`.

| Tag | Moves | Use for |
| :--- | :--- | :--- |
| `1.0.1` | never | Pinning a release |
| `1.0`, `1` | with each patch / minor release | Following a release line |
| `latest` | with each (non-prerelease) release | Trying it out |
| `sha-<short hash>` | never | One exact commit of `main` |
| `edge` | with each commit on `main` | Testing unreleased changes |

OpenTelemetry export is compiled into the image: set `telemetry.otlp.endpoint` (or `OTEL_EXPORTER_OTLP_ENDPOINT`). See [observability](observability.md).

### Endpoints

| Path | Method | |
| :--- | :--- | :--- |
| `/graphql` | `POST` | Queries, mutations, `@defer` / `@stream` (as `multipart/mixed`) and Federation `_entities` (404 when `graphql.enabled: false`) |
| `/graphql` | `GET` | Queries in the URL (`query`, `operationName`, `variables`, `extensions`, `documentId`), for caching by URL with [persisted queries](graphql-api.md#persisted-queries); a mutation is `405` |
| `/rest/{resource}` … | `GET` `POST` `PATCH` `PUT` `DELETE` | The [REST API](rest-api.md): list, search, read, create, update, upsert and delete (404 when `openapi.enabled: false`) |
| `/rest` | `GET` | REST API root |
| `/graphiql` | `GET` | GraphiQL IDE, when `graphql.ui` is on (404 otherwise) |
| `/scalar` | `GET` | Scalar API reference, when `openapi.ui` is on (404 otherwise); it shows the OpenAPI document at `/openapi.json` |
| `/schema.graphql` | `GET` | The GraphQL SDL, when `graphql.sdl` is on (404 otherwise); `/subgraph.graphql` is the Federation subgraph SDL, with `graphql.federation.enabled`. Paths: `graphql.sdl_path`, `graphql.subgraph_path` |
| `/export` | `POST` | A connection query streamed as NDJSON or CSV (404 when `graphql.enabled: false`) |
| `/import/{Type}` | `POST` | NDJSON rows of `Create<Type>Input`, inserted all-or-nothing with `COPY` (404 when `graphql.enabled: false`) |
| `/health` | `GET` | Liveness |
| `/live`, `/ready` | `GET` | Liveness, and readiness (the database answers) |
| `/metrics` | `GET` | Prometheus metrics, when `telemetry.metrics.enabled` (on in dev, off in prod); path `telemetry.metrics.path`, or a port of its own with `telemetry.metrics.listen`. See [observability](observability.md) |

### Secret environment variables

Secrets are strictly environment-only. They are never kept in `graphpg.yaml`, never exported in configuration schemas, and are redacted in debug outputs.

| Variable | Required | Description |
| :--- | :--- | :--- |
| `DATABASE_URL` | yes | PostgreSQL connection string |
| `DATABASE_REPLICA_URLS` | no | Comma-separated read replica connection URLs ([details](performance.md#operations)) |
| `AUTH_JWT_SECRET` | no | Shared HMAC secret for JWT verification ([guide](authentication.md)) |

### Command-line flags and environment

| Flag | Environment Variable | Default | Description |
| :--- | :--- | :--- | :--- |
| `--mode <dev\|prod>` | `GRAPHPG_MODE` | `prod` | Runtime mode: `--mode` flag takes precedence over `GRAPHPG_MODE` |
| `--config <path>` | | `./graphpg.yaml` if present | Path to YAML config file (an explicitly named path must exist) |

### Subcommands

| Subcommand | Flag | Default output path | Description |
| :--- | :--- | :--- | :--- |
| `export-graphql` | `--out <path>` | `config.export.graphql` (`schema.graphql`) | Export GraphQL SDL schema |
| `export-subgraph` | `--out <path>` | `config.export.subgraph` (`subgraph.graphql`) | Export Apollo Federation Subgraph SDL |
| `export-openapi` | `--out <path>` | `config.export.openapi` (`openapi.json`) | Export the OpenAPI 3.1 document (YAML for a `.yaml` / `.yml` path) |
| `schema export` | `--check` | every `export:` path | Export every schema: GraphQL SDL, OpenAPI, and the subgraph SDL when federation is on. With `--check`, write nothing and exit 1 if a file is missing or differs from what the database generates now: the CI step that [pins the schema](#pinned-schemas) |
| `export-config-schema` | `--out <path>` | `config.export.config_schema` (`graphpg.schema.json`) | Export JSON Schema for `graphpg.yaml` |
| `advise` | | | Inspect schema and print advice warnings/info |

## `graphpg.yaml`

One file configures a server for every environment. Shared settings come first; settings that differ between modes go in `dev:` and `prod:` blocks, which are merged over the shared ones for the mode the server starts in.

```yaml
database:
  pool_size: 10
  schemas: [auth, app]          # the first is the default schema (default: app_public)
  sql_comments: true            # sqlcommenter comments on every statement

server:
  host: 0.0.0.0
  port: 4000
  cors:                         # default: any origin in dev, none in prod (no CORS headers)
    origins: [https://app.example.com]   # or ['*']
    methods: [GET, POST]        # default: GET POST PUT PATCH DELETE OPTIONS HEAD
    headers: [authorization, content-type]  # default: authorization content-type accept prefer x-request-id if-none-match if-match range x-min-wal-position
    expose_headers: [x-request-id]   # headers scripts may read; default: every one graphpg sends (ETag, Link, Content-Range, X-Total-Count, X-WAL-Position, RateLimit-*, Retry-After, ...)
    credentials: false          # needs explicit origins, not '*'
    max_age: 600                # seconds a browser caches the preflight

errors:
  detail: masked                # masked (default in prod) | full (default in dev)
  map:                          # SQLSTATE or constraint name -> status, title, code, type
    "23514": { status: 422, title: Check failed, code: CHECK_FAILED }
    orders_total_check: { status: 409, code: BAD_TOTAL, type: "https://example.com/errors/total" }

headers:
  preset: strict                # strict (default in prod) | relaxed (default in dev) | off
  set:                          # overrides on top of the preset; an empty value removes a header
    x-frame-options: SAMEORIGIN

watch:
  enabled: false                # watch DDL; default: true in dev, false in prod
  export: true                  # write export files on schema change

export:
  graphql: schema.graphql
  subgraph: subgraph.graphql
  openapi: openapi.json         # YAML for .yaml / .yml
  config_schema: graphpg.schema.json
  pin: fail                     # compare these files with the database at startup: fail (prod default), warn (dev default) or off

limits:                         # shared by GraphQL and REST, enforced on the plan (see "Safety limits")
  page_size: { default: 100, max: 1000 }
  depth: 10
  cost: 10000000
  bulk_rows: 1000
  groups: 10000
  request_body_bytes: 1048576
  statement_timeout: 30s        # `off` for none
  rate_limit:                   # requests per client and minute, per instance (see "Rate limits")
    enabled: false              # off by default: nothing is counted, no headers are sent
    anonymous: { requests_per_minute: 60 }          # by IP address; burst defaults to the rate
    authenticated: { requests_per_minute: 600, burst: 100 }   # by the token's key_claim
    key_claim: sub
    trusted_proxies: [10.0.0.0/8]   # proxies whose X-Forwarded-For names the client
    max_clients: 100000

events: { ... }                 # webhooks on row changes and schedules (see events.md)

auth: { ... }                   # OAuth 2.0 bearer tokens (see authentication.md)
rbac: { enabled: false }        # prune the APIs to what a role may access

graphql:
  enabled: true                 # serve /graphql, /export, /import/...
  ui: false                     # the GraphiQL IDE: on by default in dev mode only
  introspection: false          # answer __schema / __type: on by default in dev mode only
  sdl: false                    # serve the SDL at sdl_path: on by default in dev mode only
  sdl_path: /schema.graphql
  subgraph_path: /subgraph.graphql   # the Federation subgraph SDL, with federation on
  federation: { enabled: false }
  persisted_queries:            # operations sent by hash or id (see graphql-api.md#persisted-queries)
    enabled: false              # off by default: a request naming one gets PERSISTED_QUERY_NOT_SUPPORTED
    automatic: true             # APQ: clients register a hash by sending it with the text
    manifest: persisted-operations.json   # known operations, read and validated at startup
    allow_list: false           # run only the manifest's operations (e.g. under prod:)
openapi:
  enabled: true                 # the REST API at /rest and its OpenAPI description (alias: rest)
  ui: false                     # the Scalar API reference: on by default in dev mode only
  spec: false                   # serve the OpenAPI 3.1 document: on by default in dev mode, off in prod (alias: schema)
  path: /openapi.json           # path to serve the OpenAPI document at (alias: spec_path)

telemetry:                      # logs, metrics, traces: see observability.md
  logs: { level: info, format: json }
  metrics: { enabled: true, path: /metrics }   # default: on in dev, off in prod
  otlp: { endpoint: http://otel-collector:4318 }

dev:                            # settings that differ in dev mode
  debug:
    sql: true                   # dev-only: generated SQL in response extensions (GraphQL), X-Debug-SQL header (REST)
    explain: true               # dev-only: EXPLAIN ANALYZE in response extensions (GraphQL), X-Debug-Explain header (REST)
prod: {}                        # ... and in prod mode
```

The effective config of a mode is built in four steps, each overriding the last: the built-in defaults, the mode's built-in defaults (`dev` turns the GraphiQL IDE, Scalar API reference, OpenAPI spec serving, and watch mode on), the shared settings, then the mode's block. Mappings merge key by key; anything else, a list included, is replaced. An unknown key is an error in the shared settings and in both blocks, so a typo in `dev:` is found when starting `prod`.

### The OpenAPI Document and UIs

`graphql.ui` serves the GraphiQL IDE at `/graphiql` and `openapi.ui` (alias `rest.ui`) the Scalar API reference at `/scalar`. Both are on in dev mode and off in prod, and either can be set explicitly. They are part of the binary (and of the container image), load nothing from other hosts, and work offline, so a strict Content-Security-Policy can allow `'self'` only.

`openapi.spec` (alias `schema`) serves the generated OpenAPI 3.1 document at `openapi.path` (default `/openapi.json`, alias `spec_path`). It is on by default in dev mode, and off in prod mode. When enabled, requests receive JSON by default and YAML when requested (`Accept: application/yaml`).

The document states what the database already says, with no configuration:

- **Constraints as JSON Schema keywords.** A single-column `CHECK` becomes `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum` (integer and floating-point columns), `multipleOf` (`CHECK (qty % 5 = 0)`), `minLength` / `maxLength` (`char_length`, `length`), `pattern` (`CHECK (code ~ '^[A-Z]{3}$')`) and `minItems` / `maxItems` (`cardinality`, `array_length`), on the row and on the create and update bodies. Only these simple forms are read; an `OR`, a comparison of two columns or a 64-bit or `numeric` column (strings in JSON) are left to the database, which checks every write. Where a `varchar(n)` limit and a `CHECK` both bound a length, the stricter holds. PostgreSQL regular expressions and ECMA 262 ones differ in places: treat `pattern` as a hint for clients, not a validator.
- **Single Table Inheritance as a discriminated union.** A table whose discriminator is closed by a `CHECK` (or an enum) is described as `oneOf` its variants with a `discriminator` (`propertyName` and a `mapping` of each value to `<table>.<Variant>`, each the plain row `<table>.Base` with the discriminator fixed to that value).
- **Table-per-type (CTI) hierarchies as a discriminated union.** The parent table of a table-per-type hierarchy is described as `oneOf` one schema per subtype (`<table>.<Child>`: the parent's row plus the subtype's own columns) and a fallback for parent rows without a subtype (`<table>.<Fallback>`), with a `discriminator` on `__typename`.
- **Hypermedia `links` from foreign keys.** The `200` of `GET /rest/<table>/{key}` links to the row each single-column foreign key points at (`get_<table>_by_key`, the key read from the response) and to the rows that point at this one (`search_<table>`, filtered by this row's key), for the resources the API serves and the role may read. A link name is the target table, with the constraint name added when two foreign keys lead to the same table.
- **What the API accepts.** Besides the paths of the tables, the document describes: the filter operators of every column type (spatial, `ltree`, full-text, multirange); everything `order` takes (relation counts, a to-one relation's columns, computed columns); the codes a write of each table can be rejected with, as `<table>ErrorCode` and the `<table>Problem` of its 400, 409 and 422 answers (the GraphQL `<Type>ErrorCode`); the relations a row can carry when embedded, hierarchies and arcs included; the grammar of embedding (`fields[…]`, `depth[…]`, `asOf[…]`, `args[…]`); unique-key access (`by`); `/rest/node/{id}`; and the streaming `/export` and `/import/{type}` of the GraphQL router.
- **Change events as `webhooks`.** A table with `events:` in `entities` (`operations`: `insert`, `update`, `delete`; all three by default) gets one OpenAPI 3.1 webhook per change, `<table>.<operation>` (`orders.insert`), whose payload is `{id, createdAt, table, op, old, new}` with `old` and `new` the table's own row schema (`old` is `null` for an insert, `new` for a delete). This declares what a consumer of the table's events receives, so SDKs can type the incoming payloads; delivering the events (change capture, retries) belongs to the event-trigger work, and nothing is sent by declaring them.

The Scalar reference shows that document: it loads it from `openapi.path`, and `openapi.ui: true` serves the document even when `openapi.spec` is off, so the page always has something to show. With `openapi.enabled: false` there is no REST API to describe, so neither the document nor the reference is served (404).

The GraphiQL source is `packages/graphiql-ui`; `moon run graphiql-ui:build` builds it into `crates/graphpg-ui/assets`, which the server embeds. A build without those files still runs, and `/graphiql` says so.

Strings may read non-secret environment variables: `${NAME}` (an error when unset or empty) and `${NAME:-default}`. Quote them inside `[...]` and `{...}`, write `$${` for a literal `${`, and use them for numbers and switches too (`port: '${PORT}'`; switches also read `on`, `off`, `yes`, `no`, `1` and `0`).

The sections that follow, `entities`, `functions`, `types`, `extensions` and `search`, change the generated APIs where the database cannot say what is wanted. Schema design (tables, constraints, types, functions) stays in SQL; these hold only API-level operations, keyed by **database** names so renames never break them, and shared by every API.

The server loads `--config <path>`, else `./graphpg.yaml` when present. `graphpg export-config-schema` writes a JSON Schema (`graphpg.schema.json`) listing the actual relations, columns, constraints and functions for editor completion and validation. In dev mode the config is re-read when it changes. Every reference is validated at startup and all problems are reported together.

```yaml
entities:                      # tables, views, materialized views, foreign tables
  authors:
    type_name: Writer          # derived names follow: WriterFilter, createWriter, ...
    description: People who write
    omit: false                # true removes the type and every relation to it
    omit_operations: [delete]  # read, list, connection, create, update, delete
    events:                    # change events: described as OpenAPI webhooks
      operations: [insert, update, delete]   # default: all three
      description: Fired when a writer changes
    list_field: writers        # root field names
    single_field: writer
    connection_field: writersConnection
    columns:
      full_name: { field_name: name, description: The writer's name }
      secret: { omit: true }   # hidden from types, inputs, filters, orderBy
    foreign_keys:              # by constraint name
      authors_org_fkey: { field_name: organization, reverse_field_name: members, omit_reverse: false }
  cheap_books:                 # a view
    primary_key: [id]          # key of a relation without one
    foreign_keys:
      cheap_books_author:      # a new name declares a virtual foreign key
        columns: [author_id]
        references: { table: authors }   # columns default to its primary key
functions:
  review_summary:              # table-valued function
    field_name: reviewSummary  # root Query field
    omit: false
    mount:                     # also a field of Book, `p_book` filled from the row
      on: books
      field_name: reviewSummary
      arguments: { p_book: id }
  authors_display_name:        # computed column function
    field_name: shout
types:
  book_status:                 # PostgreSQL enum type
    type_name: PrintStatus
    values: { in-print: PRINTING }   # label: GraphQL value
  books.cover:                 # text column closed by CHECK (cover IN ('hard', 'soft'))
    type_name: Cover
  money_amount:                # composite type
    type_name: Money           # input type: MoneyInput
    attributes:
      amount: { field_name: value }
extensions:
  expose: [pgcrypto]           # objects owned by other extensions stay hidden; "*" exposes all
```

---

### Error detail

Typed constraint errors (`EMAIL_TAKEN`), invalid input, limit errors and `PERMISSION_DENIED` (a missing privilege or a Row-Level Security policy refusing a row) reach the client in every mode: it can act on them. Anything else, a database error nobody can fix from the request, follows `errors.detail`. `masked` (the default in `prod`) answers `An unexpected database error occurred` with the code `INTERNAL_SERVER_ERROR` and a request ID (`extensions.requestId` in GraphQL, `requestId` in the REST problem document) and logs the real error under that ID. `full` (the default in `dev`) adds the database's message. GraphQL and REST share the setting.

### Introspection and the SDL

With `graphql.introspection: false` (the default in `prod`) a query that reads `__schema` or `__type` at the root is refused with `INTROSPECTION_DISABLED`; `__typename` still works. GraphiQL needs introspection: with `graphql.ui` on in prod, turn it on too. `graphql.sdl` serves the schema as text at `graphql.sdl_path` without authentication, so keep it off where the schema is not public.

### Security headers

`headers.preset` picks what every response carries. `relaxed` sends `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`. `strict` adds `Strict-Transport-Security`, `X-Frame-Options: DENY`, a `Content-Security-Policy` that allows nothing (`default-src 'none'; frame-ancestors 'none'`) and `Cross-Origin-Opener-Policy: same-origin`. The CSP and `X-Frame-Options` are left off the GraphiQL and Scalar pages, which need scripts and styles. `headers.set` overrides single headers after the preset.

## Multiple schemas

List the schemas to expose in `graphpg.yaml`:

```yaml
database:
  schemas: [auth, app, billing]
```

The first schema listed is the default: where two schemas have a table of the same name, unqualified names in the config resolve to it, and the REST API serves the default schema's table at `/rest/<table>`; every schema's own are at `/rest/<schema>/<table>` (see [Several schemas](rest-api.md#several-schemas)). If `database.schemas` is omitted, graphpg defaults to `app_public`.

Foreign keys across schemas become ordinary relations, joined in the same single SQL statement (`app.posts.author_id` referencing `auth.users.id` gives `Post.user` and `User.posts`). `node(id:)` and Federation `_entities` route to the right schema.

Names stay natural while they are unique across schemas (`User`, `Post`). When two schemas have a table with the same name, both get their schema as a prefix:

| Table | Unique across schemas | Type | Root fields |
| :--- | :--- | :--- | :--- |
| `auth.users` | yes | `User` | `users`, `user(id:)`, `usersConnection`, `allUsersConnection` |
| `auth.audit_logs` | no | `AuthAuditLog` | `authAuditLogs`, `authAuditLog(id:)`, `authAuditLogsConnection`, `authAllAuditLogsConnection` |
| `app.audit_logs` | no | `AppAuditLog` | `appAuditLogs`, `appAuditLog(id:)`, `appAuditLogsConnection`, `appAllAuditLogsConnection` |

In the config, qualify an entity as `schema.table` (`auth.users:`) when its name is ambiguous.

## Runtime modes and watching

The server operates in either `prod` (production, the default) or `dev` (development) mode, selected via `--mode dev|prod` or `GRAPHPG_MODE` (`--mode` takes precedence).

```yaml
watch:
  enabled: true   # re-introspect on DDL changes; default: true in dev, false in prod
  export: true    # auto-export schemas on DDL changes; default: true
```

### Reloading the schema without a restart

In any mode, graphpg reads the database again when asked, and swaps the new schema in at once. Requests in flight finish on the schema they started with. End a migration with:

```sql
NOTIFY graphpg_reload;
```

or send the process `SIGHUP`. The listener has a connection of its own, reconnects when it drops, and never affects serving; it needs a session (a pooler in transaction mode cannot `LISTEN`, so point it at the database directly or reload with `SIGHUP`).

```yaml
reload:
  enabled: true            # default
  channel: graphpg_reload  # the NOTIFY channel
```

A reload runs the checks a startup runs, and **keeps the schema being served** when one fails, logging why: pinned schemas with `export.pin: fail` (the deployed files must match the new schema, so ship them with the migration), and in prod the persisted query manifest. Only the database is read again: configuration changes need a restart. Dev mode also reloads on its own when the schema changes (see below).

### Pinned schemas

The server needs the database catalog at startup whatever else it has, so an exported schema cannot replace introspection. It can *pin* it. CI exports the schemas and commits them; the server compares what the database generates now with those files (`export.graphql`, `export.openapi`, and `export.subgraph` when federation is on):

```sh
graphpg schema export            # write the files; commit them
graphpg schema export --check    # CI: exit 1 if a file is missing or out of date, with what changed
```

At startup, `export.pin` decides what a difference does: `fail` (the default in prod mode) refuses to start and prints what changed (`+` new, `-` gone, `~` changed, with JSON pointers for the OpenAPI document), `warn` (the default in dev mode) logs it, `off` does not check. A file that does not exist pins nothing, so a deployment without the files is unaffected. The OpenAPI document is compared as a document: formatting and key order are not drift.

In dev mode the watcher rewrites the files on every schema change (`watch.export`), so they stay current as you work.

### Safety limits

`limits:` protects a deployment from expensive and abusive requests. The same limits apply to GraphQL and REST: they are enforced on the **plan** both produce, before any SQL is compiled, so the two cannot diverge. Every value is a number, or `off` for no limit; modes override them like any other setting (`dev: { limits: { cost: off } }`).

| Limit | Default | |
| :--- | :--- | :--- |
| `page_size.default` | 100 | Rows of a connection asked for neither `first` nor `last` |
| `page_size.max` | 1000 | Largest `first` / `last` (`PAGE_SIZE_EXCEEDED`). Lists that have no page size (plain list fields, to-many relations) are **cut at this many rows**: page through a connection to read more |
| `depth` | 10 | How many levels of rows an operation nests: a root list is 1, the rows of a relation under it 2, … (`DEPTH_LIMIT_EXCEEDED`). A REST `include` path counts too |
| `cost` | 10 000 000 | An estimate of the rows an operation reads: each step's rows (its page size, 1 for a single row) times the rows it hangs from, summed. 100 forums with their last 100 messages and each message's author cost `100 + 100 × 100 + 100 × 100` (`COST_LIMIT_EXCEEDED`) |
| `bulk_rows` | 1000 | Rows one write inserts, nested writes included (GraphQL `create<Types>`, REST bulk creates and upserts), and rows an update or delete by filter may affect (GraphQL `update<Types>(where:)`, `delete<Types>(where:)`, REST `PATCH` / `DELETE` on a collection): above it the write is rolled back (`BULK_LIMIT_EXCEEDED`, HTTP 413) |
| `groups` | 10 000 | Groups an aggregate request may return (`TOO_MANY_GROUPS`) |
| `request_body_bytes` | 1 MiB | Largest request body of `/graphql` and `/rest` (413). `/import` streams and is not limited |
| `require_filter` | `false` | GraphQL `update<Types>(where:)` and `delete<Types>(where:)` with no condition (`where: {}`, every row) need `unfiltered: true`. REST always asks for it (`unfiltered=true`) |
| `count_threshold` | 10 000 | Threshold for `Prefer: count=estimated`: exact count below it, planner estimate above |
| `statement_timeout` | `30s` | `statement_timeout` of every connection: a statement running longer is cancelled and the request fails with `STATEMENT_TIMEOUT` (HTTP 503), without the database's message. A role's own lower `statement_timeout` applies first. Use a unit: `500ms`, `30s`, `2m`, `1h` |

A request over a limit fails before it touches the database. GraphQL: an error with `extensions: { code, limit, actual }` (`{ "code": "PAGE_SIZE_EXCEEDED", "limit": 1000, "actual": 5000 }`). REST: a [problem](rest-api.md#errors) with the same `code`, `limit` and `actual` (400, 413 for bulk rows). The OpenAPI document states the page and bulk limits it was generated under. Mutations that match rows by a filter (`update<Types>(where:)`) count as `page_size.max` rows toward the cost, since the rows are not known beforehand. Functions returning sets of values are not capped: a function controls its own result.

Limits are global, not per entity: one number to reason about, and the same for every client of a container. For different limits per audience, run another container.

### Rate limits

`limits.rate_limit` bounds how many requests a client makes to `/graphql`, `/export`, `/import` and `/rest`. It is **off by default**; off, there is no middleware at all, nothing is counted and no headers are sent. Probes, metrics, the SDL, the OpenAPI document and the UIs are never limited.

```yaml
limits:
  rate_limit:
    enabled: true
    anonymous: { requests_per_minute: 60, burst: 20 }
    authenticated: { requests_per_minute: 600 }   # burst defaults to the rate
    trusted_proxies: [10.0.0.0/8]
prod:
  limits: { rate_limit: { enabled: true } }       # or switch it on per mode
```

- **Who a client is.** A request with an access token that verifies is counted against its `key_claim` (`sub` by default; a claim name or a dot-separated path), at the `authenticated` rate. Anything else, a missing or invalid token included, is counted against its IP address at the `anonymous` rate. The address is the connection's peer; when the peer is in `trusted_proxies`, it is the last `X-Forwarded-For` entry no trusted proxy added, so a client cannot choose its own address. `requests_per_minute: off` leaves a kind of client unlimited.
- **How.** Each client has a token bucket (the Generic Cell Rate Algorithm): it holds `burst` requests and refills at `requests_per_minute`. A request is counted before it is parsed, planned or sent to the database; a refused one does not use up the bucket.
- **Headers.** Every answer carries `RateLimit-Limit` (the burst), `RateLimit-Remaining` and `RateLimit-Reset` (seconds until the bucket is full). A refused request is `429 Too Many Requests` with `Retry-After` and the code `RATE_LIMITED`: a GraphQL error (`extensions: { code: "RATE_LIMITED", retryAfter }`), or a REST problem document.
- **Documented and readable.** With rate limits on, the OpenAPI document gives every operation a `429` response and every success the `RateLimit-*` headers; off, the document is unchanged. Cross-origin pages can read the headers: `server.cors.expose_headers` exposes them by default.
- **Per instance.** Buckets live in each instance's memory (at most `max_clients` of them; full buckets are dropped first), so behind a load balancer with N instances a client gets up to N times the rate. Divide the rates by the number of instances, or use sticky sessions.

### Development mode (`--mode dev`)

- `watch.enabled` defaults to `true`: graphpg installs an event trigger on `ddl_command_end` and `sql_drop`, listens for asynchronous notifications (`LISTEN graphpg_watch`), and re-introspects live catalogs in ~1.5s without interrupting in-flight requests.
- When `watch.export` is `true` (default), changes automatically write the files defined in `export:` (`export.graphql`, `export.openapi`, `export.subgraph`, `export.config_schema`).
- `graphql.ui` (GraphiQL), `openapi.ui` (Scalar), `graphql.introspection`, `graphql.sdl` and `errors.detail: full` default to on, and `headers.preset` to `relaxed`.
- The watcher hot-reload always uses the selected mode overlay.
- Debug outputs (`debug.sql` and `debug.explain`) may be enabled under `dev:`.

### Production mode (`--mode prod`)

- `watch.enabled`, `graphql.ui`, `openapi.ui`, `graphql.introspection` and `graphql.sdl` default to `false`, `errors.detail` to `masked` and `headers.preset` to `strict`. If any of these dev features are explicitly enabled, a startup warning is logged.
- `limits.rate_limit` and `graphql.persisted_queries` are off in both modes; turn them on under `prod:` to use them in production only. With a manifest, prod refuses to start when one of its operations no longer validates against the schema (dev logs it).
- `debug.sql` and `debug.explain` are **dev-only**: validation rejects them if enabled in the shared section or `prod:` block (`debug.sql is only allowed in the dev: block`).

---

## Comment hints

Optional lines in SQL comments, versioned with your migrations:

- `@name <GraphQLName>` names a table's type, a column's field, a function's field or a type.
- `@deprecated [reason]` deprecates a column's field, a function's fields, or a table's root fields and the relation fields leading to it.

The config (`deprecated:` on entities, columns and functions) wins over comment hints. A table or column's SQL comment is also its GraphQL description.

## Startup advice

At startup, and with `graphpg advise`, graphpg reports foreign keys no index leads with, tables without a primary key, and orderable columns no index leads with. For [foreign tables](foreign-data.md) it also reports:

- an exposed foreign table without a declared key;
- a `postgres_fdw` table without `use_remote_estimate`, or one never analyzed;
- a to-many relation into a foreign table, with its cost (one remote query, or one file read, per parent row);
- root fields on a foreign table while requests are authenticated (Row-Level Security cannot apply to it);
- a role requests run as that has no user mapping for the foreign server.

They are logged (warnings as warnings, the rest at debug level); nothing is changed.
