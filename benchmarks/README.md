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

The script exercises the GraphQL API. [`explore/`](explore) has load tests generated from a server's own schema (every field, filter, aggregate and relation, malformed inputs, writes, and a soak with live queries), for any schema. The REST API is compared with [PostgREST](https://postgrest.org) in [`rest/`](rest): the same database, schema, data and pool size, simple and filtered lists, single rows, embedded relations, keyset against offset pagination and writes, with one command (`rest/compare.sh`) and the results of a run (see the [comparison](../docs/comparison.md#throughput-against-postgrest)). When comparing tools, run each against the same database, the same queries and the same hardware, and report the setup alongside the numbers.
