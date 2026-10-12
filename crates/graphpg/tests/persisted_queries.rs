//! Persisted queries (`graphql.persisted_queries`): automatic persisted queries (Apollo's
//! protocol), manifest operations by id, the allow-list, `GET /graphql`, and the answer when
//! the feature is off.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::models::PersistedQueriesConfig;
use graphpg::schema::SchemaBuilder;
use graphpg::server::persisted::sha256_hex;
use graphpg::server::{create_pool, create_router, DebugOptions, PersistedQueries, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const FORUMS: &str = "query Forums { forums { name } }";

async fn forum_state() -> ServerState {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("app_public")
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    )
}

fn config(automatic: bool, allow_list: bool) -> PersistedQueriesConfig {
    PersistedQueriesConfig {
        enabled: true,
        automatic,
        manifest: None,
        allow_list,
    }
}

async fn app(persisted: Option<PersistedQueries>) -> axum::Router {
    let state = forum_state()
        .await
        .with_persisted_queries(persisted.map(Arc::new));
    create_router(Arc::new(state))
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

async fn send(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let res = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())),
    }
}

async fn graphql(app: &axum::Router, body: Value) -> Reply {
    send(app, "POST", "/graphql", Some(body)).await
}

fn apq(hash: &str) -> Value {
    json!({ "persistedQuery": { "version": 1, "sha256Hash": hash } })
}

fn code(reply: &Reply) -> &str {
    reply.body["errors"][0]["extensions"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("{}", reply.body))
}

fn forum_names(reply: &Reply) -> usize {
    reply.body["data"]["forums"]
        .as_array()
        .unwrap_or_else(|| panic!("{}", reply.body))
        .len()
}

/// `GET /graphql?...` with each value percent-encoded.
fn get_uri(params: &[(&str, String)]) -> String {
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}={}",
                percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
            )
        })
        .collect();
    format!("/graphql?{}", query.join("&"))
}

#[tokio::test]
async fn automatic_persisted_queries_round_trip() {
    let app = app(Some(
        PersistedQueries::with_manifest(&config(true, false), None).unwrap(),
    ))
    .await;
    let hash = sha256_hex(FORUMS);

    // 1. The hash alone: not known yet
    let miss = graphql(&app, json!({ "extensions": apq(&hash) })).await;
    assert_eq!(miss.status, StatusCode::OK);
    assert_eq!(code(&miss), "PERSISTED_QUERY_NOT_FOUND");
    assert_eq!(miss.body["errors"][0]["message"], "PersistedQueryNotFound");

    // 2. The hash and the text: registered and run
    let registered = graphql(&app, json!({ "query": FORUMS, "extensions": apq(&hash) })).await;
    assert_eq!(registered.status, StatusCode::OK, "{}", registered.body);
    let rows = forum_names(&registered);

    // 3. The hash alone runs it, over POST and GET
    let hit = graphql(&app, json!({ "extensions": apq(&hash) })).await;
    assert_eq!(forum_names(&hit), rows);
    let uri = get_uri(&[
        ("operationName", "Forums".into()),
        ("extensions", apq(&hash).to_string()),
    ]);
    let get = send(&app, "GET", &uri, None).await;
    assert_eq!(get.status, StatusCode::OK, "{}", get.body);
    assert_eq!(forum_names(&get), rows);

    // A hash that is not the text's is refused, and registers nothing
    let wrong = sha256_hex("{ forums { id } }");
    let mismatch = graphql(&app, json!({ "query": FORUMS, "extensions": apq(&wrong) })).await;
    assert_eq!(mismatch.status, StatusCode::BAD_REQUEST);
    assert_eq!(code(&mismatch), "PERSISTED_QUERY_HASH_MISMATCH");
}

