# Schema mapping

How a PostgreSQL database becomes a GraphQL schema. graphpg reads the system catalogs (`pg_class`, `pg_attribute`, `pg_type`, `pg_proc`, `pg_constraint`, `pg_index`) in a fixed number of catalog-wide queries, not one per table, and generates the schema and its SQL from what it finds. Schema design (tables, constraints, types, functions, indexes) stays in SQL; [`graphpg.yaml`](configuration.md) only adjusts the GraphQL surface.

## Overview

| PostgreSQL Feature | Catalog Inspection | GraphQL Representation | Query Compilation | Mutation Handling |
| :--- | :--- | :--- | :--- | :--- |
| **Composite Types** (`CREATE TYPE ... AS (...)`) | `pg_type.typtype = 'c'`, attributes from `pg_attribute` | `type <Name>` & `input <Name>Input` (nested, arrays) | Selected attributes only: `(t."col")."attr"` | `json_populate_record` |
| **Enums** (`CREATE TYPE ... AS ENUM`, `CHECK (col IN (...))`) | `pg_enum`; single-column `CHECK` value lists on text columns | GraphQL `enum` (`in-progress` → `IN_PROGRESS`), `<Enum>Filter` | Labels mapped to values with `CASE` | Values mapped back to labels |
| **Range Types** (`tstzrange`, `numrange`, etc.) | `pg_type.typtype = 'r'` | `<Range>FilterInput` | Native operators: `&&`, `@>`, `<<`, `>>` | Direct interval literal / driver casting |
| **Generated Columns** (`GENERATED ALWAYS AS ... STORED`) | `pg_attribute.attgenerated = 's'` | Read-only output fields | Bare column select (pre-computed on disk) | Automatically excluded from Create & Update inputs |
| **Functions** (any `LANGUAGE`) | `pg_proc` (`prokind = 'f'`), return and argument types, volatility | Root fields typed by what they return (`User`, a `UserConnection`, scalars, enums, composites, `<Func>Row`); functions of a row are fields of its type | `FROM schema."func"(args)`, inlinable by PostgreSQL | `VOLATILE` functions are mutations |
| **Exclusive Arcs** (`CHECK num_nonnulls(...) = 1`) | `pg_constraint.conbin` regex parsing | GraphQL `union <TargetUnion>` | `CASE WHEN ... THEN (SELECT JSON_OBJECT(...))` | Validated by PostgreSQL check constraint |
| **Views & Materialized Views** | `pg_class.relkind IN ('v', 'm')` (also foreign tables; partitions are hidden behind their partitioned table) | Object types with list/connection root fields; keys and relations declared in `graphpg.yaml` | Same as tables | Only for updatable views (`pg_relation_is_updatable`) |
| **Virtual JSON_TABLE Views** | View + key/foreign key declared in `graphpg.yaml` | Relational 1:M child collections | Standard `LEFT JOIN LATERAL` | Managed via parent table JSONB mutations |
| **Composite Keys** | Multi-column `pg_constraint.conkey`/`confkey` | One relation per foreign key; lookups/updates/deletes take one argument per key column | Multi-column join conditions, row-value cursors | Rows addressed by every key column |
| **Scalar & Array Types** | `pg_type` (domains as scalars of their own, over their base type) | A scalar per type (`BigInt`, `Decimal`, `Datetime`, `LocalDatetime`, `Interval`, `Base64`, `InternetAddress`, ...); ranges as objects; arrays as lists | Output in each scalar's format (exact numbers as strings, non-finite floats as null, ISO 8601 durations, base64) | Inputs checked against their scalar before any SQL runs |
| **Native UUIDv7** | `uuidv7()` default | `UUID` scalar (`databaseId`), `ID!` Relay global ID | Standard B-Tree equality/range | Generated natively in database |
| **GraphQL Variables & Fragments** | `apollo-compiler` AST normalization | Named & inline fragment spreads | Inlined into lateral joins (zero N+1) | Full parameterized variables for queries & mutations |
| **Dynamic `orderBy` on Connections** | Column list from `pg_attribute` via catalog | Per-table `FooOrderBy` enum (`NATURAL`, `PRIMARY_KEY_ASC/DESC`, `<COL>_ASC/DESC`) | Keyset pagination: `ORDER BY` the columns plus the key, cursor conditions as row comparisons | Read-only sort; mutations unaffected |
| **Search Indexes** (`pg_trgm`, `pgvector`, ParadeDB BM25) | Index access method and operator class from `pg_index`/`pg_opclass` | `<list>FuzzySearch`, `<list>SemanticSearch`, `<list>Bm25Search`, `<list>HybridSearch` root fields: best matches, no pagination | Ordered by the index's own operator, `LIMIT first + 1` | — |
| **Relay Node Interface & Global IDs** | Single-column PK inspection | `interface Node { id: ID! }`, `databaseId`, `Query.node(id: ID!): Node` | Nanosecond in-memory routing to table + PK index seek | Accepts both Global ID & raw PK in mutations & queries |
| **Event Trigger DDL Watcher** | `ddl_command_end`, `sql_drop` | Zero-downtime hot reload | `LISTEN graphpg_watch` notification | Auto-exports `schema.graphql` and the config JSON Schema |
| **Engine Config** (`graphpg.yaml`) | Keyed by relation/column/constraint/function/type name | Rename, describe or hide types, fields and enum values; pick root operations; mount functions | — | — |
| **Extension Objects** | `pg_depend.deptype = 'e'` | Hidden unless the extension is listed in `extensions.expose` | — | — |
| **Ordered Mutations** | — | Root mutation fields | One statement per field, payload re-read by key | Run in order in one transaction; any failure rolls back all |
| **Constraint-Aware Errors** | Every unique/primary/foreign key, `CHECK` and exclusion constraint and unique index (`pg_constraint`, `pg_index`) | `enum <Type>ErrorCode` (`EMAIL_TAKEN`, `TEAM_NOT_FOUND`, `REFERENCED_BY_PLAYERS`, ...) | — | Violations reported with the code and the input fields (`extensions.fields`) |
| **Type Modifiers** (`varchar(n)`, `numeric(p,s)`) | `pg_attribute.atttypmod` | Documented on fields ("At most 20 characters.") | — | Checked before any SQL runs (`<FIELD>_TOO_LONG`, `<FIELD>_OUT_OF_RANGE`) |
| **Temporal Keys** (PG 18 `WITHOUT OVERLAPS`, `PERIOD` foreign keys) | `pg_constraint.conperiod` | `asOf` arguments, `history` / `historyConnection` fields | Relations join on overlapping periods; `period @> $asOf` | Versions addressed by their full key |
| **Row Versions** | `xmin` (every table), or a configured version column | `rowVersion`, `expectedVersion` on single-row updates/deletes | Read by key | The row is locked and checked first (`STALE_VERSION`) |
| **`RETURNING OLD`** (PG 18) | `server_version_num` | `previous<Type>` payload fields of updates and upserts | `to_json(old)` | Same statement as the write |
| **Hierarchies** | Foreign keys referencing their own table; `ltree` columns | `ancestors` / `descendants` (`maxDepth`, `filter`, `orderBy`); `LTreeFilter` | `WITH RECURSIVE ... CYCLE`; `@>` / `<@` on paths | — |
| **Full-Text Search** | GIN/GiST `tsvector_ops` indexes on columns or `to_tsvector(...)` expressions | `<list>FullTextSearch` (with `highlight`), `FullTextFilter`, `globalSearch` across tables | `websearch_to_tsquery`, `ts_rank`, `ts_headline` | — |
| **PostGIS** | `geometry` / `geography` columns and their GiST/SP-GiST indexes | `GeoJSON` scalar, `GeoJSONFilter`, `<list>NearestSearch` | `ST_Intersects`, `ST_DWithin`, KNN `<->` | Written from GeoJSON |
| **Multiranges** | `tstzmultirange`, `int4multirange`, ... | Lists of ranges, `<Scalar>MultirangeFilter` | `@>`, `&&`, `<@` | Written from lists of ranges |


