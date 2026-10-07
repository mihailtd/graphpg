-- The schema and data both servers read in the REST benchmark: authors, posts, comments, and a
-- table the write scenarios change. Sizes are the defaults of `:authors`, `:posts` and
-- `:comments`; override them with `psql -v posts=1000000 …`.
\if :{?authors} \else \set authors 1000 \endif
\if :{?posts} \else \set posts 100000 \endif
\if :{?comments} \else \set comments 300000 \endif

DROP SCHEMA IF EXISTS bench CASCADE;
CREATE SCHEMA bench;

CREATE TYPE bench.post_status AS ENUM ('draft', 'published', 'archived');

CREATE TABLE bench.authors (
    id integer PRIMARY KEY,
    username text NOT NULL UNIQUE,
    bio text
);

CREATE TABLE bench.posts (
    id integer PRIMARY KEY,
    author_id integer NOT NULL REFERENCES bench.authors (id),
    title text NOT NULL,
    status bench.post_status NOT NULL,
    body text NOT NULL,
    created_at timestamptz NOT NULL
);

CREATE TABLE bench.comments (
    id integer PRIMARY KEY,
    post_id integer NOT NULL REFERENCES bench.posts (id),
    body text NOT NULL
);

-- What the write scenarios insert, change and delete
CREATE TABLE bench.notes (
    id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    body text NOT NULL,
    hits integer NOT NULL DEFAULT 0
);

INSERT INTO bench.authors
SELECT i, 'author' || i, repeat('bio ', 10) FROM generate_series(1, :authors) i;

INSERT INTO bench.posts
SELECT i,
       1 + (i % :authors),
       'Post number ' || i,
       (ARRAY['draft', 'published', 'published', 'archived'])[1 + (i % 4)]::bench.post_status,
       repeat('Lorem ipsum dolor sit amet. ', 8),
       timestamptz '2026-01-01' + (i || ' minutes')::interval
FROM generate_series(1, :posts) i;

INSERT INTO bench.comments
SELECT i, 1 + (i % :posts), 'Comment ' || i FROM generate_series(1, :comments) i;

INSERT INTO bench.notes (body) SELECT 'note ' || i FROM generate_series(1, 1000) i;

-- The indexes a real schema would have: foreign keys, the order and the filter of the lists
CREATE INDEX posts_author_id ON bench.posts (author_id);
CREATE INDEX posts_created_at ON bench.posts (created_at);
CREATE INDEX posts_status_created_at ON bench.posts (status, created_at);
CREATE INDEX comments_post_id ON bench.comments (post_id);

ANALYZE;

-- One role for both servers: PostgREST connects as it and switches to `bench_anon`
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'bench_anon') THEN
        CREATE ROLE bench_anon NOLOGIN;
    END IF;
END $$;
GRANT USAGE ON SCHEMA bench TO bench_anon;
GRANT SELECT ON ALL TABLES IN SCHEMA bench TO bench_anon;
GRANT INSERT, UPDATE, DELETE ON bench.notes TO bench_anon;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA bench TO bench_anon;
