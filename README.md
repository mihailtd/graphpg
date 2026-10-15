![graphpg: GraphQL and REST, generated from PostgreSQL](packages/website/src/public/cover.png)

# graphpg

A GraphQL **and REST** server for PostgreSQL. Point it at a database and it serves a GraphQL API and a REST API (described by an OpenAPI 3.1 document) generated from the schema: tables, relations, constraints, types, functions and privileges. There is no resolver code to write and no schema to maintain in two places.

For AI work it covers two jobs. **Agents that use your data** get typed tools over it: vector, BM25 and hybrid search from your indexes, and [MCP](docs/ai-agents.md#mcp-tools-through-apollo-mcp-server) through Apollo's MCP Server, limited to the operations you list and to what Row-Level Security lets the agent's token see. **Agents that build applications** get a typed contract to code against: the GraphQL SDL and the OpenAPI document are generated from the database, pinned in CI, and errors carry typed codes. See [AI agents](docs/ai-agents.md).

graphpg is a single Rust binary. It reads PostgreSQL's catalogs at startup, builds the APIs, and compiles each request into one SQL statement (lateral joins and JSON aggregation, so nested selections don't cause N+1 queries). Both APIs are planned onto the same engine, so they share filters, pagination, nested writes, typed errors, authorization and safety limits.

> **Status:** 1.1, the API is stable. Versions follow [semantic versioning](docs/versioning.md): the GraphQL and REST APIs derived from a database, error codes, configuration and CLI change incompatibly only in a major version. The behaviour documented here is covered by an integration test suite that runs against PostgreSQL 18. See the [changelog](CHANGELOG.md).

## Quick start

```bash
git clone https://github.com/mihailtd/graphpg && cd graphpg/examples/blog
docker compose up --build
```

Open <http://localhost:4000/graphiql> for GraphQL, or <http://localhost:4000/scalar> for the REST API reference. The [blog example](examples/blog) loads a small schema and shows relations, filtering, pagination, search and nested writes in both.

Run the published container image against your own database:

```bash
docker run --rm -p 4000:4000 \
  -e DATABASE_URL=postgres://user:pass@host.docker.internal/mydb \
  ghcr.io/mihailtd/graphpg:1.1.0 --mode dev
```