## Composite types

`CREATE TYPE ... AS (...)` gives a column a typed, structured value instead of a dozen flattened columns or an opaque `jsonb` blob.

```sql
CREATE TYPE address AS (street text, city text, postal_code text, country text);
ALTER TABLE customers ADD COLUMN billing_address address;
```

1. **Detection.** Columns whose type (or domain base type) has `pg_type.typtype = 'c'` are composite. Attributes come from `pg_attribute`, including nested composites, enums and arrays. Only types used by the schema's columns are exposed; they may live in any schema.
2. **GraphQL.** An object type and an input type per composite, attributes in camelCase and all nullable (PostgreSQL cannot declare an attribute `NOT NULL`):
   ```graphql
   type Address { street: String, city: String, postalCode: String, country: String }
   input AddressInput { street: String, city: String, postalCode: String, country: String }
   ```
   The column is `billingAddress: Address` (`AddressInput` on writes); arrays are `[Address]`. Rename types and attributes under `types` in `graphpg.yaml`.
3. **Reads** project only the selected attributes; a NULL value stays `null`:
   ```sql
   CASE WHEN (t."billing_address")::text IS NULL THEN NULL ELSE JSON_OBJECT(
     'city': ((t."billing_address")."city")::text,
     'country': ((t."billing_address")."country")::text
     RETURNING json) END
   ```
