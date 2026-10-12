//! Engine config (renames, omissions, operations, virtual keys and relations, function
//! mounts), views and materialized views, composite keys, scalar/array/composite types,
//! identifier quoting, literal `LIKE` filters, and mutations that run in order.
//!
//! Uses an `engine_fixture` schema created on demand (recreated when `FIXTURE_VERSION` changes).
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::{CatalogError, CatalogInspector};
use graphpg::config::Config;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use graphpg::DatabaseCatalog;
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "engine_fixture";
const FIXTURE_VERSION: &str = "engine_fixture v3";

const FIXTURE_SQL: &str = r#"
CREATE SCHEMA engine_fixture;
SET search_path = engine_fixture;

CREATE TABLE authors (id int PRIMARY KEY, full_name text NOT NULL, born date, secret text);
COMMENT ON TABLE authors IS 'People who write';

CREATE TABLE books (
    id bigint PRIMARY KEY,
    author_id int NOT NULL REFERENCES authors(id),
    title text NOT NULL,
    price numeric(24, 6),
    tags text[] NOT NULL DEFAULT '{}',
    ratings int[],
    published_at timestamptz,
    meta jsonb
);

-- Composite primary key, referenced by a composite foreign key
CREATE TABLE editions (
    book_id bigint REFERENCES books(id),
    edition_no int,
    label text NOT NULL,
    PRIMARY KEY (book_id, edition_no)
);
CREATE TABLE reviews (
    id int PRIMARY KEY,
    book_id bigint NOT NULL,
    edition_no int NOT NULL,
    stars int NOT NULL,
    FOREIGN KEY (book_id, edition_no) REFERENCES editions (book_id, edition_no)
);

-- Hidden by the config
CREATE TABLE internal_notes (id int PRIMARY KEY, book_id bigint REFERENCES books(id), note text);

-- Composite type column
CREATE TYPE money_amount AS (amount numeric, currency text);
CREATE TABLE listings (id int PRIMARY KEY, book_id bigint REFERENCES books(id), asking money_amount);

-- Identifiers that need quoting
CREATE TABLE "Odd ""Names""" ("Key" int PRIMARY KEY, "Mixed Case" text, "quote""col" text);

-- Views: not updatable, auto-updatable, materialized
CREATE VIEW book_tags AS SELECT b.id AS book_id, t.tag FROM books b, unnest(b.tags) AS t(tag);
CREATE VIEW cheap_books AS SELECT id, author_id, title, price FROM books WHERE price < 10;
CREATE MATERIALIZED VIEW author_stats AS SELECT author_id, count(*)::int AS book_count FROM books GROUP BY author_id;

-- Table-valued function mounted by the config; computed column renamed by the config
CREATE FUNCTION review_summary(p_book bigint, min_stars int DEFAULT 1)
RETURNS TABLE (review_id int, stars int) LANGUAGE sql STABLE SET search_path = engine_fixture AS $$
    SELECT r.id, r.stars FROM reviews r WHERE r.book_id = p_book AND r.stars >= min_stars ORDER BY r.id
$$;
CREATE FUNCTION authors_display_name(a authors) RETURNS text LANGUAGE sql STABLE AS $$ SELECT upper(a.full_name) $$;

INSERT INTO authors VALUES (1, 'Ada Lovelace', '1815-12-10', 's1'), (2, 'Alan Turing', '1912-06-23', 's2');
INSERT INTO books VALUES
    (1, 1, '50% off', 12345678901234.123456, '{math,poetry}', '{5,4}', '2024-05-01 12:30:00+00', '{"lang": "en"}'),
    (2, 1, '500 off', 5.5, '{math}', NULL, NULL, NULL),
    (9007199254740993, 2, 'big_id', 7, '{}', '{}', NULL, NULL);
INSERT INTO editions VALUES (1, 1, 'First'), (1, 2, 'Second'), (2, 1, 'Only');
INSERT INTO reviews VALUES (1, 1, 1, 5), (2, 1, 2, 3), (3, 2, 1, 4);
INSERT INTO internal_notes VALUES (1, 1, 'secret');
INSERT INTO listings VALUES (1, 1, ROW(19.99, 'EUR'));
INSERT INTO "Odd ""Names""" VALUES (1, 'hello', 'q');
REFRESH MATERIALIZED VIEW author_stats;

-- Enums: a PostgreSQL enum (and an array of it), a CHECK-closed text column
CREATE TYPE book_status AS ENUM ('draft', 'in-print', 'out_of_print');
ALTER TABLE books
    ADD COLUMN status book_status NOT NULL DEFAULT 'draft',
    ADD COLUMN past_statuses book_status[],
    ADD COLUMN cover text CHECK (cover IN ('hard', 'soft'));
UPDATE books SET status = 'in-print', past_statuses = '{draft,in-print}', cover = 'hard' WHERE id = 1;
UPDATE books SET status = 'out_of_print', cover = 'soft' WHERE id = 2;

-- Nested composite inside an array
CREATE TYPE price_point AS (asking money_amount, status book_status, labels text[]);
ALTER TABLE listings ADD COLUMN history price_point[];
UPDATE listings SET history = ARRAY[ROW(ROW(10, 'EUR'), 'draft', '{a,b}')::price_point, ROW(NULL, 'in-print', NULL)::price_point];

-- Search indexes: trigram (one on a column the config hides) and vector
CREATE INDEX authors_name_trgm ON authors USING gin (full_name public.gin_trgm_ops);
CREATE INDEX authors_secret_trgm ON authors USING gin (secret public.gin_trgm_ops);
ALTER TABLE books ADD COLUMN embedding public.vector(3);
UPDATE books SET embedding = (CASE id WHEN 1 THEN '[1,0,0]' WHEN 2 THEN '[0,1,0]' ELSE '[0,0,1]' END)::public.vector;
CREATE INDEX books_title_trgm ON books USING gin (title public.gin_trgm_ops);
CREATE INDEX books_embedding_hnsw ON books USING hnsw (embedding public.vector_cosine_ops);