The image is a static binary on `distroless/static` (about 27 MB with the embedded GraphiQL and Scalar UIs, no shell, non-root, signed with Sigstore). See [running in a container](docs/configuration.md#running-in-a-container).

Or build from source:

```bash
cargo install --path crates/graphpg          # or: cargo build --release
DATABASE_URL=postgres://user:pass@localhost/mydb graphpg --mode dev
```

```graphql
{
  authors {
    username
    postsConnection(first: 5, orderBy: [CREATED_AT_DESC]) { nodes { title tags { name } } }
  }
}
```

The same data over REST:

```bash
curl -g 'localhost:4000/rest/posts?filter[status][eq]=PUBLISHED&order=-createdAt&first=5&include=tags'
curl -X POST localhost:4000/rest/authors -H 'content-type: application/json' \
     -d '{"username": "ada", "displayName": "Ada", "posts": {"create": [{"title": "Hello", "body": "Hi there"}]}}'    # a nested write
```

## What it generates

- **Types, relations and lookups** from tables, views, foreign keys (composite ones too), unique keys and many-to-many junction tables; Relay connections with keyset pagination, `node(id:)` and global IDs.
- **Filtering, ordering and aggregates**, including filters across relations (`some` / `every` / `none`), grouped aggregates, and ordering by related columns.
- **Mutations**: create, update, delete, upsert, bulk, and nested writes across relations, in one transaction. Constraint violations come back as typed error codes derived from the constraint (`EMAIL_TAKEN`, `AUTHOR_NOT_FOUND`).
- **A REST API** at `/rest`: lists with typed filters, ordering, keyset pagination and embedding; single-row reads and writes with `ETag` / `If-Match`; nested writes and bulk creates; aggregates and grouped aggregates; functions at `/rest/rpc/…`; search endpoints; RFC 9457 `problem+json` errors with the same typed codes. It is described by an **OpenAPI 3.1** document (typed parameters, nested-write bodies) and a bundled Scalar reference. See the [REST API guide](docs/rest-api.md) and the [comparison with PostgREST](docs/comparison.md#compared-with-postgrest).
- **Functions as fields**: `STABLE` functions become queries, `VOLATILE` ones mutations, and functions of a row become computed fields.
- **PostgreSQL types**: enums, composite types, domains, arrays, ranges and multiranges, `ltree`, `jsonb`, pgvector, PostGIS.
- **Search** fields from the indexes you have: trigram, full-text, pgvector, BM25 and hybrid.
- **Modern PostgreSQL**: temporal keys (`WITHOUT OVERLAPS`, `PERIOD`), `RETURNING OLD`, `JSON_TABLE` views, `MERGE`-based syncing of children.
- **Authorization from the database**: OAuth 2.0 bearer tokens are verified and their claims are made available to **Row-Level Security** policies; with `rbac.enabled`, the GraphQL schema, REST API and OpenAPI document are pruned to what the connecting role may do, from `GRANT`s.
- **Safety limits for both APIs**: page size, query depth, estimated cost, bulk size, request size and `statement_timeout`, enforced on the plan before any SQL runs; production errors never carry PostgreSQL's own messages ([limits](docs/configuration.md#safety-limits)). Optional [rate limits](docs/configuration.md#rate-limits) per client, and [persisted queries](docs/graphql-api.md#persisted-queries) with an allow-list of operations for production.
- **Pinned schemas**: `graphpg schema export` writes the GraphQL SDL and the OpenAPI document; `--check` fails CI when they are out of date, and production refuses to start on drift.
- **Event triggers**: signed webhooks on row changes, captured in the change's own transaction, and on cron schedules; at-least-once delivery shared by every instance, with retries and a dead letter ([events](docs/events.md)).
- **Subscriptions (live queries)**, opt-in: any query field as a `subscription`, its result sent again when it changes, under Row-Level Security; over WebSocket (`graphql-transport-ws`), SSE and multipart HTTP; batched re-runs on read replicas, nothing installed in the database ([subscriptions](docs/subscriptions.md)).
- **Apollo Federation v2** subgraph mode, `@defer` / `@stream`, streaming NDJSON/CSV export and `COPY` import, read replicas, OpenTelemetry spans.

Schema design stays in SQL. An optional [`graphpg.yaml`](docs/configuration.md) renames and hides things and declares keys for views; it is not required.

## Documentation

The guides are also published as a website at <https://mihailtd.github.io/graphpg/>.

| | |
| :--- | :--- |
| [AI agents](docs/ai-agents.md) | MCP tools for agents through Apollo's MCP Server, vector and hybrid search for retrieval, typed contracts for agents that write code, and the guardrails |
| [Configuration](docs/configuration.md) | Running graphpg, environment variables, `graphpg.yaml`, safety limits, pinned schemas, multiple schemas, dev mode |
| [Schema mapping](docs/schema-mapping.md) | How tables, types, functions and constraints become GraphQL and REST |
| [GraphQL API](docs/graphql-api.md) | Pagination, filtering, aggregates, mutations, nested writes, errors, search, limits |
| [REST API](docs/rest-api.md) | Filtering, pagination, embedding, writes and nested writes, aggregates, functions, search, errors, limits |
| [Authentication](docs/authentication.md) | OAuth 2.0 resource server and Row-Level Security |
| [Federation and RBAC](docs/federation-and-rbac.md) | Apollo Federation subgraphs and schema pruning by role |
| [Performance](docs/performance.md) | Indexes and settings, replicas |
| [Observability](docs/observability.md) | Prometheus metrics, OpenTelemetry traces, metrics and logs (OTLP), JSON logs; Grafana LGTM |
| [Event triggers](docs/events.md) | Webhooks on row changes (captured in the change's transaction) and on schedules: delivery, retries, signatures |
| [Subscriptions](docs/subscriptions.md) | Live queries: transports, authentication and RLS, what they cost, scaling on replicas, Apollo Router |
| [Other databases and files](docs/foreign-data.md) | Foreign data wrappers (`postgres_fdw`, `file_fdw`, Parquet, S3): keys, relations, authorization, performance and caveats |
| [Extending graphpg](docs/extending.md) | Where computed fields, custom operations, side effects, jobs and external logic go, and why there are no plugins |
| [Comparison](docs/comparison.md) | How graphpg differs from PostGraphile and Hasura (GraphQL) and from PostgREST (REST) |
| [Versioning](docs/versioning.md) | What semantic versioning covers (the derived APIs, error codes, configuration, CLI), deprecation, supported PostgreSQL |
| [GraphQL and OpenAPI parity](docs/graphql-openapi-parity.md) | What each API knows about the database that the other does not, and whether it could be ported |

## Security notes

- Authentication is **off by default**: every request runs as the database user in `DATABASE_URL`. Turn it on and give the engine a low-privilege login role before exposing it ([guide](docs/authentication.md)).
- GraphiQL, Scalar and the `debug.sql` / `debug.explain` settings (which return generated SQL) are for development: they are on or allowed only in `--mode dev`.
- Requests are bounded by default (page size, depth, cost, bulk size, body size, a 30 s `statement_timeout`); review the [safety limits](docs/configuration.md#safety-limits) for your deployment.
- Report vulnerabilities as described in [SECURITY.md](SECURITY.md).

## Development

```bash
docker compose up -d           # PostgreSQL 18 with pgvector, database graphpg_test
cargo test --workspace         # TEST_DATABASE_URL overrides the connection string
```

Each test file creates the schema it needs. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Third-party test material is credited in [NOTICE](NOTICE).
