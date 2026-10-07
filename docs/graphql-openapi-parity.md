# GraphQL and OpenAPI: what each one knows

graphpg reads the database once (tables, keys, indexes, constraints, types, functions, comments, grants) and builds two APIs from it. This page lists what each API does with that knowledge, what only one of them does, and whether the other could. It is a working inventory, written from the code and the tests of `main` at the time of writing (October 2026), not a promise.

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

## 2. In GraphQL, not (fully) in REST / OpenAPI

| Capability (what graphpg detects) | GraphQL | REST / OpenAPI today | Port |
| :--- | :--- | :--- | :--- |
| **Hierarchies**: a self-referencing foreign key or an `ltree` column → `ancestors` / `descendants` (`WITH RECURSIVE … CYCLE`, `maxDepth`, `filter`, `orderBy`) | ✅ | ❌ (not embeddable) | **Medium.** Make the tree field an embeddable like the computed relationships already are; `depth[rel]` for `maxDepth`; OpenAPI parameters per relation. |
| **Exclusive arcs** (`CHECK (num_nonnulls(…) = 1)`) → `target` union | ✅ | ❌ | **Medium.** Embed `target` with `__typename` and the columns of each target; `oneOf` + `discriminator` in the document (as CTI already is). |
| **Temporal keys** (`WITHOUT OVERLAPS`, `PERIOD` foreign keys) → `asOf`, `history` | ✅ | ❌ (`asOf` is an unknown parameter) | **Medium.** `asOf` as a list/item parameter and per embedded relation (`asOf[rel]`); `history` as an embeddable relation. |
| **Window values** per row: `rowNumber`, `rank`, `denseRank`, `percentRank`, `runningSum` (with `partitionBy`) | ✅ (on connection edges) | ❌ | **Medium.** Computed members of the row (`window[runningSum]=total`); the compiler already has them. |
| **Aggregates with their own `filter`**, several `groupedAggregates` side by side, in one scan | ✅ | ◐ one filter, one grouping per request | **Medium.** Named aggregate sets in the body of `/aggregate`. |
| **Several root fields in one request**, mutations in one transaction (`createAuthor` then `createBook(authorId)`) | ✅ | ❌ one resource per request (nested writes cover the common case) | **Hard.** A batch endpoint with references between steps (JSON:API "atomic operations" style) is a design of its own. |
| **Computed columns / mounted functions with arguments** (`greeting(greeting: "Hi")`, `ranked(first: 3)`) | ✅ | ◐ only those that need no argument | **Medium.** `args[field][name]=value`; typed per field in the document. |
| **Access by any unique key**: `userByUsername`, `updateUserByUsername`, `deleteUserByUsername` | ✅ | ◐ the primary key in the path; a unique key through `filter` (a list, no 404) and a collection write | **Easy.** `GET/PATCH/DELETE /{resource}/by/{key}/{value}` onto the existing `…By<Key>` fields. |
| **Ordering by a relation's count** (`MESSAGES_COUNT`) or a **computed column** | ✅ | ◐ works as `order=messages_count` (snake case; `messagesCount` is a 400) and is **not in the document's `order` pattern** | **Easy.** Accept the camel-case names; list them in the OpenAPI `order` parameter. |
| **Full-text `highlight`** (`ts_headline`) | ✅ | ❌ | **Easy.** A `highlight` member in the `full-text` search body. |
| **JSON `path` argument** (`meta(path: "$.a.b")`, `jsonb_path_query_first`) | ✅ | ❌ (whole value only) | **Easy–medium.** `fields[meta]=$.a.b`-style selector, or a `path` option per JSON column. |
| **`@defer` / `@stream`** (incremental delivery) | ✅ | ❌ | **Medium.** `Accept: application/x-ndjson` for lists streams rows; no equivalent of deferring a sub-tree. `/export` already streams. |
| **Relay `node(id)`** and global IDs on every row | ✅ | ◐ rows carry the real key; global IDs are accepted as keys | **Easy** (`GET /rest/node/{id}`), little value for REST. |
| **Per-row cursors** (`edges { cursor }`) | ✅ | ◐ page cursors only (`X-Next-Cursor`, `Link`) | **Easy** if wanted (a `_cursor` member on request); low value. |
| **Error-code enums per table** (`<Type>ErrorCode`, each code documented) | ✅ | ◐ problems carry the same codes, but the document says only "a typed `code`" | **Easy.** Enumerate `code` per write operation (409 / 422 responses), with descriptions. |
| **Apollo Federation** (`_entities`, `@key`, subgraph SDL) | ✅ | ❌ | **No.** A GraphQL composition protocol. |
| **Variables, fragments, aliases**: the client shapes the response | ✅ | ◐ `fields` / `include` / controls shape it; no aliases or renames | **No** for fragments; renames (`fields=name:title`) are easy if wanted. |
| **Debug extensions** (`debug.sql`, `debug.explain`) | ✅ (dev only) | ❌ | **Easy**, dev only: a header or a problem member. |
| **Planner estimate as a mode** (`totalCount(estimate: true)`) | ✅ | ✅ `Prefer: count=planned` / `estimated` (more than GraphQL, see §3) | — |

### The OpenAPI document knows less than the REST API accepts

