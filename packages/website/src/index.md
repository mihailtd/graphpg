---
layout: home
title: graphpg
titleTemplate: GraphQL and REST for PostgreSQL

hero:
  name: graphpg
  text: GraphQL and REST, generated from your PostgreSQL schema
  tagline: One Rust binary reads the catalogs, plans every request into a single SQL statement, and serves both APIs. No resolvers to write, no schema kept in two places.
  image:
    src: /logo.svg
    alt: graphpg
  actions:
    - theme: brand
      text: Get started
      link: /docs/
    - theme: alt
      text: See the features
      link: "#everything-you-would-otherwise-write-by-hand"
    - theme: alt
      text: GitHub
      link: https://github.com/mihailtd/graphpg

features:
  - title: One statement per request
    details: Nested selections compile to lateral joins and JSON aggregation, so a deep query is one round trip and never N+1. Connections page by keyset, an index seek on every page.
    link: /docs/graphql-api
    linkText: GraphQL API
  - title: A REST API with OpenAPI 3.1
    details: Typed filters, embedding, ETags, nested and bulk writes, aggregates, functions and search at /rest, described by a generated OpenAPI 3.1 document and the bundled Scalar reference.
    link: /docs/rest-api
    linkText: REST API
  - title: Your constraints become your API
    details: Foreign keys become relations, unique keys lookups, enums and CHECKs become types, and violations come back as typed error codes like EMAIL_TAKEN or AUTHOR_NOT_FOUND.
    link: /docs/schema-mapping
    linkText: Schema mapping
  - title: Authorization stays in the database
    details: OAuth 2.0 bearer tokens are verified and their claims reach Row-Level Security policies. With RBAC on, each role sees only the schema its GRANTs allow.
    link: /docs/authentication
    linkText: Authentication
  - title: Live queries and webhooks
    details: Any query as a subscription over WebSocket, SSE or multipart HTTP, under RLS. Signed webhooks on row changes and cron schedules, captured in the change's own transaction.
    link: /docs/subscriptions
    linkText: Subscriptions
  - title: Safe by default in production
    details: Page size, depth, cost, bulk size, body size and statement_timeout are enforced on the plan before any SQL runs. Pinned schemas fail CI and startup on drift.
    link: /docs/configuration#safety-limits
    linkText: Safety limits
---

<div class="home-sections">

<section class="home-section">

<p class="eyebrow">How it works</p>

## From catalogs to an API, at startup

<div class="pipeline">
  <div class="pipeline-step">
    <span class="pipeline-num">1</span>
    <h3>Read the catalogs</h3>
    <p>Tables, views, keys, constraints, types, functions, indexes and privileges, once at startup and again on <code>NOTIFY graphpg_reload</code>.</p>
  </div>
  <div class="pipeline-step">
    <span class="pipeline-num">2</span>
    <h3>Build both APIs</h3>
    <p>A GraphQL schema and a REST API with its OpenAPI 3.1 document, planned onto one engine that shares filters, limits, errors and authorization.</p>
  </div>
  <div class="pipeline-step">
    <span class="pipeline-num">3</span>
    <h3>Compile each request</h3>
    <p>Every query or write becomes one parameterized SQL statement, run as the caller's role with their claims, so Row-Level Security applies.</p>
  </div>
</div>

</section>

<section class="home-section">

<p class="eyebrow">One schema, two APIs</p>

## Write SQL. Get GraphQL and REST.

