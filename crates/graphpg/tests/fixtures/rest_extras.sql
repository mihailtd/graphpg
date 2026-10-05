-- Fixture of the REST tests beyond the forum schema: views, aggregates, functions of every
-- return shape and the search methods.
CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public;
CREATE EXTENSION IF NOT EXISTS vector SCHEMA public;
CREATE SCHEMA rest_extras;
SET search_path = rest_extras, public;

CREATE TYPE order_status AS ENUM ('pending', 'paid', 'cancelled');
CREATE TYPE point2 AS (x int, y int);

CREATE TABLE orders (
    id int PRIMARY KEY,
    customer text NOT NULL,
    status order_status NOT NULL DEFAULT 'pending',
    total numeric(10, 2) NOT NULL,
    quantity int NOT NULL DEFAULT 1,
    created_at timestamptz NOT NULL
);
INSERT INTO orders VALUES
    (1, 'ada', 'paid', 100.00, 2, '2026-01-05 10:00+00'),
    (2, 'ada', 'paid', 50.50, 1, '2026-01-20 11:00+00'),
    (3, 'bob', 'pending', 20.00, 1, '2026-02-03 09:00+00'),
    (4, 'bob', 'paid', 75.25, 3, '2026-02-14 15:00+00'),
    (5, 'cyd', 'cancelled', 10.00, 1, '2026-02-20 08:00+00'),
    (6, 'cyd', 'paid', 200.00, 5, '2026-03-01 12:00+00');

-- Views have no primary key: they are read-only
CREATE VIEW paid_orders AS
    SELECT id, customer, total, created_at FROM orders WHERE status = 'paid';
CREATE VIEW customer_totals AS
    SELECT customer, sum(total) AS total, count(*)::int AS orders FROM orders GROUP BY customer;

CREATE MATERIALIZED VIEW customer_summary AS
    SELECT customer, count(*)::int AS orders FROM orders GROUP BY customer;

-- A parent and children with a nullable foreign key: nested writes can detach them
CREATE TABLE crews (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE sailors (
    id int PRIMARY KEY,
    crew_id int REFERENCES crews (id),
    name text NOT NULL
);

-- Columns of many types: what a row holds, and what the OpenAPI document says it holds
CREATE TABLE typed (
    id int PRIMARY KEY,
    big int8,
    price numeric(10, 2),
    cash money,
    ratio float8,
    doc jsonb,
    list json,
    raw bytea,
    point point2,
    points point2[],
    span int4range,
    during tstzrange,
    day_span daterange,
    spans int4multirange,
    stamp timestamp,
    clock time,
    wait interval,
    mood order_status,
    tags text[]
);
INSERT INTO typed VALUES
    (1, 9007199254740993, 12.50, 3.25, 0.5, '{"a": {"b": [1, 2]}}', '[1, "two", null]', '\xdeadbeef',
     ROW(1, 2), ARRAY[ROW(3, 4)::point2, ROW(5, 6)::point2], '[1,5)', '[2026-01-01 00:00+00,2026-01-02 00:00+00)',
     '[2026-01-01,2026-02-01)', '{[1,3),[5,7)}', '2026-01-02 03:04:05', '10:11:12', '1 day 2 hours', 'paid', '{a,b}'),
    (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'empty', '(,)', NULL, '{}', NULL, NULL, NULL, NULL, NULL);

CREATE TABLE counters (id int PRIMARY KEY, value int NOT NULL DEFAULT 0);
INSERT INTO counters VALUES (1, 0);

-- Search: full text, trigrams and vectors
CREATE TABLE articles (
    id int PRIMARY KEY,
    title text NOT NULL,
    body text NOT NULL,
    doc tsvector GENERATED ALWAYS AS (to_tsvector('english', title || ' ' || body)) STORED,
    embedding public.vector(3)
);
CREATE INDEX articles_doc ON articles USING gin (doc);
CREATE INDEX articles_title_trgm ON articles USING gin (title public.gin_trgm_ops);
CREATE INDEX articles_embedding ON articles USING hnsw (embedding public.vector_cosine_ops);
INSERT INTO articles (id, title, body, embedding) VALUES
    (1, 'PostgreSQL full text search', 'Searching documents with ranking and highlighting', '[1,0,0]'),
    (2, 'Rust ownership', 'Borrowing rules explained', '[0,1,0]'),
    (3, 'Cooking pasta', 'Boil water, add salt', '[0,0,1]');
CREATE TABLE people (id int PRIMARY KEY, name text NOT NULL);
CREATE INDEX people_name_trgm ON people USING gin (name public.gin_trgm_ops);
INSERT INTO people (id, name) VALUES (1, 'Search Man'), (2, 'Ada Lovelace');

-- Functions: rows of a table (a set, one, none), values, columns of their own
CREATE FUNCTION search_orders(q text) RETURNS SETOF orders LANGUAGE sql STABLE
    AS $$ SELECT * FROM rest_extras.orders WHERE customer ILIKE '%' || q || '%' $$;
CREATE FUNCTION best_order() RETURNS orders LANGUAGE sql STABLE
    AS $$ SELECT * FROM rest_extras.orders ORDER BY total DESC LIMIT 1 $$;
CREATE FUNCTION nobody() RETURNS orders LANGUAGE sql STABLE
    AS $$ SELECT * FROM rest_extras.orders WHERE false $$;
CREATE FUNCTION add(a int, b int DEFAULT 10) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT a + b $$;
CREATE FUNCTION sum_ids(ids int[]) RETURNS int LANGUAGE sql IMMUTABLE
    AS $$ SELECT coalesce(sum(i), 0)::int FROM unnest(ids) i $$;
CREATE FUNCTION statuses() RETURNS SETOF order_status LANGUAGE sql STABLE
    AS $$ SELECT unnest(enum_range(NULL::rest_extras.order_status)) $$;
CREATE FUNCTION status_of(s order_status) RETURNS order_status LANGUAGE sql IMMUTABLE AS $$ SELECT s $$;
CREATE FUNCTION origin() RETURNS point2 LANGUAGE sql IMMUTABLE AS $$ SELECT ROW(0, 1)::rest_extras.point2 $$;
CREATE FUNCTION tags() RETURNS text[] LANGUAGE sql IMMUTABLE AS $$ SELECT ARRAY['a', 'b'] $$;
CREATE FUNCTION order_stats(OUT total int, OUT top text) LANGUAGE sql STABLE
    AS $$ SELECT count(*)::int, max(customer) FROM rest_extras.orders $$;
CREATE FUNCTION customer_stats() RETURNS TABLE (customer text, orders bigint) LANGUAGE sql STABLE
    AS $$ SELECT o.customer, count(*) FROM rest_extras.orders o GROUP BY o.customer ORDER BY o.customer $$;

-- VOLATILE functions change data, so they are POST only
CREATE FUNCTION bump_counter(counter_id int, by int DEFAULT 1) RETURNS counters LANGUAGE sql
    AS $$ UPDATE rest_extras.counters SET value = value + by WHERE id = counter_id RETURNING * $$;
CREATE FUNCTION touch_counters() RETURNS void LANGUAGE sql AS $$ UPDATE rest_extras.counters SET value = value WHERE false $$;
CREATE FUNCTION counter_total() RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT sum(value)::int FROM rest_extras.counters $$;

-- A function that takes its time, for statement timeouts
CREATE FUNCTION nap(secs float8) RETURNS int LANGUAGE plpgsql STABLE
    AS $$ BEGIN PERFORM pg_sleep(secs); RETURN 1; END $$;
