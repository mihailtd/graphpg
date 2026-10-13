# PostgreSQL extensions

graphpg reads what a database's extensions add and serves it, without configuration:

- **Types**: `vector`, `geometry` / `geography`, `ltree`, `citext`, `hstore`, ranges.
- **Indexes**: search fields from pgvector `hnsw` / `ivfflat`, trigram, full-text and BM25 indexes.

Beyond types and indexes, graphpg **integrates** with popular extensions: it infers what they mean for the APIs, but only when the schema **uses** them.

## Integrations

| Integration | Extension | On when the exposed schemas have | What graphpg does |
| :--- | :--- | :--- | :--- |
| `timescaledb` | TimescaleDB | a hypertable | [Fast totals, hypertables and continuous aggregates described, advice](#timescaledb) |
| `pg_jsonschema` | pg_jsonschema | a `CHECK (jsonb_matches_schema(...))` | [The JSON Schema types the column](#pg_jsonschema) |
| `pgmq` | pgmq | a queue, named in `extensions.pgmq.queues` | [Queues as API](#pgmq) |
| `pgvectorscale` | pgvectorscale | a `diskann` index | [Semantic search on its indexes](#pgvectorscale-and-vectorchord) |
| `vectorchord` | VectorChord | a `vchordrq` / `vchordg` index | [Semantic search on its indexes](#pgvectorscale-and-vectorchord) |
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

## TimescaleDB

- **Fast totals.** A hypertable's rows live in its chunks, so its own statistics are empty. Estimated totals (`totalCount(estimate: true)`, REST `Prefer: count=planned` / `estimated`) fell back to an exact `count(*)` over every chunk; they now use TimescaleDB's `approximate_row_count()`. Exact totals stay exact.
- **Described.** A hypertable's type says it is one, its time column and its chunk interval, and that filtering on the time column reads only the chunks a request needs. A continuous aggregate's type says whether it shows only what is materialized or adds recent rows when read.
- **Advice.** A hypertable's unique indexes must include its time column, so many have no primary key, and then no single-row fields or stable cursors. `graphpg advise` says so and suggests the key to declare (`entities.<name>.primary_key: [device, time]`).

Bucketing and gap filling need nothing TimescaleDB-specific: grouped aggregates already truncate or bin time (`date_trunc` / `date_bin`, which chunk exclusion and indexes serve as well as `time_bucket`) and fill gaps (`fillGaps`), on hypertables and plain tables alike.

## pg_jsonschema

A `json` / `jsonb` column constrained by a JSON Schema has that schema as its type:

```sql
settings jsonb NOT NULL CHECK (jsonb_matches_schema('{"type": "object", "required": ["theme"], ...}', settings))
```

- **OpenAPI:** the column's property is the JSON Schema (without `$schema`), in rows and in create and update bodies, so typed clients get real types. A nullable column allows `null` too, which passes the check.
- **GraphQL:** the field stays `JSON` (GraphQL cannot express a JSON Schema), and its description states the check.
- **Errors:** a value the schema rejects fails the check: `<COLUMN>_INVALID` (REST `422`), like any check.

The constraint must be the call alone, on that column, with the schema as a literal. A check combining it with other conditions is documented, but does not type the column.

## pgmq

[pgmq](https://github.com/pgmq/pgmq) is a message queue inside PostgreSQL. Name the queues to serve; sending and reading are writes, so none are served by default:

```yaml
extensions:
  pgmq:
    queues: [jobs]          # or ["*"] for every queue
```

Each queue's operations become functions of their own, so GraphQL and REST serve them like any function: GRANTs decide who may call them (with `rbac.enabled`, the API shows only what the role may do), and limits, errors and authentication apply. For the queue `jobs`:

| GraphQL | REST | pgmq |
| :--- | :--- | :--- |
| `sendJobsMessage(message: JSON!, headers: JSON, delay: Int): [BigInt!]!` | `POST /rest/rpc/sendJobsMessage` | `send`: the id; visible after `delay` seconds |
| `sendJobsMessages(messages: [JSON!]!, headers: [JSON!], delay: Int)` | `POST /rest/rpc/sendJobsMessages` | `send_batch` |
| `readJobsMessages(visibilityTimeout: Int!, count: Int!, conditional: JSON)` | `POST /rest/rpc/readJobsMessages` | `read`: hides the messages for `visibilityTimeout` seconds |
| `popJobsMessages(count: Int)` | `POST /rest/rpc/popJobsMessages` | `pop`: reads and deletes at once |
| `deleteJobsMessages(messageIds: [BigInt!]!)` | `POST /rest/rpc/deleteJobsMessages` | `delete`: acknowledges |
| `archiveJobsMessages(messageIds: [BigInt!]!)` | `POST /rest/rpc/archiveJobsMessages` | `archive`: acknowledges, keeping a copy |
| `setJobsMessagesVisibility(messageIds: [BigInt!]!, visibilityTimeout: Int!)` | `POST /rest/rpc/setJobsMessagesVisibility` | `set_vt`: more time to process |
| `jobsQueueMetrics` (a query) | `GET /rest/rpc/jobsQueueMetrics` | `metrics`: length and message ages |

A message read and neither deleted nor archived within its visibility timeout is read again: process messages, then acknowledge them. A queue named in `queues` that does not exist stops the start.

## pgvectorscale and VectorChord

[pgvectorscale](https://github.com/timescale/pgvectorscale)'s `diskann` and [VectorChord](https://github.com/tensorchord/VectorChord)'s `vchordrq` / `vchordg` indexes serve pgvector's `vector` columns at scales where `hnsw` outgrows memory. A column with one of these indexes gets the same `<list>SemanticSearch` field and `POST /rest/<resource>/search/semantic` as with `hnsw`: the operator class (`vector_cosine_ops`, `vector_l2_ops`, `vector_ip_ops`) sets the distance it orders by and the score, and PostgreSQL's plan uses the index.

```sql
CREATE INDEX ON docs USING diskann (embedding vector_cosine_ops);
CREATE INDEX ON docs USING vchordrq (embedding vector_l2_ops);
```

Their tuning settings (`diskann.query_search_list_size`, `vchordrq.probes`, ...) are PostgreSQL settings: set them on the role or the database (`ALTER ROLE ... SET`), and every request uses them.

## Testing against the real extensions

The integration tests run against `ghcr.io/mihailtd/graphpg-test-pg:18` (`docker/test-pg`): PostgreSQL 18 with every extension above, plus PostGIS, pg_cron, hypopg and pg_partman. `docker compose up` starts it. Where an extension is missing (another `TEST_DATABASE_URL`), its tests are skipped, and the skip message names the extension.
