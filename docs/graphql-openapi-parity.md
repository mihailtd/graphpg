# GraphQL and OpenAPI: what each one knows

graphpg reads the database once (tables, keys, indexes, constraints, types, functions, comments, grants) and builds two APIs from it. This page lists what each API does with that knowledge, what only one of them does, and whether the other could. It is a working inventory, written from the code and the tests of `main` at the time of writing (October 2026), not a promise. Everything easy or medium that the first version of this page listed as missing on one side has been ported; what is left is what a protocol cannot, or should not, do.

**Legend.** ✅ done · ◐ partly · ❌ not available. **Port** says whether the missing side could get it: *easy* (an afternoon, the machinery exists), *medium* (days: new grammar, planner or compiler work), *hard* (a design of its own), *no* (it does not make sense in that protocol).

The OpenAPI side is the REST API (`/rest`) and its generated OpenAPI 3.1 document; the REST API is planned onto the same engine as GraphQL, so most gaps are in the grammar and in the document, not in the engine.

## 1. Detected from the database, in both

| What graphpg detects | GraphQL | REST / OpenAPI |
| :--- | :--- | :--- |
| Tables, views, materialized views, foreign tables | types, root fields, `entities:` config | resources, same config |
| Primary key, composite keys | `id`, `databaseId`, one argument per key column | `/{key}` (comma-separated) |
| Foreign keys → relations (both directions, many-to-many through junctions) | fields, connections, filters | `include`, filters, controls per relation |
| Column types: enums, composites, ranges, multiranges, arrays, domains, `ltree`, `vector`, PostGIS | typed scalars and inputs | JSON Schema per type (`docs/schema-mapping.md#rest-and-openapi`) |
| Generated columns | read-only fields, left out of inputs | read-only, left out of write bodies |
| Comments, `@deprecated` | descriptions, `@deprecated` | descriptions, `deprecated: true` |
| `pg_trgm`, `pgvector`, BM25 (ParadeDB), `tsvector`, PostGIS indexes → search | `<list>FuzzySearch`, `SemanticSearch`, `Bm25Search`, `HybridSearch`, `FullTextSearch`, `NearestSearch`, `globalSearch` | `POST /{resource}/search/{method}`, `/rest/search`; one endpoint per method the indexes support |
| Single Table Inheritance (discriminator, `CHECK`/enum) | interface + concrete types | `oneOf` + `discriminator` |
| Table-per-type (CTI: `INHERITS`, shared primary keys) | interface + concrete types | `__typename` + subtype columns; `oneOf` + `discriminator`; subtype columns in `filter` / `order` |
| Row-Level Security, grants, `rbac` pruning | role-pruned schema | role-pruned document and endpoints |
| Row version (`xmin` or a version column) | `rowVersion` + `expectedVersion` | `ETag` + `If-Match` / `If-None-Match` |
| Unique keys → upsert | `onConflict` | `PUT /{resource}` with `conflict` / `resolution` / `update` |
| `RETURNING OLD` (PostgreSQL 18) | `previous<Type>` | `Prefer: previous` |
| Constraints → typed errors (`EMAIL_TAKEN`, `…_NOT_FOUND`, `STALE_VERSION`) | `extensions.code` + `fields` | `code` + `fields` of the problem |
| Aggregates, grouped aggregates, `having`, time truncation and bins, `fillGaps` | `aggregates`, `groupedAggregates` | `/{resource}/aggregate` |
| Nested writes (connect / create / disconnect / set / sync / update / delete) | input types | the same input types |
| Filters: columns, relations (`some` / `every` / `none` / `count` / aggregates), `and` / `or` / `not`, `regex`, `any` / `all`, JSON, arrays, ranges, spatial | `<Type>Filter` | `filter[…]` grammar, and `POST …/search` bodies |
| Functions: root fields, computed columns | root fields; fields on the row type | `/rpc/{function}`; arguments-free ones through `include` / `fields` |
| Safety limits (page size, depth, cost, bulk rows, groups, body size, timeout) | errors with `code`, `limit`, `actual` | the same, as problems |
| Several schemas | `Schema`-prefixed names when they clash | `/rest/{schema}/…` paths |
| Introspection | SDL, `__schema`, subgraph SDL | the OpenAPI document (`/openapi.json`) |
| Hierarchies: a foreign key that references its own table, an `ltree` column | `ancestors` / `descendants` (`maxDepth`, `filter`, `orderBy`) | `include=ancestors`, with `depth[…]`, `filter[…]`, `order[…]` |
| Exclusive arcs (`num_nonnulls(…) = 1`) | `target` union | `include=target` (`__typename` + the columns of each table), `oneOf` in the document |
| Temporal keys (`WITHOUT OVERLAPS`, `PERIOD` foreign keys) | `asOf`, `history` | `asOf`, `asOf[rel]`, `include=history` |
| Window values: `rowNumber`, `rank`, `denseRank`, `percentRank`, `runningSum`, per-row cursors | on connection edges | `window`, `running`, `partition`, `cursors` → `_window`, `_cursor` on rows |
| Aggregates with their own filter, several groupings in one scan | `aggregates(filter:)` and aliases | `sets` in the body of `/aggregate` |
| Computed columns and mounted functions with arguments | field arguments | `args[field][argument]` |
| Access by any unique key | `userByUsername`, `updateUserByUsername`, `deleteUserByUsername` | `?by=username` on `GET` / `PATCH` / `DELETE` of a row |
| Full-text `highlight`, `distance` of nearest hits | on the hit | `highlight` in the body, `distance` in the hit |
| JSON `path` argument | `meta(path: "$.a.b")` | `path[meta]=$.a.b` |
| Ordering by a relation's count, a to-one relation's column, a computed column | `MESSAGES_COUNT_ASC`, `AUTHOR_NAME_ASC` | `order=messagesCount`, `author.username`, listed in the document |
| Relay `node(id:)` | `node` | `GET /rest/node/{id}` (a redirect to the row) |
| Per-table error codes | `<Type>ErrorCode` enums | `<table>ErrorCode` and `<table>Problem` components on the 400 / 409 / 422 responses |
| Streaming bulk data | `/export`, `/import/{Type}` | the same endpoints, described in the document; NDJSON and CSV lists |
| Debug output (dev) | `extensions.sql`, `extensions.explain` | `X-Debug-SQL`, `X-Debug-Explain` headers |
| Constraint keywords: `CHECK`, `varchar(n)` | `@constraint(…)` on fields and inputs | JSON Schema keywords on rows and bodies |
| Estimated totals | `totalCount(estimate: true, exactBelow: n)` | `Prefer: count=planned` / `estimated` |
| Writes by filter | `where` bounded by `limits.bulk_rows`, `unfiltered` with `limits.require_filter` | `PATCH` / `DELETE` of a collection, `FILTER_REQUIRED`, `unfiltered=true`, the same bound |

