-- A small blog schema. graphpg turns it into a GraphQL API without any further setup.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE SCHEMA blog;
SET search_path = blog, public;

COMMENT ON SCHEMA blog IS 'A small blog: authors, posts, comments and tags.';

CREATE TYPE post_status AS ENUM ('draft', 'published', 'archived');

CREATE TABLE authors (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    username text NOT NULL UNIQUE,
    display_name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE posts (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    author_id uuid NOT NULL REFERENCES authors (id),
    title text NOT NULL,
    body text NOT NULL,
    status post_status NOT NULL DEFAULT 'draft',
    word_count int GENERATED ALWAYS AS (cardinality(string_to_array(body, ' '))) STORED,
    published_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
COMMENT ON TABLE posts IS 'A blog post. Only published posts are visible to readers.';
COMMENT ON COLUMN posts.word_count IS 'Computed by PostgreSQL; read-only in GraphQL.';
CREATE INDEX posts_author_created_idx ON posts (author_id, created_at DESC, id);
CREATE INDEX posts_title_trgm_idx ON posts USING gist (title gist_trgm_ops);

CREATE TABLE comments (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    post_id uuid NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
    author_id uuid NOT NULL REFERENCES authors (id),
    body text NOT NULL CHECK (length(body) BETWEEN 1 AND 2000),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX comments_post_idx ON comments (post_id, created_at, id);
CREATE INDEX comments_author_idx ON comments (author_id);

CREATE TABLE tags (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    name text NOT NULL UNIQUE
);

CREATE TABLE post_tags (
    post_id uuid NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
    tag_id uuid NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    PRIMARY KEY (post_id, tag_id)
);
CREATE INDEX post_tags_tag_idx ON post_tags (tag_id);

-- A function of a row becomes a computed field: Post.excerpt
CREATE FUNCTION posts_excerpt(p posts, length int DEFAULT 80) RETURNS text
    LANGUAGE sql STABLE AS $$ SELECT left(p.body, length) $$;

-- A STABLE function becomes a root query field: popularTags(first: ...)
CREATE FUNCTION popular_tags() RETURNS SETOF tags
    LANGUAGE sql STABLE AS $$
    SELECT t.* FROM blog.tags t
    JOIN blog.post_tags pt ON pt.tag_id = t.id
    GROUP BY t.id ORDER BY count(*) DESC, t.name
$$;

INSERT INTO authors (id, username, display_name) VALUES
    ('0192f1a0-0000-7000-8000-000000000001', 'ada', 'Ada Lovelace'),
    ('0192f1a0-0000-7000-8000-000000000002', 'grace', 'Grace Hopper');

INSERT INTO posts (id, author_id, title, body, status, published_at) VALUES
    ('0192f1a1-0000-7000-8000-000000000001', '0192f1a0-0000-7000-8000-000000000001',
     'Notes on the Analytical Engine', 'The engine weaves algebraical patterns just as the Jacquard loom weaves flowers and leaves.', 'published', now() - interval '3 days'),
    ('0192f1a1-0000-7000-8000-000000000002', '0192f1a0-0000-7000-8000-000000000002',
     'Nanoseconds', 'A nanosecond is the distance light travels in about thirty centimetres.', 'published', now() - interval '1 day'),
    ('0192f1a1-0000-7000-8000-000000000003', '0192f1a0-0000-7000-8000-000000000002',
     'Compilers, a draft', 'Programs should be written in a language people can read.', 'draft', NULL);

INSERT INTO comments (post_id, author_id, body) VALUES
    ('0192f1a1-0000-7000-8000-000000000001', '0192f1a0-0000-7000-8000-000000000002', 'A delight to read.'),
    ('0192f1a1-0000-7000-8000-000000000002', '0192f1a0-0000-7000-8000-000000000001', 'Do you have a wire to show?');

INSERT INTO tags (id, name) VALUES
    ('0192f1a2-0000-7000-8000-000000000001', 'history'),
    ('0192f1a2-0000-7000-8000-000000000002', 'computing');

INSERT INTO post_tags (post_id, tag_id) VALUES
    ('0192f1a1-0000-7000-8000-000000000001', '0192f1a2-0000-7000-8000-000000000001'),
    ('0192f1a1-0000-7000-8000-000000000001', '0192f1a2-0000-7000-8000-000000000002'),
    ('0192f1a1-0000-7000-8000-000000000002', '0192f1a2-0000-7000-8000-000000000002');