-- Partitioned table: only the parent is exposed
CREATE TABLE events (id int, at date, PRIMARY KEY (id, at)) PARTITION BY RANGE (at);
CREATE TABLE events_2026 PARTITION OF events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
INSERT INTO events VALUES (1, '2026-03-01');

-- Objects owned by an extension are hidden unless the extension is exposed
CREATE EXTENSION tablefunc SCHEMA engine_fixture;
CREATE TABLE ext_settings (id int PRIMARY KEY, value text);
CREATE FUNCTION ext_numbers(n int) RETURNS TABLE (i int) LANGUAGE sql AS $$ SELECT generate_series(1, n) $$;
ALTER EXTENSION tablefunc ADD TABLE ext_settings;
ALTER EXTENSION tablefunc ADD FUNCTION ext_numbers(int);
"#;

const CONFIG: &str = r#"
entities:
  authors:
    type_name: Writer
    description: Configured description
    list_field: writers
    single_field: writer
    connection_field: writersConnection
    omit_operations: [delete]
    columns:
      full_name: { field_name: name, description: "The writer's name" }
      secret: { omit: true }
  books:
    foreign_keys:
      books_author_id_fkey:
        field_name: writer
        reverse_field_name: works
  book_tags:
    foreign_keys:
      book_tags_book:
        columns: [book_id]
        references: { table: books }
        field_name: book
        reverse_field_name: tagRows
  cheap_books:
    primary_key: [id]
  author_stats:
    primary_key: [author_id]
    foreign_keys:
      author_stats_author:
        columns: [author_id]
        references: { table: authors, columns: [id] }
        field_name: writer
        omit_reverse: true
  internal_notes:
    omit: true
functions:
  review_summary:
    mount: { on: books, field_name: reviewSummary, arguments: { p_book: id } }
  authors_display_name:
    field_name: shout
types:
  book_status:
    values: { in-print: PRINTING }
  books.cover:
    type_name: Cover
  money_amount:
    type_name: Money
    attributes:
      amount: { field_name: value }
"#;

async fn connect() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to PostgreSQL");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    client
}

/// Creates the fixture schema once (serialised by an advisory lock, since tests run
/// concurrently), recreating it when the fixture changed.
async fn ensure_fixture(client: &tokio_postgres::Client) {
    client
        .execute("SELECT pg_advisory_lock(424243)", &[])
        .await
        .unwrap();
    let version: Option<String> = client
        .query_opt(
            "SELECT obj_description(oid, 'pg_namespace') FROM pg_namespace WHERE nspname = $1",
            &[&SCHEMA],
        )
        .await
        .unwrap()
        .and_then(|row| row.get(0));
    if version.as_deref() != Some(FIXTURE_VERSION) {
        client
            .batch_execute(&format!(
                "BEGIN; DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; {FIXTURE_SQL} COMMENT ON SCHEMA {SCHEMA} IS '{FIXTURE_VERSION}'; COMMIT;"
            ))
            .await
            .expect("fixture setup failed");
    }
    client
        .execute("SELECT pg_advisory_unlock(424243)", &[])
        .await
        .unwrap();
}

struct Env {
    client: tokio_postgres::Client,
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: DatabaseCatalog,
    sdl: String,
}

async fn setup_with(config: &str) -> Env {
    let client = connect().await;
    ensure_fixture(&client).await;
    let config = Config::from_yaml(config).expect("config parses");
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .expect("inspection failed");
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    let schema =
        SchemaBuilder::build(&catalog).unwrap_or_else(|e| panic!("invalid schema: {e}\n{sdl}"));
    Env {
        client,
        schema,
        catalog,
        sdl,
    }
}

async fn setup() -> Env {
    setup_with(CONFIG).await
}

impl Env {
    async fn try_run(&self, query: &str) -> Result<serde_json::Value, String> {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &self.schema,
            query,
            "q.graphql",
        )
        .map_err(|e| format!("validation: {e}"))?;
        let plan = QueryPlanner::new(&self.catalog, SCHEMA)
            .plan(&doc)
            .map_err(|e| format!("planning: {e}"))?;
        let result = ExecutionEngine::execute(&self.client, &plan)
            .await
            .map_err(|e| format!("execution: {e}"))?;
        Ok(result["data"].clone())
    }

    async fn run(&self, query: &str) -> serde_json::Value {
        self.try_run(query)
            .await
            .unwrap_or_else(|e| panic!("{e}\nquery: {query}"))
    }

    /// Runs `query` in a transaction that is rolled back afterwards.
    async fn run_rolled_back(&self, query: &str) -> Result<serde_json::Value, String> {
        self.client.batch_execute("BEGIN").await.unwrap();
        let result = self.try_run(query).await;
        self.client.batch_execute("ROLLBACK").await.unwrap();
        result
    }

    /// SDL block of type/input/enum `name`.
    fn type_sdl(&self, keyword: &str, name: &str) -> String {
        let start = self
            .sdl
            .find(&format!("{keyword} {name} "))
            .unwrap_or_else(|| panic!("no {keyword} {name} in SDL:\n{}", self.sdl));
        let end = self.sdl[start..]
            .find("\n}")
            .map(|e| start + e)
            .unwrap_or(self.sdl.len());
        self.sdl[start..end].to_string()
    }
}

