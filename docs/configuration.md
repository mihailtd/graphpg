# Configuration

graphpg is configured by one optional file, `graphpg.yaml`, which serves every environment (shared settings, then `dev:` and `prod:` blocks for what differs), and, for now, by environment variables and command-line flags that override it. The file's `${NAME}` references read the environment, so credentials stay out of it.

## Running

```bash
graphpg                              # serve on :4000 (DATABASE_URL, PORT)
graphpg --dev                        # also watch the database for DDL changes
graphpg --config path/to/file.yaml   # explicit config file
graphpg advise                       # print schema advice and exit
graphpg export-graphql --out schema.graphql
graphpg export-subgraph --out subgraph.graphql          # Apollo Federation SDL
graphpg export-config-schema --out .engine/schema.config.json
```

`DATABASE_URL` defaults to `postgres://postgres:postgres@localhost:5432/graphpg`, which is only meant for local development: set it explicitly everywhere else. A `.env` file in the working directory is loaded when present.

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
| `/graphql` | `POST` | Queries, mutations, `@defer` / `@stream` (as `multipart/mixed`) and Federation `_entities` |
| `/graphiql` | `GET` | GraphiQL IDE, when `graphql.ui` is on (404 otherwise) |
| `/scalar` | `GET` | Scalar API reference, when `openapi.ui` is on (404 otherwise); it shows the OpenAPI document at `/openapi.json` once the REST API serves one |
| `/export` | `POST` | A connection query streamed as NDJSON or CSV |
| `/import/{Type}` | `POST` | NDJSON rows of `Create<Type>Input`, inserted all-or-nothing with `COPY` |
| `/health` | `GET` | Liveness |

### Environment variables and flags

