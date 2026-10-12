# GraphQL API

What the generated schema lets clients do. For how database objects map to types, see [schema mapping](schema-mapping.md).

## Variables and fragments

Named fragments, inline fragments and variables are supported at every level.

- **Fragments** (`...Name`, `... on Type`) are flattened into the same query: spreads on `Query`, `Mutation`, the `Node` interface and concrete types compile into the lateral joins of one SQL statement, identical fields across spreads are merged, and cycles are rejected.
- **Variables** become SQL parameters wherever they appear: lookup keys (`customer(id: $id)`), pagination (`first`, `after`, `offset`), `condition` and `filter` objects, `orderBy`, and mutation `input`s. An omitted variable takes the default declared in the operation (`query Q($limit: Int = 10)`).

## Pagination and `orderBy`

Every table gets a `<Type>OrderBy` enum used by `orderBy` on every connection, root-level and nested:

```graphql
enum CustomerOrderBy {
  NATURAL            # same as PRIMARY_KEY_ASC
  PRIMARY_KEY_ASC
  PRIMARY_KEY_DESC
  NAME_ASC
  NAME_DESC
  CREATED_AT_ASC
  CREATED_AT_DESC    # one ASC/DESC pair per column
}

type Query {
  customersConnection(first: Int, after: String, last: Int, before: String, offset: Int,
                      condition: CustomerCondition, filter: CustomerFilter,
                      orderBy: [CustomerOrderBy!]): CustomerConnection!
}
```

`orderBy: [NAME_ASC, PRIMARY_KEY_ASC]` becomes `ORDER BY c0."name" ASC, c0."id" ASC`; the planner resolves enum values to SQL before execution. It works with variables (`$ord: [CustomerOrderBy!]`).

Connections page by **keyset**. The `orderBy` columns are followed by the primary-key columns they leave out, which makes the order total. A cursor holds the values of all of them, and `after`/`before` select the rows past it, so no row is skipped or repeated even when sort values tie, and every page is an index seek, never an `OFFSET` scan.