#[tokio::test]
async fn test_config_renames_types_fields_and_root_fields() {
    let env = setup().await;

    let writer = env.type_sdl("type", "Writer");
    assert!(
        env.sdl
            .contains("\"\"\"Configured description\"\"\"\ntype Writer implements Node"),
        "{}",
        env.sdl
    );
    assert!(
        writer.contains("\"\"\"The writer's name\"\"\"\n  name: String!"),
        "{writer}"
    );
    assert!(
        !writer.contains("fullName") && !writer.contains("secret"),
        "{writer}"
    );
    assert!(
        writer.contains("  shout: String"),
        "computed column renamed: {writer}"
    );
    assert!(
        writer.contains("  works: [Book!]!") && writer.contains("  worksConnection("),
        "{writer}"
    );
    assert!(
        !writer.contains("authorStats"),
        "reverse relation omitted: {writer}"
    );
    assert!(!env.sdl.contains("type Author "), "old type name gone");
    assert!(env
        .type_sdl("input", "WriterFilter")
        .contains("  name: StringFilter"));
    assert!(env
        .type_sdl("enum", "WriterOrderBy")
        .contains("  NAME_DESC"));

    let query = env.type_sdl("type", "Query");
    assert!(query.contains("  writers: [Writer!]!"), "{query}");
    assert!(query.contains("  writer(id: Int!): Writer"), "{query}");
    assert!(query.contains("  writersConnection("), "{query}");
    assert!(
        !query.contains("allAuthorsConnection") && !query.contains("  authors"),
        "{query}"
    );

    let mutation = env.type_sdl("type", "Mutation");
    assert!(
        mutation.contains("  createWriter(input: CreateWriterInput!, onConflict: WriterOnConflict): CreateWriterPayload!"),
        "{mutation}"
    );
    assert!(mutation.contains("  updateWriter(id: ID!, input: UpdateWriterInput!, \"\"\"Write only if the row's `rowVersion` is still this one (else `STALE_VERSION`).\"\"\" expectedVersion: String): UpdateWriterPayload!"), "{mutation}");
    assert!(!mutation.contains("deleteWriter"), "{mutation}");
    assert!(!env
        .type_sdl("input", "CreateWriterInput")
        .contains("secret"));

    let book = env.type_sdl("type", "Book");
    assert!(
        book.contains("  writer: Writer") && !book.contains("  author: "),
        "{book}"
    );
    assert!(!book.contains("internalNotes"), "omitted table: {book}");
    assert!(
        !env.sdl.contains("InternalNote"),
        "omitted table has no types"
    );
    assert!(
        book.contains("  reviewSummary(minStars: Int): [ReviewSummaryRow!]!"),
        "mounted, p_book from the row: {book}"
    );

    let data = env
        .run(
            r#"{
                writers { databaseId name shout works { title writer { name } } }
                writersConnection(filter: { name: { startsWith: "Ada" } }, orderBy: [NAME_DESC], condition: { name: "Ada Lovelace" }) {
                    totalCount nodes { databaseId born }
                }
                book(id: "1") { reviewSummary(minStars: 4) { reviewId stars } all: reviewSummary { reviewId } }
            }"#,
        )
        .await;
    let ada = data["writers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["databaseId"] == 1)
        .unwrap();
    assert_eq!(ada["name"], "Ada Lovelace");
    assert_eq!(ada["shout"], "ADA LOVELACE");
    assert_eq!(ada["works"][0]["writer"]["name"], "Ada Lovelace");
    assert_eq!(
        data["writersConnection"],
        json!({ "totalCount": 1, "nodes": [{ "databaseId": 1, "born": "1815-12-10" }] })
    );
    assert_eq!(
        data["book"]["reviewSummary"],
        json!([{ "reviewId": 1, "stars": 5 }])
    );
    assert_eq!(
        data["book"]["all"],
        json!([{ "reviewId": 1 }, { "reviewId": 2 }])
    );

    let data = env
        .run_rolled_back(r#"mutation { createWriter(input: { id: 50, name: "Grace Hopper" }) { writer { name works { title } } } }"#)
        .await
        .unwrap();
    assert_eq!(
        data["createWriter"]["writer"],
        json!({ "name": "Grace Hopper", "works": [] })
    );
}

#[tokio::test]
async fn test_views_and_materialized_views() {
    let env = setup().await;

    // Views are exposed; relations declared in the config work in both directions
    let data = env
        .run(
            r#"{
                bookTags { tag book { title } }
                cheapBook(id: "2") { id title }
                authorStats { bookCount writer { name } }
                book(id: "1") { tagRows { tag } tagRowsConnection { totalCount } }
            }"#,
        )
        .await;
    let mut tags: Vec<String> = data["bookTags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            format!(
                "{}:{}",
                t["book"]["title"].as_str().unwrap(),
                t["tag"].as_str().unwrap()
            )
        })
        .collect();
    tags.sort();
    assert_eq!(tags, vec!["50% off:math", "50% off:poetry", "500 off:math"]);
    assert_eq!(data["cheapBook"]["title"], "500 off");
    assert_eq!(data["book"]["tagRowsConnection"]["totalCount"], 2);
    let mut stats: Vec<(i64, String)> = data["authorStats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["bookCount"].as_i64().unwrap(),
                s["writer"]["name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    stats.sort();
    assert_eq!(
        stats,
        vec![
            (1, "Alan Turing".to_string()),
            (2, "Ada Lovelace".to_string())
        ]
    );

    // A declared single-column key makes the view a Relay node
    let id = data["cheapBook"]["id"].as_str().unwrap().to_string();
    let node = env
        .run(&format!(
            r#"{{ node(id: "{id}") {{ ... on CheapBook {{ title }} }} }}"#
        ))
        .await;
    assert_eq!(node["node"]["title"], "500 off");

    // Mutations follow updatability: auto-updatable view yes, others no
    let mutation = env.type_sdl("type", "Mutation");
    assert!(
        mutation.contains("createCheapBook") && mutation.contains("updateCheapBook"),
        "{mutation}"
    );
    assert!(
        !mutation.contains("BookTag") && !mutation.contains("AuthorStat"),
        "{mutation}"
    );
    let data = env
        .run_rolled_back(r#"mutation { updateCheapBook(id: "2", input: { title: "Renamed" }) { cheapBook { title } } }"#)
        .await
        .unwrap();
    assert_eq!(data["updateCheapBook"]["cheapBook"]["title"], "Renamed");
}

#[tokio::test]
async fn test_composite_foreign_and_primary_keys() {
    let env = setup().await;

    // One relation per composite foreign key (no cross product of its columns)
    let review = env.type_sdl("type", "Review");
    assert!(review.contains("  edition: Edition"), "{review}");
    let edition = env.type_sdl("type", "Edition");
    assert!(edition.contains("  reviews: [Review!]!"), "{edition}");
    assert_eq!(review.matches(": Edition").count(), 1, "{review}");

    let data = env
        .run(
            r#"{
                reviews { databaseId stars edition { label book { title } } }
                edition(bookId: "1", editionNo: 2) { label reviews { stars } reviewsConnection { totalCount } }
                reviewsConnection(filter: { edition: { label: { equalTo: "First" } } }) { nodes { databaseId } }
            }"#,
        )
        .await;
    let reviews: Vec<(i64, String)> = data["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["databaseId"].as_i64().unwrap(),
                r["edition"]["label"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        reviews,
        vec![
            (1, "First".into()),
            (2, "Second".into()),
            (3, "Only".into())
        ]
    );
    assert_eq!(
        data["edition"],
        json!({ "label": "Second", "reviews": [{ "stars": 3 }], "reviewsConnection": { "totalCount": 1 } })
    );
    assert_eq!(
        data["reviewsConnection"]["nodes"],
        json!([{ "databaseId": 1 }])
    );

    // Composite-key cursors page through every row exactly once
    let mut seen = Vec::new();
    let mut after = String::new();
    loop {
        let q = format!(
            r#"{{ editionsConnection(first: 1{after}) {{ nodes {{ label }} pageInfo {{ hasNextPage endCursor }} }} }}"#
        );
        let page = env.run(&q).await["editionsConnection"].clone();
        seen.push(page["nodes"][0]["label"].as_str().unwrap().to_string());
        if !page["pageInfo"]["hasNextPage"].as_bool().unwrap() {
            break;
        }
        after = format!(
            r#", after: "{}""#,
            page["pageInfo"]["endCursor"].as_str().unwrap()
        );
    }
    assert_eq!(seen, vec!["First", "Second", "Only"]);

    // Rows addressed by every key column
    let mutation = env.type_sdl("type", "Mutation");
    assert!(mutation.contains("updateEdition(bookId: BigInt!, editionNo: Int!, input: UpdateEditionInput!, \"\"\"Write only if the row's `rowVersion` is still this one (else `STALE_VERSION`).\"\"\" expectedVersion: String)"), "{mutation}");
    let data = env
        .run_rolled_back(
            r#"mutation {
                u: updateEdition(bookId: "1", editionNo: 2, input: { label: "2nd" }) { edition { label } }
                d: deleteReview(id: "2") { review { databaseId } }
                x: deleteEdition(bookId: "1", editionNo: 2) { edition { label } }
            }"#,
        )
        .await
        .unwrap();
    assert_eq!(
        data,
        json!({ "u": { "edition": { "label": "2nd" } }, "d": { "review": { "databaseId": 2 } }, "x": { "edition": { "label": "2nd" } } })
    );
}

#[tokio::test]
async fn test_scalar_array_and_composite_types() {
    let env = setup().await;
    let book = env.type_sdl("type", "Book");
    for field in [
        "databaseId: BigInt!",
        "price: Decimal",
        "tags: [String]!",
        "ratings: [Int]",
        "publishedAt: Datetime",
        "path: String): JSON",
    ] {
        assert!(book.contains(field), "{field}: {book}");
    }
    assert!(env.type_sdl("type", "Writer").contains("  born: Date"));
    assert!(env.type_sdl("type", "Listing").contains("  asking: Money"));
    assert!(env
        .type_sdl("type", "Listing")
        .contains("  history: [PricePoint]"));
    assert!(env
        .type_sdl("input", "MoneyInput")
        .contains("  value: Decimal"));
    assert!(env
        .type_sdl("type", "PricePoint")
        .contains("  status: BookStatus"));
    assert!(env
        .type_sdl("input", "BookFilter")
        .contains("  tags: StringListFilter"));
    assert!(env
        .type_sdl("type", "Query")
        .contains("  book(id: BigInt!): Book"));

    let data = env
        .run(
            r#"{
                big: book(id: "9007199254740993") { databaseId title }
                book(id: "1") { databaseId price tags ratings publishedAt meta }
                booksConnection(filter: { tags: { contains: ["poetry"] } }) { nodes { title } }
                overlap: booksConnection(filter: { tags: { overlaps: ["math", "x"] } }, orderBy: [PRIMARY_KEY_ASC]) {
                    nodes { title } aggregates { max { id price } }
                }
                listing(id: 1) { asking { value currency __typename } history { asking { value } status labels } }
            }"#,
        )
        .await;
    // Values past 2^53 and past float precision survive the trip
    assert_eq!(
        data["big"],
        json!({ "databaseId": "9007199254740993", "title": "big_id" })
    );
    assert_eq!(data["book"]["databaseId"], "1");
    assert_eq!(data["book"]["price"], "12345678901234.123456");
    assert_eq!(data["book"]["tags"], json!(["math", "poetry"]));
    assert_eq!(data["book"]["ratings"], json!([5, 4]));
    assert_eq!(data["book"]["publishedAt"], "2024-05-01T12:30:00+00:00");
    assert_eq!(data["book"]["meta"], json!({ "lang": "en" }));
    assert_eq!(
        data["booksConnection"]["nodes"],
        json!([{ "title": "50% off" }])
    );
    assert_eq!(
        data["overlap"]["nodes"],
        json!([{ "title": "50% off" }, { "title": "500 off" }])
    );
    assert_eq!(
        data["overlap"]["aggregates"]["max"],
        json!({ "id": "2", "price": "12345678901234.123456" })
    );
    assert_eq!(
        data["listing"]["asking"],
        json!({ "value": "19.99", "currency": "EUR", "__typename": "Money" })
    );
    assert_eq!(
        data["listing"]["history"],
        json!([
            { "asking": { "value": "10" }, "status": "DRAFT", "labels": ["a", "b"] },
            { "asking": null, "status": "PRINTING", "labels": null }
        ])
    );

    let data = env
        .run_rolled_back(
            r#"mutation {
                createBook(input: { id: "9007199254740995", authorId: 2, title: "New", price: "0.000001", tags: ["a", "b \"c\""], ratings: [1, null] }) {
                    book { databaseId price tags ratings }
                }
                createListing(input: {
                    id: 2, bookId: "2", asking: { value: "1.5", currency: "USD" },
                    history: [{ asking: { value: "2", currency: "USD" }, status: PRINTING, labels: ["x"] }, null]
                }) { listing { asking { value currency } history { asking { currency } status labels } } }
            }"#,
        )
        .await
        .unwrap();
    assert_eq!(
        data["createBook"]["book"],
        json!({ "databaseId": "9007199254740995", "price": "0.000001", "tags": ["a", "b \"c\""], "ratings": [1, null] })
    );
    assert_eq!(
        data["createListing"]["listing"],
        json!({
            "asking": { "value": "1.5", "currency": "USD" },
            "history": [{ "asking": { "currency": "USD" }, "status": "PRINTING", "labels": ["x"] }, null]
        })
    );
}

