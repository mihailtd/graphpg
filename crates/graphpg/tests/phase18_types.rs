//! More types, each detected from the database: full-text search over `tsvector` documents
//! (columns and `to_tsvector` expression indexes, with highlights), JSON path filters and
//! extraction, multiranges, global search across tables, and PostGIS geometries (GeoJSON,
//! spatial filters, nearest search).
//!
//! Uses a `types_fixture` schema created on demand. PostGIS is not installed on the test
//! server, so its support is checked on a catalog built in memory: the schema it produces
//! and the SQL it compiles to.
mod common;
use graphpg::catalog::{finalize, CatalogInspector};
use graphpg::config::EngineConfig;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::{DatabaseCatalog, SqlCompiler};
use serde_json::json;
use tokio_postgres::NoTls;

const SCHEMA: &str = "types_fixture";
const FIXTURE_VERSION: &str = "types_fixture v1";

const FIXTURE_SQL: &str = r#"
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE SCHEMA types_fixture;
SET search_path = types_fixture, public;

-- A generated document column with a GIN index
CREATE TABLE articles (
    id serial PRIMARY KEY,
    title text NOT NULL,
    body text NOT NULL,
    doc tsvector GENERATED ALWAYS AS (to_tsvector('english', title || ' ' || body)) STORED,
    meta jsonb NOT NULL DEFAULT '{}'
);
CREATE INDEX articles_doc ON articles USING gin (doc);
-- An expression index, in another configuration
CREATE TABLE notes (id serial PRIMARY KEY, text text NOT NULL);
CREATE INDEX notes_fts ON notes USING gin (to_tsvector('simple', text));
-- Only a trigram index
CREATE TABLE people (id serial PRIMARY KEY, name text NOT NULL);
CREATE INDEX people_name_trgm ON people USING gin (name gin_trgm_ops);

CREATE TABLE rooms (id serial PRIMARY KEY, name text NOT NULL, open_at tstzmultirange, seats int4multirange);

INSERT INTO articles (title, body, meta) VALUES
    ('PostgreSQL full text search', 'Searching documents with ranking and highlighting', '{"tags": ["db", "search"], "stars": 5, "author": {"name": "Ada"}}'),
    ('Rust ownership', 'Borrowing rules explained', '{"tags": ["lang"], "stars": 3}'),
    ('Cooking pasta', 'Boil water, add salt', '{"stars": 1}');
INSERT INTO notes (text) VALUES ('search engines index documents'), ('buy milk');
INSERT INTO people (name) VALUES ('Search Man'), ('Ada Lovelace');
INSERT INTO rooms (name, open_at, seats) VALUES
    ('A', '{[2024-01-01 09:00+00, 2024-01-01 12:00+00), [2024-01-01 13:00+00, 2024-01-01 17:00+00)}', '{[1,10), [20,30)}'),
    ('B', '{}', NULL);
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

async fn ensure_fixture(client: &tokio_postgres::Client) {
    client
        .execute("SELECT pg_advisory_lock(424249)", &[])
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
        .execute("SELECT pg_advisory_unlock(424249)", &[])
        .await
        .unwrap();
}

struct Env {
    client: tokio_postgres::Client,
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: DatabaseCatalog,
    sdl: String,
}

async fn setup() -> Env {
    let client = connect().await;
    ensure_fixture(&client).await;
    let catalog = CatalogInspector::new(&client)
        .inspect_schema(SCHEMA)
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

impl Env {
    async fn run(&self, query: &str) -> serde_json::Value {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &self.schema,
            query,
            "q.graphql",
        )
        .unwrap_or_else(|e| panic!("validation: {e}\nquery: {query}"));
        let plan = QueryPlanner::new(&self.catalog, SCHEMA)
            .plan(&doc)
            .unwrap_or_else(|e| panic!("planning: {e}"));
        let result = ExecutionEngine::execute(&self.client, &plan)
            .await
            .unwrap_or_else(|e| panic!("execution: {e}\nquery: {query}"));
        result["data"].clone()
    }

    fn block(&self, keyword: &str, name: &str) -> String {
        block(&self.sdl, keyword, name)
    }
}

