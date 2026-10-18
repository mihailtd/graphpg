---
layout: home
title: graphpg
titleTemplate: Type-safe GraphQL and REST from PostgreSQL
description: graphpg turns a PostgreSQL schema into type-safe GraphQL and REST APIs, with the SDL and OpenAPI 3.1 files your TypeScript types come from. Built for developers and their coding agents.

hero:
  name: graphpg
  text: Type-safe GraphQL and REST, generated from PostgreSQL
  tagline: Write the schema in SQL. One Rust binary serves both APIs and keeps the GraphQL SDL and OpenAPI 3.1 files current as migrations land. Rename a column and the type checker lists every place that broke, for you or your coding agent.
  image:
    src: /logo.svg
    alt: graphpg
  actions:
    - theme: brand
      text: Run the quick start
      link: /docs/#quick-start
    - theme: alt
      text: End-to-end type safety
      link: /docs/type-safety
    - theme: alt
      text: GitHub
      link: https://github.com/mihailtd/graphpg

features:
  - title: Types from the database to the UI
    details: The GraphQL SDL and the OpenAPI 3.1 document are generated from the schema, rewritten on every migration and pinned in CI. Generated TypeScript catches a renamed column before anything runs.
    link: /docs/type-safety
    linkText: End-to-end types
  - title: Retrieval from your indexes
    details: A vector index and a lexical one give semantic, BM25 and hybrid search fields, fused by reciprocal rank in one SQL statement, scoped by filters and Row-Level Security.
    link: /docs/ai-agents#retrieval-vector-bm25-and-hybrid-search
    linkText: Search for RAG
  - title: MCP tools for your agents
    details: Put Apollo's MCP Server in front of graphpg and each GraphQL operation you list becomes a typed tool, run with the agent's own token.
    link: /docs/ai-agents#mcp-tools-through-apollo-mcp-server
    linkText: Set up MCP
  - title: Guardrails in the database
    details: Each agent gets a role, its rows and nothing else. Page size, depth, cost and timeouts are checked before any SQL runs; an allow-list can limit it to known operations.
    link: /docs/ai-agents#guardrails
    linkText: Guardrails
  - title: One statement per request
    details: Nested selections compile to lateral joins and JSON aggregation, so a deep query is one round trip and never N+1. Connections page by keyset, an index seek on every page.
    link: /docs/graphql-api
    linkText: GraphQL API
  - title: Agents that react to change
    details: Signed webhooks on row changes and schedules, captured in the change's own transaction, start agent work without polling. Live queries push new results.
    link: /docs/events
    linkText: Event triggers
---

<div class="home-sections">

<section class="home-section">

<p class="eyebrow">End-to-end types</p>

## Change the database, and the type checker finds every place that broke

How often have you asked an agent to "change this everywhere" and found, a day later, the one place it missed? In a large codebase, search finds most uses of a column, not all of them. When the types come from the database, the missed places stop compiling, and the agent reads the errors and fixes them in the same session.

<TypeFlow />

</section>

<section class="home-section">

<p class="eyebrow">Schema in SQL</p>

## SQL is the schema language agents already write well

<div class="capabilities why-sql">
  <div class="capability">
    <h3>Declarative and compact</h3>
    <p>A table states its columns, keys, relations and rules in a few lines. Constraints, policies and grants say what is allowed, not how to check it, and PostgreSQL enforces them on every write, from any client.</p>
  </div>
  <div class="capability">
    <h3>Familiar to models</h3>
    <p>SQL has decades of public examples and documentation, and models write it fluently. The API that follows from it needs no resolver code, so most of an agent's backend work is one migration.</p>
  </div>
  <div class="capability">
    <h3>Auditable</h3>
    <p>Every change is a migration a person can review: a SQL file, or a Drizzle schema diff that generates one. The database rejects what breaks its own rules, such as dropping a column a function still reads.</p>
  </div>
  <div class="capability">
    <h3>One contract, two APIs</h3>
    <p>The same schema becomes the GraphQL SDL and the OpenAPI 3.1 document, so typed GraphQL clients and typed REST clients both follow it. Neither is maintained by hand.</p>
  </div>
  <div class="capability">
    <h3>Typed errors</h3>
    <p>A violated constraint comes back as a code derived from it, such as <code>EMAIL_TAKEN</code> or <code>AUTHOR_NOT_FOUND</code>, listed per type in both documents, so generated code can handle each one.</p>
  </div>
  <div class="capability">
    <h3>Drift fails CI</h3>
    <p><code>graphpg schema export --check</code> fails when the committed files are out of date, and production refuses to start on a schema it was not built for.</p>
  </div>
</div>

</section>

<section class="home-section">

<p class="eyebrow">Built for AI</p>

## Two ways AI uses graphpg

