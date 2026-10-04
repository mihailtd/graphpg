-- Custom migration: what a schema file cannot express. graphpg turns these into GraphQL.
COMMENT ON SCHEMA blog IS 'A small blog: authors, posts, comments and tags.';
--> statement-breakpoint
COMMENT ON TABLE blog.posts IS 'A blog post. Only published posts are visible to readers.';
--> statement-breakpoint
-- A function of a row becomes a computed field: Post.excerpt
CREATE FUNCTION blog.posts_excerpt(p blog.posts, length int DEFAULT 80) RETURNS text
    LANGUAGE sql STABLE AS $$ SELECT left(p.body, length) $$;
--> statement-breakpoint
-- A STABLE function becomes a root query field: popularTags
CREATE FUNCTION blog.popular_tags() RETURNS SETOF blog.tags
    LANGUAGE sql STABLE AS $$
    SELECT t.* FROM blog.tags t
    JOIN blog.post_tags pt ON pt.tag_id = t.id
    GROUP BY t.id ORDER BY count(*) DESC, t.name
$$;