fn block(sdl: &str, keyword: &str, name: &str) -> String {
    let start = sdl
        .find(&format!("{keyword} {name} "))
        .unwrap_or_else(|| panic!("no {keyword} {name}:\n{sdl}"));
    let end = sdl[start..]
        .find("\n}")
        .map(|e| start + e + 1)
        .unwrap_or(sdl.len());
    sdl[start..end].to_string()
}

fn field_values(data: &serde_json::Value, key: &str, field: &str) -> Vec<serde_json::Value> {
    data[key]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("no {key} in {data}"))
        .iter()
        .map(|n| n[field].clone())
        .collect()
}

#[tokio::test]
async fn test_full_text_search() {
    let env = setup().await;
    let query = env.block("type", "Query");
    assert!(
        query.contains("  articlesFullTextSearch(") && query.contains("  notesFullTextSearch("),
        "{query}"
    );
    assert!(env
        .block("enum", "ArticleFullTextField")
        .contains("  DOC\n"));
    // An expression index is named after the columns it reads
    assert!(env.block("enum", "NoteFullTextField").contains("  TEXT\n"));
    assert!(env
        .block("type", "ArticleSearchHit")
        .contains("  highlight(startSel: String = \"<b>\""));
    assert!(env
        .block("input", "ArticleFilter")
        .contains("  doc: FullTextFilter\n"));

    let data = env
        .run(
            r#"{
                stemmed: articlesFullTextSearch(query: "search documents") {
                    hits { score highlight node { title } }
                    hasMore
                }
                custom: articlesFullTextSearch(query: "borrowing", first: 1) {
                    hits { highlight(startSel: "[", stopSel: "]") node { title } }
                }
                excluded: articlesFullTextSearch(query: "rules -rust") { hits { node { title } } }
                phrase: articlesFullTextSearch(query: "\"add salt\"") { hits { node { title } } }
                notes: notesFullTextSearch(query: "documents") { hits { highlight node { text } } }
                filtered: articlesConnection(filter: { doc: { matches: "borrowing or pasta" } }) { nodes { title } }
            }"#,
        )
        .await;
    let hit = &data["stemmed"]["hits"][0];
    assert_eq!(hit["node"]["title"], "PostgreSQL full text search");
    assert!(hit["score"].as_f64().unwrap() > 0.0);
    let highlight = hit["highlight"].as_str().unwrap();
    assert!(
        highlight.contains("<b>search</b>") && highlight.contains("<b>Searching</b>"),
        "stemmed matches are marked: {highlight}"
    );
    assert_eq!(data["stemmed"]["hasMore"], false);
    assert!(
        data["custom"]["hits"][0]["highlight"]
            .as_str()
            .unwrap()
            .contains("[Borrowing]"),
        "{data}"
    );
    assert_eq!(data["excluded"]["hits"], json!([]));
    assert_eq!(
        data["phrase"]["hits"],
        json!([{ "node": { "title": "Cooking pasta" } }])
    );
    // An expression index: its own configuration (`simple`) and text
    assert_eq!(
        data["notes"]["hits"][0]["node"]["text"],
        "search engines index documents"
    );
    assert!(data["notes"]["hits"][0]["highlight"]
        .as_str()
        .unwrap()
        .contains("<b>documents</b>"));
    assert_eq!(
        field_values(&data, "filtered", "title"),
        [json!("Rust ownership"), json!("Cooking pasta")]
    );
}

