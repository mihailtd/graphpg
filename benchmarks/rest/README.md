# REST benchmark: graphpg and PostgREST

Compares the REST APIs of graphpg and [PostgREST](https://postgrest.org) on **one PostgreSQL, one schema, the same data and the same pool size**, with [k6](https://k6.io). Each scenario sends the same logical request in the grammar of each server and runs alone, one after the other.

| Scenario | graphpg | PostgREST |
| :--- | :--- | :--- |
| `simple` | `GET /rest/posts?fields=id,title,status&order=id&first=20` | `GET /posts?select=id,title,status&order=id&limit=20` |
| `filtered` | `…?filter[status][eq]=PUBLISHED&order=-createdAt&first=20` | `…?status=eq.published&order=created_at.desc&limit=20` |
| `by_key` | `GET /rest/posts/{id}` | `GET /posts?id=eq.{id}` (single object) |
| `embedded` | `…?include=author,comments&fields[comments]=id,body…` | `…?select=id,title,author:authors(username),comments(id,body)` |
| `offset_deep` | `…?order=id&first=20&offset=50000` | `…?order=id&limit=20&offset=50000` |
| `keyset_deep` | `…?filter[id][gt]=50000&order=id&first=20` | `…?id=gt.50000&order=id&limit=20` |
| `insert` | `POST /rest/notes` | `POST /notes` |
| `update` | `PATCH /rest/notes/{id}` | `PATCH /notes?id=eq.{id}` |
| `update_many` | `PATCH /rest/notes?filter[id][gte]=…&filter[id][lt]=…` | `PATCH /notes?id=gte.…&id=lt.…` |

Starting rows of the pagination scenarios are random between 10 % and 90 % of the table, so `offset_deep` and `keyset_deep` really skip rows. Writes use `Prefer: return=minimal` on both.

## Run it

```bash
cargo build --release
PGURL=postgres://postgres:postgres@localhost:5432 benchmarks/rest/compare.sh
VUS=50 DURATION=30 POSTS=1000000 COMMENTS=3000000 benchmarks/rest/compare.sh   # bigger
```

`compare.sh` (needs `psql`, `k6`, a container runtime for PostgREST) creates the database `bench` from [`schema.sql`](schema.sql) (it drops it first), starts graphpg (`graphpg.yaml`: REST only, pool of 10) and PostgREST (`PGRST_DB_POOL=10`, `PGRST_DB_MAX_ROWS=1000`, as graphpg's `limits.page_size.max`), waits for both and runs [`rest.js`](rest.js) against each; the tables land in `results/<target>.md`. To run one side: `TARGET=postgrest URL=http://localhost:3000 k6 run benchmarks/rest/rest.js` (`SCENARIOS=simple,embedded` picks scenarios).

## What the numbers do and do not say

- **One machine runs everything**: PostgreSQL, both servers and the load generator compete for the same cores. Read the ratios, not the absolute numbers, and expect different ones on your hardware. The run in [`results/`](results) is a single one.
- **Both servers connect as the same database user.** PostgREST then switches to `bench_anon` for each request (`SET LOCAL ROLE`), which is part of how it enforces roles; graphpg, without `auth`, runs as the connecting user. A deployment that authenticates users pays for role switching in both.
- **The schema is small and indexed**: foreign keys, the order and the filter of the lists have indexes. A query PostgreSQL cannot serve well (`offset_deep`: about 6 ms in both) measures PostgreSQL, not the server.
- Everything is on loopback with keep-alive, and k6 uses its HTTP/1.1 client. TLS, a proxy and the network change the picture.
- Compare the two servers on **your** queries before choosing: the embedding and write grammars differ (see the [comparison](../../docs/comparison.md#compared-with-postgrest)).
