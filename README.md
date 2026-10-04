# graphpg

A GraphQL server for PostgreSQL. Point it at a database and it serves a GraphQL API generated from the schema: tables, relations, constraints, types, functions and privileges. There is no resolver code to write and no schema to maintain in two places.

graphpg is a single Rust binary. It reads PostgreSQL's catalogs at startup, builds the GraphQL schema, and compiles each request into one SQL statement (lateral joins and JSON aggregation, so nested selections don't cause N+1 queries).

> **Status:** 1.0. Versions follow [semantic versioning](https://semver.org); the behaviour documented here is covered by an integration test suite that runs against PostgreSQL 18. See the [changelog](CHANGELOG.md).

## Quick start

```bash
git clone https://github.com/mihailtd/graphpg && cd graphpg/examples/blog
docker compose up --build
```

Open <http://localhost:4000/graphiql>. The [blog example](examples/blog) loads a small schema and shows relations, filtering, pagination, search and nested writes.

Run the published container image against your own database:

```bash
docker run --rm -p 4000:4000 \
  -e DATABASE_URL=postgres://user:pass@host.docker.internal/mydb -e DATABASE_SCHEMA=public \
  ghcr.io/mihailtd/graphpg:1.0.1
```

The image is a static binary on `distroless/static` (about 20 MB, no shell, non-root, signed with Sigstore). See [running in a container](docs/configuration.md#running-in-a-container).

Or build from source:

```bash
cargo install --path crates/graphpg          # or: cargo build --release
DATABASE_URL=postgres://user:pass@localhost/mydb DATABASE_SCHEMA=public graphpg --graphiql
```

```graphql
{
  authors {
    username
    postsConnection(first: 5, orderBy: [CREATED_AT_DESC]) { nodes { title tags { name } } }
  }
}
```

## What it generates

- **Types, relations and lookups** from tables, views, foreign keys (composite ones too), unique keys and many-to-many junction tables; Relay connections with keyset pagination, `node(id:)` and global IDs.
- **Filtering, ordering and aggregates**, including filters across relations (`some` / `every` / `none`), grouped aggregates, and ordering by related columns.
- **Mutations**: create, update, delete, upsert, bulk, and nested writes across relations, in one transaction. Constraint violations come back as typed error codes derived from the constraint (`EMAIL_TAKEN`, `AUTHOR_NOT_FOUND`).
- **Functions as fields**: `STABLE` functions become queries, `VOLATILE` ones mutations, and functions of a row become computed fields.
- **PostgreSQL types**: enums, composite types, domains, arrays, ranges and multiranges, `ltree`, `jsonb`, pgvector, PostGIS.
- **Search** fields from the indexes you have: trigram, full-text, pgvector, BM25 and hybrid.
- **Modern PostgreSQL**: temporal keys (`WITHOUT OVERLAPS`, `PERIOD`), `RETURNING OLD`, `JSON_TABLE` views, `MERGE`-based syncing of children.
- **Authorization from the database**: OAuth 2.0 bearer tokens are verified and their claims are made available to **Row-Level Security** policies; the GraphQL schema is pruned to what the connecting role may do, from `GRANT`s.
- **Apollo Federation v2** subgraph mode, `@defer` / `@stream`, streaming NDJSON/CSV export and `COPY` import, read replicas, OpenTelemetry spans.

Schema design stays in SQL. An optional [`engine.config.yaml`](docs/configuration.md) renames and hides things and declares keys for views; it is not required.

## Documentation

| | |
| :--- | :--- |
| [Configuration](docs/configuration.md) | Running graphpg, environment variables, `engine.config.yaml`, multiple schemas, dev mode |
| [Schema mapping](docs/schema-mapping.md) | How tables, types, functions and constraints become GraphQL |
| [GraphQL API](docs/graphql-api.md) | Pagination, filtering, aggregates, mutations, nested writes, errors, search |
| [Authentication](docs/authentication.md) | OAuth 2.0 resource server and Row-Level Security |
| [Federation and RBAC](docs/federation-and-rbac.md) | Apollo Federation subgraphs and schema pruning by role |
| [Performance](docs/performance.md) | Indexes and settings, replicas, observability |
| [Comparison](docs/comparison.md) | How graphpg differs from PostGraphile and Hasura |

## Security notes

- Authentication is **off by default**: every request runs as the database user in `DATABASE_URL`. Turn it on and give the engine a low-privilege login role before exposing it ([guide](docs/authentication.md)).
- GraphiQL and the `--debug` flags (which return generated SQL) are for development only.
- Report vulnerabilities as described in [SECURITY.md](SECURITY.md).

## Development

```bash
docker compose up -d           # PostgreSQL 18 with pgvector, database graphpg_test
cargo test --workspace         # TEST_DATABASE_URL overrides the connection string
```

Each test file creates the schema it needs. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Third-party test material is credited in [NOTICE](NOTICE).
