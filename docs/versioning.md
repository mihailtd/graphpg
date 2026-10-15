# Versioning

graphpg follows [semantic versioning](https://semver.org) from **1.1.0**, the first published release. Releases are tagged `vX.Y.Z`; each publishes the container image (`ghcr.io/mihailtd/graphpg:X.Y.Z`, `:X.Y`, `:X`, `:latest`) and a GitHub Release. The [changelog](../CHANGELOG.md) lists what each one changes.

- **Patch** (`1.1.x`): fixes. Nothing that worked as documented stops working.
- **Minor** (`1.x.0`): additions. New fields, arguments, routes, error codes, config keys and features, with defaults that leave existing deployments as they were.
- **Major** (`x.0.0`): anything that can break a client, a deployment or a database that worked with the previous version.

## What the version covers

For the same database and the same `graphpg.yaml`, a minor or patch upgrade serves the same API:

- **The GraphQL schema**: type, field, argument, enum value and input names (and the rules deriving them from database names: singulars and plurals, camelCase, clash resolution), nullability, and the root fields each table, view and function gets.
- **The REST API**: routes, query parameters, headers, status codes, the shape of bodies and of the OpenAPI document.
- **Errors**: `extensions.code` values (`BAD_USER_INPUT`, `SCHEMA_CHANGED`, `<Type>ErrorCode` values, ...) and the HTTP status of each.
- **Configuration**: `graphpg.yaml` keys, their meaning and defaults; command-line commands and flags; environment variables.
- **Behaviour the database relies on**: the session settings authenticated requests set (`role`, `request.jwt.claims`, the prefix and mappings), the transaction each request runs in, and what graphpg creates in the database (only in dev mode: its DDL event trigger).
- **Telemetry names**: metric names and attributes, and span names, which dashboards and alerts depend on.

New things may appear in a minor version: a new root field, argument, enum value or error code. Clients must tolerate them (an unknown enum value or error code, a new field in a REST body), as GraphQL and HTTP clients generally do.

## What it does not cover

- **The SQL graphpg generates**, its plans and performance: they improve in any release.
- **Log messages**, the text of error messages (the codes are covered), and the content of dev-mode-only output (`X-Debug-SQL`, `extensions.sql`).
- **The Rust crate's API** (`graphpg` as a library): it is the server's implementation, not a supported interface.
- **Behaviour that was a vulnerability**: a security fix may refuse what a request could do before (for example the global ID of another type in a key argument), in a patch release.
- **A database that changed**: renaming a table renames its type. Pin the API with `export.pin` (see [configuration](configuration.md)) to make such a change fail at startup instead of reaching clients.

## Deprecation

Something the next major version removes or changes is deprecated first, in a minor version: marked `@deprecated` in the GraphQL schema and `deprecated: true` in the OpenAPI document, logged as a warning at startup when a configuration uses it, and listed in the changelog. It keeps working until the next major version.

## Supported PostgreSQL

The test suite runs against PostgreSQL 18. Dropping a PostgreSQL version is a major change.
