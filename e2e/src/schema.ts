import { sql } from "drizzle-orm";
import { check, index, integer, pgSchema, primaryKey, text, timestamp, uuid } from "drizzle-orm/pg-core";

// The blog example, described with Drizzle. `drizzle-kit generate` turns this into the SQL
// migrations in ../drizzle; the end-to-end tests apply them to a real PostgreSQL and then
// check what graphpg serves.
export const blog = pgSchema("blog");

export const postStatus = blog.enum("post_status", ["draft", "published", "archived"]);

export const authors = blog.table("authors", {
  id: uuid().primaryKey().default(sql`uuidv7()`),
  username: text().notNull().unique(),
  displayName: text("display_name").notNull(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
});

export const posts = blog.table(
  "posts",
  {
    id: uuid().primaryKey().default(sql`uuidv7()`),
    authorId: uuid("author_id")
      .notNull()
      .references(() => authors.id),
    title: text().notNull(),
    body: text().notNull(),
    status: postStatus().notNull().default("draft"),
    wordCount: integer("word_count").generatedAlwaysAs(sql`cardinality(string_to_array(body, ' '))`),
    publishedAt: timestamp("published_at", { withTimezone: true }),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [index("posts_author_created_idx").on(t.authorId, t.createdAt.desc(), t.id)],
);

export const comments = blog.table(
  "comments",
  {
    id: uuid().primaryKey().default(sql`uuidv7()`),
    postId: uuid("post_id")
      .notNull()
      .references(() => posts.id, { onDelete: "cascade" }),
    authorId: uuid("author_id")
      .notNull()
      .references(() => authors.id),
    body: text().notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [
    check("comments_body_check", sql`length(${t.body}) BETWEEN 1 AND 2000`),
    index("comments_post_idx").on(t.postId, t.createdAt, t.id),
  ],
);

export const tags = blog.table("tags", {
  id: uuid().primaryKey().default(sql`uuidv7()`),
  name: text().notNull().unique(),
});

export const postTags = blog.table(
  "post_tags",
  {
    postId: uuid("post_id")
      .notNull()
      .references(() => posts.id, { onDelete: "cascade" }),
    tagId: uuid("tag_id")
      .notNull()
      .references(() => tags.id, { onDelete: "cascade" }),
  },
  (t) => [primaryKey({ columns: [t.postId, t.tagId] }), index("post_tags_tag_idx").on(t.tagId)],
);
