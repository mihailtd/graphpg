# REST API

graphpg serves a REST API next to GraphQL, planned onto the same operation plan: the same lateral-join SQL, keyset pagination, nested writes, typed constraint errors and Row-Level Security. It is served under `/rest` when `openapi.enabled` is on, and described by an [OpenAPI 3.1 document](configuration.md#the-openapi-document-and-uis) generated from the same catalog and configuration, so the document and the API cannot drift apart.

A resource is a table or view, named as in the database (`/rest/forums`). Rows use the API's field names (`archivedAt`) and carry the real primary key under the key column's name (`id` is the UUID, not a Relay global ID).

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
| 404 | Unknown resource, or no such row |
| 409 | A unique constraint (`*_TAKEN`, `*_CONFLICT`) or foreign key |
| 412 | `If-Match` does not match the row's version |
| 413 | A bulk create with more than 1000 rows, or a body over 1 MiB |
| 422 | A validation failure (not null, check, `*_REQUIRED`, …) |
| 503 | The database is unavailable |

In **prod** mode raw PostgreSQL messages never reach the client: unexpected failures say so in general terms, with a `requestId` that is also written to the server log together with the real error. **Dev** mode adds the detail.

## Limits

The page size, include and bulk limits above are fixed for now; configurable, shared limits for GraphQL and REST are tracked in the safety-limits stories.
