//! `min` and `max` of columns of every orderable type, including those PostgreSQL has no
//! `min` / `max` aggregate for (`uuid`, `boolean`, `macaddr`, `macaddr8`, `ltree`, `citext`):
//! over GraphQL (`aggregates`, `groupedAggregates`) and REST (`/aggregate`).
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const FIXTURE: &str = r#"
CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS ltree;
DROP SCHEMA IF EXISTS agg_types_fixture CASCADE;
CREATE SCHEMA agg_types_fixture;
CREATE TABLE agg_types_fixture.things (
    id uuid PRIMARY KEY,
    kind text NOT NULL,
    flag boolean,
    mac macaddr,
    mac8 macaddr8,
    path ltree,
    label citext,
    span int4range
);
INSERT INTO agg_types_fixture.things VALUES
    ('00000000-0000-0000-0000-00000000000b', 'a', true,  '08:00:2b:01:02:03', '08:00:2b:01:02:03:04:05', 'top.b', 'Bravo', '[5,10)'),
    ('00000000-0000-0000-0000-00000000000a', 'a', false, '08:00:2b:01:02:01', '08:00:2b:01:02:03:04:01', 'top.a.deep', 'alpha', '[1,3)'),
    ('ffffffff-0000-0000-0000-000000000000', 'b', NULL,  NULL, NULL, NULL, NULL, NULL),
    ('00000000-0000-0000-0000-00000000000c', 'b', true,  '08:00:2b:01:02:09', '08:00:2b:01:02:03:04:09', 'top', 'charlie', '[1,2)');
"#;

static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn app() -> axum::Router {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("connect to the test database");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    INIT.get_or_init(|| async {
        client
            .batch_execute(&format!(
                "SELECT pg_advisory_lock(7461001); {FIXTURE} SELECT pg_advisory_unlock(7461001);"
            ))
            .await
            .expect("aggregate types fixture");
    })
    .await;
    let config = Config {
        database: graphpg::config::DatabaseConfig {
            schemas: vec!["agg_types_fixture".to_string()],
            ..Default::default()
        },
        ..Config::default()
    };
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config("agg_types_fixture", &config)
        .await
        .expect("inspect agg_types_fixture");
    let schema = SchemaBuilder::build(&catalog).expect("schema");
    let state = ServerState::new(
        create_pool(common::db_url(), 4).expect("pool"),
        schema,
        catalog,
        "agg_types_fixture".to_string(),
        Default::default(),
    )
    .with_config(config);
    create_router(Arc::new(state))
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn graphql(app: &axum::Router, query: &str) -> Value {
    let (_, body) = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "query": query }).to_string()))
            .unwrap(),
    )
    .await;
    assert!(body.get("errors").is_none(), "{body}");
    body["data"].clone()
}

const FIELDS: &str = "id flag mac mac8 path label";

#[tokio::test]
async fn min_and_max_of_types_without_those_aggregates() {
    let app = app().await;
    let data = graphql(
        &app,
        &format!(
            "{{ thingsConnection {{ aggregates {{ min {{ {FIELDS} }} max {{ {FIELDS} }} }} }} }}"
        ),
    )
    .await;
    let aggregates = &data["thingsConnection"]["aggregates"];
    assert_eq!(
        aggregates["min"],
        json!({
            "id": "00000000-0000-0000-0000-00000000000a",
            "flag": false,
            "mac": "08:00:2b:01:02:01",
            "mac8": "08:00:2b:01:02:03:04:01",
            "path": "top",
            "label": "alpha",
        })
    );
    assert_eq!(
        aggregates["max"],
        json!({
            "id": "ffffffff-0000-0000-0000-000000000000",
            "flag": true,
            "mac": "08:00:2b:01:02:09",
            "mac8": "08:00:2b:01:02:03:04:09",
            "path": "top.b",
            // citext orders case-insensitively: "charlie" > "Bravo"
            "label": "charlie",
        })
    );
}

#[tokio::test]
async fn min_and_max_of_a_range_are_ranges() {
    let app = app().await;
    let data = graphql(
        &app,
        "{ thingsConnection { aggregates { min { span { start { value inclusive } end { value inclusive } } } max { span { start { value } end { value } } } } } }",
    )
    .await;
    // In the range type's order: by lower bound, then upper
    let aggregates = &data["thingsConnection"]["aggregates"];
    assert_eq!(
        aggregates["min"]["span"],
        json!({ "start": { "value": 1, "inclusive": true }, "end": { "value": 2, "inclusive": false } })
    );
    assert_eq!(
        aggregates["max"]["span"],
        json!({ "start": { "value": 5 }, "end": { "value": 10 } })
    );
}

#[tokio::test]
async fn grouped_and_filtered_min_and_max_skip_nulls() {
    let app = app().await;
    let data = graphql(
        &app,
        &format!("{{ thingsConnection {{ groupedAggregates(groupBy: [KIND]) {{ keys min {{ {FIELDS} }} max {{ flag path }} }} }} }}"),
    )
    .await;
    let groups = data["thingsConnection"]["groupedAggregates"]
        .as_array()
        .unwrap();
    let b = groups.iter().find(|g| g["keys"] == json!(["b"])).unwrap();
    // The group's other row has only NULLs: they do not win
    assert_eq!(b["min"]["flag"], true);
    assert_eq!(b["min"]["path"], "top");
    assert_eq!(b["min"]["id"], "00000000-0000-0000-0000-00000000000c");
    assert_eq!(b["max"]["path"], "top");

    let data = graphql(
        &app,
        "{ thingsConnection { aggregates(filter: { kind: { equalTo: \"a\" } }) { min { flag label } max { flag label } } } }",
    )
    .await;
    let aggregates = &data["thingsConnection"]["aggregates"];
    assert_eq!(
        aggregates["min"],
        json!({ "flag": false, "label": "alpha" })
    );
    assert_eq!(aggregates["max"], json!({ "flag": true, "label": "Bravo" }));
}

#[tokio::test]
async fn rest_aggregates_of_those_types() {
    let app = app().await;
    let (status, body) = send(
        &app,
        Request::builder()
            .uri("/rest/things/aggregate?min=id,flag,mac,path,label&max=id,flag")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let text = body.to_string();
    assert!(
        text.contains("00000000-0000-0000-0000-00000000000a"),
        "{text}"
    );
    assert!(
        text.contains("ffffffff-0000-0000-0000-000000000000"),
        "{text}"
    );
}
