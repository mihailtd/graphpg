# Other databases and files: foreign data wrappers

graphpg serves one PostgreSQL database, and that database can reach others. PostgreSQL's **foreign data wrappers** (SQL/MED) make another PostgreSQL server, a MySQL or Oracle database, or a CSV or Parquet file look like a table: a *foreign table*. graphpg reads foreign tables like any other relation. With keys and relations declared in `graphpg.yaml`, they join your own tables in the GraphQL and REST APIs.

The join happens **inside PostgreSQL**. A request still compiles to one SQL statement. PostgreSQL's planner decides what to send to the other side (`postgres_fdw` pushes filters, ordering, limits, aggregates and joins between tables of the same server), and graphpg's filters, pagination, limits, error masking and partial results apply as usual. Nothing in graphpg joins rows itself.

This guide covers setup, what works, and what to watch out for. Every behavior it states is covered by `crates/graphpg/tests/foreign_data.rs`, which runs against `postgres_fdw` and `file_fdw`. The other wrappers it mentions are not tested by graphpg.

- [Quick start: another PostgreSQL database](#quick-start-another-postgresql-database)
- [Keys and relations](#keys-and-relations)
- [What works](#what-works)
- [Authorization: no Row-Level Security on foreign tables](#authorization-no-row-level-security-on-foreign-tables)
- [Performance](#performance)
- [Writes and transactions](#writes-and-transactions)
- [Errors](#errors)
- [Files: CSV, Parquet and object storage](#files-csv-parquet-and-object-storage)
- [Other databases](#other-databases)
- [Schema changes](#schema-changes)
- [Checklist](#checklist)

## Quick start: another PostgreSQL database

`postgres_fdw` ships with PostgreSQL and is available on most managed services. In the database graphpg connects to:

```sql
CREATE EXTENSION postgres_fdw;

CREATE SERVER billing FOREIGN DATA WRAPPER postgres_fdw
    OPTIONS (host 'billing-db.internal', port '5432', dbname 'billing',
             use_remote_estimate 'true', fetch_size '1000');

-- Which remote user each local role connects as (see "Authorization")
CREATE USER MAPPING FOR app_user SERVER billing
    OPTIONS (user 'billing_reader', password '...');

-- Foreign tables for the remote tables, in a schema of their own
CREATE SCHEMA billing;
IMPORT FOREIGN SCHEMA public LIMIT TO (invoices) FROM SERVER billing INTO billing;
ANALYZE billing.invoices;
```

List the schema in `database.schemas`. The remote primary key and foreign keys are imported (next section); declare what the remote side does not have, here a relation to a local table:

```yaml
database:
  schemas: [app, billing]

entities:
  billing.invoices:
    foreign_keys:
      invoices_customer:                 # a new name declares a virtual foreign key
        columns: [customer_id]
        references: { table: app.customers }
        field_name: customer
        reverse_field_name: invoices
```

Then, in one statement:

```graphql
{ customer(id: 1) { name invoicesConnection(filter: { status: { equalTo: "OPEN" } }) { totalCount nodes { amount } } } }
```

```bash
curl -g 'localhost:4000/rest/customers/1?include=invoices'
```

## Keys and relations

A foreign table has no indexes or constraints of its own: PostgreSQL rejects `PRIMARY KEY`, `UNIQUE` and `FOREIGN KEY` on it.

**From another PostgreSQL (`postgres_fdw`), graphpg imports them.** At startup it reads the remote table's primary key, unique constraints and foreign keys from the remote catalog, over the same foreign server and user mapping:

- Columns are matched by their remote names (a `column_name` option renames them), and must all be columns of the foreign table.
- A remote foreign key becomes a relation when the table it references is reached through a foreign table too (one in the same schema first). Foreign keys to tables graphpg cannot see are skipped.
- Keys declared in `graphpg.yaml` win: an imported key never replaces a declared one.
- The startup log lists each imported key. A server that cannot be read is logged as a warning and skipped; declare its keys instead.
- The remote catalog is read through temporary foreign tables on its `pg_catalog`, in a transaction rolled back afterwards: nothing is created, and nothing is needed beyond the user mapping (and the right to create temporary tables, which every role has by default).
- `extensions.integrations.postgres_fdw: off` turns the import off.

Otherwise (other wrappers, files, a remote key you want different), declare them in `graphpg.yaml`, as for views:

- `entities.<table>.primary_key` gives the type its key: the single-row lookup (`invoice(id:)`, `GET /rest/invoices/{id}`), Relay `id` / `node(id:)`, and the key of updates and deletes.
- `entities.<table>.foreign_keys.<new name>` declares a relation in either direction: from a foreign table to a local one, from a local table to a foreign one (on the local table's entry), or between two foreign tables. The reverse field is generated as for a real foreign key.

**These keys are promises, not constraints.** PostgreSQL does not check them. If the remote rows are not unique on the declared key, Relay IDs repeat, a single-row lookup can match two rows, and a to-one relation can return the wrong row. Declare the key the remote side really enforces (its primary key), and keep the two in step.

## What works

| | Another PostgreSQL (`postgres_fdw`) | A file (`file_fdw`) |
| :--- | :--- | :--- |
| Lists, connections, lookups, `node(id:)` | Yes | Yes |
| Relations in both directions, nested | Yes | Yes |
| Relation filters (`some` / `every` / `none`), aggregates | Yes | Yes |
| REST lists, filters, `include` | Yes | Yes |
| Create, update, delete | Yes (`pg_relation_is_updatable`) | No: no mutations or REST writes are generated |
| Upserts (`onConflict`) | No: they need a unique constraint, which a foreign table cannot have | No |
| `rowVersion` / `expectedVersion` | No: a foreign table has no `xmin` | No |
| Typed constraint error codes | For the declared primary key (`ID_TAKEN`); see [Errors](#errors) | — |

## Authorization: no Row-Level Security on foreign tables

PostgreSQL cannot enable Row-Level Security on a foreign table. Two things still apply:

- **`GRANT`s on the foreign table.** With `rbac.enabled`, graphpg prunes the APIs to them as for any table.
- **The user mapping.** The remote side sees the remote user the request's role is mapped to. graphpg runs requests as `auth.roles.authenticated` (and `auth.roles.anonymous`), so those roles need a user mapping, or a `FOR PUBLIC` one. The remote side's own policies then apply to that remote user, not to the end user, since claims do not cross the connection.

When rows must be filtered per user, use one of these.

**1. Reach the foreign table only through local rows (recommended).** Hide its root fields and keep its relations. Its rows are then only reachable under the local rows the request may see, which the local table's policies already filtered:

```yaml
entities:
  billing.invoices:
    primary_key: [id]
    omit_operations: [read, list, connection, create, update, delete]
    foreign_keys:
      invoices_customer:
        columns: [customer_id]
        references: { table: app.customers }
        field_name: customer
        reverse_field_name: invoices
```

`customer(id: 1) { invoices { amount } }` and `GET /rest/customers/1?include=invoices` still work, filtered by the policies on `customers`. There is no `invoices` root field or `/rest/invoices` collection, and `node(id:)` (or Federation `_entities`) answers `null` for an `Invoice` ID, so a forged global ID cannot skip the parent. This holds when the join key is what scopes the rows (an invoice belongs to the customer it references). Relation filters on the parent (`customers(filter: { invoices: { some: ... } })`) only reveal facts about the children of parents the request can already see.

**2. A filtering view.** When the foreign rows need roots of their own, expose a view instead of the foreign table, filtering on the request's claims, and hide the foreign table (`omit: true`):

```sql
CREATE VIEW billing.my_invoices WITH (security_barrier) AS
SELECT * FROM billing.invoices
WHERE customer_id = current_setting('request.jwt.claim.customer_id', true)::int;
```

`security_barrier` keeps a caller's own functions from seeing rows before the filter runs. The filter is pushed to the remote side like any other condition. Give the view its keys in `graphpg.yaml`.

## Performance

graphpg compiles nested selections into one statement with `LATERAL` subqueries. Across a foreign table, how PostgreSQL executes them depends on the relation's direction. This is what `EXPLAIN ANALYZE` shows for graphpg's statements:

| Relation | Plan | Cost |
| :--- | :--- | :--- |
| Foreign row → local row (`invoice { customer }`) | Remote scan, then a local index lookup per row | Cheap |
| Local rows → one foreign row each, to-one (`customer { country }`) | Hash join: the foreign table is read once | Cheap for small foreign tables |
| Local rows → foreign rows, to-many (`customers { invoices }`) | Nested loop: **one remote query per parent row** (`Foreign Scan ... loops=N`) | A network round trip per parent |
| Local rows → rows of a file, to-many | Nested loop: **the whole file is read once per parent row** | Grows with file size × parents |

A to-many relation is a subquery with a `LIMIT` per parent, which PostgreSQL cannot turn into one join. With `postgres_fdw` each remote query is parameterized and filtered on the remote side, so it uses the remote indexes. It is still one round trip per parent row. Keep to-many relations into foreign tables under parents with small pages, turn them around (query the foreign side and nest the local one), or batch them.

**Batched relations.** Mark the virtual foreign key `batch: true` and a list relation into the foreign table reads the children of a whole page of parents in **one** remote query: `WHERE customer_id = ANY ($1)`, the parents' keys, shipped to the remote side. It then numbers them per parent and keeps each parent's page locally:

```yaml
entities:
  billing.invoices:
    primary_key: [id]
    foreign_keys:
      invoices_customer:
        columns: [customer_id]
        references: { table: app.customers }
        batch: true
```

Measured on a loopback connection (no network latency), a page of 100 customers with their first 5 invoices took 2.7 ms batched against 7.5 ms per parent: 1 remote query instead of 100. Over a network the gap grows with the round-trip time. The trade-off: the per-parent limit cannot be shipped, so **every** child of the page's parents is transferred, then trimmed. Use it when parents have a bounded number of children (a customer's invoices), not when one parent can have millions. It applies to list relations under list parents (`customers { invoices }`, nested ones included), on a single-column key; connections and other shapes keep the per-parent query. For files it does not help: the file is read once either way.

Settings that matter:

- **`use_remote_estimate 'true'`** (on the server or the table). Without it, PostgreSQL plans with guesses for foreign tables, which can pick poor join orders.
- **`ANALYZE` the foreign tables** after importing them and when their data changes a lot. It samples the remote table to keep local statistics.
- **`fetch_size`** (rows per round trip, default 100): raise it for large lists and exports.
- **`limits.cost` does not know about the network.** It estimates rows, not round trips. Lower `limits.page_size.max` or `limits.depth` if remote relations get expensive.
- **`limits.statement_timeout`** cancels the remote query too, when the local statement is cancelled.
- **Materialize what is read often.** A materialized view over a foreign table has real indexes and statistics, at the cost of staleness until the next refresh. graphpg serves materialized views with keys from `graphpg.yaml`:

  ```sql
  CREATE MATERIALIZED VIEW billing.invoices_cached AS SELECT * FROM billing.invoices;
  CREATE UNIQUE INDEX ON billing.invoices_cached (id);
  CREATE INDEX ON billing.invoices_cached (customer_id);
  REFRESH MATERIALIZED VIEW CONCURRENTLY billing.invoices_cached;   -- on a schedule (pg_cron)
  ```

## Writes and transactions

`postgres_fdw` foreign tables are writable, and graphpg generates `create`, `update` and `delete` mutations and REST writes for them. Nested writes can span local and foreign tables. Keep in mind:

- **Not atomic across servers.** `postgres_fdw` opens a remote transaction with the local one and commits it when the local one commits, without two-phase commit. If the local commit fails after the remote one succeeded (or the server crashes in between), the two sides disagree. Keep writes that must be atomic on one server.
- **Isolation.** The remote transaction runs at `REPEATABLE READ` (or `SERIALIZABLE` when the local one does).
- **Read-only foreign tables.** To keep graphpg from writing, use `omit_operations: [create, update, delete]`, or grant only `SELECT`, or set `updatable 'false'` on the server or table.

## Errors

- **The declared primary key is typed.** `postgres_fdw` passes on a remote violation's SQLSTATE (`23505`) and its detail (`Key (id)=(1) already exists.`), but not the constraint's name. graphpg matches the detail's columns to the primary key declared in `graphpg.yaml` and answers as for a local key: `ID_TAKEN` with the input fields (REST: `409`). The code is listed in the type's `<Type>ErrorCode` enum and the OpenAPI document.
- **Other remote constraints are not typed.** A remote unique index, check or foreign key has no local definition, and its name doesn't come through, so `errors.map` can't match it by name either. The client gets a masked `INTERNAL_SERVER_ERROR` in prod (the full message in dev). Mapping the SQLSTATE in `errors.map` works, but applies to every table, local ones included. Virtual foreign keys get no code: the remote side cannot enforce a key that points at a local table.
- **A source that is down is `SOURCE_UNAVAILABLE`** (`503`). A wrapper that cannot reach its server reports a connection error (SQLSTATE class `08`). graphpg's own connection failing never does (that is an I/O error, not something the server reports), so graphpg knows a foreign source is down. The client gets the code without the remote's address, which is logged.
- **Remote errors are masked** like local ones (`errors.detail`).
- **Partial results.** A remote error about data (a bad value, a constraint, a permission) or an unreachable source fails only the root fields that read the foreign table; the others keep their data (see [partial results](graphql-api.md#partial-results)). A root list or connection is non-null, so ask with `onError: "NULL"` to keep the rest.

## Files: CSV, Parquet and object storage

| Wrapper | Reads | Notes |
| :--- | :--- | :--- |
| `file_fdw` (ships with PostgreSQL) | CSV and text files **on the database server's disk**, or a program's output (`program 'gunzip -c ...'`) | Creating one needs `pg_read_server_files` (`pg_execute_server_program` for `program`); often unavailable on managed services |
| [`parquet_fdw`](https://github.com/adjust/parquet_fdw) | Parquet files on the server's disk | Skips row groups using the files' statistics |
| [`pg_duckdb`](https://github.com/duckdb/pg_duckdb) | Parquet, CSV, JSON, Iceberg and Delta, on disk or in S3 / GCS / Azure | Functions such as `read_parquet(...)`: load them into a materialized view (below) |
| [Supabase Wrappers](https://github.com/supabase/wrappers) (S3) | CSV, JSON Lines and Parquet in S3 | Foreign tables |

Only `file_fdw` is tested by graphpg. The others reach graphpg as foreign tables or views, the same shapes.

Files are read-only (graphpg generates no writes for them) and have **no indexes**. `file_fdw` reads the whole file for every scan, and a to-many relation into a file reads it once per parent row (see [Performance](#performance)). Read a file directly only when it is small or rarely queried. Otherwise load it into a materialized view with indexes and refresh it when the file changes:

```sql
CREATE EXTENSION file_fdw;
CREATE SERVER files FOREIGN DATA WRAPPER file_fdw;
CREATE FOREIGN TABLE reference.countries_file (code text NOT NULL, name text NOT NULL)
    SERVER files OPTIONS (filename '/data/countries.csv', format 'csv', header 'true');
ANALYZE reference.countries_file;

CREATE MATERIALIZED VIEW reference.countries AS SELECT * FROM reference.countries_file;
CREATE UNIQUE INDEX ON reference.countries (code);
REFRESH MATERIALIZED VIEW CONCURRENTLY reference.countries;   -- when the file changes
```

```yaml
entities:
  reference.countries_file: { omit: true }      # serve the cached copy only
  reference.countries:
    primary_key: [code]
  app.customers:
    foreign_keys:
      customers_country:
        columns: [country_code]
        references: { table: reference.countries, columns: [code] }
        field_name: country
        reverse_field_name: customers
```

With `pg_duckdb`, **load the files into a materialized view**; do not serve a plain view over them. A statement that reads a DuckDB function runs entirely in DuckDB, and DuckDB does not parse the SQL/JSON constructors (`JSON_OBJECT(... RETURNING json)`) graphpg's statements are built with, so every request on such a view fails. A materialized view is a PostgreSQL table: it takes indexes and keys, and only its refresh runs in DuckDB. Columns are read as `r['name']` (pg_duckdb 1.x):

```sql
CREATE MATERIALIZED VIEW analytics.sales AS
SELECT r['id']::bigint AS id, r['customer_id']::int AS customer_id, r['total']::numeric AS total
FROM read_parquet('s3://bucket/sales/*.parquet') r;
CREATE UNIQUE INDEX ON analytics.sales (id);   -- a key, and REFRESH ... CONCURRENTLY
REFRESH MATERIALIZED VIEW CONCURRENTLY analytics.sales;   -- when the files change (pg_cron)
```

pg_duckdb must be in `shared_preload_libraries`. Its DuckDB threads can crash the server alongside other preloaded extensions built with pgrx (VectorChord 1.1 with pg_duckdb 1.1.1, on PostgreSQL 18); test the combination before production.

Materialized views lose `NOT NULL`, so their fields are nullable in the schema.

## Other databases

`mysql_fdw`, `oracle_fdw`, `tds_fdw` (SQL Server, Sybase), `mongo_fdw`, `redis_fdw` and others produce foreign tables, which graphpg serves the same way. How much they push down (filters, joins, sorts, limits), whether they can write, and how they map types differs from one wrapper to the next. Check with `EXPLAIN (VERBOSE)` that filters reach the remote side. Managed services offer different sets: Amazon RDS, for example, offers `postgres_fdw`, `mysql_fdw`, `oracle_fdw` and `tds_fdw`.

## Schema changes

- **Local definitions.** `CREATE FOREIGN TABLE`, `IMPORT FOREIGN SCHEMA` and `ALTER FOREIGN TABLE` are DDL: in dev mode the watcher reloads the schema, and pinned schemas (`graphpg schema export --check`) catch the change in CI.
- **Remote changes are invisible.** A column added or dropped on the remote side does not change the foreign table. Queries that select a dropped column fail at runtime. Re-run `IMPORT FOREIGN SCHEMA` (or `ALTER FOREIGN TABLE`) as part of the remote side's migrations.

## Checklist

`graphpg advise` (and the startup log) checks most of these for you.

- [ ] Foreign tables in a schema of their own, listed in `database.schemas`
- [ ] Keys: imported from a `postgres_fdw` server, or `primary_key` and relations declared in `graphpg.yaml`, matching what the remote side enforces
- [ ] User mappings for `auth.roles.authenticated` / `anonymous` (or `PUBLIC`), with a least-privilege remote user
- [ ] Per-user rows: roots hidden and reached through local rows, or a `security_barrier` view
- [ ] `use_remote_estimate`, `ANALYZE` and `fetch_size` set; `EXPLAIN` checked for the heavy queries
- [ ] Files and hot remote tables cached in materialized views with indexes, refreshed on a schedule
- [ ] Writes that must be atomic kept on one server; read-only foreign tables made read-only
- [ ] Remote migrations re-import the foreign tables