4. **Writes** convert the input object to attribute names (recursively, enum values mapped to labels) and use `json_populate_record`:
   ```sql
   INSERT INTO "public"."customers" (..., "billing_address")
   VALUES (..., json_populate_record(NULL::"public"."address", $5::text::json))
   ```
5. **Not yet:** filters on composite attributes.

## Range types

Intervals stored as two columns (`starts_at`, `ends_at`) need hand-written overlap logic and can't carry an exclusion constraint. PostgreSQL's range types (`tstzrange`, `numrange`, `daterange`, `int4range`, `int8range`) can.

```sql
CREATE TABLE bookings (
  id uuid PRIMARY KEY DEFAULT uuidv7(),
  room_id int NOT NULL,
  during tstzrange NOT NULL,
  EXCLUDE USING gist (room_id WITH =, during WITH &&)
);
```

1. **Detection:** `pg_type.typtype = 'r'`.
2. **Values** are objects, one family per bound type (`IntRange`, `BigIntRange`, `DecimalRange`, `DateRange`, `DatetimeRange`):
   ```graphql
   type DatetimeRange { start: DatetimeRangeBound  end: DatetimeRangeBound  isEmpty: Boolean! }
   type DatetimeRangeBound { value: Datetime!  inclusive: Boolean! }
   ```
   A missing bound is unbounded (both are null for an empty range). Inputs (`DatetimeRangeInput`) take the same shape; `start` is inclusive and `end` exclusive unless `inclusive` says otherwise. PostgreSQL normalises discrete ranges (`int4range`, `daterange`) to `[start, end)`.
3. **Filters** (`DatetimeRangeFilter`, ...) compile to native, GiST-indexable operators: `contains: Datetime` (`@>`, a value), and `containedBy`, `overlaps` (`&&`), `strictlyLeftOf` (`<<`), `strictlyRightOf` (`>>`), `adjacentTo` (`-|-`), `equalTo`, which take a `DatetimeRangeInput`.

Multiranges (`tstzmultirange`, `int4multirange`, ...) read and write as lists of ranges and filter with `<Scalar>MultirangeFilter` (`contains`, `containsRange`, `overlaps`, `containedBy`, `isEmpty`, `equalTo`).