#[tokio::test]
async fn test_string_filters_match_wildcards_literally() {
    let env = setup().await;
    let data = env
        .run(
            r#"{
                percent: booksConnection(filter: { title: { startsWith: "50%" } }) { nodes { title } }
                underscore: booksConnection(filter: { title: { contains: "_" } }) { nodes { title } }
                pattern: booksConnection(filter: { title: { like: "50%" } }, orderBy: [PRIMARY_KEY_ASC]) { nodes { title } }
            }"#,
        )
        .await;
    assert_eq!(data["percent"]["nodes"], json!([{ "title": "50% off" }]));
    assert_eq!(data["underscore"]["nodes"], json!([{ "title": "big_id" }]));
    // `like` keeps SQL wildcards
    assert_eq!(
        data["pattern"]["nodes"],
        json!([{ "title": "50% off" }, { "title": "500 off" }])
    );
}

#[tokio::test]
async fn test_identifiers_needing_quotes() {
    let env = setup().await;
    assert!(
        env.sdl.contains("type OddNames implements Node"),
        "{}",
        env.sdl
    );
    let data = env
        .run(r#"{ oddNames { databaseId mixedCase quoteCol } oddNamesConnection(filter: { mixedCase: { equalTo: "hello" } }) { totalCount } }"#)
        .await;
    assert_eq!(
        data["oddNames"],
        json!([{ "databaseId": 1, "mixedCase": "hello", "quoteCol": "q" }])
    );
    assert_eq!(data["oddNamesConnection"]["totalCount"], 1);
    let data = env
        .run_rolled_back(r#"mutation { createOddNames(input: { key: 2, mixedCase: "x", quoteCol: "y" }) { oddNames { mixedCase } } }"#)
        .await
        .unwrap();
    assert_eq!(data["createOddNames"]["oddNames"]["mixedCase"], "x");
}

/// Mutation fields run in order in one transaction: each sees earlier writes, payloads see
/// their own write, and a failure rolls back the whole operation.
#[tokio::test]
async fn test_mutations_run_in_order() {
    let env = setup().await;
    let data = env
        .run_rolled_back(
            r#"mutation {
                a: createWriter(input: { id: 60, name: "First" }) { writer { name } }
                b: createBook(input: { id: "600", authorId: 60, title: "T" }) { book { title writer { name works { title } } } }
                c: updateWriter(id: 60, input: { name: "Renamed" }) { writer { name works { title } } }
            }"#,
        )
        .await
        .unwrap();
    assert_eq!(data["a"]["writer"]["name"], "First");
    assert_eq!(
        data["b"]["book"],
        json!({ "title": "T", "writer": { "name": "First", "works": [{ "title": "T" }] } })
    );
    assert_eq!(
        data["c"]["writer"],
        json!({ "name": "Renamed", "works": [{ "title": "T" }] })
    );

    // Pooled HTTP path: the second field fails, the first is rolled back
    let pool = create_pool(common::db_url(), 2).unwrap();
    let state = Arc::new(ServerState::new(
        pool,
        env.schema.clone(),
        env.catalog.clone(),
        SCHEMA.to_string(),
        DebugOptions::default(),
    ));
    let body = json!({ "query": r#"mutation {
        a: createWriter(input: { id: 61, name: "Rolled back" }) { writer { name } }
        b: createBook(input: { id: "601", authorId: 999999, title: "Orphan" }) { book { title } }
    }"# });
    let request = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = create_router(state).oneshot(request).await.unwrap();
    // A foreign key violation is a typed error of the field, not a server error
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.contains("`b`") && message.contains("rolled back"),
        "{message}"
    );
    assert_eq!(json["errors"][0]["path"], json!(["b"]));
    assert_eq!(
        json["errors"][0]["extensions"]["type"], "BookErrorCode",
        "{json}"
    );
    assert_eq!(
        json["errors"][0]["extensions"]["fields"],
        json!(["input.authorId"]),
        "{json}"
    );
    let count: i64 = env
        .client
        .query_one(
            "SELECT count(*) FROM engine_fixture.authors WHERE id = 61",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "first mutation was rolled back");
}

#[tokio::test]
async fn test_config_errors_are_reported_together() {
    let client = connect().await;
    ensure_fixture(&client).await;
    let config = Config::from_yaml(
        r#"
entities:
  nope: { type_name: X }
  authors:
    type_name: "not valid"
    columns:
      missing: { omit: true }
  book_tags:
    primary_key: [nope]
    foreign_keys:
      no_such_constraint: { field_name: x }
      bad_target:
        columns: [book_id]
        references: { table: ghosts }
functions:
  review_summary:
    mount: { on: books, arguments: { nope: id } }
"#,
    )
    .unwrap();
    let err = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap_err();
    let CatalogError::Config(errors) = &err else {
        panic!("{err}")
    };
    let text = errors.join("\n");
    for expected in [
        "entities.nope: no table",
        "entities.book_tags.primary_key: no column `nope`",
        "entities.book_tags.foreign_keys.no_such_constraint: `book_tags` has no foreign key",
        "entities.book_tags.foreign_keys.bad_target.references.table: no relation `ghosts`",
        "functions.review_summary.mount.arguments: `review_summary` has no argument `nope`",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }

    // Structural problems stop before naming ones are checked; those are reported together too
    let config = Config::from_yaml(
        r#"
entities:
  authors:
    type_name: "not valid"
    columns:
      missing: { omit: true }
types:
  nope: {}
  book_status:
    values: { missing: X, draft: PRINTING, in-print: PRINTING }
  money_amount:
    values: { a: B }
"#,
    )
    .unwrap();
    let err = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap_err();
    let text = err.to_string();
    for expected in [
        "entities.authors.type_name: `not valid` is not a valid GraphQL name",
        "entities.authors.columns.missing: `authors` has no column `missing`",
        "types.nope: no enum, composite type or domain `nope`",
        "types.book_status.values: `book_status` has no value `missing`",
        "types.book_status.values: two values are named `PRINTING`",
        "types.money_amount.values: `money_amount` is a composite type",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }

    let config = Config::from_yaml("extensions: { expose: [nope] }").unwrap();
    let err = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("extensions.expose: no extension `nope` is installed"),
        "{err}"
    );
}