#[tokio::test]
async fn test_json_paths() {
    let env = setup().await;
    let filter = env.block("input", "JSONFilter");
    for op in [
        "matchesPath: String",
        "existsPath: String",
        "hasKey: String",
        "hasAnyKeys: [String!]",
    ] {
        assert!(filter.contains(op), "{op}: {filter}");
    }
    let data = env
        .run(
            r#"{
                stars: articlesConnection(filter: { meta: { matchesPath: "$.stars > 2" } }) { nodes { title } }
                tagged: articlesConnection(filter: { meta: { existsPath: "$.tags[*] ? (@ == \"db\")" } }) { nodes { title } }
                hasTags: articlesConnection(filter: { meta: { hasKey: "tags" } }) { nodes { title } }
                any: articlesConnection(filter: { meta: { hasAnyKeys: ["author", "nope"] } }) { nodes { title } }
                all: articlesConnection(filter: { meta: { hasAllKeys: ["tags", "stars"] } }) { nodes { title } }
                extract: articles { author: meta(path: "$.author.name") firstTag: meta(path: "$.tags[0]") meta }
            }"#,
        )
        .await;
    assert_eq!(
        field_values(&data, "stars", "title"),
        [
            json!("PostgreSQL full text search"),
            json!("Rust ownership")
        ]
    );
    assert_eq!(
        field_values(&data, "tagged", "title"),
        [json!("PostgreSQL full text search")]
    );
    assert_eq!(field_values(&data, "hasTags", "title").len(), 2);
    assert_eq!(
        field_values(&data, "any", "title"),
        [json!("PostgreSQL full text search")]
    );
    assert_eq!(field_values(&data, "all", "title").len(), 2);
    assert_eq!(data["extract"][0]["author"], "Ada");
    assert_eq!(data["extract"][0]["firstTag"], "db");
    assert_eq!(
        data["extract"][2]["author"],
        json!(null),
        "a path selecting nothing is null"
    );
    assert_eq!(
        data["extract"][2]["meta"],
        json!({ "stars": 1 }),
        "without a path, the whole value"
    );
}

#[tokio::test]
async fn test_multiranges() {
    let env = setup().await;
    let room = env.block("type", "Room");
    assert!(
        room.contains("  openAt: [DatetimeRange!]\n") && room.contains("  seats: [IntRange!]\n"),
        "{room}"
    );
    assert!(env
        .block("input", "CreateRoomInput")
        .contains("  seats: [IntRangeInput!]\n"));
    assert!(env
        .block("input", "RoomFilter")
        .contains("  openAt: DatetimeMultirangeFilter\n"));

    let data = env
        .run(
            r#"{
                rooms { name seats { start { value inclusive } end { value } } openAt { start { value } } }
                open: roomsConnection(filter: { openAt: { contains: "2024-01-01T10:00:00Z" } }) { nodes { name } }
                closed: roomsConnection(filter: { openAt: { contains: "2024-01-01T12:30:00Z" } }) { nodes { name } }
                block: roomsConnection(filter: { seats: { containsRange: { start: { value: 2 }, end: { value: 5 } } } }) { nodes { name } }
                overlap: roomsConnection(filter: { seats: { overlaps: { start: { value: 15 }, end: { value: 21 } } } }) { nodes { name } }
                empty: roomsConnection(filter: { openAt: { isEmpty: true } }) { nodes { name } }
                same: roomsConnection(filter: { seats: { equalTo: [{ start: { value: 20 }, end: { value: 30 } }, { start: { value: 1 }, end: { value: 10 } }] } }) { nodes { name } }
            }"#,
        )
        .await;
    assert_eq!(
        data["rooms"][0]["seats"],
        json!([{ "start": { "value": 1, "inclusive": true }, "end": { "value": 10 } }, { "start": { "value": 20, "inclusive": true }, "end": { "value": 30 } }])
    );
    assert_eq!(
        data["rooms"][0]["openAt"][1]["start"]["value"],
        "2024-01-01T13:00:00+00:00"
    );
    assert_eq!(
        data["rooms"][1],
        json!({ "name": "B", "seats": null, "openAt": [] })
    );
    assert_eq!(field_values(&data, "open", "name"), [json!("A")]);
    assert_eq!(
        field_values(&data, "closed", "name"),
        Vec::<serde_json::Value>::new(),
        "between the ranges"
    );
    assert_eq!(field_values(&data, "block", "name"), [json!("A")]);
    assert_eq!(field_values(&data, "overlap", "name"), [json!("A")]);
    assert_eq!(field_values(&data, "empty", "name"), [json!("B")]);
    assert_eq!(
        field_values(&data, "same", "name"),
        [json!("A")],
        "multiranges are normalised"
    );

    // Written from a list of ranges (overlapping ones merge)
    env.client.batch_execute("BEGIN").await.unwrap();
    let created = env
        .run(r#"mutation { createRoom(input: { name: "C", seats: [{ start: { value: 1 }, end: { value: 3 } }, { start: { value: 2 }, end: { value: 6 } }] }) { room { seats { start { value } end { value } } } } }"#)
        .await;
    env.client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(
        created["createRoom"]["room"]["seats"],
        json!([{ "start": { "value": 1 }, "end": { "value": 6 } }])
    );
}