## Generated columns

`GENERATED ALWAYS AS (...) STORED` columns are computed by PostgreSQL on `INSERT` and `UPDATE`.

```sql
CREATE TABLE products (
  id uuid PRIMARY KEY DEFAULT uuidv7(),
  price numeric NOT NULL,
  tax_rate numeric NOT NULL DEFAULT 0.2,
  price_with_tax numeric GENERATED ALWAYS AS (price * (1 + tax_rate)) STORED
);
```

They are detected through `pg_attribute.attgenerated = 's'`, exposed as read-only fields (`priceWithTax: Decimal`), and left out of `CreateProductInput` and `UpdateProductInput`, so a client cannot try to set them.

## Exclusive arcs (polymorphic unions)

A row that points at exactly one of several tables, with real foreign keys, is modelled as an exclusive arc: one nullable foreign key per target and a check constraint that exactly one is set.

```sql
CREATE TABLE likes (
  id uuid PRIMARY KEY DEFAULT uuidv7(),
  user_id uuid NOT NULL REFERENCES users(id),
  post_id uuid REFERENCES posts(id),
  comment_id uuid REFERENCES comments(id),
  CHECK (num_nonnulls(post_id, comment_id) = 1)
);
```

The engine recognises `num_nonnulls(...) = 1` in `pg_constraint.conbin`, creates `union LikesTarget = Post | Comment`, and mounts `target: LikesTarget!` on `Like`. A single `CASE` expression resolves the concrete type and object in the same query.

## Views with declared keys (`JSON_TABLE`)

Views are introspected like tables (`relkind = 'v'` or `'m'`), but PostgreSQL cannot attach primary or foreign keys to a view. Declare them in `graphpg.yaml` and the view becomes a relation. This pairs well with PostgreSQL 17's `JSON_TABLE`, which turns a `jsonb` array into virtual rows without a child table:

```sql
CREATE VIEW customer_phone_view AS
SELECT c.id AS customer_id, jt.kind, jt.number
FROM customers c,
     JSON_TABLE(c.contact, '$.phones[*]' COLUMNS (kind text PATH '$.kind', number text PATH '$.number')) jt;
```

```yaml
entities:
  customer_phone_view:
    type_name: CustomerPhone
    primary_key: [customer_id, number]
    foreign_keys:
      customer_phone_view_customer:     # a new name declares a virtual foreign key
        columns: [customer_id]
        references: { table: customers }
        field_name: customer
        reverse_field_name: phones
```

`customer { phones { kind number } phonesConnection { totalCount } }` and `CustomerPhone.customer` then work like any foreign key, relation filters included, with no stored child rows.

## UUIDv7

Tables keyed with `uuid DEFAULT uuidv7()` (PostgreSQL 18) get time-ordered, index-friendly keys. The engine treats them as ordinary `uuid` keys: `databaseId` is the raw value and `id` is the Relay global ID.

## Functions

Auto-CRUD cannot express every piece of business logic, so any SQL function becomes a field, with no config: searches, reports, state transitions, workflows. What a function returns decides its type, and its volatility decides where it goes: `STABLE`/`IMMUTABLE` functions are Query fields, `VOLATILE` ones Mutation fields. PostgreSQL's default when none is declared is `VOLATILE`, so declare `STABLE` for functions that only read.

### What a function becomes

| The function returns | Query field (`STABLE`/`IMMUTABLE`) | Mutation field (`VOLATILE`) |
| :--- | :--- | :--- |
| `SETOF users`, or `users[]` | `searchUsers(q: String!, first, after, last, before, offset, condition, filter, orderBy): UserConnection!` | `[User!]!` |
| `users` (one row) | `bestUser: User` (null when it returns none) | `User` |
| A value: base type, enum, composite type, array | `add(a: Int!, b: Int): Int`, `moods: [Mood]!` for `SETOF` | the same |
| `TABLE (...)` columns, or several `OUT` parameters | `stats: StatsRow`, `[StatsRow!]!` for a set | the same |
| `void` | — | `resetScores: Boolean` |