Schema design stays in PostgreSQL. This is a slice of the [blog example](https://github.com/mihailtd/graphpg/tree/main/examples/blog); everything below is served from it with no further setup.

::: code-group

```sql [schema.sql]
CREATE TYPE post_status AS ENUM ('draft', 'published', 'archived');

CREATE TABLE posts (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    author_id uuid NOT NULL REFERENCES authors (id),
    title text NOT NULL,
    body text NOT NULL,
    status post_status NOT NULL DEFAULT 'draft',
    word_count int GENERATED ALWAYS AS (cardinality(string_to_array(body, ' '))) STORED,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX posts_title_trgm_idx ON posts USING gist (title gist_trgm_ops);

-- a function of a row becomes a computed field: Post.excerpt
CREATE FUNCTION posts_excerpt(p posts, length int DEFAULT 80) RETURNS text
    LANGUAGE sql STABLE AS $$ SELECT left(p.body, length) $$;
```

```graphql [GraphQL query]
{
  postsConnection(
    first: 10
    filter: { status: { equalTo: PUBLISHED } }
    orderBy: [CREATED_AT_DESC]
  ) {
    nodes {
      title
      wordCount             # generated column, read-only
      excerpt(length: 30)   # posts_excerpt(p posts, length int)
      author { displayName }
      tags { name }         # many-to-many through post_tags
    }
    pageInfo { hasNextPage endCursor }
  }
  postsFuzzySearch(query: "nanosec") { hits { score node { title } } }
}
```

```graphql [GraphQL mutation]
mutation {
  createPost(input: {
    authorId: "0192f1a0-0000-7000-8000-000000000001"
    title: "Hello"
    body: "First words."
    tags: { connect: [{ name: "computing" }] }   # nested write, one transaction
  }) {
    post { id title wordCount tags { name } }
  }
}
# an unknown authorId fails with AUTHOR_NOT_FOUND on input.authorId,
# and the whole operation rolls back
```

```bash [REST]
# filter, order, keyset pagination (Link: rel="next") and embedding
curl -g 'localhost:4000/rest/posts?filter[status][eq]=PUBLISHED&order=-createdAt&first=5&include=tags'

# a nested write: the author and a post in one transaction
curl -X POST localhost:4000/rest/authors -H 'content-type: application/json' \
     -d '{"username": "ada2", "displayName": "Ada", "posts": {"create": [{"title": "Hello", "body": "Hi"}]}}'

# grouped aggregates, fuzzy search, and a function
curl -g 'localhost:4000/rest/posts/aggregate?group=status&max=createdAt'
curl -X POST localhost:4000/rest/posts/search/fuzzy -d '{"query": "nanosec"}' -H 'content-type: application/json'
curl 'localhost:4000/rest/rpc/popular_tags'
```

:::

</section>

<section class="home-section">

<p class="eyebrow">What it generates</p>

## Everything you would otherwise write by hand

<div class="capabilities">
  <div class="capability">
    <h3>Types, relations and lookups</h3>
    <p>From tables, views, foreign keys (composite too), unique keys and many-to-many junctions. Relay connections, <code>node(id:)</code> and global IDs.</p>
  </div>
  <div class="capability">
    <h3>Filtering, ordering, aggregates</h3>
    <p>Filters across relations (<code>some</code> / <code>every</code> / <code>none</code>), grouped aggregates, ordering by related columns and NULL placement.</p>
  </div>
  <div class="capability">
    <h3>Mutations</h3>
    <p>Create, update, delete, upsert, bulk, and nested writes across relations in one transaction, with typed constraint errors.</p>
  </div>
  <div class="capability">
    <h3>Functions as fields</h3>
    <p><code>STABLE</code> functions become queries, <code>VOLATILE</code> ones mutations, and functions of a row become computed fields.</p>
  </div>
  <div class="capability">
    <h3>The whole type system</h3>
    <p>Enums, composites, domains, arrays, ranges and multiranges, <code>ltree</code>, <code>jsonb</code>, pgvector and PostGIS.</p>
  </div>
  <div class="capability">
    <h3>Search from your indexes</h3>
    <p>Trigram, full-text, pgvector, BM25 and hybrid search fields, plus PGroonga, pgvectorscale and VectorChord.</p>
  </div>
  <div class="capability">
    <h3>Modern PostgreSQL</h3>
    <p>Temporal keys (<code>WITHOUT OVERLAPS</code>, <code>PERIOD</code>), <code>RETURNING OLD</code>, <code>JSON_TABLE</code> views, <code>MERGE</code>-based syncing of children.</p>
  </div>
  <div class="capability">
    <h3>Federation and streaming</h3>
    <p>Apollo Federation v2 subgraphs, <code>@defer</code> / <code>@stream</code>, NDJSON and CSV export, <code>COPY</code> import, read replicas.</p>
  </div>
  <div class="capability">
    <h3>Other databases and files</h3>
    <p>Foreign tables through <code>postgres_fdw</code>, <code>file_fdw</code> and Parquet join local tables in the same statement.</p>
  </div>
  <div class="capability">
    <h3>Observability</h3>
    <p>Prometheus metrics, OpenTelemetry traces, metrics and logs over OTLP, and JSON logs, ready for Grafana LGTM.</p>
  </div>
  <div class="capability">
    <h3>Pinned schemas</h3>
    <p><code>graphpg schema export --check</code> fails CI when the SDL or OpenAPI document drifts; production refuses to start on drift.</p>
  </div>
  <div class="capability">
    <h3>Index advice</h3>
    <p><code>graphpg advise --statements</code> finds the costliest operations in <code>pg_stat_statements</code> and the indexes they lack, checked with hypopg.</p>
  </div>
</div>

</section>

<section class="home-section">

<p class="eyebrow">Compared</p>

## Where it sits

graphpg covers the ground of PostGraphile and Hasura on the GraphQL side and PostgREST on the REST side, from one binary. The [comparison](/docs/comparison) lists what each tool does, including where the others are broader.

| | graphpg | PostGraphile v5 | Hasura | PostgREST |
| :--- | :--- | :--- | :--- | :--- |
| Runtime | Rust, single binary | Node.js | Haskell (v2), Rust (v3) | Haskell, single binary |
| GraphQL | Yes | Yes | Yes | No |
| REST | Yes, described by OpenAPI 3.1 | No | From saved queries | Yes, described by OpenAPI 2.0 |
| Schema from | Catalogs, optional YAML | Catalogs, smart comments, plugins | Metadata | Catalogs |
| Authorization | RLS with token claims; schema pruned by `GRANT` | RLS through `pgSettings` | Role permissions in metadata | RLS with JWT claims |

</section>

<section class="home-section">

<p class="eyebrow">Latest release</p>

## What's new

<ReleaseHighlights />

</section>

<section class="home-section home-cta">

## Try it in a minute

```bash
docker run --rm -p 4000:4000 \
  -e DATABASE_URL=postgres://user:pass@host.docker.internal/mydb \
  ghcr.io/mihailtd/graphpg:latest --mode dev
```

Then open `localhost:4000/graphiql` for GraphQL or `localhost:4000/scalar` for the REST reference. The image is a static binary on `distroless/static`, about 27 MB, non-root and signed with Sigstore.

<div class="cta-actions">
  <a class="cta-button brand" href="./docs/">Read the docs</a>
  <a class="cta-button alt" href="./docs/configuration">Configuration</a>
</div>

</section>

</div>
