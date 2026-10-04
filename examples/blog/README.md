# Blog example

A small schema (authors, posts, comments, tags) that graphpg turns into a GraphQL API. Everything you see in the API comes from [`schema.sql`](schema.sql); [`engine.config.yaml`](engine.config.yaml) only adds a description and turns off one mutation.

```bash
docker compose up --build        # PostgreSQL 18 + graphpg
open http://localhost:4000/graphiql
```

Or run graphpg yourself against any PostgreSQL 17+ database:

```bash
psql "$DATABASE_URL" -f schema.sql
DATABASE_URL=postgres://... graphpg      # from the repository root: cargo run --release
```

## Things to try

Relations, a computed column, a generated column, a many-to-many relation and a count, in one request and one SQL statement:

```graphql
{
  authors {
    username
    posts {
      title
      status                 # enum from CREATE TYPE post_status
      wordCount              # GENERATED ALWAYS AS ... STORED: read-only
      excerpt(length: 30)    # the function posts_excerpt(p posts, length int)
      tags { name }          # many-to-many through post_tags
      commentsConnection { totalCount }
    }
  }
}
```

Filtering, ordering and keyset pagination:

```graphql
{
  postsConnection(first: 10, filter: { status: { equalTo: PUBLISHED } }, orderBy: [CREATED_AT_DESC]) {
    nodes { title author { displayName } }
    pageInfo { hasNextPage endCursor }
  }
}
```

Fuzzy search, served by the trigram index on `posts.title`:

```graphql
{ postsFuzzySearch(query: "nanosec") { hits { score node { title } } } }
```

A mutation that writes related rows in one transaction:

```graphql
mutation {
  createPost(input: {
    authorId: "0192f1a0-0000-7000-8000-000000000001"
    title: "Hello"
    body: "First words."
    tags: { connect: [{ name: "computing" }] }
  }) {
    post { id title wordCount tags { name } }
  }
}
```

Constraint violations come back as typed errors: an unknown `authorId` fails with `AUTHOR_NOT_FOUND` on `input.authorId`, and the whole operation rolls back.

A function that returns a table becomes a field: `{ popularTags(first: 5) { nodes { name } } }`.

## Editor support for the config

`graphpg export-config-schema` writes `.engine/schema.config.json`, a JSON Schema listing the real tables, columns, constraints and functions of your database. `engine.config.yaml` points at it, so editors that use the YAML language server offer completion and validation.
