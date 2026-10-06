# Blog example

A small schema (authors, posts, comments, tags) that graphpg turns into a GraphQL API and a REST API. Everything you see in the API comes from [`schema.sql`](schema.sql); [`graphpg.yaml`](graphpg.yaml) selects the schema, turns on GraphiQL, adds a description and turns off one mutation.

```bash
docker compose up --build        # PostgreSQL 18 + graphpg
open http://localhost:4000/graphiql    # GraphQL
open http://localhost:4000/scalar      # the REST API reference (OpenAPI 3.1 at /openapi.json)
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

## The same, over REST

Every table is a resource at `/rest/<table>`; the [Scalar reference](http://localhost:4000/scalar) lists them with their typed parameters. (`curl -g` stops curl treating the brackets as a range.)

```bash
# filtering, ordering, keyset pagination (follow the `Link: rel="next"` header) and embedding
curl -g 'localhost:4000/rest/posts?filter[status][eq]=PUBLISHED&order=-createdAt&first=5&include=tags'

# a single row, with an ETag; update it only if it did not change since (If-Match)
curl -i localhost:4000/rest/authors/<id>
curl -X PATCH localhost:4000/rest/authors/<id> -H 'content-type: application/json' \
     -H 'if-match: "<etag>"' -d '{"displayName": "Ada L."}'

# a nested write: the author and a post in one transaction
curl -X POST localhost:4000/rest/authors -H 'content-type: application/json' \
     -d '{"username": "ada2", "displayName": "Ada", "posts": {"create": [{"title": "Hello", "body": "Hi there"}]}}'

# aggregates, fuzzy search, and the function behind `popularTags`
curl -g 'localhost:4000/rest/posts/aggregate?group=status&max=createdAt'
curl -X POST localhost:4000/rest/posts/search/fuzzy -H 'content-type: application/json' -d '{"query": "nanosec"}'
curl 'localhost:4000/rest/rpc/popular_tags'
```

A constraint violation is a `application/problem+json` response with the same typed code as in GraphQL (`AUTHOR_NOT_FOUND`), and a request over a [safety limit](../../docs/configuration.md#safety-limits) (try `first=100000`) is refused before it reaches the database.

## Editor support for the config

`graphpg export-config-schema` writes `graphpg.schema.json`, a JSON Schema listing the real tables, columns, constraints and functions of your database. `graphpg.yaml` points at it, so editors that use the YAML language server offer completion and validation.