#[tokio::test]
async fn test_global_search_across_tables() {
    let env = setup().await;
    assert!(
        env.sdl
            .contains("union GlobalSearchNode = Article | Note | People\n"),
        "{}",
        env.block("type", "Query")
    );
    let data = env
        .run(
            r#"{ globalSearch(query: "search") {
                hits { score node { __typename ... on Article { title } ... on Note { text } ... on People { name } } }
                hasMore
            } }"#,
        )
        .await;
    let hits = data["globalSearch"]["hits"].as_array().unwrap();
    let found: Vec<(&str, &serde_json::Value)> = hits
        .iter()
        .map(|h| (h["node"]["__typename"].as_str().unwrap(), &h["node"]))
        .collect();
    assert!(
        found
            .iter()
            .any(|(t, n)| *t == "Article" && n["title"] == "PostgreSQL full text search"),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|(t, n)| *t == "Note" && n["text"] == "search engines index documents"),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|(t, n)| *t == "People" && n["name"] == "Search Man"),
        "trigram-indexed types too: {found:?}"
    );
    assert!(found.iter().all(|(_, n)| n.get("title").is_some()
        || n.get("text").is_some()
        || n.get("name").is_some()));
    let scores: Vec<f64> = hits.iter().map(|h| h["score"].as_f64().unwrap()).collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "best first: {scores:?}"
    );

    let limited = env.run(r#"{ globalSearch(query: "search", first: 1) { hits { node { __typename } } hasMore } }"#).await;
    assert_eq!(limited["globalSearch"]["hits"].as_array().unwrap().len(), 1);
    assert_eq!(limited["globalSearch"]["hasMore"], true);
}

/// A catalog with PostGIS columns, as the inspector reads it from a database with PostGIS.
fn spatial_catalog() -> DatabaseCatalog {
    let column = |name: &str, pg_type: serde_json::Value, pk: bool| json!({ "name": name, "pg_type": pg_type, "is_nullable": !pk, "is_primary_key": pk, "default_value": null, "is_generated": false, "description": null });
    let catalog: DatabaseCatalog = serde_json::from_value(json!({
        "tables": [{
            "schema": "geo", "name": "places", "primary_key": ["id"], "foreign_keys": [], "description": null,
            "columns": [column("id", json!("Int4"), true), column("location", json!("Geography"), false), column("area", json!("Geometry"), false)],
        }],
        "functions": [], "exclusive_arcs": [], "many_to_many": [],
        "search_indexes": [{
            "table_schema": "geo", "table_name": "places", "index_name": "places_location", "column_name": "location",
            "access_method": "gist", "operator_class": "gist_geography_ops", "kind": "Spatial",
        }],
        "server_version": 180000,
    }))
    .expect("catalog");
    finalize(catalog, &EngineConfig::default()).expect("finalize")
}

