# REST API

graphpg serves a REST API next to GraphQL, planned onto the same operation plan: the same lateral-join SQL, keyset pagination, nested writes, typed constraint errors and Row-Level Security. It is served under `/rest` when `openapi.enabled` is on, and described by an [OpenAPI 3.1 document](configuration.md#the-openapi-document-and-uis) generated from the same catalog and configuration, so the document and the API cannot drift apart.

A resource is a table, view or materialized view, named as in the database (`/rest/forums`; with several schemas, the default schema's table when names collide). Views follow what PostgreSQL allows: they are always readable, writable only where PostgreSQL can update them and a key is declared (`graphpg.yaml`), and read by key only once they have one. Coming from PostgREST? The [comparison](comparison.md#compared-with-postgrest) maps its grammar to this one. Rows use the API's field names (`archivedAt`) and carry the real primary key under the key column's name (`id` is the UUID, not a Relay global ID). For tables that are the parent of a table-per-type (CTI) hierarchy, rows automatically include `__typename` and subtype columns matching their concrete type. Subtype columns can also be filtered (`filter[seats][gte]=4`: some subtype row has a `seats` of at least 4) and ordered by (`order=seats`: parents without a subtype row sort as `NULL`); GraphQL's `Filter` and `OrderBy` inputs have them too.

`GET /rest` lists what the API serves: its resources (`name`, `schema`, GraphQL `type` and `path`), its functions (`path` under `/rest/rpc/`), and the OpenAPI document's path when it is served.

## Strict by design

Anything the API does not understand is a **400**, never ignored: an unknown query parameter, column, operator or body member, an invalid value, a body that is not an object. A typo cannot turn into an unfiltered read or a write that does nothing. Bodies and filters are checked against the same input types GraphQL uses, so nested writes are validated down to their members.

## Reading

| Request | |
| :--- | :--- |
| `GET /rest/{resource}` | A list of rows (a plain JSON array) |
| `POST /rest/{resource}/search` | The same, with the options in a JSON body |
| `GET /rest/{resource}/{key}` | One row, with an `ETag` if the table has a row version; conditional `GET` or `HEAD` with `If-None-Match` answers `304 Not Modified` with no body |

The key is the primary key (composite keys: the values separated by commas). The Relay global ID of the row is accepted for uuid and numeric keys. A row can be addressed by one of its other unique keys instead, as GraphQL's `userByUsername`: `GET /rest/users/alice?by=username` (the path holds the key's values in the order `by` names them, comma-separated for a composite key), and `PATCH` and `DELETE` likewise; the unique key's columns can then be changed, the primary key's cannot. `GET /rest/node/{id}` takes a Relay global ID (the `id` GraphQL gives every row) and answers `303 See Other` with the row's URL.

### Query parameters

(With `curl`, use `-g` so it does not treat the brackets of `filter[…]` as a range, and encode `%` as `%25`.)

| Parameter | |
| :--- | :--- |
| `filter[column][operator]=value` | `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in`, `nin`, `like`, `ilike`, `startsWith`, `endsWith`, `isNull`, and the other operators of the GraphQL filter. Several filters are combined with `AND`. Through a relation: `filter[author][username][eq]=ada`; to-many relations: `filter[posts][some][title][like]=%25sql%25`. `filter={json}` takes a whole filter object. More operators are below. |
| `order=name,-created_at` | `column`, `-column` or `column:desc`; unknown columns are a 400. NULLs go last ascending and first descending, or where `:nullsfirst` / `:nullslast` puts them (`order=-rank:nullslast`) |
| `first=n&after=<cursor>` | Keyset pagination forwards. Default page size 100, **at most 1000** |
| `last=n&before=<cursor>` | Keyset pagination backwards |
| `offset=n` | Rows to skip |
| `fields=a,b` | Columns to return (default: all) |
| `include=posts,posts.author` | Embed relations, at most **10** relations and **3 levels** deep |

A single-row `GET` accepts only `fields` and `include`.

#### More filter operators and groups

| | |
| :--- | :--- |
| `filter[name][regex]=^A` / `[iregex]` | POSIX regular expressions (`~`, `~*`) on text columns (`match` and `imatch` are accepted too) |
| `filter[x][isDistinctFrom]=v` | `IS DISTINCT FROM`: like `neq`, but a null is a value (`=null` finds the rows that are not null) |
| `filter[id][gtAny]=1,2`, `[gtAll]`, `[ltAny]`, `[lteAll]`, ... | A comparison against any or all of a list (`> ANY (...)`, `<= ALL (...)`); `gt`, `gte`, `lt`, `lte`, with `Any` or `All` |
| `filter[name][likeAny]=a%25,b%25` | `like`, `ilike` against any (`likeAny`) or all (`likeAll`) of a list of patterns |
| `filter[tags][contains]=a,b`, `[containedBy]`, `[overlaps]` | Array operators; the cells are typed by the element type |
| `filter[span][contains]=3`, `[strictlyLeftOf]=[10,20)`, `[strictlyRightOf]`, `[adjacentTo]`, `[overlaps]`, `[containedBy]` | Range operators (`contains` takes an element; others take `[lower,upper)` bounds or JSON objects) |
| `filter[doc][contains]={"a":1}`, `[hasKey]`, `[hasAnyKeys]`, `[hasAllKeys]`, `[matchesPath]`, `[existsPath]` | `json` / `jsonb` operators (`contains` takes JSON, `matchesPath` and `existsPath` a JSON path) |
| `filter[or][0][age][lt]=18&filter[or][1][age][gt]=65` | A group: `or` (or `and`) with its members numbered. Groups nest (`filter[or][1][and][0]…`) and sit next to plain conditions, which must all hold as well |
| `filter[not][status][eq]=draft` | The condition does not hold |

Operators that need a typed value (`gtAny` on an `int` column) report a cell that does not fit as a `400` naming the parameter. Operators of a column's type only: `regex` on a number is a `400`.

Lists carry RFC 8288 `Link` headers (`rel="next"` / `rel="prev"`) that keep every other parameter. The total is **opt-in**, because it scans every matching row:
- `Prefer: count=exact`: Scans matching rows and returns the exact count in `X-Total-Count` (with `Preference-Applied: count=exact`).
- `Prefer: count=planned`: Uses the PostgreSQL planner's row estimate (from `reltuples` or `EXPLAIN`), answering with `Preference-Applied: count=planned`, `X-Total-Count`, and `X-Total-Count-Estimate: true`.
- `Prefer: count=estimated`: Computes an exact count when the estimate is below `limits.count_threshold` (default 10,000), falling back to the planner estimate above it. Sets `X-Total-Count-Estimate: true` whenever the estimate was used. Above the threshold the answer is one query; below it a second, exact count follows the page, so the two are not one snapshot. A `Range` past the end is a `416` with the exact size whichever count was asked for.

### Controls inside an embedding

`include=posts` embeds every column of every related row. Each embedded relation takes controls of its own, named by its `include` path (`posts`, `posts.comments`) and listed per relation in the OpenAPI document:

| Parameter | Does |
| :--- | :--- |
| `fields[posts]=id,title` | Columns of the embedded rows (any relation: to-one, to-many, many-to-many) |
| `filter[posts][status][eq]=PUBLISHED` | Keeps only the embedded rows that match; the parents stay (an empty list for those without a match). Same grammar and operators as `filter`, with the relation's columns |
| `order[posts]=-createdAt` | Order of the embedded rows |
| `first[posts]=5`, `offset[posts]=10` | A page of them (at most `limits.page_size.max`) |
| `inner=posts` | Only parents that have such a row (with its `filter[posts]`), like an inner join |
| `anti=posts` | Only parents that have no such row |

`filter`, `order`, `first` and `offset` are for to-many relations (a to-one has one row: `fields` only); `filter[posts][some][…]` (also `every`, `none`, `count`, `sum`…) still filters the *parents* by their related rows, as before. A relation read with a filter, order or page is its GraphQL connection, so rows are paged by `limits.page_size` (default 100) when you give no `first[posts]`. A single row (`GET /rest/{resource}/{key}`) and a `POST …/search` body take the same controls (`"embed": {"posts": {"filter": …, "order": "-createdAt", "first": 5, "fields": [...]}}`, `"inner"`, `"anti"`).

A column of the row a to-one relation leads to orders the list: `order=author.username`.

`order` takes, besides the table's columns: a to-one relation's column (`author.username`), the number of rows of a to-many relation (`messagesCount`, `messages_count` and `MESSAGES_COUNT` are the same), a computed column that returns an orderable value (`uniqueAuthorCount`), and `natural` / `primary_key`. The OpenAPI document lists them in the `order` parameter.

What GraphQL walks beyond foreign keys can be embedded as well, with the controls that fit it:

| `include=` | What | Controls |
| :--- | :--- | :--- |
| `ancestors`, `descendants` (and `pathAncestors`, `pathDescendants` for an `ltree` column) | A hierarchy: a foreign key that references its own table, or a path. The walk is recursive and ends at a cycle | `fields`, `filter`, `order`, `depth[ancestors]=2` (how many levels); no page |
| `target` | The row an exclusive arc (`CHECK (num_nonnulls(post_id, comment_id) = 1)`) points at: one row of one of several tables | `fields[target]=title`: the columns each table has. The row says which table with `__typename` |
| `history` | The versions of a row of a temporal table | as a relation |
| a function mounted on the table | A computed relationship (above) | its own arguments, `args[reports][minId]=3`, and a relation's controls |

**Computed fields with arguments.** A function of a row that takes arguments is selected like the ones that do not, and the arguments go in `args[<field>][<argument>]=<value>`: `fields=name,doubleId&args[doubleId][factor]=3`, `include=reports&args[reports][minId]=3`; for a field of an embedded relation its path, `args[manager.doubleId][factor]=10`. Arguments are typed from the function (arrays as `1,2,3`), unknown ones are a 400, and one without a default is required.

**Time.** A table with a temporal key (`PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)`) is read as it was with `asOf=2024-03-01T00:00:00Z`, and embedded rows of one with `asOf[account]=…`; without it you get what the table gives without a point in time. `asOf` on a table that has no temporal key is a 400.

**JSON columns.** `path[doc]=$.address.city` returns the first item a JSON path selects from a `json` / `jsonb` column instead of the whole value, `path[posts.meta]=…` in an embedded relation.

**Rows with their cursor and window values.** On a list that is a connection, `cursors=true` puts each row's own cursor in `_cursor` (send it back as `after` or `before` to continue from that row), and `window=rowNumber,rank,denseRank,percentRank` puts the row's position among *all* the matching rows, in the list's order and pages before this one included, in `_window`; `partition=status,createdAt:month` numbers within the groups of rows that share these values. `running=total` adds a running sum (a ledger's balance) of number columns: `_window.runningSum.total`.

