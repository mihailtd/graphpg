# Benchmarks

`queries.js` is a [k6](https://k6.io) script that runs four read queries (a baseline, a list, a filtered list and a nested relation query) against graphpg with the [blog example](../examples/blog) loaded.

```bash
# 1. PostgreSQL with the blog schema, and graphpg
docker compose -f examples/blog/docker-compose.yml up --build -d

# 2. Run the load
k6 run benchmarks/queries.js
VUS=50 DURATION=30s k6 run benchmarks/queries.js
```

## Reading the results

Numbers depend on your hardware, PostgreSQL configuration and, above all, **your schema and its indexes**: a nested query on an unindexed foreign key measures PostgreSQL, not graphpg. The example is tiny (three posts), so it mostly measures request handling. To get numbers that mean something for you, load a realistic data volume and point the script at your own queries.

The script exercises the GraphQL API. To compare the REST API with [PostgREST](https://postgrest.org) (see the [comparison](../docs/comparison.md#compared-with-postgrest)), point a copy of it at equivalent requests: `GET /rest/posts?filter[status][eq]=PUBLISHED&first=20` against graphpg and `GET /posts?status=eq.published&limit=20` against PostgREST, with the same database, indexes and connection pool size, and compare the nested-embedding case (`include=tags` and `select=*,tags(*)`) separately from the flat one.

No results are published here. When comparing tools, run each against the same database, the same queries and the same hardware, and report the setup alongside the numbers.
