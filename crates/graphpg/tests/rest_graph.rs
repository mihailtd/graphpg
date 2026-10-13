//! What GraphQL walks beyond foreign keys, over REST: hierarchies (`ancestors`, `descendants`),
//! exclusive arcs (`target`), temporal keys (`asOf`) and computed fields with arguments.
#![allow(dead_code)]
mod common;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SQL: &str = r#"
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE EXTENSION IF NOT EXISTS ltree;
DROP SCHEMA IF EXISTS rest_graph CASCADE;
CREATE SCHEMA rest_graph;
SET search_path = rest_graph, public;

-- A hierarchy through a foreign key, and one through an ltree path
CREATE TABLE staff (id int PRIMARY KEY, manager_id int REFERENCES staff (id), name text NOT NULL);
INSERT INTO staff VALUES (1, NULL, 'CEO'), (2, 1, 'CTO'), (3, 2, 'Dev'), (4, 3, 'Intern');
CREATE TABLE categories (id serial PRIMARY KEY, path ltree NOT NULL UNIQUE, name text NOT NULL);
INSERT INTO categories (path, name) VALUES
    ('top', 'Top'), ('top.science', 'Science'), ('top.science.astronomy', 'Astronomy'), ('top.arts', 'Arts');

-- Computed fields: with a required argument, and a set of rows with a default one
CREATE FUNCTION staff_double_id(s staff, factor int) RETURNS int LANGUAGE sql STABLE
    AS $$ SELECT s.id * factor $$;
CREATE FUNCTION staff_reports(s staff, min_id int DEFAULT 0) RETURNS SETOF staff LANGUAGE sql STABLE
    AS $$ SELECT * FROM rest_graph.staff WHERE manager_id = s.id AND id >= min_id $$;

-- An exclusive arc: a like is of a post or of a comment
CREATE TABLE posts (id int PRIMARY KEY, title text NOT NULL);
CREATE TABLE comments (id int PRIMARY KEY, body text NOT NULL);
CREATE TABLE owners (id int PRIMARY KEY, name text NOT NULL);
INSERT INTO owners VALUES (1, 'Ada');
CREATE TABLE likes (
    id int PRIMARY KEY,
    owner_id int NOT NULL REFERENCES owners (id),
    post_id int REFERENCES posts (id),
    comment_id int REFERENCES comments (id),
    CHECK (num_nonnulls(post_id, comment_id) = 1)
);
INSERT INTO posts VALUES (1, 'Hello');
INSERT INTO comments VALUES (1, 'Nice');
INSERT INTO likes VALUES (1, 1, 1, NULL), (2, 1, NULL, 1);

-- Versions of an account over time, and entries that refer to the version covering theirs
CREATE TABLE accounts (
    id int NOT NULL,
    valid_at tstzrange NOT NULL,
    owner text NOT NULL,
    PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)
);
CREATE TABLE entries (
    id serial PRIMARY KEY,
    account_id int NOT NULL,
    valid_at tstzrange NOT NULL,
    amount int NOT NULL,
    FOREIGN KEY (account_id, PERIOD valid_at) REFERENCES accounts (id, PERIOD valid_at)
);
INSERT INTO accounts VALUES
    (1, '[2024-01-01 00:00+00, 2024-06-01 00:00+00)', 'Ada'),
    (1, '[2024-06-01 00:00+00,)', 'Ada Lovelace'),
    (2, '[2024-01-01 00:00+00,)', 'Bob');
INSERT INTO entries (account_id, valid_at, amount) VALUES
    (1, '[2024-02-01 00:00+00, 2024-02-02 00:00+00)', 10),
    (1, '[2024-07-01 00:00+00, 2024-07-02 00:00+00)', 20);
"#;

struct Env {
    app: axum::Router,
    doc: Value,
}