```
GET /rest/orders?order=createdAt&window=rowNumber,rank&running=total&partition=customer&cursors=true
→ [ { "id": 1, "customer": "ada", …, "_cursor": "…",
      "_window": { "rowNumber": 1, "rank": 1, "runningSum": { "total": 100.0 } } }, … ]
```

Functions mounted on a table that need no argument of their own are selectable too (the ones with arguments stay GraphQL fields): a function returning rows of a table is a relation to `include` (a set of rows takes all the controls above: `include=featuredMessages&first[featuredMessages]=3`), and one returning a value or a record is a column to name in `fields` (`fields=name,uniqueAuthorCount`). Parents cannot be filtered by them with `inner` or `anti`.

```
GET /rest/forums?include=messages&fields[messages]=id,body&filter[messages][featured][eq]=true&order[messages]=-createdAt&first[messages]=3
GET /rest/forums?include=messages&inner=messages&filter[messages][body][startsWith]=Hello
```

### Ranges

A list takes `Range: items=A-B` (`items=0-24`, or `items=25-` to the end): rows `A` to `B`, counting from 0. The answer is `206 Partial Content` with `Content-Range: items A-B/N`, where `N` is `*` unless `Prefer: count=...` asked for the count (it costs a scan, so it is not done unasked), and a `200` when the range covers a list whose size is known to fit it. A range past the end is `416` with `Content-Range: items */N`, as is one that ends before it starts. A page larger than `limits.page_size.max` is cut at the maximum, and the `Content-Range` says which rows came. Another unit, a suffix (`items=-5`) or several ranges are ignored and the whole list is sent. A range does not combine with `first`, `last`, `after`, `before` or `offset` (`400`). Every list answers with `Accept-Ranges: items`, and `HEAD` carries the same headers. Keyset cursors and `Link` headers stay the way to page through a list that changes.

