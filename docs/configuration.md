# Configuration

graphpg is configured by environment variables and command-line flags (deployment concerns), and by an optional `engine.config.yaml` (changes to the generated schema that the database cannot express). Environment variables and flags override the file.

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

### Endpoints

| Path | Method | |
| :--- | :--- | :--- |
| `/graphql` | `POST` | Queries, mutations, `@defer` / `@stream` (as `multipart/mixed`) and Federation `_entities` |
| `/graphiql` | `GET` | GraphiQL IDE, when enabled (404 otherwise) |
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
| `ENGINE_CONFIG` | `--config <path>` | `./engine.config.yaml` if present | Config file |
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

## `engine.config.yaml`

The config changes the generated GraphQL schema where the database cannot say what is wanted. Schema design (tables, constraints, types, functions) stays in SQL; the config only holds GraphQL-level operations, keyed by **database** names so renames never break it.

The server loads `--config <path>`, else `ENGINE_CONFIG`, else `./engine.config.yaml` when present. `graphpg export-config-schema` writes a JSON Schema (`.engine/schema.config.json`) listing the actual relations, columns, constraints and functions for editor completion and validation. In `--dev` mode the config is re-read when it changes. Every reference is validated at startup and all problems are reported together.

```yaml
version: "1.0"
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

List the schemas to expose, in `engine.config.yaml` or the environment:

```yaml
schemas: [auth, app, billing]
```

```bash
DATABASE_SCHEMAS="auth,app,billing" graphpg
```

Schemas are chosen in this order, at startup and on every dev-mode reload: `DATABASE_SCHEMAS`, else the config's `schemas`, else `DATABASE_SCHEMA`, else `app_public`. The first schema listed is the default: where two schemas have a table of the same name, unqualified names in the config resolve to it.

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