These work today and are missing from the generated document, so SDKs and the Scalar reference do not show them. All are **easy**: the generator already has the catalog.

| Accepted by the API | In the document |
| :--- | :--- |
| Spatial (`within`, `withinDistance`, `intersects`, `intersectsBox`), `ltree` (`ancestorOf`, `descendantOf`), full-text (`matchesText`), multirange (`containsRange`) filter operators | ❌ the `{Table}Filter` schema lists only part of the operators |
| `order` by a relation count, a to-one relation's column (`author.username`), a computed column | ❌ the `order` pattern lists the table's own columns |
| `/export` and `/import/{type}` (streaming CSV in and out) | ❌ |
| The GraphQL endpoint itself, `/graphql` | ❌ (by design: the document is REST's; a link to the SDL would do) |
| Row schema of an embedded relation (`include=posts` adds `posts` to the row) | ❌ rows are described without their embedded relations |

## 3. In REST / OpenAPI, not in GraphQL

| Capability | REST / OpenAPI | GraphQL today | Port |
| :--- | :--- | :--- | :--- |
| **Constraints as machine-readable keywords**: `CHECK` → `minimum`, `maximum`, `multipleOf`, `pattern`, `minLength`, `maxItems`; `varchar(n)` → `maxLength` | ✅ JSON Schema keywords on rows and bodies | ◐ in field descriptions, and enforced before SQL (`…_TOO_LONG`, `…_OUT_OF_RANGE`) | **Medium.** Custom directives (`@constraint(min:, pattern:)`) on input fields; codegen tools can read directives. |
| **Hypermedia `links`** from foreign keys (`get_<table>_by_key`, `search_<table>`) | ✅ | ❌ | **No.** A graph already links by its structure. |
| **`webhooks`**: change events of a table, payload typed from the row (`orders.insert`) | ✅ (declared with `entities.<table>.events`) | ❌ | **Medium**, once there is delivery: the same `old` / `new` types as SDL types, for subscriptions. Nothing delivers events yet. |
| **HTTP semantics**: conditional `GET` (`304`), `Range` / `Content-Range` / `206` / `416`, `Link` headers, `HEAD`, `OPTIONS` with `Allow` / `Accept-Patch` | ✅ | ❌ (`POST` only) | **No.** Needs `GET` + persisted queries; not planned. |
| **Content negotiation**: CSV in and out, `multipart/form-data` for rows with files, raw `bytea` streaming (`/{key}/{column}`) | ✅ | ◐ `/export` and `/import` stream CSV; `bytea` is base64 | **Hard** for GraphQL multipart uploads; `/export` / `/import` already cover bulk. |
| **Count modes**: `planned` and `estimated` (exact below `limits.count_threshold`, planner above) | ✅ | ◐ `totalCount(estimate: true)` only | **Easy.** `estimate: ESTIMATED` as an enum next to the boolean. |
| **Patch formats**: `merge-patch+json`, RFC 6902 `json-patch+json` for one row | ✅ | ❌ (update inputs are partial by nature) | **No.** |
| **Writes by filter with guards**: `FILTER_REQUIRED`, `unfiltered=true`, `X-Affected-Count` | ✅ | ◐ `update<Types>(where:)` has the same row bound (`limits.bulk_rows`) but no "filter required" | **Easy.** `where: {}` is allowed today; refuse it unless an argument says so. |
| **`missing=default|null`** for bulk writes | ✅ | ◐ absent and `null` are different in an input object, so it is expressible | — |
| **RFC 9457 problems**: `type`, `title`, `status`, `instance`, `errors.map`, `PTxxx` SQLSTATE → status | ✅ | ◐ the mapped `code` and message only; GraphQL answers `200` | **No** for the status; the rest is shared (`sql_errors.rs`). |
| **Path per schema** (`/rest/{schema}/…`, fixed paths in the document) | ✅ | ◐ names are prefixed only when they clash | **No.** One GraphQL schema is one endpoint; a second endpoint per schema is possible but unasked. |
| **Operation ids, per-operation request / response schemas** for SDK generation | ✅ | — (codegen reads the SDL) | — |

## 4. Where the work is

Ordered by value for the effort, assuming REST keeps being the second API of the same engine:

1. **Make the document complete (all easy):** the operators, `order` options, `/export` / `/import`, error-code enums, embedded relations in row schemas. Nothing changes at runtime; every SDK gets better.
2. **Close small grammar gaps (easy):** access by unique key, `highlight` in full-text search, camel-case names for relation-count and computed ordering, `estimate: ESTIMATED` in GraphQL, a required filter for GraphQL writes by `where`.
3. **Embed what GraphQL already walks (medium):** hierarchies, exclusive arcs, temporal keys. They are plan nodes the compiler already has; REST needs the parameter and the document.
4. **Per-row analytics (medium):** window values, named aggregate sets, computed fields with arguments.
5. **A batch / transaction endpoint (hard):** the only GraphQL capability that changes what a client can do in one round trip; decide it as its own story.
6. **Not worth porting:** Federation, fragments, hypermedia links, HTTP caching semantics, patch formats.

The reverse direction is shorter: the REST API's extras are mostly *delivery* (HTTP semantics, content types, SDK-friendly schemas), and the one with substance for GraphQL is the **constraint keywords**, as directives.