### Search

`POST /rest/{resource}/search` takes the filter and every option in one JSON body, for what URLs express badly (`and` / `or` / `not`, relation filters):

```json
{
  "filter": { "or": [{ "name": { "like": "%cat%" } }, { "messages": { "some": { "featured": { "equalTo": true } } } }] },
  "order": ["name:desc"],
  "first": 20,
  "after": "<cursor>",
  "fields": ["id", "name"],
  "include": ["messages"]
}
```

Every member is optional and unknown members are a 400; the query string must be empty. A search cannot be continued with a link (the filter is in the body), so a paged response carries `X-Next-Cursor` / `X-Previous-Cursor` to send back as `after` / `before`.

### Search methods

A table whose indexes support a search gets one endpoint per method, `POST /rest/{resource}/search/{method}`: the best matches first, **not paginated**, built for combo boxes and global search.

| Method | Index | Body |
| :--- | :--- | :--- |
| `fuzzy` | `pg_trgm` | `query`, `field`, `minSimilarity` |
| `semantic` | `pgvector` | `vector`, `field`, `minScore` |
| `bm25` | BM25 (ParadeDB) | `query`, `field` |
| `hybrid` | vector and lexical | `query`, `vector`, `lexicalWeight`, `vectorWeight`, `rrfK` |
| `full-text` | `tsvector` | `query`, `field` |
| `nearest` | PostGIS | `near`, `maxDistance`, … |