#[test]
fn test_postgis_schema_and_sql() {
    let catalog = spatial_catalog();
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    let schema =
        SchemaBuilder::build(&catalog).unwrap_or_else(|e| panic!("invalid schema: {e}\n{sdl}"));
    assert!(sdl.contains("scalar GeoJSON\n"));
    let place = block(&sdl, "type", "Place");
    assert!(
        place.contains("  location: GeoJSON\n") && place.contains("  area: GeoJSON\n"),
        "{place}"
    );
    assert!(block(&sdl, "input", "PlaceFilter").contains("  location: GeoJSONFilter\n"));
    assert!(
        block(&sdl, "input", "GeoJSONFilter").contains("  withinDistance: GeoJSONDistanceInput\n")
    );
    assert!(block(&sdl, "type", "Query").contains("  placesNearestSearch(\"\"\"The geometry to find the nearest rows to.\"\"\" near: GeoJSON!"));
    assert!(block(&sdl, "type", "PlaceSearchHit").contains("  distance: Float\n"));

    let compile = |query: &str| -> String {
        let doc =
            apollo_compiler::ExecutableDocument::parse_and_validate(&schema, query, "q.graphql")
                .unwrap_or_else(|e| panic!("{e}"));
        let plan = QueryPlanner::new(&catalog, "geo")
            .plan(&doc)
            .unwrap_or_else(|e| panic!("planning: {e}"));
        format!("{:?}", SqlCompiler::compile_operation(&plan))
    };
    let point = r#"{ type: "Point", coordinates: [2.35, 48.85] }"#;
    let sql = compile(&format!(
        "{{ placesNearestSearch(near: {point}, maxDistance: 500) {{ hits {{ distance node {{ location }} }} }} }}"
    ));
    assert!(
        sql.contains("<-> ST_GeomFromGeoJSON($"),
        "KNN ordering: {sql}"
    );
    assert!(
        sql.contains("::geography"),
        "geography columns compare as geographies: {sql}"
    );
    assert!(sql.contains("ST_DWithin("), "{sql}");
    assert!(sql.contains("ST_AsGeoJSON("), "output as GeoJSON: {sql}");

    let sql = compile(&format!(
        "{{ placesConnection(filter: {{ area: {{ intersects: {point} }}, location: {{ withinDistance: {{ geometry: {point}, distance: 100 }} }} }}) {{ nodes {{ area }} }} }}"
    ));
    assert!(
        sql.contains("ST_Intersects(c0.\\\"area\\\", ST_GeomFromGeoJSON($"),
        "{sql}"
    );
    assert!(
        sql.contains("ST_DWithin(c0.\\\"location\\\", ST_GeomFromGeoJSON($"),
        "{sql}"
    );
    let sql = compile(
        r#"{ placesConnection(filter: { area: { intersectsBox: { xmin: 0, ymin: 0, xmax: 1, ymax: 1 } } }) { totalCount } }"#,
    );
    assert!(sql.contains("&& ST_MakeEnvelope("), "{sql}");

    let sql = compile(&format!(
        "mutation {{ createPlace(input: {{ id: 1, area: {point} }}) {{ place {{ area }} }} }}"
    ));
    assert!(
        sql.contains("ST_GeomFromGeoJSON($"),
        "written from GeoJSON: {sql}"
    );
    assert!(
        sql.contains(r#"{\"coordinates\":[2.35,48.85],\"type\":\"Point\"}"#)
            || sql.contains(r#"{\"type\":\"Point\""#),
        "{sql}"
    );

    // Anything but a GeoJSON geometry object is rejected
    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        r#"mutation { createPlace(input: { id: 1, area: "nope" }) { place { id } } }"#,
        "q.graphql",
    )
    .unwrap();
    let err = QueryPlanner::new(&catalog, "geo")
        .plan(&doc)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("`input.area`") && err.contains("GeoJSON"),
        "{err}"
    );
}
