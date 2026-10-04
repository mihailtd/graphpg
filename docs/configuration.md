# Configuration

graphpg is configured by one optional file, `graphpg.yaml`, which serves every environment (shared settings, then `dev:` and `prod:` blocks for what differs). Secrets (`DATABASE_URL`, `DATABASE_REPLICA_URLS`, `AUTH_JWT_SECRET`) are environment-only. The file's `${NAME}` references read non-secret environment variables, so settings can also be parameterized per environment.

## Running

```bash
graphpg                              # serve in prod mode on :4000
graphpg --mode dev                   # dev mode (watch DDL, auto-export, GraphiQL/Scalar on)
graphpg --config path/to/file.yaml   # explicit config file
graphpg advise                       # print schema advice and exit
graphpg export-graphql --out schema.graphql
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

OpenTelemetry export is compiled into the image: set `OTEL_EXPORTER_OTLP_ENDPOINT`.

### Endpoints

| Path | Method | |
| :--- | :--- | :--- |
| `/graphql` | `POST` | Queries, mutations, `@defer` / `@stream` (as `multipart/mixed`) and Federation `_entities` (404 when `graphql.enabled: false`) |
| `/rest` | `GET` | REST API (404 when `openapi.enabled: false`) |
| `/graphiql` | `GET` | GraphiQL IDE, when `graphql.ui` is on (404 otherwise) |
| `/scalar` | `GET` | Scalar API reference, when `openapi.ui` is on (404 otherwise); it shows the OpenAPI document at `/openapi.json` |
| `/export` | `POST` | A connection query streamed as NDJSON or CSV (404 when `graphql.enabled: false`) |
| `/import/{Type}` | `POST` | NDJSON rows of `Create<Type>Input`, inserted all-or-nothing with `COPY` (404 when `graphql.enabled: false`) |
| `/health` | `GET` | Liveness |

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

watch:
  enabled: false                # watch DDL; default: true in dev, false in prod
  export: true                  # write export files on schema change

export:
  graphql: schema.graphql
  subgraph: subgraph.graphql
  config_schema: graphpg.schema.json

auth: { ... }                   # OAuth 2.0 bearer tokens (see authentication.md)
rbac: { enabled: false }        # prune the APIs to what a role may access

graphql:
  enabled: true                 # serve /graphql, /export, /import/...
  ui: false                     # the GraphiQL IDE: on by default in dev mode only
  federation: { enabled: false }
openapi:
  enabled: true                 # the REST API at /rest and its OpenAPI description (alias: rest)
  ui: false                     # the Scalar API reference: on by default in dev mode only

dev:                            # settings that differ in dev mode
  debug:
    sql: true                   # dev-only: generated SQL in response extensions
    explain: true               # dev-only: EXPLAIN ANALYZE in response extensions
prod: {}                        # ... and in prod mode
```

The effective config of a mode is built in four steps, each overriding the last: the built-in defaults, the mode's built-in defaults (`dev` turns the GraphiQL IDE, Scalar API reference, and watch mode on), the shared settings, then the mode's block. Mappings merge key by key; anything else, a list included, is replaced. An unknown key is an error in the shared settings and in both blocks, so a typo in `dev:` is found when starting `prod`.

### The UIs

`graphql.ui` serves the GraphiQL IDE at `/graphiql` and `openapi.ui` (alias `rest.ui`) the Scalar API reference at `/scalar`. Both are on in dev mode and off in prod, and either can be set explicitly. They are part of the binary (and of the container image), load nothing from other hosts, and work offline, so a strict Content-Security-Policy can allow `'self'` only.

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

## Multiple schemas

List the schemas to expose in `graphpg.yaml`:

```yaml
database:
  schemas: [auth, app, billing]
```

The first schema listed is the default: where two schemas have a table of the same name, unqualified names in the config resolve to it. If `database.schemas` is omitted, graphpg defaults to `app_public`.

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

### Development mode (`--mode dev`)

- `watch.enabled` defaults to `true`: graphpg installs an event trigger on `ddl_command_end` and `sql_drop`, listens for asynchronous notifications (`LISTEN graphpg_watch`), and re-introspects live catalogs in ~1.5s without interrupting in-flight requests.
- When `watch.export` is `true` (default), changes automatically write the files defined in `export:` (`export.graphql`, `export.subgraph`, `export.config_schema`).
- `graphql.ui` (GraphiQL) and `openapi.ui` (Scalar) default to `true`.
- The watcher hot-reload always uses the selected mode overlay.
- Debug outputs (`debug.sql` and `debug.explain`) may be enabled under `dev:`.

### Production mode (`--mode prod`)

- `watch.enabled`, `graphql.ui`, and `openapi.ui` default to `false`. If any of these dev features are explicitly enabled, a startup warning is logged.
- `debug.sql` and `debug.explain` are **dev-only**: validation rejects them if enabled in the shared section or `prod:` block (`debug.sql is only allowed in the dev: block`).

---

## Comment hints

Optional lines in SQL comments, versioned with your migrations:

- `@name <GraphQLName>` names a table's type, a column's field, a function's field or a type.
- `@deprecated [reason]` deprecates a column's field, a function's fields, or a table's root fields and the relation fields leading to it.

The config (`deprecated:` on entities, columns and functions) wins over comment hints. A table or column's SQL comment is also its GraphQL description.

## Startup advice

At startup, and with `graphpg advise`, graphpg reports foreign keys no index leads with, tables without a primary key, and orderable columns no index leads with. They are logged as warnings; nothing is changed.