The method arguments are those of the GraphQL search field (see [Search fields](graphql-api.md#search-fields)); every search also takes `first` (default 10, at most `search.max_limit`), `filter` to scope the matches and `fields`. Unknown members are a 400, and a method the table does not support is a 404 that lists the ones it does.

```json
{ "query": "postgre", "first": 5, "filter": { "forumId": { "equalTo": "…" } } }
```

```json
{ "hits": [{ "score": 0.875, "row": { "id": 1, "title": "PostgreSQL full text search" } }], "hasMore": false }
```

A `full-text` search can mark the words that matched (`ts_headline`): `"highlight": true`, or `{"startSel": "<mark>", "stopSel": "</mark>", "maxWords": 35, "minWords": 15, "maxFragments": 2}`; each hit then has a `highlight` string. A `nearest` search puts the `distance` from `near` in each hit.

`GET` or `POST /rest/search` (`?query=…&first=…`) searches every searchable resource at once; each hit names the resource its row is from:

```json
{ "hits": [{ "score": 1, "type": "people", "row": { "id": 1, "name": "Search Man" } }], "hasMore": false }
```

### Aggregates

`GET` or `POST /rest/{resource}/aggregate` summarises the rows a filter selects, in one scan. The members are the same in the query string and the JSON body:

| Member | |
| :--- | :--- |
| `filter` | Which rows, like a list's (`filter[status][eq]=PAID`) |
| `min`, `max`, `sum`, `avg`, `distinctCount`, `stddevSample`, `stddevPopulation`, `varianceSample`, `variancePopulation`, `median` | Columns to aggregate: `sum=total,quantity`. Sums and averages need numbers, minimums and maximums orderable columns |
| `percentile`, `fraction` | Columns and the fraction (0 to 1) of `percentile_cont` |
| `group` | What to group by, at most 4 values: a column, `column:unit` with a unit of `minute`, `hour`, `day`, `week`, `month`, `quarter` or `year`, or `column:binned` |
| `having` | Conditions on the groups: `having[count][gt]=5`, `having[sum][total][gte]=100` (JSON: `{"count": {"greaterThan": 5}}`) |
| `timeZone`, `binWidth`, `binOrigin`, `fillGaps` | The time zone dates are truncated in, the width (`PT15M`) and origin of `:binned` groups, and whether the empty groups between the first and the last are included |

```
GET /rest/orders/aggregate?filter[status][eq]=PAID&sum=total&max=createdAt
→ { "count": 4, "sum": { "total": 425.75 }, "max": { "createdAt": "2026-03-01T12:00:00+00:00" } }

GET /rest/orders/aggregate?group=customer,createdAt:month&sum=total&having[count][gt]=1
→ [ { "keys": { "customer": "ada", "createdAt:month": "2026-01-01T00:00:00+00:00" }, "count": 2, "sum": { "total": 150.5 } }, … ]
```

**Aggregates side by side.** A body can ask for several aggregates in one scan, each with its own filter or grouping, with `sets`: the sum of paid orders next to the sum of all of them, and a grouping.

```json
{ "filter": { "createdAt": { "greaterThan": "2026-01-01T00:00:00Z" } },
  "sets": {
    "paid":       { "filter": { "status": { "equalTo": "PAID" } }, "sum": ["total"] },
    "everything": { "sum": ["total"], "max": ["createdAt"] },
    "byCustomer": { "group": ["customer"], "sum": ["total"], "having": { "count": { "greaterThan": 1 } } }
  } }
```

The answer is an object of the same names (`{"paid": {"sum": {"total": 425.75}}, "byCustomer": [ … ]}`). The request's `filter` applies to every set; an ungrouped set can have a `filter` of its own (`FILTER (WHERE …)`), a grouped one has `group`, `having` and the time options. Nothing else goes beside `sets`.

Which aggregates and columns exist is what the GraphQL schema offers for the table; anything else is a 400. A response holds at most `limits.groups` groups (10 000 by default; 422 `TOO_MANY_GROUPS`: narrow the filter or group more coarsely). Aggregates need the table's connection, so a resource whose list is not permitted has none.

## Media types

Lists (`GET /rest/<table>` and `POST …/search`) are `application/json` unless `Accept` prefers `application/x-ndjson` (one row per line) or `text/csv` (RFC 4180, a header of the selected fields; `null` is empty, objects and lists are JSON text, 64-bit integers are text as in JSON). An `Accept` that allows neither is `406`. CSV is the page of rows the request asks for, bounded by `limits.page_size`: for a table of any size, use `/export`, which streams.

Writes take three request media types, the wrong one is `415`:

| `Content-Type` | Where | Body |
| :--- | :--- | :--- |
| `application/json` | every write | the JSON shapes above |
| `text/csv` | `POST /rest/<table>` | a header naming the fields, then one record per row: a bulk create bounded by `limits.bulk_rows`. Cells are typed by their columns (integers, floats, booleans, JSON for `json`, `jsonb` and array columns); an empty cell is `null`, so leave a column out to get its default. A cell that does not fit names its row and column (`body[3].price`) |
| `multipart/form-data` | `POST`, `PATCH` and `PUT` of a row | one part per field (text typed by the column); a file part of a `bytea` column is its bytes, no Base64 |

The bytes of a `bytea` column have their own URL, `/rest/<table>/<key>/<column>`: `GET` answers with them, as the media type their first bytes name (`image/png`, `application/pdf`, ... else `application/octet-stream`) and `nosniff`; `PUT` replaces them from a body of any media type (`If-Match` applies). Neither the file name nor its media type is stored: keep them in columns if you need them.

`HEAD` answers like `GET` without a body, on every readable path. `OPTIONS` answers `204` with `Allow` and, for writes, `Accept-Post` / `Accept-Patch` (RFC 7694); with CORS configured, browsers' preflights are answered by the CORS layer first. All of it is in the OpenAPI document.

## Functions

Every SQL function the GraphQL schema exposes as a root field is callable at `/rest/rpc/{function}` by its SQL name (`/rest/rpc/search_orders`). Computed columns (functions of a row) are fields of that row, not endpoints. `STABLE` and `IMMUTABLE` functions answer `GET` and `POST`; `VOLATILE` ones, which change data, answer `POST` only (`GET` is a 405 with `Allow: POST`) and run in a write transaction.

- **Arguments** use the GraphQL names (`camelCase`). `GET`: query parameters (`?a=1&b=5`; arrays as `ids=1,2,3` or a JSON array). `POST`: the body's `args` object (`{"args": {"a": 1}}`), or for functions with a single `json` or `jsonb` argument, the raw JSON payload directly (`{"msg": "hi"}` or `[1, 2]`). Arguments with a default are optional; a missing, unknown or mistyped one is a 400, and key-like arguments also take Relay IDs.
- **Rows of a table, as a set** (`RETURNS SETOF orders`): a list like the table's own (`filter`, `order`, `first`, `after`, `fields`, `include`, `Prefer: count=exact`, `Link` headers for `GET`, `X-Next-Cursor` for `POST`). Without `order`, rows keep the order the function returns them in.
- **One row of a table**: the row, or `null`; `fields` and `include` only.
- **A value** (base type, enum, composite, array) or **columns of its own** (`RETURNS TABLE`, `OUT`): the JSON value, or a list for a set.
- **`void`**: `204`.

```
GET  /rest/rpc/add?a=1&b=5                       → 6
POST /rest/rpc/search_orders  {"args": {"q": "ada"}, "order": "-total", "first": 10}
POST /rest/rpc/bump_counter   {"args": {"counterId": 1, "by": 5}}   → {"id": 1, "value": 5}
```

## Debugging

With `debug.sql` and `debug.explain` on (dev only, see [configuration](configuration.md)) every response, errors included, carries the statements it ran in `X-Debug-SQL` and, with `explain`, the `EXPLAIN ANALYZE` plans in `X-Debug-Explain`: JSON, percent-encoded. The counterpart of GraphQL's `extensions.sql` and `extensions.explain`. A header is at most 8 KiB (clients and proxies refuse larger ones: Node's limit is 16 KiB for all headers, Nginx's buffers 4 to 8 KiB); a larger value is replaced by `{"omitted": ..., "at": "/debug/sql/<id>"}`, where the full value is served (the last 50 are kept).

## Several schemas

With several `database.schemas`, `/rest/{resource}` is a shortcut: the default schema's table (the first schema listed), else the table of that name in another schema. Every schema also has a path of its own, with every route below it: `/rest/{schema}/{resource}`, `/rest/{schema}/{resource}/{key}`, `/rest/{schema}/{resource}/search`, `/rest/{schema}/rpc/{function}` and so on (`/rest/auth/users`, `/rest/app/audit_logs`, `/rest/auth/rpc/login`). Two schemas with an `audit_logs` table are both reachable, unambiguously, at `/rest/auth/audit_logs` and `/rest/app/audit_logs`; the same goes for functions. A schema's path serves that schema's tables and functions only (another schema's is a `404`), `Location` and `Link` headers and the `instance` of an error keep the path the request used, and `include` can still embed a row of another schema through a foreign key. `/rest/search` (global search) spans all of them.

The schema is part of the path, never a header or a parameter, and it is a fixed set: only the schemas of `database.schemas` that have tables or functions have a path, so `/rest/anything_else/users` is a `404` before any SQL is built. The [OpenAPI document](configuration.md#the-openapi-document-and-uis) lists these concrete paths (`/rest/auth/users`, `/rest/app/posts`, ...) with an operation id (`auth_list_users`) and components (`auth.users`) per schema, not a path template. A schema is added by restarting: the paths are fixed when the server starts. A table named like a schema is reached at its schema's path, since `/rest/{schema}/…` wins (a warning is logged).

## Reserved names

`rpc`, `search` and `node` are paths of their own, so tables and schemas with these names are not served (the OpenAPI document leaves them out too). `search` and `aggregate` are also the last segments of `/rest/{resource}/search` and `/rest/{resource}/aggregate`: a row whose key is literally `aggregate` or `search` cannot be read with `GET /rest/{resource}/{key}`.

## Writing

| Request | |
| :--- | :--- |
| `POST /rest/{resource}` | Create a row (object body) → `201` with `Location` and `ETag`. An array body creates up to **1000** rows in one statement and answers with an array |
| `PATCH /rest/{resource}/{key}` | Update the given fields → `200` (JSON, an RFC 7396 merge patch, or an RFC 6902 JSON Patch) |
| `PUT /rest/{resource}/{key}` | Insert the row or update its columns → `200` either way |
| `DELETE /rest/{resource}/{key}` | Delete → `200` with the row |
| `PATCH /rest/{resource}?filter…` | Update the rows a filter matches → `200` with the rows and `X-Affected-Count` |
| `DELETE /rest/{resource}?filter…` | Delete the rows a filter matches, likewise |
| `PUT /rest/{resource}` | Upsert an array of rows by a declared unique key |

- **Nested writes:** nested objects in the body are written in the same transaction, in the shape of the GraphQL input.
- **The URL owns the key.** A key in a body must match the URL (the global ID of the same row is fine, so a representation read from the API can be sent back); a different one is a 400. `PATCH` with no fields is a 400.
- **Optimistic concurrency:** `ETag` is the row's version. `If-Match: "<etag>"` on `PATCH`, `PUT` or `DELETE` fails with `412` and code `STALE_VERSION` if the row changed. `PUT` with `If-Match` is an update: the row must exist (404 otherwise). `If-Match: *` only requires the row to exist.
- **`Prefer: return=minimal`** answers `204` without a body (`Preference-Applied: return=minimal`).
- A request is **all-or-nothing** in one transaction: there is no partial data, unlike GraphQL.
- Bodies are limited by `limits.request_body_bytes` (1 MiB by default).
- **`Prefer: previous`** on `PATCH` and `PUT` answers `{"row": …, "previous": …}`: the row as it is now and as it was before the write, read in the writing statement itself (`RETURNING OLD`, PostgreSQL 18; ignored without it). `previous` is `null` for a row a `PUT` inserted, and has no `rowVersion`. `Preference-Applied: previous` says it was honoured; `return=minimal` wins over it.

### Writes to a collection

`PATCH` and `DELETE` on `/rest/{resource}` take the `filter` a list takes (`filter[status][eq]=draft`, `filter[or][0]…`, relation filters), nothing else: `order`, `first`, `include` and the like are a 400, as is an unknown operator. The body of a `PATCH` is the columns to set (`application/json` or `application/merge-patch+json`: a `null` sets the column to `NULL`). One statement, one transaction, planned onto the GraphQL `update<Types>(where:)` and `delete<Types>(where:)`.

- **No runaway writes.** Without a `filter` the write is refused (`400`, code `FILTER_REQUIRED`) unless you send `unfiltered=true`. A write by filter may affect at most `limits.bulk_rows` rows (1000 by default): if more match, the transaction is rolled back and nothing changes (`413`, `BULK_LIMIT_EXCEEDED`, with the `limit` and the `actual` count). GraphQL's `update<Types>(where:)` and `delete<Types>(where:)` have the same bound.
- **The answer** is the affected rows, with their number in `X-Affected-Count`; `Prefer: return=minimal` answers `204` with just the header.

```
PATCH  /rest/orders?filter[status][eq]=draft&filter[createdAt][lt]=2026-01-01   {"status": "ARCHIVED"}
DELETE /rest/sessions?filter[expiresAt][lt]=2026-10-01
PATCH  /rest/orders?unfiltered=true   {"seen": true}
```

**Bulk upsert**: `PUT /rest/{resource}` with an array (`application/json` or CSV, at most `limits.bulk_rows` rows). `conflict=email` names the unique key the rows are matched by (its columns, comma-separated; the primary key by default), `resolution=merge` (the default) updates the rows that exist from the proposed ones (`update=name,qty` limits it to those columns; by default the columns the rows set, apart from the key) and `resolution=ignore` leaves them. Rows that do not exist are inserted. The answer is the rows as they are now. `conflict` must be a unique key of the table: anything else is a 400 naming the keys there are.

**Missing columns** (bulk `POST` and `PUT`): when some rows of the array set a column and others do not, `missing=default` (the default) inserts the latter with the column's default, and `missing=null` sets the column to `NULL` in every row that does not set it.

**JSON Patch.** `PATCH /rest/{resource}/{key}` with `Content-Type: application/json-patch+json` takes RFC 6902 operations (`add`, `remove`, `replace`, `move`, `copy`, `test`) applied to the row as the API shows it: `/title` is the column, `/settings/theme` reaches into a JSON column. The columns that changed are updated, as with a plain `PATCH`; an operation that does not apply (a failed `test`, a missing path) is `422` with code `PATCH_FAILED` and nothing is written. Without `If-Match` the row is read and then written in two statements, so send the `ETag` back when another writer may be at it. A JSON Patch is for one row: on a collection it is a `415`.

### Nested writes

The body of a `POST`, `PATCH` or `PUT` has the shape of the GraphQL input, relations included, and writes the whole graph in the request's transaction. A relation member takes:

| Relation | Create | Update (single row) also |
| :--- | :--- | :--- |
| To-one | `{"create": {…}}`, `{"connect": <key>}`, `{"connectOrCreate": {"where": <key>, "create": {…}}}` | `{"update": {…}}`, `{"disconnect": true}` |
| To-many, many-to-many | `{"create": [{…}], "connect": [<key>], "connectOrCreate": [{…}]}` | `"disconnect": [<key>]`, `"set": [<key>]` (exactly these), `"update": [{"where": <key>, "data": {…}}]`, `"delete": [<key>]` (its own children), `"sync": [{…}]` |

`<key>` is `{"id": …}` (or another unique key). `sync` makes the children exactly the rows listed, in one `MERGE`: a row whose key matches updates its child, a row with a new key is inserted, and the children no row matches are deleted. Everything is validated against the GraphQL input types (an unknown member at any depth is a 400 naming its path, like `body.messages.creat`), and the OpenAPI document describes every shape as `Input.<Type>` components. A failure anywhere rolls back the lot: a constraint of a nested row keeps its typed code (`USERNAME_TAKEN`) and names the input path (`input.messages.create[1].body`), and an `If-Match` that no longer matches (`412`) refuses the whole nested update.

## Errors

Errors are RFC 9457 `application/problem+json`:

```json
{
  "type": "about:blank",
  "title": "Unprocessable Content",
  "status": 422,
  "detail": "`input.name` is required: `Forum` cannot be null",
  "instance": "/rest/forums",
  "code": "NAME_REQUIRED",
  "fields": ["input.name"],
  "requestId": "6d1c…"
}
```

`type` is always `about:blank`: the status gives the class of problem and `code` identifies it, using the same typed constraint codes as GraphQL (`EMAIL_TAKEN`, `STALE_VERSION`, …).

| Status | When |
| :--- | :--- |
| 400 | Unknown or malformed parameter, operator, field or body |
| 401 | Missing or invalid access token (with `WWW-Authenticate`) |
| 403 | The operation is not permitted on the resource (grants, `omit_operations`) |
| 404 | Unknown resource, function or search method, or no such row |
| 405 | A `GET` of a function that changes data |
| 409 | A unique constraint (`*_TAKEN`, `*_CONFLICT`) or foreign key |
| 412 | `If-Match` does not match the row's version |
| 413 | A bulk create over `limits.bulk_rows`, or a body over `limits.request_body_bytes` |
| 422 | A validation failure (not null, check, `*_REQUIRED`, …), or more than 10 000 groups |
| 429 | Over the client's [rate limit](configuration.md#rate-limits) (`RATE_LIMITED`, with `Retry-After`) |
| 503 | The database is unavailable, a statement ran past `limits.statement_timeout` (`STATEMENT_TIMEOUT`), or a [foreign data source](foreign-data.md#errors) is down (`SOURCE_UNAVAILABLE`) |

In **prod** mode raw PostgreSQL messages never reach the client: unexpected failures say so in general terms, with a `requestId` that is also written to the server log together with the real error. **Dev** mode adds the detail.

### Custom SQL errors (`PTxxx` and `errors.map`)

Two ways to give a database error its own status and code:

- **`PTxxx` SQLSTATEs.** A function or trigger raises `RAISE EXCEPTION ... USING ERRCODE = 'PT402'`; the digits are the HTTP status (`402 Payment Required`), and a status outside 400-599 is a `400`. The author wrote the message, so it is **never masked**, in production either. A plain message becomes the `detail`, and the `HINT` the `code` (and `hint`). A JSON message (`{"message": "...", "code": "...", "hint": "...", "title": "...", "type": "...", ...}`) sets the members itself, and any further key becomes an extension member of the problem. The `DETAIL` line is `databaseDetail`.
- **`errors.map`.** In the configuration, a SQLSTATE or a constraint name maps to a `status`, `title`, `code` and problem `type`. A constraint name wins over a SQLSTATE. Unlike a `PTxxx` error, the message is the database's own, so in **prod** a mapped error carries only its title as the `detail`, with no `hint` or `databaseDetail`; **dev** shows them.

Both also set `errors[].extensions.code` over GraphQL (with the same masking; GraphQL answers `200` whatever the status).

## Limits

The limits are the deployment's [`limits:`](configuration.md#safety-limits), shared with GraphQL and enforced on the plan: page size (default 100, at most 1000 by default; `PAGE_SIZE_EXCEEDED`), depth and cost (`include` paths count as nested rows: `DEPTH_LIMIT_EXCEEDED`, `COST_LIMIT_EXCEEDED`), bulk rows, groups, body size and `statement_timeout`. A request over one fails before it touches the database with a problem carrying the `code`, the `limit` and what the request `actual`ly asked for:

```json
{ "type": "about:blank", "title": "Bad Request", "status": 400, "code": "PAGE_SIZE_EXCEEDED",
  "detail": "`first` is 5000, more than the maximum page size of 1000", "limit": 1000, "actual": 5000 }
```

With [`limits.rate_limit`](configuration.md#rate-limits) on, every answer carries `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset`, and a client over its rate gets `429` with `Retry-After` and the code `RATE_LIMITED`. GraphQL and REST requests share a client's bucket.

## Observability and health probes

- **`X-Request-Id`**: Every request echoes the incoming `X-Request-Id` header (or generates a random UUIDv4 if omitted) on the response, linking logs, client metrics and problem details.
- **`Server-Timing`**: In development mode (`server.mode: dev`), responses include a `Server-Timing: total;dur=...` header detailing the request processing latency in milliseconds.
- **Probes**:
  - `GET /live`: Liveness probe answering `200 OK` (`{"status":"ok"}`) indicating the HTTP server is alive and accepting connections.
  - `GET /ready`: Readiness probe verifying database pool health by executing `SELECT 1`. Answers `200 OK` (`{"status":"ok","database":"connected"}`) or `503 Service Unavailable` (`{"status":"error","database":"unreachable"}`) if the connection pool cannot reach PostgreSQL.
  - `GET /health`: Combined health check returning detailed status and timestamp.