## 2. In GraphQL, not in REST / OpenAPI

| Capability | GraphQL | REST / OpenAPI today | Port |
| :--- | :--- | :--- | :--- |
| **Several root fields in one request**, mutations in one transaction (`createAuthor` then `createBook(authorId)`) | ✅ | ❌ one resource per request; nested writes cover the common case | **Hard.** A batch endpoint with references between steps (JSON:API "atomic operations" style) is a design of its own: the one thing left that changes what a client can do in one round trip |
| **`@defer` / `@stream`**: parts of an answer later | ✅ (`multipart/mixed`) | ◐ lists stream as NDJSON and `/export` streams whole tables; no deferring of a sub-tree | **No** for deferring; the rest is there |
| **Apollo Federation** (`_entities`, `@key`, subgraph SDL) | ✅ | ❌ | **No.** A GraphQL composition protocol |
| **Variables, fragments, aliases**: the client shapes the response | ✅ | ◐ `fields`, `include` and their controls shape it; no aliases or renames | **No** for fragments; renames (`fields=name:title`) would be easy if wanted |
| **Per-row `id` as a global ID** on every row | ✅ | ◐ rows carry the real key; global IDs are accepted as keys and by `/rest/node/{id}` | **No**: a REST client has the URL |

## 3. In REST / OpenAPI, not in GraphQL

| Capability | REST / OpenAPI | GraphQL today | Port |
| :--- | :--- | :--- | :--- |
| **`webhooks`**: change events of a table, payload typed from the row (`orders.insert`) | ✅ (declared with `entities.<table>.events`) | ❌ | **Later.** The same `old` / `new` types as SDL types, for subscriptions, once there is delivery and there are subscriptions |
| **Hypermedia `links`** from foreign keys | ✅ | ❌ | **No.** A graph already links by its structure |
| **HTTP semantics**: conditional `GET` (`304`), `Range` / `Content-Range` / `206` / `416`, `Link` headers, `HEAD`, `OPTIONS` | ✅ | ❌ (`POST` only) | **No.** Needs `GET` + persisted queries; not planned |
| **Content negotiation**: CSV in and out, `multipart/form-data` for rows with files, raw `bytea` streaming (`/{key}/{column}`) | ✅ | ◐ `/export` and `/import` stream CSV and NDJSON; `bytea` is base64 | **Hard** for GraphQL multipart uploads; bulk is covered |
| **Patch formats**: `merge-patch+json`, RFC 6902 `json-patch+json` for one row | ✅ | ❌ (update inputs are partial by nature) | **No** |
| **RFC 9457 problems**: `type`, `title`, `status`, `instance`; HTTP status of `errors.map` and `PTxxx` | ✅ | ◐ the mapped `code` and message; GraphQL answers `200` | **No** for the status |
| **Path per schema** (`/rest/{schema}/…`) | ✅ | ◐ names are prefixed only when they clash | **No.** One GraphQL schema is one endpoint |
| **Operation ids and per-operation schemas** for SDK generation | ✅ | — (codegen reads the SDL) | — |

## 4. What is left

- **The batch / transaction endpoint** is the only capability of GraphQL that REST lacks and could have; it is a story of its own.
- **Webhooks for GraphQL** wait for event delivery and subscriptions.
- Everything else on the two lists above is a difference of protocol, kept on purpose.

When a database feature is added, the question is the same as before: what does it become in the SDL, and what does it become in the OpenAPI document? This page is where the answer goes.