- **Arguments** are typed like columns (enums, composites as `<Type>Input`, arrays, `BigInt`...). Those with a default are optional. Key-like ones (`uuid`, or named `*_id`) are `ID`s and also take Relay IDs.
- **Connections over a function** get the table's `condition`, `filter` and `orderBy`, and cursor pagination. Without an `orderBy`, rows keep the order the function returns them in (a search's ranking, for example), and cursors page through that order.
- **Mutations** run in the operation's transaction, one after another like every mutation field. Rows of a table with a key are read back by key in a statement of their own, so their relations reflect what the function wrote.
- **Not exposed:** procedures, aggregate and window functions, trigger and other handler functions, variadic and polymorphic (`anyelement`) functions, functions the role cannot `EXECUTE`, and functions taking rows of a table other than as their first argument. Of overloaded functions (same name), the first one created is exposed.

### Functions of a row: computed columns

A function whose first argument is a row of a table is a field of that table's type. Its other arguments are the field's arguments, and it takes the same return shapes:

```sql
CREATE FUNCTION teams_captain(t teams) RETURNS members ...           -- Team.captain: Member
CREATE FUNCTION teams_ranked(t teams) RETURNS SETOF members ...      -- Team.ranked(first, ..., orderBy): MemberConnection!
CREATE FUNCTION members_greeting(m members, greeting text DEFAULT 'Hi') RETURNS text ...
                                                                     -- Member.greeting(greeting: String): String
```

The field name drops the table's name from the function's (`teams_captain` → `captain`). `VOLATILE` functions of a row are not exposed.

### Functions returning columns of their own, mounted on a table

A set-returning function with `TABLE (...)` columns, whose first argument takes a table's primary key (`flatten_questionnaire_items(q_id uuid)` and table `questionnaires`), is also mounted as a field of that table's type: `Questionnaire.items: [FlattenQuestionnaireItemsRow!]!`. `graphpg.yaml` can mount it elsewhere or map more arguments to columns (`functions.<name>.mount`).

### Config

`functions.<name>` renames (`field_name`), describes (`description`) or hides (`omit`) the field. For a function of a row, these apply to its field on the table's type.

### Performance

A simple `LANGUAGE sql` function (one `SELECT`, not `SECURITY DEFINER`, no `SET` options, `STABLE` or `IMMUTABLE`) is inlined by PostgreSQL: it becomes part of the query, so the connection's `filter`, `condition` and `orderBy` are planned together with its body and can use the indexes of its tables. Other functions run as a whole, then their result is filtered and sorted. Schema-qualify the tables in a function body (or it depends on the caller's `search_path`); `SET search_path` on the function also works, but prevents inlining.

---

## REST and OpenAPI

The REST API names a **resource** after its table or view (`/rest/forums`) and a function after its SQL name (`/rest/rpc/add`); fields and function arguments use the GraphQL names (`camelCase`, so `archived_at` is `archivedAt`), and a table's `entities:` configuration (`omit`, `omit_operations`, `description`, ...) applies to both APIs. With several schemas, `/rest/<resource>` is the default schema's table and every schema's own are at `/rest/<schema>/<resource>` (see [Several schemas](rest-api.md#several-schemas)). `rpc` and `search` are reserved paths: tables with those names are not served over REST.

Values are JSON, in the shapes the OpenAPI document describes:

