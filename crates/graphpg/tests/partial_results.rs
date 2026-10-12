//! Partial results: a root field failing on its own is `null` with an error carrying its
//! `path`, the other root fields keep their data. A query still runs as one statement; only
//! when it fails are the root fields run apart. `onError` (graphql-spec RFC 1163) chooses
//! between propagating nulls (the default), `NULL` and `HALT`.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool_with, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn app() -> axum::Router {
    common::ensure_partial_results_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("partial_results")
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    let pool = create_pool_with(common::db_url(), 4, Some(Duration::from_millis(500))).unwrap();
    let state = ServerState::new(
        pool,
        schema,
        catalog,
        "partial_results".to_string(),
        DebugOptions::default(),
    );
    create_router(Arc::new(state))
}

struct Reply {
    status: StatusCode,
    body: Value,
}

async fn graphql(app: &axum::Router, body: Value) -> Reply {
    let request = Request::post("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(request).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        body: serde_json::from_slice(&bytes).unwrap(),
    }
}

fn paths(reply: &Reply) -> Vec<Value> {
    reply.body["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("no errors: {}", reply.body))
        .iter()
        .map(|e| e["path"].clone())
        .collect()
}

#[tokio::test]
async fn a_failing_nullable_root_field_is_null_and_the_others_keep_their_data() {
    let app = app().await;
    let reply = graphql(&app, json!({ "query": "{ items { name } broken }" })).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.body["data"]["items"],
        json!([{ "name": "one" }, { "name": "two" }])
    );
    assert_eq!(reply.body["data"]["broken"], Value::Null);
    assert_eq!(paths(&reply), [json!(["broken"])]);
    // Masked as any unexpected database error
    assert_eq!(
        reply.body["errors"][0]["extensions"]["code"],
        "INTERNAL_SERVER_ERROR"
    );

    // Alone, it needs no second run: the failure is its own
    let alone = graphql(&app, json!({ "query": "{ broken }" })).await;
    assert_eq!(alone.status, StatusCode::OK);
    assert_eq!(alone.body["data"], json!({ "broken": null }));
    assert_eq!(paths(&alone), [json!(["broken"])]);
}

#[tokio::test]
async fn a_failing_non_null_root_field_nulls_data_unless_on_error_is_null() {
    let app = app().await;
    let query = "{ items { name checked } names: items { name } }";

    // The spec's default: `[Item!]!` cannot be null, so `data` is
    let reply = graphql(&app, json!({ "query": query })).await;
    assert_eq!(reply.body["data"], Value::Null, "{}", reply.body);
    assert_eq!(paths(&reply), [json!(["items"])]);
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);

    // `onError: NULL`: the client takes the null
    let reply = graphql(&app, json!({ "query": query, "onError": "NULL" })).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["data"]["items"], Value::Null);
    assert_eq!(
        reply.body["data"]["names"],
        json!([{ "name": "one" }, { "name": "two" }])
    );
    assert_eq!(paths(&reply), [json!(["items"])]);
}

#[tokio::test]
async fn several_failing_root_fields_each_get_an_error() {
    let app = app().await;
    let reply = graphql(
        &app,
        json!({ "query": "{ first: broken items { name } second: broken }", "onError": "NULL" }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.body["data"],
        json!({ "first": null, "items": [{ "name": "one" }, { "name": "two" }], "second": null })
    );
    assert_eq!(paths(&reply), [json!(["first"]), json!(["second"])]);
}

#[tokio::test]
async fn halt_fails_the_whole_request() {
    let app = app().await;
    let reply = graphql(
        &app,
        json!({ "query": "{ items { name } broken }", "onError": "HALT" }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.body.get("data").is_none(), "{}", reply.body);
    assert!(reply.body["errors"][0].get("path").is_none());

    let bad = graphql(&app, json!({ "query": "{ broken }", "onError": "IGNORE" })).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_timeout_fails_the_request_rather_than_a_field() {
    let app = app().await;
    let reply = graphql(&app, json!({ "query": "{ items { name } slow }" })).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.body
    );
    assert_eq!(
        reply.body["errors"][0]["extensions"]["code"],
        "STATEMENT_TIMEOUT"
    );
    assert!(reply.body.get("data").is_none());
}
