# Performance and operations

graphpg generates one SQL statement per request, for GraphQL and REST alike (lateral joins and JSON aggregation, so nested selections don't cause N+1 queries). How fast it runs depends on the indexes and settings of your database, and what a request may cost is bounded by the [safety limits](configuration.md#safety-limits).

## Database-level tuning

graphpg is generic. It reads what the database has (tables, constraints, indexes and their operator classes, extensions) and generates SQL that PostgreSQL can serve with them. It never creates indexes or changes settings, and it doesn't prefer one migration tool over another. Everything below is optional. Apply it with whatever manages your schema; graphpg picks it up on its next introspection (index changes) or connection (settings).

Settings can be set for a database (`ALTER DATABASE app SET ...`), for the role graphpg connects as (`ALTER ROLE api SET ...`), or in `postgresql.conf`.

### Prepared statements

Values, page sizes (`first`, `last`), `offset` and search limits are bound as parameters. The SQL of an operation depends only on its shape: the fields selected and which arguments are passed. Each pooled connection caches its prepared statements, so paging with different sizes reuses one statement and skips parsing and planning.

After five executions of a statement, PostgreSQL may switch to a *generic plan*, which doesn't know the parameter values. It keeps planning per execution when the generic plan is estimated to cost more. That is usually the case for ordered, limited queries, because a generic plan must assume a much larger `LIMIT`. To force one behaviour, set `plan_cache_mode` (`auto`, `force_custom_plan` or `force_generic_plan`) for the database or graphpg's role.

### Connections and pagination

| To get | Do this at the database level |
| :--- | :--- |
| Each page is an index seek, not a sort of all matching rows | An index matching the order, key last: `orderBy: [CREATED_AT_DESC]` on a table keyed by `id` → `CREATE INDEX ON messages (created_at DESC, id)`. `last`/`before` pages read the same index backwards. |
| Fast pages of a nested connection or a `condition` | Equality columns first: `(forum_id, created_at DESC, id)` serves `forum.messagesConnection(orderBy: [CREATED_AT_DESC])` |
| Mixed directions (`[FEATURED_DESC, CREATED_AT_ASC]`) | An index with the same mixed directions: `(featured DESC, created_at, id)` |
| Nullable sort columns | Nothing: graphpg uses PostgreSQL's default NULL order (last ascending, first descending), which plain indexes already have. An index declared with other `NULLS FIRST/LAST` is not used for that order. |

`offset` skips rows by reading them, so its cost grows with the offset; cursors don't. `totalCount`, `aggregates` and `groupedAggregates` read every matching row (in one scan together), so their cost follows the `filter`: indexes on the filtered columns keep it down.

The exception is a root connection's `distinctCount`. PostgreSQL computes `count(DISTINCT x)` by sorting the values, in one process. graphpg instead counts the distinct values (`SELECT count(*) FROM (SELECT DISTINCT x ...)`), a hash aggregate that parallel workers share. It costs one more scan, but over 10M rows with `totalCount` it runs in 0.4 s instead of 1.6 s (PostgreSQL 18, 24 cores). Grouped and nested distinct counts keep `count(DISTINCT x)`, which is faster per group. Percentiles (`median`, `percentile`) sort their group's values in one process; on large tables, filter them down, or precompute them in a materialized view.

### Fuzzy search (`pg_trgm`)

| To get | Do this at the database level |
| :--- | :--- |
| Fuzzy search on a column at all | A trigram index on it: `CREATE INDEX ON users USING gin (name gin_trgm_ops)` (or `gist_trgm_ops`) |
| The best matches without sorting every match | A **GiST** index (`gist_trgm_ops`). It returns rows nearest-first (`<->>`) and stops after `first + 1`. A **GIN** index finds every match, which is then sorted: faster to build and update, and fine while a query matches few rows, but slower for short, broad typeahead queries on large tables. |
| Looser or stricter matching when `minSimilarity` is not passed | `pg_trgm.word_similarity_threshold` (default 0.6), e.g. `ALTER DATABASE app SET pg_trgm.word_similarity_threshold = 0.4` |

### Semantic search (`pgvector`)

| To get | Do this at the database level |
| :--- | :--- |
| Semantic search on a column at all | A vector index: `CREATE INDEX ON docs USING hnsw (embedding vector_cosine_ops)` (or `ivfflat`, pgvectorscale `diskann`, VectorChord `vchordrq` / `vchordg`). graphpg uses the index's operator class (`vector_cosine_ops`, `vector_l2_ops`, `vector_ip_ops`) for both the distance it orders by and the score it returns. |
| Up to `first` hits when `first` is above 40 | An HNSW scan returns at most `hnsw.ef_search` rows (default 40): raise it, e.g. `ALTER DATABASE app SET hnsw.ef_search = 100` |
| `first` hits even when `condition`/`filter` excludes many rows | pgvector 0.8+: `hnsw.iterative_scan = relaxed_order` keeps scanning until enough rows pass the filter (bounded by `hnsw.max_scan_tuples`). graphpg re-sorts the hits by score, so the relaxed order is safe. For IVFFlat use `ivfflat.probes` and `ivfflat.iterative_scan`. |
| Exact results | No index: every row is compared, which is exact but reads the whole table |

### BM25 search (ParadeDB) and hybrid search

- BM25 search needs a ParadeDB `bm25` index. graphpg orders by `paradedb.score`, which the index serves.
- Hybrid search is offered when a table has a vector index and a lexical one (BM25, or trigram if there's no BM25), and a single-column primary key. Each ranking takes its 50 best candidates (or `first + 1` if more) among the rows passing `condition`/`filter`, so the settings above for each ranking apply to it too.

### Full-text search

- A GIN or GiST index with `tsvector_ops` on a `tsvector` column, or on a `to_tsvector(...)` expression, gives a `<list>FullTextSearch` field. Queries use the document's own text search configuration (`to_tsvector('english', ...)`), else `default_text_search_config`: set it per database (`ALTER DATABASE app SET default_text_search_config = 'pg_catalog.french'`).
- `highlight` (`ts_headline`) needs the text the document is built from: it is read from the index expression or from a generated column's expression (`GENERATED ALWAYS AS (to_tsvector(...)) STORED`). A document column a trigger maintains has no highlights.
- `globalSearch` searches each table by its full-text index, or its trigram index when it has none, and merges the hits by score.

### Isolation, retries and locks

- Mutations run at the database's `default_transaction_isolation` (per database or role: `ALTER ROLE app SET default_transaction_isolation = 'serializable'`). Operations failing with a serialization failure (`40001`) or a deadlock (`40P01`) are run again, up to 10 times with jittered backoff, when they ran in a transaction of their own.
- `expectedVersion` locks the row (`SELECT ... FOR UPDATE`) before writing it.
- Statement timeouts and other settings are the database's: the engine sets none.

### Temporal keys

A `PERIOD` foreign key is served by a GiST index on the referencing columns and period (`CREATE INDEX ON entries USING gist (account_id, valid_at)`, `btree_gist` for the scalar columns); the `WITHOUT OVERLAPS` key's own GiST index serves `asOf` lookups and `history`.

## Operations

- **Request cache**: a document is parsed and validated once; an operation run again with the same variables reuses its compiled SQL. Both are bounded and emptied on schema reloads. (Compiled statements depend on variable values — enum values, which fields an input sets, list lengths — so they are not shared across values.)
- **Estimated counts**: `totalCount(estimate: true)` reads table statistics (`reltuples`, summed over partitions) without conditions and the planner's `EXPLAIN` estimate with them; nested connections count exactly.
- **Streaming exports**: `POST /export` with a query whose single root field is a connection streams its nodes as NDJSON or CSV (`"format": "csv"`), fetched from a portal 1,000 rows at a time.
- **COPY imports**: `POST /import/<Type>` with `Create<Type>Input` rows, one JSON object per line: checked like mutation input, streamed into a binary `COPY` of a staging table and inserted in one statement, in one transaction. Constraint violations are reported as in mutations.
- **Read replicas**: `DATABASE_REPLICA_URLS` (comma-separated). Replicas that are standbys (`pg_is_in_recovery()`, checked every 10 s) serve queries round-robin; mutations run on the primary and return its WAL position in `X-WAL-Position`. A client sending it back as `X-Min-WAL-Position` reads from a replica only once it has replayed it (`pg_last_wal_replay_lsn()`), else from the primary.
- **REST**: lists use the same keyset pagination and statements as GraphQL connections, so the same indexes serve both (`GET /rest/messages?order=-createdAt&first=20` wants `(created_at DESC, id)`). `X-Total-Count` is opt-in (`Prefer: count=exact`) because it scans every matching row; aggregates (`/aggregate`) read the filtered rows once, whatever the number of functions and groups; `include` paths are nested lateral joins and cost like GraphQL's nested connections (counted by `limits.cost`).
- **Limits**: `limits.statement_timeout` is set on every pooled connection, so a runaway statement is cancelled (`STATEMENT_TIMEOUT`) instead of holding a connection; `limits.cost` and `limits.depth` reject expensive operations before any SQL is compiled.
- **Observability**: statements carry sqlcommenter comments (`/*application='graphpg',graphql_operation='GetUsers',graphql_type='query'*/`, or `rest_resource='posts',rest_method='GET'` for REST; `database.sql_comments: false` turns them off). GraphQL requests run in `graphql.parse`, `graphql.plan`, `graphql.compile` and `graphql.execute` tracing spans, REST requests in `rest.execute`, exported over OTLP with the `otel` cargo feature and `OTEL_EXPORTER_OTLP_ENDPOINT`.
- **Startup advice**: at startup (and with the `advise` subcommand) the engine reports foreign keys no index leads with, tables without a primary key, and orderable columns no index leads with.
- **Comment hints**: optional lines in SQL comments, versioned with migrations: `@name <GraphQLName>` names a table's type, a column's field, a function's field or a type; `@deprecated [reason]` deprecates a column's field, a function's fields, or a table's root fields and the relation fields leading to it. The config (`deprecated:` on entities, columns and functions) wins over them.