| Case | Page condition (`after`) |
| :--- | :--- |
| One direction, no NULLs | `ROW(c0."name", c0."id") > ROW($1, $2)` |
| Mixed directions (`[NAME_DESC]` + key `ASC`) | `c0."name" <= $1 AND (c0."name" < $1 OR (c0."name" = $1 AND c0."id" > $2))` |
| Nullable column | NULLs sort as the largest value (PostgreSQL's default: last ascending, first descending), spelled out as `IS NULL` / `IS NOT NULL` terms |

- `first`/`after` reads forward. `last` (without `first`) reads backward: the SQL order is reversed, from the end or from `before`, and the page is flipped back into order. `hasPreviousPage` comes from the extra row fetched past the page.
- `first` and `last` together return the last `last` rows of the first `first` (Relay). `offset` skips rows past the cursor, or past the start, in the direction being read; it is an `OFFSET` scan, so deep offsets cost what they skip.
- A cursor is only valid for the `orderBy` it was issued under; a cursor of the wrong shape is rejected.

### Totals: exact, estimated, or both

`totalCount` is exact by default. `totalCount(estimate: true)` is the planner's estimate (table statistics, or `EXPLAIN` for filtered rows): cheap on a large table. `totalCount(estimate: true, exactBelow: 10000)` counts exactly when the estimate is at most that many rows and answers the estimate above, so a small result has its exact total and a huge one costs no scan (the REST `Prefer: count=estimated`, with `limits.count_threshold`).

### Aggregates and grouped aggregates

Every connection has `aggregates` (`min`/`max` of orderable columns, `sum`/`avg` of numeric ones) and `groupedAggregates`, over the rows its `condition` and `filter` select (before paging):

```graphql
ordersConnection(filter: { status: { notEqualTo: CANCELLED } }) {
  totalCount
  groupedAggregates(groupBy: [STATUS, CREATED_AT_TRUNCATED_TO_MONTH], having: { sum: { total: { greaterThan: 100 } } }) {
    keys          # ["PAID", "2026-09-01T00:00:00+00:00"]
    count
    sum { total }
    max { createdAt }
  }
}
```

- `groupBy` (`<Type>GroupBy`) takes any orderable column, and dates and timestamps truncated to `HOUR`, `DAY`, `WEEK`, `MONTH`, `QUARTER` or `YEAR` (`<COL>_TRUNCATED_TO_<UNIT>`; no hours for dates; truncation follows the session's time zone).
- `keys` holds the group's values in `groupBy` order, as their GraphQL values (enum values, `BigInt`/`Decimal` as strings, truncated dates and timestamps as ISO strings). Groups are ordered by their keys.
- `having` (`<Type>HavingInput`) keeps the groups whose `count` (`IntFilter`) and `sum`/`avg`/`min`/`max` of numeric columns (`FloatFilter`) match, all conditions together.
- Several `groupedAggregates` fields (with aliases), `totalCount` and `aggregates` are computed in one scan: `GROUP BY GROUPING SETS`, one set per grouping plus the empty set for the totals.

### Limits

Page sizes and the cost of an operation are bounded by the deployment's [`limits:`](configuration.md#safety-limits). A connection without `first` / `last` reads `limits.page_size.default` rows (100); `first` / `last` above `limits.page_size.max` (1000) is an error, and so are operations nested deeper than `limits.depth` or estimated to read more than `limits.cost` rows. A request over a limit fails before it runs, with `extensions: { code, limit, actual }`:

```json
{ "errors": [{ "message": "`first` is 5000, more than the maximum page size of 1000",
               "extensions": { "code": "PAGE_SIZE_EXCEEDED", "limit": 1000, "actual": 5000 } }] }
```

Plain list fields (`forums: [Forum!]!`) and to-many relations (`messages: [Message!]!`) have no page size: they return at most `limits.page_size.max` rows. Page through the `…Connection` fields to read more. `createForums(inputs:)` accepts at most `limits.bulk_rows` rows, nested ones included. A statement running longer than `limits.statement_timeout` is cancelled (`STATEMENT_TIMEOUT`). `/export` streams a whole connection and is not limited.

### Rate limits

With [`limits.rate_limit`](configuration.md#rate-limits) on, a client over its rate is answered `429` before the operation is parsed, with `Retry-After` and:

```json
{ "errors": [{ "message": "too many requests: retry in 12 s",
               "extensions": { "code": "RATE_LIMITED", "retryAfter": 12 } }] }
```

Every answer carries `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset`.

## Persisted queries

`graphql.persisted_queries` lets clients send an operation by reference instead of by text, and lets production run only known operations. It is off by default.

```yaml
graphql:
  persisted_queries:
    enabled: true
    manifest: persisted-operations.json
prod:
  graphql:
    persisted_queries: { allow_list: true }
```

- **Automatic persisted queries** (Apollo's protocol, `automatic: true`): the client sends `extensions: { persistedQuery: { version: 1, sha256Hash } }` with the SHA-256 of the text, in hex. An unknown hash is answered `PERSISTED_QUERY_NOT_FOUND` (message `PersistedQueryNotFound`, HTTP 200); the client sends the hash with the text, which registers it in this instance's memory, and the hash alone from then on. A hash that is not the text's is `PERSISTED_QUERY_HASH_MISMATCH` (400). Apollo Client's `createPersistedQueryLink` speaks this protocol.
- **A manifest** of known operations, read at startup: an Apollo persisted query manifest (`{ "format": "apollo-persisted-query-manifest", "version": 1, "operations": [{ "id", "name", "type", "body" }] }`, from `generate-persisted-query-manifest`), or a map of ids to texts (`{ "<id>": "<text>" }`, as GraphQL Code Generator's persisted documents and Relay write it). A client names one by `documentId`, or by the SHA-256 of its text. Every operation is validated against the schema at startup: prod refuses to start when one does not validate (a column it reads was dropped), dev logs it, and the schema watcher logs it after a reload.
- **The allow-list** (`allow_list: true`, needs a manifest): only the manifest's operations run, by id, by hash, or sent as text whose hash is listed (so clients that send the text keep working). Anything else is `OPERATION_NOT_ALLOWED` (403) before it is parsed, and nothing can be registered. `/export` queries must be listed too. Introspection is refused unless an introspection query is in the manifest, so leave the allow-list off where GraphiQL is used.
- **Off**, a request naming a persisted query (by hash or `documentId`) is answered `PERSISTED_QUERY_NOT_SUPPORTED` (message `PersistedQueryNotSupported`); Apollo clients then send the text alone.

A persisted query is planned like any other: the document and compiled-operation caches are keyed by its text. `GET /graphql?extensions=…&variables=…` runs a query (never a mutation: `405`) with a URL short enough for a CDN to cache.

## Constraints as `@constraint`

What the database enforces on a value is stated on the field, in the keywords of JSON Schema, the same ones the [OpenAPI document](configuration.md#the-openapi-document-and-uis) uses: a single-column `CHECK` and a `varchar(n)` length.

```graphql
directive @constraint(minimum: Float, maximum: Float, exclusiveMinimum: Float, exclusiveMaximum: Float,
                      multipleOf: Float, minLength: Int, maxLength: Int, pattern: String,
                      minItems: Int, maxItems: Int) on FIELD_DEFINITION | INPUT_FIELD_DEFINITION

type Bounded { score: Int @constraint(minimum: 0, maximum: 100)  title: String @constraint(minLength: 3, maxLength: 20) }
input CreateBoundedInput { score: Int @constraint(minimum: 0, maximum: 100) … }
```

It is documentation for clients and code generators (a form can check before it sends); the database checks every write and the typed errors (`SCORE_INVALID`, `TITLE_TOO_LONG`) are unchanged. Only the simple forms of `CHECK` are read, as in the document.

## Global object identification (Relay)

Every table with a single-column primary key implements `Node`:

```graphql
interface Node { id: ID! }

type Customer implements Node {
  id: ID!           # Relay global ID: base64("Customer:<primary key>")
  databaseId: UUID! # the raw primary key
  name: String
}

type Query { node(id: ID!): Node }
```

- **`id`** is the stable client-cache key and the argument of `node(id:)`; **`databaseId`** is the raw database value.
- `node(id:)` decodes the ID in memory, finds the table, and compiles one primary-key lookup (`WHERE id = $1 LIMIT 1`); there is no `UNION ALL` across tables.
  ```graphql
  query ($id: ID!) { node(id: $id) { __typename id ... on Customer { name } ... on Order { total } } }
  ```
- **Both identifier forms are accepted** everywhere an ID goes in: `customer(id:)`, `updateCustomer(id:)`, `createOrder(input: { customerId: ... })` take either a global ID or the raw key.
- A malformed ID, malformed key or missing row gives `{ "node": null }`, never a database error. PostgreSQL's `encode(..., 'base64')` wraps lines every 76 characters; the engine strips the newlines, so IDs are always single-line.

## Relational expressiveness

- **To-many filters:** `forums(filter: { messages: { some: {...}, every: {...}, none: {...}, count: { greaterThan: 5 }, sum: { score: { lessThan: 10 } } } })`. `every` treats a NULL condition as not matching. Many-to-many relations filter through their junction table.
- **Ordering** by more than the row's own columns, with keyset pagination (the values go into cursors): a to-one relation's columns (`AUTHOR_NAME_ASC`; rows without one last ascending), a to-many relation's row count (`MESSAGES_COUNT_DESC`), and computed columns returning an orderable value (`POST_TOTAL_DESC`).
- **Hierarchies:** a foreign key referencing its own table gives `ancestors` and `descendants` (the first one; others are prefixed with their relation's name, `mentorAncestors`): a `WITH RECURSIVE` walk with PostgreSQL's `CYCLE` clause, so cycles end it, nearest rows first, with `maxDepth`, `filter` and `orderBy`. An `ltree` column gives `<field>Ancestors` / `<field>Descendants` by path prefix, and `LTreeFilter` (`ancestorOf`, `descendantOf`, `matches` an `lquery`, `matchesText` an `ltxtquery`). Hierarchies are derived from the catalog, never from names like `parent_id`.
- **Polymorphism:** single-table inheritance (a discriminator column), class-table inheritance (`INHERITS` or shared primary keys) and exclusive arcs become interfaces and unions.

## Mutations

Root mutation fields run **in order**, each as its own statement, inside one transaction (a savepoint when the caller already has a transaction open on the connection):
- A later field sees what earlier fields wrote (`createAuthor` then `createBook(input: { authorId })` in one request).
- Inserts and updates return the row's key; the payload is then read by key in a separate statement, so nested relations reflect the write (`createBook { book { author { books { title } } } }` includes the new book).
- If any field fails, the whole operation is rolled back and the error names the failing field.
- Rows are addressed by their key: `id: ID!` (Relay ID or raw value) for a single-column key, one argument per column for a composite key. Relations without a key get no update/delete mutations.

### Writes by filter

`update<Types>(where:, set:)` and `delete<Types>(where:)` affect at most `limits.bulk_rows` rows (1000 by default): if more match, the operation is rolled back with `BULK_LIMIT_EXCEEDED`, whatever the payload selects. With `limits.require_filter: true` a `where` without a condition (`where: {}`, every row) is refused unless the field also says `unfiltered: true`.

### Nested writes

Create and update inputs carry the row's relations, detected from its foreign keys (and many-to-many relations through junction tables). One mutation field writes the whole graph, each level after the rows it takes keys from, in the field's transaction:

| Relation | `Create<Type>Input` | `Update<Type>Input` (single-row updates) |
| :--- | :--- | :--- |
| To-one (this row holds the foreign key) | `{ connect: <key> }` or `{ create: <input> }` | also `{ disconnect: true }` (sets the foreign key to NULL) |
| To-many (children hold the foreign key) | `{ create: [...], connect: [<key>] }` (connect moves the child here) | also `disconnect: [<key>]` and `set: [<key>]` |
| Many-to-many (junction rows) | `{ create: [...], connect: [<key>] }` | also `disconnect: [<key>]` and `set: [<key>]` |

- A `<key>` (`<Type>KeyInput`) identifies a row by its primary key or any unique key.
- `disconnect` detaches: a child's foreign key becomes NULL (so it must be nullable; delete the row otherwise), a junction row is deleted. Detaching a row that is not related does nothing.
- `set` makes the related rows exactly those listed, plus any the same input creates: the others are detached first.
- Connecting a many-to-many row that is already related keeps its junction row.
- An update with nested writes needs the row to exist (else it is an error, rather than a `null` payload) and the table to have a primary key. The bulk `update<Types>(set:)` does not write relations.

### Upserts with nested writes

`create<Type>(input:, onConflict:)` may nest relations too. After the upsert, the row is looked up by its conflict key, so rows nested under it get its key whether it was inserted, updated, or left alone (`action: NOTHING`). Rows it refers to (to-one `create`) are written before the upsert, so they are created even when the conflicting row is left alone.

### More nested writes

Beyond `create` and `connect`, relation inputs take:

- **`connectOrCreate: { where, create }`** (to-one, to-many and many-to-many): the row with the key, or else a new one. One statement: `WITH _found AS (SELECT ...), _new AS (INSERT ... WHERE NOT EXISTS (SELECT 1 FROM _found) RETURNING ...)`. For a child, a found row is moved to the parent. The `create` input's own relations are not written.
- In updates, **`update: [{ where, data }]`** and **`delete: [key]`** change or delete the row's own children — only rows whose foreign key holds the row's key, else the mutation fails (`matches no ... row related to it`). Rows linked through a junction table are linked, not owned: use `disconnect` or `set` for them.
- In updates, a to-one relation's **`update: { ... }`** changes the row it refers to (the foreign key stays the same).
- In updates, **`sync: [Create<Child>Input!]`** makes the children exactly the rows listed, in one `MERGE` (PostgreSQL 17+): rows whose key matches a child update it, rows without a key are inserted, and children no row matches are deleted (`WHEN NOT MATCHED BY SOURCE AND <child of this row> THEN DELETE`). Every row sets the same fields, apart from the key.

### Errors

Constraint violations are errors of the mutation field (`path: ["createUser"]`) with the violated constraint's code, derived from the catalog with no naming convention:

| Constraint | Code | `extensions.fields` |
| :--- | :--- | :--- |
| Primary key, unique constraint or unique index (partial and expression ones too) | `<COLUMNS>_TAKEN` (`EMAIL_TAKEN`, `ORG_AND_CODE_TAKEN`) | The input path of each column (`input.email`, `inputs[1].email`) |
| Foreign key, on the referencing row | `<RELATION>_NOT_FOUND` (`AUTHOR_NOT_FOUND`) | The foreign key's columns |
| Foreign key, when a referenced row is deleted or re-keyed | `REFERENCED_BY_<RELATION>`, on the referenced table's enum | — |
| `CHECK` | Its name when chosen (`BALANCE_NON_NEGATIVE`), else `<COLUMNS>_INVALID` | Its columns |
| Exclusion | Its name when chosen (`OVERLAPPING_BOOKING`), else `<COLUMNS>_CONFLICT` | Its columns |
| `NOT NULL` | `<FIELD>_REQUIRED` | The column |
| `expectedVersion` | `STALE_VERSION` | `expectedVersion` |

Each writable table's `enum <Type>ErrorCode` lists its codes, documented. `extensions.type` names the enum a code belongs to: a nested write can fail on a child table's constraint. When one statement wrote several rows, the row is found from the key PostgreSQL quotes in the error's detail; when it can't be, `fields` holds the bare field names.

`varchar(n)`/`char(n)` lengths and `numeric(p, s)` precision are read from `atttypmod`, documented on the fields, and checked (with PostgreSQL's rounding) before any SQL runs: `<FIELD>_TOO_LONG` and `<FIELD>_OUT_OF_RANGE`, with the input path in `extensions.field`. Single-column `CHECK` expressions appear in the field's description; multi-column ones in the type's.

## Rows before a write (`RETURNING OLD`, PostgreSQL 18)

Update payloads have `previous<Type>` (`previous<Types>` for many rows, in the order of the updated rows): the rows as they were, read with `to_json(old)` in the writing statement itself. Upsert payloads have it too: null for rows the upsert inserted. Their relations are read as they are now; their `rowVersion` is null.

## Optimistic concurrency

Every type of a table with a key has `rowVersion`: the row's `xmin`, which every write of it changes, or a column a trigger maintains, named in the config (`entities.<table>.version_column`: the database cannot declare what a column means). Single-row updates and deletes take `expectedVersion`: the row is locked, and unless its version is still that one the write fails with `STALE_VERSION` (the equivalent of HTTP `If-Match`: it prevents lost updates).

## Search fields

Search is built for combo boxes and global search. A user who doesn't see the expected row types a better query rather than paging, so every keystroke must be fast. Each search method a table's indexes support is its own root field. It returns the best matches, best first, and is **not paginated**: the schema has no cursors, `last`, `offset`, `orderBy` or `totalCount` for searches.

| Index | Root field | Method arguments |
| :--- | :--- | :--- |
| `pg_trgm` | `usersFuzzySearch` | `query: String!`, `field: UserFuzzyField`, `minSimilarity: Float` (word similarity) |
| `pgvector` | `usersSemanticSearch` | `vector: [Float!]!`, `field: UserVectorField`, `minScore: Float` |
| BM25 (ParadeDB) | `usersBm25Search` | `query: String!`, `field: UserBm25Field` |
| vector + lexical | `usersHybridSearch` | `query: String!`, `vector: [Float!]!`, `lexicalWeight`, `vectorWeight`, `rrfK` |

Every search field also takes `first`, `condition` and `filter` (scoping the matches, e.g. to one forum), and returns:

```graphql
type UserSearchResults { hits: [UserSearchHit!]!  hasMore: Boolean! }
type UserSearchHit { score: Float!  node: User! }
```

- `first` defaults to 10 and may be at most 50. Both are set in `graphpg.yaml` (`search: { default_limit, max_limit }`). A larger `first` is an error.
- `hasMore` says more rows matched than were returned (the query fetches one extra row). A UI can use it to prompt "refine your search".
- `score` is the relevance, higher is better. Its scale depends on the method: word similarity for fuzzy, vector similarity for semantic, BM25 score, or fused reciprocal rank.
- Fuzzy search uses trigram **word similarity**: how well the query matches some part of the value. A partly typed `"ada lov"` matches `"Ada Lovelace"` (0.875; whole-string similarity would give 0.5), which is what a typeahead needs. A row matches when `column %> query` holds, that is, when its word similarity reaches `pg_trgm.word_similarity_threshold` (0.6 by default), or `minSimilarity` when it is given.
- Rows are ordered by the method's own operator (`<->>` word distance, the vector index's distance operator, `paradedb.score`), so an index that can return the nearest rows first does, and the scan stops after `first + 1` rows. See [performance](performance.md#database-level-tuning) for which indexes and settings do this.
- Hybrid search takes each ranking's best candidates (50, or `first + 1` if more) among the rows passing `condition`/`filter`, then fuses them.

Field enums use GraphQL names: `<Type>FuzzyField` values are the columns' `orderBy` names (`NAME`). Indexes on hidden columns are not exposed. Hybrid search fuses the rankings on the table's single-column primary key, so it is offered only for tables that have one.

## Analytics

- `aggregates` objects have `distinctCount`, `stddevSample`, `stddevPopulation`, `varianceSample`, `variancePopulation`, `median` and `percentile(fraction:)` (`percentile_cont`) besides `min`/`max`/`sum`/`avg`, and take a `filter` of their own (`FILTER (WHERE ...)`): `deposits: aggregates(filter: { kind: { equalTo: "deposit" } }) { sum { amount } }`. So do aggregate groups.
- `groupedAggregates` takes `timeZone` (truncation in that zone), `_BINNED` keys with `binWidth` / `binOrigin` (`date_bin`, e.g. 15-minute buckets), and `fillGaps`: empty groups between the first and last are included, from `generate_series` over the groups' own keys (no second scan).
- Connection edges have window values over every matching row in the connection's order, pages before included: `rowNumber`, `rank`, `denseRank`, `percentRank` (ranks compare the `orderBy` values only), and `runningSum` (a ledger's running balance), each with `partitionBy`.
