//! Input the database cannot take is refused as input (`BAD_USER_INPUT`, `400` over REST),
//! never a server error: cursors not issued by the connection, text with a NUL character,
//! invalid JSON paths, regular expressions and `LIKE` patterns. Found by fuzzing every filter
//! operator of a large schema.
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
DROP SCHEMA IF EXISTS input_fixture CASCADE;
CREATE SCHEMA input_fixture;
CREATE TYPE input_fixture.state AS ENUM ('open', 'closed');
CREATE TABLE input_fixture.docs (
    id uuid PRIMARY KEY,
    seq int NOT NULL UNIQUE,
    title text NOT NULL,
    meta jsonb,
    raw json,
    state input_fixture.state NOT NULL DEFAULT 'open'
);
INSERT INTO input_fixture.docs VALUES
    ('00000000-0000-0000-0000-000000000001', 1, 'alpha', '"draft"', '"draft"'),
    ('00000000-0000-0000-0000-000000000002', 2, 'beta', '{"tags": ["a"]}', '{"tags": ["a"]}'),
    ('00000000-0000-0000-0000-000000000003', 3, '50% off', '"final"', '"final"');
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
            .expect("input fixture");
    })
    .await;
    let config = Config {
        database: graphpg::config::DatabaseConfig {
            schemas: vec!["input_fixture".to_string()],
            ..Default::default()
        },
        ..Config::default()
    };
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config("input_fixture", &config)
        .await
        .expect("inspect input_fixture");
    let schema = SchemaBuilder::build(&catalog).expect("schema");
    let state = ServerState::new(
        create_pool(common::db_url(), 4).expect("pool"),
        schema,
        catalog,
        "input_fixture".to_string(),
        Default::default(),
    )
    .with_config(config);
    create_router(Arc::new(state))
}