| Variable | Flag | Default | |
| :--- | :--- | :--- | :--- |
| `DATABASE_URL` | | local dev URL | PostgreSQL connection string |
| `DATABASE_POOL_SIZE` | | `10` | Connections in the pool |
| `DATABASE_REPLICA_URLS` | | | Comma-separated read replicas ([details](performance.md#operations)) |
| `DATABASE_SCHEMAS` / `DATABASE_SCHEMA` | | `app_public` | Schemas to expose, comma-separated ([multiple schemas](#multiple-schemas)) |
| `PORT` | | `4000` | Listening port |
| `ENGINE_CONFIG` | `--config <path>` | `./graphpg.yaml` if present | Config file |
| `GRAPHIQL` | `--graphiql` / `--no-graphiql` | off | GraphiQL IDE: development and pre-production only |
| `FEDERATION` | `--federation` | off | Apollo Federation subgraph mode ([guide](federation-and-rbac.md)) |
| `AUTH_ENABLED`, `AUTH_ISSUER`, `AUTH_AUDIENCE`, `AUTH_JWKS_URI`, `AUTH_JWT_SECRET` | `--auth` / `--no-auth` | off | OAuth 2.0 bearer tokens ([guide](authentication.md)) |
| `DEV` / `WATCH` | `--dev` / `--watch` | off | [Development mode](#development-mode---dev) |
| `DEBUG` | `--debug` | off | Include the generated SQL in response `extensions` |
| `DEBUG_EXPLAIN` | `--debug-explain` | off | Include PostgreSQL's `EXPLAIN` output. Never enable the debug flags in production |
| `SQL_COMMENTS` | | on | `0` turns off [sqlcommenter](performance.md#operations) comments |
| `GRAPHQL_SCHEMA_OUT`, `CONFIG_SCHEMA_OUT`, `SUBGRAPH_SCHEMA_OUT` | `--out <path>` | | Output paths of the export commands and of dev-mode exports |
| `RUST_LOG` | | `info` | Log filter |
| `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME` | | | Trace export, when built with the `otel` feature |

## `graphpg.yaml`

One file configures a server for every environment. Shared settings come first; the settings that differ between modes go in `dev:` and `prod:` blocks, which are merged over the shared ones for the mode the server starts in (`--dev` selects development, otherwise production).

```yaml
database:
  url: ${DATABASE_URL}          # strings read the environment; ${NAME:-default} gives a default
  pool_size: 10
  schemas: [auth, app]          # the first is the default schema (default: app_public)
  replicas: []                  # read replica connection strings
  sql_comments: true            # sqlcommenter comments on every statement

server:
  host: 0.0.0.0
  port: 4000

auth: { ... }                   # OAuth 2.0 bearer tokens (see authentication.md)
rbac: { enabled: false }        # prune the APIs to what a role may access
debug: { sql: false, explain: false }   # development only

graphql:
  enabled: true                 # serve /graphql
  ui: false                     # the GraphiQL IDE: on by default in dev mode only
  federation: { enabled: false }
openapi:
  enabled: false                # the REST API and its OpenAPI description
  ui: false                     # the Scalar API reference: on by default in dev mode only

dev:                            # settings that differ in dev mode
  database: { pool_size: 2 }
prod: {}                        # ... and in prod mode
```

The effective config of a mode is built in four steps, each overriding the last: the built-in defaults, the mode's built-in defaults (`dev` turns the GraphiQL IDE and the Scalar API reference on), the shared settings, then the mode's block. Mappings merge key by key; anything else, a list included, is replaced. An unknown key is an error in the shared settings and in both blocks, so a typo in `dev:` is found when starting `prod`.

### The UIs

`graphql.ui` serves the GraphiQL IDE at `/graphiql` and `openapi.ui` the Scalar API reference at `/scalar`. Both are on in dev mode and off in prod, and either can be set explicitly. They are part of the binary (and of the container image), load nothing from other hosts, and work offline, so a strict Content-Security-Policy can allow `'self'` only. In a container, turn one on with a mounted config (`graphql: { ui: true }`) or, for GraphiQL, `GRAPHIQL=true`.

The GraphiQL source is `packages/graphiql-ui`; `moon run graphiql-ui:build` builds it into `crates/graphpg-ui/assets`, which the server embeds. A build without those files still runs, and `/graphiql` says so.

Strings may read the environment: `${NAME}` (an error when unset or empty) and `${NAME:-default}`. Quote them inside `[...]` and `{...}`, write `$${` for a literal `${`, and use them for numbers and switches too (`port: '${PORT}'`; switches also read `on`, `off`, `yes`, `no`, `1` and `0`).

The sections that follow, `entities`, `functions`, `types`, `extensions` and `search`, change the generated APIs where the database cannot say what is wanted. Schema design (tables, constraints, types, functions) stays in SQL; these hold only API-level operations, keyed by **database** names so renames never break them, and shared by every API.

The server loads `--config <path>`, else `ENGINE_CONFIG`, else `./graphpg.yaml` when present. `graphpg export-config-schema` writes a JSON Schema (`.engine/schema.config.json`) listing the actual relations, columns, constraints and functions for editor completion and validation. In `--dev` mode the config is re-read when it changes. Every reference is validated at startup and all problems are reported together.

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

List the schemas to expose, in `graphpg.yaml` or the environment:

```yaml
database:
  schemas: [auth, app, billing]
```

```bash
DATABASE_SCHEMAS="auth,app,billing" graphpg
```

Schemas are chosen in this order, at startup and on every dev-mode reload: `DATABASE_SCHEMAS`, else the config's `database.schemas`, else `DATABASE_SCHEMA`, else `app_public`. The first schema listed is the default: where two schemas have a table of the same name, unqualified names in the config resolve to it.

Foreign keys across schemas become ordinary relations, joined in the same single SQL statement (`app.posts.author_id` referencing `auth.users.id` gives `Post.user` and `User.posts`). `node(id:)` and Federation `_entities` route to the right schema.

Names stay natural while they are unique across schemas (`User`, `Post`). When two schemas have a table with the same name, both get their schema as a prefix:

| Table | Unique across schemas | Type | Root fields |
| :--- | :--- | :--- | :--- |
| `auth.users` | yes | `User` | `users`, `user(id:)`, `usersConnection`, `allUsersConnection` |
| `auth.audit_logs` | no | `AuthAuditLog` | `authAuditLogs`, `authAuditLog(id:)`, `authAuditLogsConnection`, `authAllAuditLogsConnection` |
| `app.audit_logs` | no | `AppAuditLog` | `appAuditLogs`, `appAuditLog(id:)`, `appAuditLogsConnection`, `appAllAuditLogsConnection` |

In the config, qualify an entity as `schema.table` (`auth.users:`) when its name is ambiguous.

## Development mode (`--dev`)

The engine provides continuous schema synchronization without server restarts:
1. Installs an event trigger on `ddl_command_end` and `sql_drop`.
2. Emits asynchronous PostgreSQL notifications via `pg_notify('graphpg_watch', ...)`.
3. The engine server holds an `Arc<RwLock<Schema>>` and `Arc<RwLock<DatabaseCatalog>>`.
4. When a DDL notification is received:
   - Live catalogs are re-introspected in ~1.5s.
   - GraphQL SDL and YAML JSON Schema hints are updated in memory.
   - Files are auto-exported to `schema.graphql` and `.engine/schema.config.json`.
   - In-flight HTTP requests continue without interruption.

---

## Comment hints

Optional lines in SQL comments, versioned with your migrations:

- `@name <GraphQLName>` names a table's type, a column's field, a function's field or a type.
- `@deprecated [reason]` deprecates a column's field, a function's fields, or a table's root fields and the relation fields leading to it.

The config (`deprecated:` on entities, columns and functions) wins over comment hints. A table or column's SQL comment is also its GraphQL description.

## Startup advice

At startup, and with `graphpg advise`, graphpg reports foreign keys no index leads with, tables without a primary key, and orderable columns no index leads with. They are logged as warnings; nothing is changed.
