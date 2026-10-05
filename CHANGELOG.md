# Changelog

Releases are tagged `vX.Y.Z`; each one has generated notes on the
[Releases page](https://github.com/mihailtd/graphpg/releases). This file records the highlights.

## Unreleased

- **REST API** at `/rest`, planned onto the same engine as GraphQL: typed filters, ordering, keyset pagination, embedding, single-row reads and writes with `ETag` / `If-Match`, nested writes, bulk creates, aggregates and grouped aggregates, functions (`/rest/rpc/…`), search methods and global search, `Prefer: previous` (`RETURNING OLD`), views. Errors are RFC 9457 `application/problem+json` with the typed constraint codes, masked in production.
- **OpenAPI 3.1** document generated from the catalog and configuration: typed parameters, nested-write bodies (the GraphQL input types), the configured limits; JSON or YAML; served at `openapi.path` and shown by the bundled **Scalar** reference (which now follows `openapi.path` and works without `openapi.spec`).
- **Safety limits** (`limits:`) shared by GraphQL and REST and enforced on the plan: page size (default and maximum), depth, cost, bulk rows, aggregate groups, request body size, and `statement_timeout`. GraphQL previously had none: `first` above `limits.page_size.max` is now an error and plain lists are cut at that size.
- **Pinned schemas**: `graphpg schema export [--check]`, `graphpg export-openapi`, `export.openapi`, and `export.pin` (`fail` in prod, `warn` in dev) compare the exported GraphQL SDL, subgraph and OpenAPI files with the database at startup.
- Fixed: REST rows returned composite, range and multirange columns as empty objects; the OpenAPI document described `bigint` / `numeric` as numbers (they are strings), `json` as objects, `bytea` as binary and enums by their database labels.
- Docs: a [comparison with PostgREST](docs/comparison.md#compared-with-postgrest), the REST API guide, safety limits and pinned schemas in the configuration guide.

## 1.0.1 - 2026-10-04

First public release.

- GraphQL generated from PostgreSQL catalogs: relations, Relay connections with keyset pagination, filters, aggregates, search, nested writes and typed constraint errors.
- OAuth 2.0 bearer tokens with Row-Level Security, schema pruning from `GRANT`s, Apollo Federation v2 subgraph mode, `@defer` / `@stream`.
- Container image on the GitHub Container Registry: a static binary on `distroless/static`, running as a non-root user, signed with Sigstore. Tagged `X.Y.Z`, `X.Y`, `X`, `latest` for releases, and `sha-<short hash>` and `edge` for every commit on `main`.
- Graceful shutdown on SIGTERM.