async fn graphql(app: &axum::Router, query: &str, variables: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "query": query, "variables": variables }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn rest(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn code(body: &Value) -> &str {
    body["errors"][0]["extensions"]["code"]
        .as_str()
        .unwrap_or("")
}

/// The request is refused as input, not failed.
fn refused(body: &Value) {
    assert_eq!(code(body), "BAD_USER_INPUT", "{body}");
}

#[tokio::test]
async fn a_cursor_with_only_the_total_selected() {
    let app = app().await;
    let (_, page) = graphql(
        &app,
        "{ docsConnection(first: 1) { pageInfo { endCursor } } }",
        json!({}),
    )
    .await;
    let cursor = page["data"]["docsConnection"]["pageInfo"]["endCursor"].clone();
    // The page is not read: its cursor must not be bound either
    for selection in [
        "totalCount",
        "aggregates { max { seq } }",
        "totalCount aggregates { min { seq } }",
    ] {
        let (_, body) = graphql(
            &app,
            &format!(
                "query($c: String) {{ docsConnection(first: 1, after: $c) {{ {selection} }} }}"
            ),
            json!({ "c": cursor }),
        )
        .await;
        assert!(body.get("errors").is_none(), "{selection}: {body}");
    }
    let (_, body) = graphql(
        &app,
        "query($c: String) { docsConnection(first: 1, after: $c) { totalCount nodes { seq } } }",
        json!({ "c": cursor }),
    )
    .await;
    assert_eq!(body["data"]["docsConnection"]["totalCount"], 3);
    assert_eq!(
        body["data"]["docsConnection"]["nodes"],
        json!([{ "seq": 2 }])
    );
}

#[tokio::test]
async fn cursors_not_issued_by_the_connection() {
    let app = app().await;
    for cursor in [
        "",
        "x",
        "%%%",
        "bm90LWpzb24=",
        "WzFd",
        "AAAA",
        "MTsgRFJPUCBUQUJMRSB4Ow==",
    ] {
        for (arg, page) in [("after", "first"), ("before", "last")] {
            let (_, body) = graphql(
                &app,
                &format!("query($c: String) {{ docsConnection({page}: 1, {arg}: $c) {{ nodes {{ seq }} }} }}"),
                json!({ "c": cursor }),
            )
            .await;
            assert!(
                body["errors"][0]["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Invalid cursor"),
                "{arg} {cursor:?}: {body}"
            );
        }
    }
    for query in ["after=garbage", "before=%25%25%25", "after=AAAA"] {
        let (status, body) = rest(&app, &format!("/rest/docs?first=1&{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
}

#[tokio::test]
async fn text_with_a_nul_character() {
    let app = app().await;
    let (_, body) = graphql(
        &app,
        "query($v: String) { docsConnection(filter: { title: { equalTo: $v } }) { totalCount } }",
        json!({ "v": "a\u{0}b" }),
    )
    .await;
    refused(&body);
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("NUL"),
        "{body}"
    );
    // In a JSON value too
    let (_, body) = graphql(
        &app,
        "query($v: JSON) { docsConnection(filter: { meta: { equalTo: $v } }) { totalCount } }",
        json!({ "v": "a\u{0}b" }),
    )
    .await;
    refused(&body);
    let (status, _) = rest(&app, "/rest/docs?filter[title][eq]=a%00b").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn enum_values_that_come_in_variables() {
    let app = app().await;
    // A literal GraphQL refuses itself; a variable's value is checked by the engine
    for (query, variables) in [
        ("query($v: State) { docsConnection(filter: { state: { equalTo: $v } }) { totalCount } }", json!({ "v": "nope" })),
        ("query($v: [State!]) { docsConnection(filter: { state: { in: $v } }) { totalCount } }", json!({ "v": ["OPEN", "nope"] })),
    ] {
        let (_, body) = graphql(&app, query, variables).await;
        refused(&body);
    }
    let (_, body) = graphql(
        &app,
        "query($v: [State!]) { docsConnection(filter: { state: { in: $v } }) { totalCount } }",
        json!({ "v": ["OPEN", "CLOSED"] }),
    )
    .await;
    assert_eq!(body["data"]["docsConnection"]["totalCount"], 3, "{body}");
    let (status, _) = rest(&app, "/rest/docs?filter[state][eq]=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_json_string_compared_with_a_json_column_is_a_string() {
    let app = app().await;
    for column in ["meta", "raw"] {
        let (_, body) = graphql(
            &app,
            &format!("query($v: JSON) {{ docsConnection(filter: {{ {column}: {{ equalTo: $v }} }}) {{ nodes {{ seq }} }} }}"),
            json!({ "v": "draft" }),
        )
        .await;
        assert_eq!(
            body["data"]["docsConnection"]["nodes"],
            json!([{ "seq": 1 }]),
            "{column}: {body}"
        );
    }
    let (_, body) = graphql(
        &app,
        "query($v: JSON) { docsConnection(filter: { raw: { notEqualTo: $v } }, orderBy: [SEQ_ASC]) { nodes { seq } } }",
        json!({ "v": { "tags": ["a"] } }),
    )
    .await;
    // `json` compares as `jsonb`: `{"tags": ["a"]}` is stored with spaces, sent without
    assert_eq!(
        body["data"]["docsConnection"]["nodes"],
        json!([{ "seq": 1 }, { "seq": 3 }]),
        "{body}"
    );
    let (_, body) = graphql(
        &app,
        "query($v: JSON) { docsConnection(filter: { meta: { contains: $v } }) { nodes { seq } } }",
        json!({ "v": { "tags": ["a"] } }),
    )
    .await;
    assert_eq!(
        body["data"]["docsConnection"]["nodes"],
        json!([{ "seq": 2 }]),
        "{body}"
    );
}

#[tokio::test]
async fn invalid_json_paths_regular_expressions_and_like_patterns() {
    let app = app().await;
    for path in ["", " ", "'", "[]", "$.a[", "$$"] {
        let (_, body) = graphql(
            &app,
            "query($v: String) { docsConnection(filter: { meta: { matchesPath: $v } }) { totalCount } }",
            json!({ "v": path }),
        )
        .await;
        refused(&body);
    }
    for pattern in ["(", "[", "a{99999}", "*"] {
        let (_, body) = graphql(
            &app,
            "query($v: String) { docsConnection(filter: { title: { regex: $v } }) { totalCount } }",
            json!({ "v": pattern }),
        )
        .await;
        refused(&body);
    }
    let (_, body) = graphql(
        &app,
        "query($v: String) { docsConnection(filter: { title: { like: $v } }) { totalCount } }",
        json!({ "v": "abc\\" }),
    )
    .await;
    refused(&body);
    // Valid patterns still match
    let (_, body) = graphql(
        &app,
        "{ docsConnection(filter: { title: { like: \"50\\\\%%\" } }) { nodes { seq } } }",
        json!({}),
    )
    .await;
    assert_eq!(
        body["data"]["docsConnection"]["nodes"],
        json!([{ "seq": 3 }]),
        "{body}"
    );
    let (status, body) = rest(&app, "/rest/docs?filter[title][regex]=(").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "BAD_USER_INPUT");
}
