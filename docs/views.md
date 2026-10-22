# Views

Views and materialized views are exposed like tables: each is a GraphQL type with list and connection fields, a REST resource and a part of the OpenAPI document. graphpg reads from the database what it can tell about a view (whether PostgreSQL can write through it, its key, its comment) and needs no configuration for most of them. This page describes what a view gets out of the box, what to declare when the database cannot say it, and the cases to watch.

- [What a view gets](#what-a-view-gets)
- [Keys](#keys)
- [Writing through a view](#writing-through-a-view)
- [Relations](#relations)
- [Names and descriptions](#names-and-descriptions)
- [Security](#security)
- [Configuration reference](#configuration-reference)
- [Gotchas](#gotchas)

## What a view gets

| | View | Materialized view |
| --- | --- | --- |
| Lists, connections, filters, `orderBy`, aggregates, grouped aggregates | Always | Always |
| REST collection, `/aggregate`, `/search`, OpenAPI | Always | Always |
| A key: `Node`, `id`, `databaseId`, single-row lookup, `GET /rest/<view>/{key}`, stable cursors | When [detected or declared](#keys) | When [detected or declared](#keys) |
| Creates | When PostgreSQL can insert through it | Never |
| Updates and deletes by key | When PostgreSQL can update or delete through it, and it has a key | Never |
| Relations to and from tables | [Declared](#relations) in `graphpg.yaml` | [Declared](#relations) in `graphpg.yaml` |
| Search methods (full text, trigram, vector) | No (a view has no indexes) | From its indexes, as for a table |
| `rowVersion` and `expectedVersion` | With `version_column` | Not applicable (read-only) |
| Live queries ([subscriptions](subscriptions.md)) | Yes, run with each user's claims | Yes, and shared between users when nothing else in the query depends on the claims |
| [Change events](events.md) | No (a view cannot have the triggers) | No |

A function returning a view's rows (`RETURNS SETOF my_view`) is typed with the view's type, and a function taking a view's row as its first argument is a computed column of the view, as for tables.

## Keys

PostgreSQL cannot attach a primary key to a view, but a key is what gives rows an identity: Relay ids and `node(id:)`, single-row lookups, cursors that mark an exact position, and updates and deletes. graphpg looks for one in this order, and the first that applies wins:

1. **Declared in the view's comment.** Versioned with the migration that creates the view:
   ```sql
   COMMENT ON VIEW programme_adherence IS E'Adherence of each client to their programme\n@primaryKey patient_id';
   ```
   Several columns are separated by commas (`@primaryKey customer_id, number`). The hint line is not part of the description. The same hint declares the key of a foreign table, or of a table without a primary key.
2. **A materialized view's unique index**, such as the one `REFRESH MATERIALIZED VIEW CONCURRENTLY` needs. With several, the one with the fewest columns; partial indexes and indexes on expressions do not count.
   ```sql
   CREATE MATERIALIZED VIEW client_index AS SELECT client_id, name FROM clients;
   CREATE UNIQUE INDEX ON client_index (client_id);
   ```
3. **A simple view's base relation's key.** A view that PostgreSQL updates automatically has a single relation in `FROM` and no join, `GROUP BY`, `HAVING`, `DISTINCT`, aggregate, window or set-returning function, `UNION` / `INTERSECT` / `EXCEPT`, `LIMIT` or `OFFSET`. It can only filter its relation's rows, so the relation's key is unique in the view too. When the view selects every column of that key as a plain column, under any names, those columns are the view's key. graphpg reads which relation column each view column is from the view's stored query, so renamed columns count:
   ```sql
   CREATE VIEW active_clients AS SELECT client_id, name FROM clients WHERE NOT archived;  -- key: client_id
   CREATE VIEW client_refs AS SELECT client_id AS ref, name FROM clients;                -- key: ref
   CREATE VIEW active_a_clients AS SELECT * FROM active_clients WHERE name LIKE 'A%';    -- key: client_id
   ```
   A view over such a view has the same key, and so does a view over a view whose key a comment declares.

`entities.<view>.primary_key` in `graphpg.yaml` overrides any of them. A view without a key is still listed, filtered, ordered, aggregated and paged, but has no single-row lookup, no Relay id, and no updates or deletes.

A key column keeps its own field: a `patient_id` key is `patientId` in the type, in filters, conditions and `orderBy` (`PATIENT_ID_ASC`), and `databaseId` too. A key column named `id` gives that name to the Relay `id` and is read as `databaseId`. The single-row lookup takes the key columns by their field names: `clientRef(ref: 1)`, `GET /rest/client_refs/1`.

## Writing through a view

graphpg asks PostgreSQL what it can write through each view (`pg_relation_is_updatable`) and generates exactly those operations, in GraphQL, REST and OpenAPI:

- **Automatically updatable views** (the simple views above) can be inserted into, updated and deleted from. PostgreSQL writes the base relation. Add `WITH CHECK OPTION` to refuse writes that would produce rows the view does not show.
- **Views with `INSTEAD OF` triggers**, or with `DO INSTEAD` rules, can be written in whatever ways the triggers or rules handle. This is how a join or a grouping is made writable.
- **Materialized views and other views** are read-only: they have no create, update or delete.

Creates need no key. Updates and deletes address a row by its key, so a writable view without one gets neither until a key is detected or declared. For `expectedVersion` (optimistic concurrency), name the column a trigger maintains in `entities.<view>.version_column`: views have no `xmin`.

## Relations

A view has no foreign keys of its own, so it is related to tables by declaring virtual foreign keys in `graphpg.yaml`. They work like real ones: a to-one field on the view, a to-many field and connection on the referenced type, and relation filters both ways.

```yaml
entities:
  active_clients:
    foreign_keys:
      active_clients_region:            # a new name declares a virtual foreign key
        columns: [region_id]
        references: { table: regions }  # columns default to its key
        field_name: region
        reverse_field_name: activeClients
```

A view can also be the referenced side: a table's virtual foreign key may reference a view, by its key or by the columns you name in `references.columns`. See [Views with declared keys](schema-mapping.md#views-with-declared-keys-json_table) for a view over `JSON_TABLE` rows, related to their parent table as child rows.

## Names and descriptions

A view is named like a table: `client_refs` is the type `ClientRef`, with `clientRefs`, `clientRef` and `clientRefsConnection` root fields and the REST resource `/rest/client_refs`. Its comment is the type's description, and [comment hints](configuration.md#comment-hints) apply: `@name` names the type (or a column's field), `@deprecated` deprecates it. `entities.<view>` renames it, hides it (`omit: true`) or some of its operations (`omit_operations`), and `columns` renames or hides its columns, as for tables.

## Security

- **Use `security_invoker` with Row-Level Security.** A view reads its relations with its owner's privileges by default, and Row-Level Security does not apply to a table's owner. A view created by the role that owns the tables therefore returns every row, whatever the policies say. With `WITH (security_invoker = true)` (PostgreSQL 15 and later), the view reads them as the role of the request, so the policies of `auth.roles.authenticated` apply:
  ```sql
  CREATE VIEW my_documents WITH (security_invoker = true) AS
  SELECT id, title, updated_at FROM documents;
  ```
  The request's role then needs privileges on the view and on the relations it reads.
- **Use `security_barrier` for a view that filters rows itself** (`WHERE owner = current_setting(...)`), so a caller's functions cannot see rows before the filter runs. This is the pattern for [foreign tables](foreign-data.md#authorization-no-row-level-security-on-foreign-tables), which Row-Level Security cannot protect.
- **A view is exposed when a request role has a privilege on it** (`GRANT SELECT ON my_documents TO authenticated`), or the login role has one. With [`rbac.enabled`](federation-and-rbac.md), its operations follow that role's privileges, as for tables.

## Configuration reference

Everything below is optional, for what the database cannot say:

```yaml
entities:
  programme_adherence:            # a view or materialized view, by its SQL name
    primary_key: [patient_id]     # its key, overriding a detected or commented one
    version_column: revision      # the column a trigger bumps on every write (expectedVersion)
    type_name: Adherence          # names, as for tables
    omit_operations: [delete]     # create, update, delete, read, list, connection
    columns:
      internal_score: { omit: true }
    foreign_keys:
      programme_adherence_patient:
        columns: [patient_id]
        references: { table: patients }
        field_name: patient
        reverse_field_name: adherence
```

## Gotchas

- **Keys are not detected for views that combine rows**, such as joins, groupings, `DISTINCT`, unnested arrays or `LATERAL`, even when a column is unique in practice. PostgreSQL does not say so, and graphpg does not guess. Declare those keys with `@primaryKey` or `primary_key`.
- **Views made writable by rules or triggers get no detected key.** A view with a `DO INSTEAD` rule on updates or deletes, or with `INSTEAD OF` triggers, may be a join whose rows repeat, so its base relation's key is not taken. Declare its key if its rows are unique.
- **A declared key is not checked.** If a `@primaryKey` or `primary_key` names columns that repeat, Relay ids collide, lookups return an arbitrary one of the rows, cursor pages skip or repeat rows, and an update or delete by key can affect several rows.
- **A unique index does not exclude `NULL`.** Several rows of a materialized view may have `NULL` in its unique index's columns, unless the index is `NULLS NOT DISTINCT`. Make sure the key columns are never `NULL` (a column from the outer side of a join can be), or declare another key.
- **Keys declared only in `graphpg.yaml` are not inherited.** A simple view over a foreign table, or over a relation whose key comes from `graphpg.yaml` or is imported from a `postgres_fdw` server, does not take that key: detection runs before the configuration is applied. Declare the view's key too. A key declared with `@primaryKey` is inherited.
- **Inheritance parents.** A primary key on a table with inheritance children (not a partitioned table) is not unique across the children, and a view over the parent shows their rows too. Partitioned tables are not affected.
- **View columns are nullable.** PostgreSQL records no `NOT NULL` for a view's or materialized view's columns, so their fields are nullable in the schema. `id` and `databaseId` are not.
- **A view without a key pages by its first column.** Its cursors mark a position in that column alone, so rows with the same value can be skipped or repeated between pages. Give the view a key, or page with an `orderBy` that is unique.
- **Materialized views are as fresh as their last refresh.** Refresh them on a schedule, for example with `pg_cron`. `REFRESH ... CONCURRENTLY` needs a unique index, which is also their detected key.
- **Upgrading from 1.2.** Views with a detected key now implement `Node` and gain lookups, and updates and deletes when they are writable. The generated SDL and OpenAPI document change: re-export pinned schemas (`graphpg schema export`) before deploying with `export.pin: fail`, or set `omit_operations` on the views whose new operations you do not want.
