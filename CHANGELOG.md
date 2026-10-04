# Changelog

Releases are tagged `vX.Y.Z`; each one has generated notes on the
[Releases page](https://github.com/mihailtd/graphpg/releases). This file records the highlights.

## 1.0.1 - 2026-10-04

First public release.

- GraphQL generated from PostgreSQL catalogs: relations, Relay connections with keyset pagination, filters, aggregates, search, nested writes and typed constraint errors.
- OAuth 2.0 bearer tokens with Row-Level Security, schema pruning from `GRANT`s, Apollo Federation v2 subgraph mode, `@defer` / `@stream`.
- Container image on the GitHub Container Registry: a static binary on `distroless/static`, running as a non-root user, signed with Sigstore. Tagged `X.Y.Z`, `X.Y`, `X`, `latest` for releases, and `sha-<short hash>` and `edge` for every commit on `main`.
- Graceful shutdown on SIGTERM.
