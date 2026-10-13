# PostgreSQL extensions

graphpg reads what a database's extensions add and serves it, without configuration:

- **Types**: `vector`, `geometry` / `geography`, `ltree`, `citext`, `hstore`, ranges.
- **Indexes**: search fields from pgvector `hnsw` / `ivfflat`, trigram, full-text and BM25 indexes.

Beyond types and indexes, graphpg **integrates** with popular extensions: it infers what they mean for the APIs, but only when the schema **uses** them.

## Integrations

| Integration | Extension | On when the exposed schemas have | What graphpg does |
| :--- | :--- | :--- | :--- |
| `timescaledb` | TimescaleDB | a hypertable | (#170) |
| `pg_jsonschema` | pg_jsonschema | a `CHECK (jsonb_matches_schema(...))` | (#171) |
| `pgmq` | pgmq | a queue | (#172) |
| `pgvectorscale` | pgvectorscale | a `diskann` index | (#173) |
| `vectorchord` | VectorChord | a `vchordrq` / `vchordg` index | (#173) |
| `pgroonga` | PGroonga | a `pgroonga` index | (#176) |
| `pg_stat_statements` | pg_stat_statements | (installed) | (#174) |
| `pg_duckdb` | pg_duckdb | (installed) | (#175) |

An extension that is installed but unused changes nothing.

At startup graphpg logs each integration that is on, and what turned it on:

```
INFO extension integration on: 3 hypertables integration="timescaledb" version="2.30.2"
```

`graphpg advise` lists them too.

### Switching one off

```yaml
extensions:
  integrations:
    timescaledb: off      # auto (the default) | off
```

Off, the integration does nothing, as if the extension were not installed. An unknown name is an error, listing the known ones.

## Testing against the real extensions

The integration tests run against `ghcr.io/mihailtd/graphpg-test-pg:18` (`docker/test-pg`): PostgreSQL 18 with every extension above, plus PostGIS, pg_cron, hypopg and pg_partman. `docker compose up` starts it. Where an extension is missing (another `TEST_DATABASE_URL`), its tests are skipped, and the skip message names the extension.