async fn env() -> Env {
    let (mut client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    // One setup at a time, and only once the schema is there
    let lock = client.transaction().await.unwrap();
    lock.batch_execute("SELECT pg_advisory_xact_lock(7122001)")
        .await
        .unwrap();
    let exists = lock
        .query_opt(
            "SELECT 1 FROM pg_namespace WHERE nspname = 'rest_graph' AND obj_description(oid, 'pg_namespace') = 'v3'",
            &[],
        )
        .await
        .unwrap();
    if exists.is_none() {
        lock.batch_execute(SQL).await.unwrap();
        lock.batch_execute("RESET search_path").await.unwrap();
        lock.batch_execute("COMMENT ON SCHEMA rest_graph IS 'v3'")
            .await
            .unwrap();
    }
    lock.commit().await.unwrap();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("rest_graph")
        .await
        .unwrap();
    let graphql = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        graphql,
        catalog,
        "rest_graph".to_string(),
        DebugOptions::default(),
    );
    let doc = (*state.current_openapi_document()).clone();
    Env {
        app: create_router(Arc::new(state)),
        doc,
    }
}

impl Env {
    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        let res = self
            .app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn names(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn hierarchies_are_embedded_with_their_controls() {
    let env = env().await;
    let (status, rows) = env
        .get("/rest/staff?include=ancestors&fields=name&fields[ancestors]=name&order=id")
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(names(&rows[3]["ancestors"]), ["Dev", "CTO", "CEO"]);
    assert_eq!(rows[0]["ancestors"], json!([]));

    // `depth` is how far the walk goes; filter and order work as in a relation
    let (_, rows) = env
        .get("/rest/staff?include=ancestors&fields=name&fields[ancestors]=name&depth[ancestors]=2&order=id")
        .await;
    assert_eq!(names(&rows[3]["ancestors"]), ["Dev", "CTO"]);
    let (_, rows) = env
        .get("/rest/staff?include=descendants&fields=name&fields[descendants]=name&order[descendants]=-name&filter[descendants][name][ne]=Dev&filter[id][eq]=1")
        .await;
    assert_eq!(names(&rows[0]["descendants"]), ["Intern", "CTO"]);

    // The same on a single row, and an ltree path
    let (_, row) = env
        .get("/rest/staff/4?include=ancestors&fields=name&fields[ancestors]=name")
        .await;
    assert_eq!(names(&row["ancestors"]), ["Dev", "CTO", "CEO"]);
    let (status, rows) = env
        .get("/rest/categories?include=pathDescendants&fields=name&fields[pathDescendants]=name&filter[name][eq]=Science")
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(names(&rows[0]["pathDescendants"]), ["Astronomy"]);

    // A hierarchy has no page, and `depth` is for hierarchies
    for uri in [
        "/rest/staff?include=ancestors&first[ancestors]=1",
        "/rest/staff?include=ancestors&offset[ancestors]=1",
        "/rest/staff?include=manager&depth[manager]=1",
    ] {
        assert_eq!(env.get(uri).await.0, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn the_target_of_an_exclusive_arc_is_embedded() {
    let env = env().await;
    let (status, rows) = env.get("/rest/likes?include=target&order=id").await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(
        rows[0]["target"],
        json!({ "__typename": "Post", "id": 1, "title": "Hello" })
    );
    assert_eq!(
        rows[1]["target"],
        json!({ "__typename": "Comment", "id": 1, "body": "Nice" })
    );
    let (_, rows) = env
        .get("/rest/likes?include=target&fields[target]=title&order=id")
        .await;
    assert_eq!(
        rows[0]["target"],
        json!({ "__typename": "Post", "title": "Hello" })
    );
    assert_eq!(rows[1]["target"], json!({ "__typename": "Comment" }));
    for uri in [
        "/rest/likes?include=target&fields[target]=nope",
        "/rest/likes?include=target&first[target]=1",
    ] {
        assert_eq!(env.get(uri).await.0, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn temporal_tables_are_read_as_of_a_time() {
    let env = env().await;
    let (status, rows) = env
        .get("/rest/accounts?asOf=2024-03-01T00:00:00Z&order=id&fields=id,owner")
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[0], json!({ "id": 1, "owner": "Ada" }));
    let (_, rows) = env
        .get("/rest/accounts?asOf=2024-08-01T00:00:00Z&order=id&fields=id,owner")
        .await;
    assert_eq!(rows[0], json!({ "id": 1, "owner": "Ada Lovelace" }));

    // Embedded rows are read at their own time, and a row's history is a relation
    let (status, rows) = env
        .get("/rest/entries?include=account&fields[account]=owner&order=id&fields=amount&asOf[account]=2024-03-01T00:00:00Z")
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[1]["account"], json!({ "owner": "Ada" }));
    let (_, rows) = env
        .get("/rest/entries?include=account&fields[account]=owner&order=id&fields=amount")
        .await;
    assert_eq!(rows[1]["account"], json!({ "owner": "Ada Lovelace" }));
    let (status, rows) = env
        .get("/rest/accounts?include=history&fields=id&fields[history]=owner&filter[id][eq]=2")
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");

    // Only temporal tables have a time to be read at
    for uri in [
        "/rest/staff?asOf=2024-03-01T00:00:00Z",
        "/rest/staff?include=ancestors&asOf[ancestors]=2024-03-01T00:00:00Z",
        "/rest/accounts?asOf=yesterday",
    ] {
        assert_eq!(env.get(uri).await.0, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn computed_fields_take_arguments() {
    let env = env().await;
    let (status, rows) = env
        .get("/rest/staff?fields=name,doubleId&args[doubleId][factor]=3&order=id")
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[2]["doubleId"], 9);

    // A computed relationship takes its arguments and the controls of a relation
    let (status, rows) = env
        .get("/rest/staff?include=reports&fields=name&fields[reports]=name&args[reports][minId]=3&filter[id][in]=1,2")
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let mut by_name = std::collections::BTreeMap::new();
    for row in rows.as_array().unwrap() {
        by_name.insert(row["name"].as_str().unwrap(), row["reports"].clone());
    }
    assert_eq!(by_name["CEO"], json!([]));
    assert_eq!(by_name["CTO"], json!([{ "name": "Dev" }]));

    // An embedded computed field, by its path
    let (status, rows) = env
        .get("/rest/staff?include=manager&fields=name&fields[manager]=name,doubleId&args[manager.doubleId][factor]=10&filter[id][eq]=3")
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[0]["manager"], json!({ "name": "CTO", "doubleId": 20 }));

    for uri in [
        // a required argument, an unknown one, a bad value, not a computed field
        "/rest/staff?fields=name,doubleId",
        "/rest/staff?fields=name,doubleId&args[doubleId][factor]=3&args[doubleId][nope]=1",
        "/rest/staff?fields=name,doubleId&args[doubleId][factor]=many",
        "/rest/staff?fields=name&args[name][x]=1",
        "/rest/staff?args[doubleId][factor]=3&include=nope",
    ] {
        assert_eq!(env.get(uri).await.0, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn the_document_describes_the_hierarchy_arc_temporal_and_computed_grammar() {
    let env = env().await;
    let params = |path: &str| -> Vec<String> {
        env.doc["paths"][path]["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect()
    };
    let staff = params("/rest/staff");
    for name in [
        "depth[ancestors]",
        "depth[descendants]",
        "filter[ancestors]",
        "order[descendants]",
        "args[doubleId][factor]",
        "args[reports][minId]",
        "args[manager.doubleId][factor]",
        "fields[reports]",
        "first[reports]",
    ] {
        assert!(staff.contains(&name.to_string()), "{name} in {staff:?}");
    }
    // A hierarchy is not paged
    assert!(!staff.contains(&"first[ancestors]".to_string()));
    let double = env.doc["paths"]["/rest/staff"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "args[doubleId][factor]")
        .unwrap()
        .clone();
    assert_eq!(double["required"], true);

    assert!(params("/rest/accounts").contains(&"asOf".to_string()));
    assert!(params("/rest/entries").contains(&"asOf[account]".to_string()));
    assert!(!params("/rest/staff").contains(&"asOf".to_string()));
    assert!(params("/rest/likes").contains(&"fields[target]".to_string()));

    // The row schemas have the embedded fields
    let schemas = &env.doc["components"]["schemas"];
    assert_eq!(schemas["staff"]["properties"]["ancestors"]["type"], "array");
    let target = &schemas["likes"]["properties"]["target"]["anyOf"][0]["oneOf"];
    assert_eq!(target.as_array().unwrap().len(), 2);
}