| PostgreSQL | JSON in REST rows | OpenAPI schema |
| :--- | :--- | :--- |
| `int2`, `int4` | number | `integer` (`int32`) |
| `int8` | **string** (`"9007199254740993"`): JSON numbers are doubles | `string`, digits |
| `float4`, `float8` | number | `number` |
| `numeric`, `money` | **string** (`"12.50"`): exact | `string`, decimal |
| `bool` | boolean | `boolean` |
| `text`, `varchar`, `citext` | string (`maxLength` from `varchar(n)`) | `string` |
| `uuid` | string | `string`, `uuid` |
| `date` | `"2026-01-02"` | `string`, `date` |
| `timestamptz` | ISO 8601 with offset, `"2026-01-02T03:04:05+00:00"` | `string`, `date-time` |
| `timestamp`, `time` | ISO 8601 without a zone | `string` |
| `interval` | ISO 8601 duration, `"P1DT2H"` | `string`, `duration` |
| `bytea` | base64 string | `string`, `contentEncoding: base64` |
| `json`, `jsonb` | any JSON value | any |
| enum | the GraphQL value (`PAID`, not the label `paid`) | `string`, `enum` |
| composite type | object of its attributes | `object` |
| range (`int4range`, `tstzrange`, ...) | `{"start": {"value": 1, "inclusive": true}, "end": {...}, "isEmpty": false}`; a missing bound is `null` | `object` |
| multirange | list of ranges | `array` |
| array | array | `array` |
| `inet`, `cidr`, `macaddr`, `tsvector`, `ltree` | string | `string` |
| `vector` (pgvector) | array of numbers | `array` of `number` |
| PostGIS `geometry` / `geography` | GeoJSON | any |