#[tokio::test]
async fn test_without_config_views_have_no_keys() {
    let env = setup_with("").await;
    let query = env.type_sdl("type", "Query");
    // Keyless relations: lists and connections, no single-row lookups
    assert!(
        query.contains("  bookTags: [BookTag!]!") && query.contains("  cheapBooks: [CheapBook!]!"),
        "{query}"
    );
    assert!(
        !query.contains("  cheapBook(") && !query.contains("  bookTag("),
        "{query}"
    );
    assert!(env.sdl.contains("type Author implements Node"));
    // Default enum names: the type's name, and <Type><Field> for CHECK-closed columns
    assert!(
        env.type_sdl("enum", "BookStatus").contains(
            "  \"\"\"`in-print`\"\"\"\n  IN_PRINT\n  \"\"\"`out_of_print`\"\"\"\n  OUT_OF_PRINT"
        ),
        "{}",
        env.sdl
    );
    assert!(env.type_sdl("enum", "BookCover").contains("  HARD\n"));
    assert!(env
        .type_sdl("type", "Listing")
        .contains("  asking: MoneyAmount"));
    let data = env
        .run(r#"{ cheapBooksConnection { totalCount nodes { title } } }"#)
        .await;
    assert_eq!(
        data["cheapBooksConnection"],
        json!({ "totalCount": 2, "nodes": [{ "title": "500 off" }, { "title": "big_id" }] })
    );
}

#[tokio::test]
async fn test_enum_types() {
    let env = setup().await;
    let status = env.type_sdl("enum", "BookStatus");
    assert!(status.contains("  DRAFT\n  \"\"\"`in-print`\"\"\"\n  PRINTING\n  \"\"\"`out_of_print`\"\"\"\n  OUT_OF_PRINT"), "{status}");
    assert!(env
        .type_sdl("enum", "Cover")
        .contains("  \"\"\"`hard`\"\"\"\n  HARD"));
    let book = env.type_sdl("type", "Book");
    for field in [
        "status: BookStatus!",
        "pastStatuses: [BookStatus]",
        "cover: Cover",
    ] {
        assert!(book.contains(field), "{field}: {book}");
    }
    let filter = env.type_sdl("input", "BookFilter");
    for field in [
        "status: BookStatusFilter",
        "pastStatuses: BookStatusListFilter",
        "cover: CoverFilter",
    ] {
        assert!(filter.contains(field), "{field}: {filter}");
    }

    let data = env
        .run(
            r#"{
                book(id: "1") { status pastStatuses cover }
                byStatus: booksConnection(filter: { status: { in: [PRINTING, OUT_OF_PRINT] } }, orderBy: [STATUS_DESC]) {
                    nodes { title status } aggregates { max { status } min { cover } }
                }
                soft: booksConnection(condition: { cover: SOFT }) { nodes { title } }
                past: booksConnection(filter: { pastStatuses: { contains: [PRINTING] } }) { nodes { title } }
                notDraft: booksConnection(filter: { status: { notEqualTo: DRAFT } }) { totalCount }
            }"#,
        )
        .await;
    assert_eq!(
        data["book"],
        json!({ "status": "PRINTING", "pastStatuses": ["DRAFT", "PRINTING"], "cover": "HARD" })
    );
    assert_eq!(
        data["byStatus"],
        json!({
            "nodes": [{ "title": "500 off", "status": "OUT_OF_PRINT" }, { "title": "50% off", "status": "PRINTING" }],
            "aggregates": { "max": { "status": "OUT_OF_PRINT" }, "min": { "cover": "HARD" } }
        })
    );
    assert_eq!(data["soft"]["nodes"], json!([{ "title": "500 off" }]));
    assert_eq!(data["past"]["nodes"], json!([{ "title": "50% off" }]));
    assert_eq!(data["notDraft"]["totalCount"], 2);

    let data = env
        .run_rolled_back(
            r#"mutation {
                createBook(input: { id: "700", authorId: 1, title: "E", status: PRINTING, cover: SOFT, pastStatuses: [DRAFT, null] }) {
                    book { status cover pastStatuses }
                }
            }"#,
        )
        .await
        .unwrap();
    assert_eq!(
        data["createBook"]["book"],
        json!({ "status": "PRINTING", "cover": "SOFT", "pastStatuses": ["DRAFT", null] })
    );
}