#[tokio::test]
async fn off_answers_not_supported_and_runs_text() {
    let app = app(None).await;
    let refused = graphql(&app, json!({ "extensions": apq(&sha256_hex(FORUMS)) })).await;
    assert_eq!(code(&refused), "PERSISTED_QUERY_NOT_SUPPORTED");
    assert_eq!(
        refused.body["errors"][0]["message"],
        "PersistedQueryNotSupported"
    );
    let by_id = graphql(&app, json!({ "documentId": "abc" })).await;
    assert_eq!(code(&by_id), "PERSISTED_QUERY_NOT_SUPPORTED");

    let text = graphql(&app, json!({ "query": FORUMS })).await;
    assert_eq!(text.status, StatusCode::OK, "{}", text.body);
    let missing = graphql(&app, json!({})).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_runs_queries_only() {
    let app = app(None).await;
    let query = send(&app, "GET", &get_uri(&[("query", FORUMS.into())]), None).await;
    assert_eq!(query.status, StatusCode::OK, "{}", query.body);
    assert!(forum_names(&query) > 0);

    let mutation = r#"mutation { createForums(inputs: [{ name: "get" }]) { forums { name } } }"#;
    let refused = send(&app, "GET", &get_uri(&[("query", mutation.into())]), None).await;
    assert_eq!(refused.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(refused.headers["allow"], "POST");

    let variables = get_uri(&[("query", FORUMS.into()), ("variables", "{nope".into())]);
    let bad = send(&app, "GET", &variables, None).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_allow_list_runs_only_the_manifest() {
    let manifest = json!({
        "format": "apollo-persisted-query-manifest",
        "version": 1,
        "operations": [{ "id": "forums-v1", "name": "Forums", "type": "query", "body": FORUMS }]
    });
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join("persisted-allow-list.json");
    std::fs::write(&path, manifest.to_string()).unwrap();
    let persisted = PersistedQueries::new(&PersistedQueriesConfig {
        manifest: Some(path.display().to_string()),
        ..config(true, true)
    })
    .unwrap()
    .unwrap();
    let app = create_router(Arc::new(
        forum_state().await.with_persisted_queries(Some(persisted)),
    ));

    // By id, by hash, and as listed text
    for body in [
        json!({ "documentId": "forums-v1" }),
        json!({ "extensions": apq(&sha256_hex(FORUMS)) }),
        json!({ "query": FORUMS }),
    ] {
        let reply = graphql(&app, body.clone()).await;
        assert_eq!(reply.status, StatusCode::OK, "{body}: {}", reply.body);
        assert!(forum_names(&reply) > 0);
    }

    // Anything else is refused before it is parsed, registration included
    let other = "{ forums { id } }";
    let refused = graphql(&app, json!({ "query": other })).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(code(&refused), "OPERATION_NOT_ALLOWED");
    let register = graphql(
        &app,
        json!({ "query": other, "extensions": apq(&sha256_hex(other)) }),
    )
    .await;
    assert_eq!(code(&register), "OPERATION_NOT_ALLOWED");
    let unknown = graphql(&app, json!({ "documentId": "forums-v2" })).await;
    assert_eq!(code(&unknown), "PERSISTED_QUERY_NOT_FOUND");

    // Exports too
    let export = send(
        &app,
        "POST",
        "/export",
        Some(json!({ "query": "{ forumsConnection { nodes { name } } }" })),
    )
    .await;
    assert_eq!(export.status, StatusCode::FORBIDDEN);
    assert_eq!(
        export.body["errors"][0]["extensions"]["code"],
        "OPERATION_NOT_ALLOWED"
    );
}

#[tokio::test]
async fn manifest_operations_are_checked_against_the_schema() {
    let state = forum_state().await;
    let manifest = json!({ "good": FORUMS, "stale": "{ forums { droppedColumn } }" }).to_string();
    let persisted = PersistedQueries::with_manifest(&config(false, true), Some(&manifest)).unwrap();
    let problems = persisted.check(&state.current_schema());
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("operation `stale`"), "{problems:?}");

    let missing = PersistedQueries::new(&PersistedQueriesConfig {
        manifest: Some("/nonexistent/manifest.json".into()),
        ..config(false, true)
    });
    assert!(missing.is_err());
}