Nullable columns are `["<type>", "null"]` in the document (JSON Schema 2020-12), `NOT NULL` columns without a default are `required`, generated and read-only columns are left out of write bodies, and column comments become descriptions. The bodies of `POST` and `PATCH` are the GraphQL input types, relations included (see [nested writes](rest-api.md#nested-writes)).

## Scalar & Array Types

Every column type has a GraphQL type whose values round-trip:

| PostgreSQL | GraphQL | Wire format |
| :--- | :--- | :--- |
| `int2`, `int4` | `Int` | number |
| `int8` | `BigInt` | string (exact past 2^53); input also takes an integer number |
| `numeric` | `Decimal` | string (exact); input also takes a number |
| `money` | `Decimal` | string (`12.50`, not the locale-formatted `$12.50`) |
| `float4`, `float8` | `Float` | number; `NaN` and infinities are `null` (they are not GraphQL `Float`s) |
| `timestamptz` | `Datetime` | ISO 8601 with a UTC offset (`2024-05-01T10:30:00+00:00`); input without an offset is in the server's time zone |
| `timestamp` | `LocalDatetime` | ISO 8601 without an offset (`2024-05-01T12:30:00`) |
| `date` / `time` / `timetz` | `Date` / `Time` / `TimeTz` | ISO 8601 (`2024-05-01`, `12:30:00`, `12:30:00+02`) |
| `interval` | `Interval` | ISO 8601 duration (`P1Y2M-3DT4H5M6.5S`, `PT0S`); parts may be negative |
| `uuid` | `UUID` | string |
| `bytea` | `Base64` | base64 (RFC 4648, padded) |
| `inet` / `cidr` | `InternetAddress` / `CidrAddress` | `10.0.0.1/24` / `10.0.0.0/24` |
| `macaddr`, `macaddr8` | `MacAddress` | `08:00:2b:01:02:03` |
| `vector` (pgvector) | `[Float!]` | list of numbers |
| `json`, `jsonb` | `JSON` | JSON value |
| ranges | `IntRange`, `BigIntRange`, `DecimalRange`, `DateRange`, `DatetimeRange` | `{ start { value inclusive } end { ... } isEmpty }` (see [range types](schema-mapping.md#range-types)) |
| enum types, text columns closed by `CHECK (col IN (...))` | GraphQL `enum` | enum value (`in-progress` → `IN_PROGRESS`; the label is the value's description) |
| composite types | object type / `<Name>Input` | JSON object |
| domains | a scalar named after the domain (`email` → `Email`) | the base type's; see below |
| `text`, `varchar`, `citext`, and types without a GraphQL form (`tsvector`, geometric types, `xml`, ...) | `String` | PostgreSQL's text form |
| `T[]` | `[T]` | JSON list |

**Inputs are checked before any SQL runs.** Every value, in arguments, inputs, conditions, filters, range bounds and function arguments, is checked against its scalar (UUID syntax, 64-bit integer range, decimal syntax, ISO 8601 dates, times, offsets and durations, base64, IP addresses and networks, MAC addresses) and converted to what PostgreSQL parses. All invalid values of an operation are reported together, each naming its field (`` `ip`: "10.0.0.300" is not an InternetAddress ``), and nothing is executed.

**Domains** (`CREATE DOMAIN email AS citext CHECK (value ~ '@')`) become scalars of their own, described by the domain's comment. Values are those of the base type: checked and converted like them (a domain's custom scalar gets GraphQL's built-in checks for `Int`, `Float`, `Boolean` and `String` from the engine), while the domain's `CHECK` constraints are enforced by PostgreSQL. `types.<domain>` in `graphpg.yaml` renames (`type_name`) or describes the scalar. Domains over arrays, ranges, composites and enums use their base type's GraphQL type.

Each scalar and enum has a matching filter input (`BigIntFilter`, `DatetimeFilter`, `IntervalFilter`, `InternetAddressFilter`, `<Enum>Filter`, ...); domain columns filter like their base type, `money` like `Decimal`; `bytea` and `vector` columns are not filtered. `citext` compares case-insensitively. Array columns get `<T>ListFilter` with `equalTo`, `contains` (`@>`), `containedBy` (`<@`) and `overlaps` (`&&`). In `StringFilter`, `startsWith`/`endsWith`/`contains` match the given text literally (`%` and `_` are escaped); `like`/`ilike` take SQL patterns.

---

## Temporal keys

A table whose primary (or unique) key ends with a period, `PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)`, holds versions of each entity over non-overlapping periods:

- Its single-row field takes the entity key and a point in time: `account(id: 1, asOf: "2024-03-01T00:00:00Z")` is the version valid then (`valid_at @> $asOf`); without `asOf`, the latest version.
- Its list and connection fields, and relation fields leading to it, take `asOf` too.
- `history` / `historyConnection` list every version of the row's entity, oldest first.
- A temporal foreign key, `FOREIGN KEY (account_id, PERIOD valid_at) REFERENCES accounts (id, PERIOD valid_at)`, relates rows whose periods overlap (never equal ranges): an entry's `account` is the version its period overlaps (the latest one), or the one valid at `asOf`; an account version's `entries` are those within its period. Relation filters overlap the same way.

## More types

- **Full-text search**: see [search fields](graphql-api.md#search-fields) and [performance](performance.md#full-text-search).
- **JSON**: `JSONFilter` has `matchesPath` (`@@`), `existsPath` (`@?`), `hasKey`, `hasAnyKeys`, `hasAllKeys` (served by `jsonb_path_ops` / `jsonb_ops` GIN indexes); JSON fields take `path` (`meta(path: "$.author.name")`, `jsonb_path_query_first`).
- **Multiranges**: output and input as lists of ranges; `<Scalar>MultirangeFilter` (`contains` a value, `containsRange`, `overlaps`, `containedBy`, `isEmpty`, `equalTo`).
- **PostGIS**: `geometry` and `geography` columns are `GeoJSON` (`ST_AsGeoJSON` / `ST_GeomFromGeoJSON`); `GeoJSONFilter` has `intersects`, `within`, `contains`, `withinDistance` (`ST_DWithin`) and `intersectsBox` (`&&`); a GiST or SP-GiST index gives `<list>NearestSearch(near:, maxDistance:)`, nearest first by KNN (`<->`), with each hit's `distance`.
- **Global search**: `globalSearch(query:, first:)` returns hits whose `node` is a `GlobalSearchNode` union of every searchable type.