/// Search is a root field per method returning the best matches, not a connection: no
/// pagination, `first` within the configured limits, `hasMore` past them. Fields are named
/// by GraphQL names; hidden columns are not searchable; hybrid search fuses rankings on the
/// table's own key.
#[tokio::test]
async fn test_search_fields() {
    let env = setup().await;
    let fuzzy_field = env.type_sdl("enum", "WriterFuzzyField");
    assert!(
        fuzzy_field.contains("  NAME")
            && !fuzzy_field.contains("SECRET")
            && !fuzzy_field.contains("FULL_NAME"),
        "{fuzzy_field}"
    );
    let query = env.type_sdl("type", "Query");
    for field in [
        "writersFuzzySearch(",
        "booksFuzzySearch(",
        "booksSemanticSearch(",
        "booksHybridSearch(",
    ] {
        assert!(query.contains(field), "{field}: {query}");
    }
    assert!(
        !query.contains("writersSemanticSearch") && !query.contains("Bm25Search"),
        "{query}"
    );
    assert!(
        query.contains("vector: [Float!]!, \"\"\"Weight of the lexical ranking"),
        "{query}"
    );
    assert!(env
        .type_sdl("type", "BookSearchHit")
        .contains("score: Float!"));
    assert!(!env.type_sdl("type", "BookEdge").contains("Score"));

    // Connections do not search, and searches do not page
    for query in [
        r#"{ writersConnection(fuzzy: { name: "Ada" }) { totalCount } }"#,
        r#"{ writersFuzzySearch(query: "Ada", after: "x") { hasMore } }"#,
        r#"{ writersFuzzySearch(query: "Ada", last: 1) { hasMore } }"#,
        r#"{ writersFuzzySearch(query: "Ada") { totalCount } }"#,
        r#"{ writersFuzzySearch { hasMore } }"#,
    ] {
        let err = env.try_run(query).await.unwrap_err();
        assert!(err.starts_with("validation"), "{query}: {err}");
    }

    let data = env
        .run(
            r#"{
                fuzzy: writersFuzzySearch(query: "Ada Lovlace") { hasMore hits { score node { name } } }
                explicit: writersFuzzySearch(field: NAME, query: "Alan Turin") { hits { node { name } } }
                semantic: booksSemanticSearch(vector: [1, 0.1, 0], first: 1) { hasMore hits { score node { title } } }
                filtered: booksSemanticSearch(vector: [1, 0.1, 0], filter: { title: { equalTo: "500 off" } }) {
                    hasMore hits { node { title } }
                }
                hybrid: booksHybridSearch(query: "500 off", vector: [0, 1, 0], first: 1) { hits { score node { title } } }
                typed: writersFuzzySearch(query: "ada lov") { hits { score node { name } } }
                none: writersFuzzySearch(query: "zzzz") { hasMore hits { node { name } } }
            }"#,
        )
        .await;
    assert_eq!(
        data["fuzzy"]["hits"][0]["node"],
        json!({ "name": "Ada Lovelace" })
    );
    assert!(data["fuzzy"]["hits"][0]["score"].as_f64().unwrap() > 0.5);
    assert_eq!(data["fuzzy"]["hasMore"], false);
    assert_eq!(
        data["explicit"]["hits"],
        json!([{ "node": { "name": "Alan Turing" } }])
    );
    assert_eq!(
        data["semantic"]["hits"][0]["node"],
        json!({ "title": "50% off" })
    );
    assert!(data["semantic"]["hits"][0]["score"].as_f64().unwrap() > 0.9);
    assert_eq!(data["semantic"]["hasMore"], true);
    assert_eq!(
        data["filtered"],
        json!({ "hasMore": false, "hits": [{ "node": { "title": "500 off" } }] })
    );
    assert_eq!(
        data["hybrid"]["hits"][0]["node"],
        json!({ "title": "500 off" })
    );
    assert!(data["hybrid"]["hits"][0]["score"].as_f64().unwrap() > 0.0);
    // Word similarity matches a partly typed value
    assert_eq!(
        data["typed"]["hits"][0]["node"],
        json!({ "name": "Ada Lovelace" })
    );
    assert!(data["typed"]["hits"][0]["score"].as_f64().unwrap() > 0.8);
    assert_eq!(data["none"], json!({ "hasMore": false, "hits": [] }));

    // Sizes are parameters: every `first` shares one prepared statement
    let sql = |first: u32| {
        let query = format!(
            r#"{{ booksHybridSearch(query: "x", vector: [0, 1, 0], first: {first}, rrfK: {first}) {{ hits {{ score }} }} }}"#
        );
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &env.schema,
            &query,
            "q.graphql",
        )
        .unwrap();
        graphpg::compiler::SqlCompiler::compile(
            &QueryPlanner::new(&env.catalog, SCHEMA).plan(&doc).unwrap(),
        )
        .sql
    };
    assert_eq!(sql(1), sql(40));

    // `first`: 10 by default, at most 50, both configurable
    let err = env
        .try_run("{ booksSemanticSearch(vector: [1, 0, 0], first: 51) { hasMore } }")
        .await
        .unwrap_err();
    assert!(err.contains("`first` must be between 0 and 50"), "{err}");
    let env = setup_with(&format!(
        "{CONFIG}\nsearch: {{ default_limit: 2, max_limit: 2 }}\n"
    ))
    .await;
    assert!(env
        .type_sdl("type", "Query")
        .contains("Number of hits: 2 by default, at most 2."));
    let data = env
        .run("{ booksSemanticSearch(vector: [1, 0, 0]) { hasMore hits { node { title } } } }")
        .await;
    assert_eq!(
        data["booksSemanticSearch"]["hits"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(data["booksSemanticSearch"]["hasMore"], true);
    assert!(env
        .try_run("{ booksSemanticSearch(vector: [1, 0, 0], first: 3) { hasMore } }")
        .await
        .is_err());
    let config = Config::from_yaml(&format!(
        "{CONFIG}\nsearch: {{ default_limit: 20, max_limit: 5 }}\n"
    ))
    .unwrap();
    let err = CatalogInspector::new(&env.client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("search.default_limit"),
        "{err:?}"
    );
}

#[tokio::test]
async fn test_partitions_and_extension_objects() {
    // Partitions are hidden behind their partitioned table; extension objects are hidden
    let env = setup().await;
    assert!(env.sdl.contains("type Event {"), "{}", env.sdl);
    assert!(!env.sdl.contains("Events2026") && !env.sdl.contains("events2026"));
    assert!(!env.sdl.contains("ExtSetting") && !env.sdl.contains("extNumbers"));
    let data = env.run("{ events { id at } }").await;
    assert_eq!(data["events"], json!([{ "id": 1, "at": "2026-03-01" }]));

    // ... unless their extension is exposed
    for expose in ["[tablefunc]", "['*']"] {
        let env = setup_with(&format!("extensions: {{ expose: {expose} }}")).await;
        assert!(
            env.sdl.contains("type ExtSetting implements Node"),
            "{expose}"
        );
        assert_eq!(
            env.run("{ extSettings { value } }").await["extSettings"],
            json!([]),
            "{expose}"
        );
        // Declared without a volatility, so VOLATILE: a mutation
        let data = env
            .run_rolled_back("mutation { extNumbers(n: 3) { i } }")
            .await
            .unwrap();
        assert_eq!(
            data["extNumbers"],
            json!([{ "i": 1 }, { "i": 2 }, { "i": 3 }]),
            "{expose}"
        );
    }
}

/// `groupedAggregates` keys use GraphQL values (enum values, dates), and `having` filters
/// groups by their aggregates.
#[tokio::test]
async fn test_grouped_aggregates_keys_and_having() {
    let env = setup().await;
    let values = |name: &str| -> Vec<String> {
        env.type_sdl("enum", name)
            .lines()
            .skip(1)
            .map(|l| l.trim().to_string())
            .collect()
    };
    let book = values("BookGroupBy");
    assert!(
        book.iter().any(|v| v == "STATUS")
            && book
                .iter()
                .any(|v| v == "PUBLISHED_AT_TRUNCATED_TO_QUARTER"),
        "{book:?}"
    );
    assert!(
        !book.iter().any(|v| v.starts_with("TITLE_TRUNCATED")),
        "only dates and timestamps are truncated: {book:?}"
    );
    let writer = values("WriterGroupBy");
    assert!(
        writer.iter().any(|v| v == "BORN_TRUNCATED_TO_YEAR"),
        "{writer:?}"
    );
    assert!(
        !writer.iter().any(|v| v == "BORN_TRUNCATED_TO_HOUR"),
        "dates have no hours: {writer:?}"
    );

    let data = env
        .run(
            r#"{
                byStatus: booksConnection { groupedAggregates(groupBy: [STATUS]) { keys count max { price } } }
                expensive: booksConnection { groupedAggregates(groupBy: [STATUS], having: { sum: { price: { greaterThan: 100 } } }) { keys } }
                byYear: writersConnection { groupedAggregates(groupBy: [BORN_TRUNCATED_TO_YEAR]) { keys count } }
            }"#,
        )
        .await;
    assert_eq!(
        data["byStatus"]["groupedAggregates"],
        json!([
            { "keys": ["DRAFT"], "count": 1, "max": { "price": "7.000000" } },
            { "keys": ["PRINTING"], "count": 1, "max": { "price": "12345678901234.123456" } },
            { "keys": ["OUT_OF_PRINT"], "count": 1, "max": { "price": "5.500000" } }
        ])
    );
    assert_eq!(
        data["expensive"]["groupedAggregates"],
        json!([{ "keys": ["PRINTING"] }])
    );
    assert_eq!(
        data["byYear"]["groupedAggregates"],
        json!([{ "keys": ["1815-01-01"], "count": 1 }, { "keys": ["1912-01-01"], "count": 1 }])
    );
}

/// A type whose root reads are omitted is reachable through its relations only: neither
/// `node(id:)` nor Federation's `_entities` looks it up from an ID a client wrote.
#[tokio::test]
async fn test_omitted_reads_are_not_reachable_by_id() {
    let env = setup_with(
        r#"
entities:
  authors:
    type_name: Writer
    omit_operations: [read, list, connection]
"#,
    )
    .await;
    let id = SchemaBuilder::encode_global_id("Writer", "1");
    let data = env
        .run(&format!(
            r#"{{ node(id: "{id}") {{ id ... on Writer {{ fullName }} }} }}"#
        ))
        .await;
    assert_eq!(data["node"], serde_json::Value::Null, "{data}");

    // Through a relation it is still there
    let data = env.run("{ book(id: 1) { author { fullName } } }").await;
    assert_eq!(data["book"]["author"]["fullName"], "Ada Lovelace", "{data}");
}
