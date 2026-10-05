# REST API

graphpg serves a REST API next to GraphQL, planned onto the same operation plan: the same lateral-join SQL, keyset pagination, nested writes, typed constraint errors and Row-Level Security. It is served under `/rest` when `openapi.enabled` is on, and described by an [OpenAPI 3.1 document](configuration.md#the-openapi-document-and-uis) generated from the same catalog and configuration, so the document and the API cannot drift apart.

A resource is a table, view or materialized view, named as in the database (`/rest/forums`). Views follow what PostgreSQL allows: they are always readable, writable only where PostgreSQL can update them and a key is declared (`graphpg.yaml`), and read by key only once they have one. Rows use the API's field names (`archivedAt`) and carry the real primary key under the key column's name (`id` is the UUID, not a Relay global ID).

## Strict by design

Anything the API does not understand is a **400**, never ignored: an unknown query parameter, column, operator or body member, an invalid value, a body that is not an object. A typo cannot turn into an unfiltered read or a write that does nothing. Bodies and filters are checked against the same input types GraphQL uses, so nested writes are validated down to their members.

## Reading

| Request | |
| :--- | :--- |
| `GET /rest/{resource}` | A list of rows (a plain JSON array) |
| `POST /rest/{resource}/search` | The same, with the options in a JSON body |
| `GET /rest/{resource}/{key}` | One row, with an `ETag` if the table has a row version |

The key is the primary key (composite keys: the values separated by commas). The Relay global ID of the row is accepted for uuid and numeric keys.

### Query parameters

| Parameter | |
| :--- | :--- |
| `filter[column][operator]=value` | `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in`, `nin`, `like`, `ilike`, `startsWith`, `endsWith`, `isNull`, and the other operators of the GraphQL filter. Several filters are combined with `AND`. Through a relation: `filter[author][username][eq]=ada`; to-many relations: `filter[posts][some][title][like]=%25sql%25`. `filter={json}` takes a whole filter object. |
| `order=name,-created_at` | `column`, `-column` or `column:desc`; unknown columns are a 400 |
| `first=n&after=<cursor>` | Keyset pagination forwards. Default page size 100, **at most 1000** |
| `last=n&before=<cursor>` | Keyset pagination backwards |
| `offset=n` | Rows to skip |
| `fields=a,b` | Columns to return (default: all) |
| `include=posts,posts.author` | Embed relations, at most **10** relations and **3 levels** deep |

A single-row `GET` accepts only `fields` and `include`.

Lists carry RFC 8288 `Link` headers (`rel="next"` / `rel="prev"`) that keep every other parameter. The total is **opt-in**, because it scans every matching row: send `Prefer: count=exact` to get `X-Total-Count` (answered with `Preference-Applied: count=exact`).

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

Which aggregates and columns exist is what the GraphQL schema offers for the table; anything else is a 400. A response holds at most 10 000 groups (422 `TOO_MANY_GROUPS`: narrow the filter or group more coarsely). Aggregates need the table's connection, so a resource whose list is not permitted has none.

## Functions

Every SQL function the GraphQL schema exposes as a root field is callable at `/rest/rpc/{function}` by its SQL name (`/rest/rpc/search_orders`). Computed columns (functions of a row) are fields of that row, not endpoints. `STABLE` and `IMMUTABLE` functions answer `GET` and `POST`; `VOLATILE` ones, which change data, answer `POST` only (`GET` is a 405 with `Allow: POST`) and run in a write transaction.

- **Arguments** use the GraphQL names (`camelCase`). `GET`: query parameters (`?a=1&b=5`; arrays as `ids=1,2,3` or a JSON array). `POST`: the body's `args` object (`{"args": {"a": 1}}`). Arguments with a default are optional; a missing, unknown or mistyped one is a 400, and key-like arguments also take Relay IDs.
- **Rows of a table, as a set** (`RETURNS SETOF orders`): a list like the table's own (`filter`, `order`, `first`, `after`, `fields`, `include`, `Prefer: count=exact`, `Link` headers for `GET`, `X-Next-Cursor` for `POST`). Without `order`, rows keep the order the function returns them in.
- **One row of a table**: the row, or `null`; `fields` and `include` only.
- **A value** (base type, enum, composite, array) or **columns of its own** (`RETURNS TABLE`, `OUT`): the JSON value, or a list for a set.
- **`void`**: `204`.

```
GET  /rest/rpc/add?a=1&b=5                       → 6
POST /rest/rpc/search_orders  {"args": {"q": "ada"}, "order": "-total", "first": 10}
POST /rest/rpc/bump_counter   {"args": {"counterId": 1, "by": 5}}   → {"id": 1, "value": 5}
```

## Reserved names

`rpc` and `search` are paths of their own, so tables with these names are not served (the OpenAPI document leaves them out too). `search` and `aggregate` are also the last segments of `/rest/{resource}/search` and `/rest/{resource}/aggregate`: a row whose key is literally `aggregate` or `search` cannot be read with `GET /rest/{resource}/{key}`.

## Writing

| Request | |
| :--- | :--- |
| `POST /rest/{resource}` | Create a row (object body) → `201` with `Location` and `ETag`. An array body creates up to **1000** rows in one statement and answers with an array |
| `PATCH /rest/{resource}/{key}` | Update the given fields → `200` |
| `PUT /rest/{resource}/{key}` | Insert the row or update its columns → `200` either way |
| `DELETE /rest/{resource}/{key}` | Delete → `200` with the row |

- **Nested writes:** nested objects in the body are written in the same transaction, in the shape of the GraphQL input.
- **The URL owns the key.** A key in a body must match the URL (the global ID of the same row is fine, so a representation read from the API can be sent back); a different one is a 400. `PATCH` with no fields is a 400.
- **Optimistic concurrency:** `ETag` is the row's version. `If-Match: "<etag>"` on `PATCH`, `PUT` or `DELETE` fails with `412` and code `STALE_VERSION` if the row changed. `PUT` with `If-Match` is an update: the row must exist (404 otherwise). `If-Match: *` only requires the row to exist.
- **`Prefer: return=minimal`** answers `204` without a body (`Preference-Applied: return=minimal`).
- A request is **all-or-nothing** in one transaction: there is no partial data, unlike GraphQL.
- Bodies are limited to 1 MiB.

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
| 413 | A bulk create with more than 1000 rows, or a body over 1 MiB |
| 422 | A validation failure (not null, check, `*_REQUIRED`, …), or more than 10 000 groups |
| 503 | The database is unavailable |

In **prod** mode raw PostgreSQL messages never reach the client: unexpected failures say so in general terms, with a `requestId` that is also written to the server log together with the real error. **Dev** mode adds the detail.

## Limits

The page size, include, bulk and group limits above are fixed for now; configurable, shared limits for GraphQL and REST are tracked in the safety-limits stories.