<div class="usecases">
  <div class="usecase">
    <h3>Agents that use your data</h3>
    <p>Assistants and autonomous agents read and write your data through tools: search for retrieval, lookups and lists for context, mutations for actions.</p>
    <ul>
      <li>Vector, BM25 and hybrid search from the indexes you already have</li>
      <li>MCP tools through Apollo's MCP Server, or OpenAPI tools over <code>/rest</code></li>
      <li>The agent's own role, Row-Level Security, limits and an operation allow-list</li>
      <li>Webhooks on row changes to start agent runs</li>
    </ul>
    <a href="./docs/ai-agents#agents-that-use-your-data">Agents that use your data →</a>
  </div>
  <div class="usecase">
    <h3>Agents that build applications</h3>
    <p>Coding agents write the schema in SQL and the app against generated, typed APIs, with checks that catch their mistakes early.</p>
    <ul>
      <li>GraphQL SDL and OpenAPI 3.1 in files, for typed client generation</li>
      <li>Drift between database and contract fails CI and production startup</li>
      <li>Typed error codes from constraints, <code>@constraint</code> on fields</li>
      <li>No resolvers: behaviour lives in SQL, where it can be tested</li>
    </ul>
    <a href="./docs/ai-agents#agents-that-build-applications">Agents that build applications →</a>
  </div>
</div>

</section>

<section class="home-section">

<p class="eyebrow">Retrieval</p>

## Hybrid search is one field

Create a vector index and a lexical one; graphpg adds semantic, lexical and hybrid search to both APIs. Hybrid search fuses the two rankings by reciprocal rank inside one SQL statement, after your filters and policies have scoped the candidates.

::: code-group

```sql [schema.sql]
CREATE TABLE documents (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid NOT NULL,
    title text NOT NULL,
    body text NOT NULL,
    embedding vector(1536) NOT NULL
);
CREATE INDEX ON documents USING hnsw (embedding vector_cosine_ops);  -- semantic
CREATE INDEX ON documents USING gist (title gist_trgm_ops);          -- lexical, or ParadeDB bm25
```

```graphql [GraphQL]
query Retrieve($embedding: [Float!]!, $tenant: UUID!) {
  documentsHybridSearch(query: "refund policy", vector: $embedding, first: 5,
                        filter: { tenantId: { equalTo: $tenant } }) {
    hits { score node { id title body } }
  }
}
```

```bash [REST]
curl -X POST localhost:4000/rest/documents/search/hybrid \
     -H 'content-type: application/json' \
     -d '{"query": "refund policy", "vector": [0.012, -0.034, ...], "first": 5}'
```

:::

<figure class="home-figure">
  <img src="/diagrams/search-vectors.webp" alt="Hybrid search: a lexical and a semantic ranking computed in the database, fused by reciprocal rank and returned as one result" loading="lazy" width="1600" height="2000">
</figure>

</section>

<section class="home-section">

<p class="eyebrow">MCP</p>

## Tools for your agents, from operations you choose

[Apollo MCP Server](https://www.apollographql.com/docs/apollo-mcp-server) turns GraphQL operations into MCP tools. Point it at graphpg, list the operations an agent may run, and give it the agent's token: each call runs as that role, under its policies and limits.

::: code-group

```yaml [mcp.yaml]
endpoint: http://localhost:4000/graphql
transport:
  type: stdio
headers:
  Authorization: "Bearer ${env.GRAPHPG_AGENT_TOKEN}"
schema:
  source: local
  path: ./schema.graphql          # graphpg export-graphql
operations:
  source: local
  paths:
    - ./operations/SearchPosts.graphql
    - ./operations/AddComment.graphql
overrides:
  mutation_mode: explicit         # listed mutations only
```

```graphql [SearchPosts.graphql]
query SearchPosts($query: String!) {
  postsFuzzySearch(query: $query, first: 5) {
    hits { score node { id title excerpt(length: 200) } }
  }
}
```

```json [Claude Desktop]
{
  "mcpServers": {
    "graphpg": {
      "command": "apollo-mcp-server",
      "args": ["/path/to/mcp.yaml"],
      "env": { "GRAPHPG_AGENT_TOKEN": "..." }
    }
  }
}
```

:::

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

<p class="eyebrow">Performance</p>

## Fast where it counts

Each request is one statement, so most of the time is PostgreSQL's own work, and graphpg keeps the rest small. Against PostgREST on the same database, the reads below ran at 1.4 to 3 times the throughput. Where PostgreSQL dominates it was about even: deep `offset` pagination, and bulk updates by filter (2% behind). Single-row writes ran at 1.1 to 1.4 times.

<figure class="home-figure home-figure-wide">
  <img src="/charts/benchmark-throughput.webp" alt="Requests per second, graphpg against PostgREST 16.4 on PostgreSQL 18.6: single row by key 74,031 against 24,606; keyset pagination 35,040 against 22,077; list of 20 rows 33,426 against 21,282; filtered list 32,501 against 19,894; 20 posts with author and comments 17,227 against 11,995" loading="lazy" width="1600" height="885">
  <figcaption>graphpg 1.0.1 and PostgREST 16.4, one run of <code>benchmarks/rest</code> on one machine (2026-10-07). Reads only; every scenario, writes included, is in the <a href="./docs/comparison#throughput-against-postgrest">comparison</a>.</figcaption>
</figure>

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

<p class="eyebrow">From the blog</p>

## Why it works this way

<WritingCards />

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
