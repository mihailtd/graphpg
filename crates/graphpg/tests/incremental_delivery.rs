mod common;
use axum::http::header;
use axum::http::StatusCode;
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, ServerState};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn setup_app() -> axum::Router {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    let inspector = CatalogInspector::new(&client);
    let catalog = inspector
        .inspect_schema("app_public")
        .await
        .expect("Failed to inspect app_public schema");

    let schema = SchemaBuilder::build(&catalog).expect("Failed to build valid GraphQL schema");

    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema,
        catalog,
        "app_public".to_string(),
        Default::default(),
    ));

    create_router(state)
}

/// Helper to parse multipart/mixed body into parsed JSON chunks and verify closing boundary delimiter.
fn parse_multipart_body(body_bytes: &[u8]) -> (Vec<serde_json::Value>, bool) {
    let text = String::from_utf8(body_bytes.to_vec()).expect("utf8 multipart body");
    let has_closing = text.contains("-----");
    let parts: Vec<&str> = text.split("---").collect();
    let mut json_parts = Vec::new();
    for part in parts {
        let trimmed = part.trim();
        if trimmed.is_empty() || trimmed == "--" {
            continue;
        }
        if let Some(pos) = trimmed.find("\r\n\r\n").or_else(|| trimmed.find("\n\n")) {
            let json_str = trimmed[pos..].trim();
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                json_parts.push(val);
            }
        }
    }
    (json_parts, has_closing)
}

#[tokio::test]
async fn test_defer_inline_fragment() {
    let app = setup_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "multipart/mixed; boundary=\"-\"")
        .body(axum::body::Body::from(
            json!({
                "query": r#"
                    query GetForum {
                        forum(id: "ca700000-0000-0000-0000-000000000ca7") {
                            id
                            name
                            ... @defer(label: "forum_messages") {
                                messagesConnection(first: 3) {
                                    nodes {
                                        id
                                        body
                                    }
                                }
                            }
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let res = app.oneshot(req).await.expect("handler failed");
    assert_eq!(res.status(), StatusCode::OK);

    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("multipart/mixed"),
        "expected multipart/mixed, got: {content_type}"
    );

    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let (parts, has_closing) = parse_multipart_body(&body_bytes);

    assert_eq!(
        parts.len(),
        2,
        "expected initial chunk and 1 deferred chunk"
    );
    assert!(has_closing, "expected closing delimiter '-----'");

    // 1. Verify initial chunk
    let initial = &parts[0];
    assert_eq!(initial["hasNext"], true);
    assert_eq!(
        initial["data"]["forum"]["id"],
        "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3"
    );
    assert!(initial["data"]["forum"]["name"].is_string());
    // Deferred field must not be present in initial response
    assert!(initial["data"]["forum"].get("messagesConnection").is_none());

    // 2. Verify incremental chunk
    let patch = &parts[1];
    assert_eq!(patch["hasNext"], false);
    let inc = &patch["incremental"][0];
    assert_eq!(inc["label"], "forum_messages");
    assert_eq!(inc["path"], json!(["forum"]));
    let nodes = &inc["data"]["messagesConnection"]["nodes"];
    assert!(nodes.is_array());
    assert!(!nodes.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_defer_fragment_spread() {
    let app = setup_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({
                "query": r#"
                    query GetForumSpread {
                        forum(id: "ca700000-0000-0000-0000-000000000ca7") {
                            id
                            name
                            ...MessagesFrag @defer(label: "spread_messages")
                        }
                    }
                    fragment MessagesFrag on Forum {
                        messagesConnection(first: 2) {
                            nodes {
                                id
                            }
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let res = app.oneshot(req).await.expect("handler failed");
    assert_eq!(res.status(), StatusCode::OK);

    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("multipart/mixed"));

    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let (parts, has_closing) = parse_multipart_body(&body_bytes);

    assert_eq!(parts.len(), 2);
    assert!(has_closing);

    let initial = &parts[0];
    assert_eq!(initial["hasNext"], true);
    assert_eq!(
        initial["data"]["forum"]["id"],
        "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3"
    );
    assert!(initial["data"]["forum"].get("messagesConnection").is_none());

    let patch = &parts[1];
    assert_eq!(patch["hasNext"], false);
    let inc = &patch["incremental"][0];
    assert_eq!(inc["label"], "spread_messages");
    assert_eq!(inc["path"], json!(["forum"]));
    assert!(inc["data"]["messagesConnection"]["nodes"].is_array());
}

#[tokio::test]
async fn test_defer_root_level() {
    let app = setup_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({
                "query": r#"
                    query GetRootDefer {
                        forum1: forum(id: "ca700000-0000-0000-0000-000000000ca7") {
                            id
                            name
                        }
                        ... @defer(label: "root_deferred_forum") {
                            forum2: forum(id: "f1700000-0000-0000-0000-000000000f17") {
                                id
                                name
                            }
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let res = app.oneshot(req).await.expect("handler failed");
    assert_eq!(res.status(), StatusCode::OK);

    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let (parts, has_closing) = parse_multipart_body(&body_bytes);

    assert_eq!(parts.len(), 2);
    assert!(has_closing);

    let initial = &parts[0];
    assert_eq!(initial["hasNext"], true);
    assert_eq!(
        initial["data"]["forum1"]["id"],
        "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3"
    );
    assert!(initial["data"].get("forum2").is_none());

    let patch = &parts[1];
    assert_eq!(patch["hasNext"], false);
    let inc = &patch["incremental"][0];
    assert_eq!(inc["label"], "root_deferred_forum");
    assert_eq!(inc["path"], json!([]));
    assert_eq!(
        inc["data"]["forum2"]["id"],
        "Rm9ydW06ZjE3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwZjE3"
    );
}

#[tokio::test]
async fn test_defer_conditional_if_false_falls_back_to_single_json() {
    let app = setup_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({
                "query": r#"
                    query GetForumSync {
                        forum(id: "ca700000-0000-0000-0000-000000000ca7") {
                            id
                            name
                            ... @defer(if: false, label: "disabled_defer") {
                                messagesConnection(first: 1) {
                                    nodes {
                                        id
                                    }
                                }
                            }
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let res = app.oneshot(req).await.expect("handler failed");
    assert_eq!(res.status(), StatusCode::OK);

    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    // Must NOT be multipart
    assert!(!content_type.contains("multipart/mixed"));

    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json_resp: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

    // Data should include the fragment fields synchronously
    assert_eq!(
        json_resp["data"]["forum"]["id"],
        "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3"
    );
    assert!(json_resp["data"]["forum"]["messagesConnection"]["nodes"].is_array());
    assert!(json_resp.get("hasNext").is_none());
}

#[tokio::test]
async fn test_defer_variable_condition() {
    let app = setup_app().await;

    let query_str = r#"
        query GetForumVar($deferMessages: Boolean!) {
            forum(id: "ca700000-0000-0000-0000-000000000ca7") {
                id
                name
                ... @defer(if: $deferMessages, label: "var_messages") {
                    messagesConnection(first: 1) {
                        nodes {
                            id
                        }
                    }
                }
            }
        }
    "#;

    // Test 1: $deferMessages = false -> single JSON
    let req_false = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({
                "query": query_str,
                "variables": { "deferMessages": false }
            })
            .to_string(),
        ))
        .unwrap();

    let res_false = app.clone().oneshot(req_false).await.unwrap();
    let ct_false = res_false
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(!ct_false.contains("multipart/mixed"));
    let bytes_false = res_false.into_body().collect().await.unwrap().to_bytes();
    let json_false: serde_json::Value = serde_json::from_slice(&bytes_false).unwrap();
    assert!(json_false["data"]["forum"]["messagesConnection"]["nodes"].is_array());

    // Test 2: $deferMessages = true -> multipart stream
    let req_true = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({
                "query": query_str,
                "variables": { "deferMessages": true }
            })
            .to_string(),
        ))
        .unwrap();

    let res_true = app.oneshot(req_true).await.unwrap();
    let ct_true = res_true
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct_true.contains("multipart/mixed"));
    let bytes_true = res_true.into_body().collect().await.unwrap().to_bytes();
    let (parts, has_closing) = parse_multipart_body(&bytes_true);
    assert_eq!(parts.len(), 2);
    assert!(has_closing);
    assert_eq!(parts[0]["hasNext"], true);
    assert!(parts[0]["data"]["forum"]
        .get("messagesConnection")
        .is_none());
    assert_eq!(parts[1]["hasNext"], false);
    assert_eq!(parts[1]["incremental"][0]["label"], "var_messages");
}

#[tokio::test]
async fn test_stream_connection() {
    let app = setup_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({
                "query": r#"
                    query StreamForums {
                        allForumsConnection(first: 3) @stream(initialCount: 1, label: "streamed_forums") {
                            nodes {
                                id
                                name
                            }
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let res = app.oneshot(req).await.expect("handler failed");
    assert_eq!(res.status(), StatusCode::OK);

    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("multipart/mixed"));

    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let (parts, has_closing) = parse_multipart_body(&body_bytes);

    assert_eq!(parts.len(), 2, "expected initial chunk and 1 stream chunk");
    assert!(has_closing);

    // Initial chunk capped to initialCount: 1
    let initial = &parts[0];
    assert_eq!(initial["hasNext"], true);
    let initial_nodes = initial["data"]["allForumsConnection"]["nodes"]
        .as_array()
        .unwrap();
    assert_eq!(initial_nodes.len(), 1, "initialCount was 1");

    // Stream patch chunk
    let patch = &parts[1];
    assert_eq!(patch["hasNext"], false);
    let inc = &patch["incremental"][0];
    assert_eq!(inc["label"], "streamed_forums");
    assert_eq!(inc["path"], json!(["allForumsConnection", "nodes", 1]));
    let stream_items = inc["items"].as_array().unwrap();
    assert!(
        !stream_items.is_empty(),
        "expected remaining streamed forums"
    );
}

#[tokio::test]
async fn test_stream_connection_initial_count_zero() {
    let app = setup_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({
                "query": r#"
                    query StreamForumsZero {
                        allForumsConnection(first: 2) @stream(initialCount: 0, label: "stream_from_zero") {
                            nodes {
                                id
                                name
                            }
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let res = app.oneshot(req).await.expect("handler failed");
    assert_eq!(res.status(), StatusCode::OK);

    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let (parts, has_closing) = parse_multipart_body(&body_bytes);

    assert_eq!(parts.len(), 2);
    assert!(has_closing);

    // Initial chunk has 0 items
    let initial = &parts[0];
    assert_eq!(initial["hasNext"], true);
    let initial_nodes = initial["data"]["allForumsConnection"]["nodes"]
        .as_array()
        .unwrap();
    assert_eq!(initial_nodes.len(), 0, "initialCount was 0");

    // Stream patch chunk starts at index 0
    let patch = &parts[1];
    assert_eq!(patch["hasNext"], false);
    let inc = &patch["incremental"][0];
    assert_eq!(inc["label"], "stream_from_zero");
    assert_eq!(inc["path"], json!(["allForumsConnection", "nodes", 0]));
    let stream_items = inc["items"].as_array().unwrap();
    assert_eq!(stream_items.len(), 2, "expected 2 items streamed");
}

/// POSTs `query`: the response's content type and body.
async fn post_query(app: &axum::Router, query: &str) -> (String, Vec<u8>) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({ "query": query }).to_string(),
        ))
        .unwrap();
    let res = app.clone().oneshot(req).await.expect("handler failed");
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    (
        content_type,
        res.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

/// The data of `query` sent without incremental delivery.
async fn plain_data(app: &axum::Router, query: &str) -> serde_json::Value {
    let (_, body) = post_query(app, query).await;
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(res.get("errors").is_none(), "{res}");
    res["data"].clone()
}

#[tokio::test]
async fn test_stream_keeps_first_and_offset() {
    let app = setup_app().await;
    let expected = plain_data(
        &app,
        "{ allForumsConnection(first: 2, offset: 1) { nodes { id } } }",
    )
    .await;
    let expected = expected["allForumsConnection"]["nodes"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(expected.len(), 2, "fixture needs at least 3 forums");

    let (_, body) = post_query(
        &app,
        "{ allForumsConnection(first: 2, offset: 1) @stream(initialCount: 1) { nodes { id } } }",
    )
    .await;
    let (parts, _) = parse_multipart_body(&body);
    assert_eq!(parts.len(), 2);
    let initial = parts[0]["data"]["allForumsConnection"]["nodes"]
        .as_array()
        .unwrap();
    let streamed = parts[1]["incremental"][0]["items"].as_array().unwrap();
    // The stream starts after the initial rows of the field's own page and ends with it
    let all: Vec<_> = initial.iter().chain(streamed).cloned().collect();
    assert_eq!(all, expected);
}

#[tokio::test]
async fn test_stream_initial_count_covering_first_is_not_streamed() {
    let app = setup_app().await;
    let (content_type, body) = post_query(
        &app,
        "{ allForumsConnection(first: 2) @stream(initialCount: 5) { nodes { id } } }",
    )
    .await;
    // Nothing is left to stream: a single response holding the field's own page
    assert!(!content_type.contains("multipart/mixed"), "{content_type}");
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        res["data"]["allForumsConnection"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn test_defer_under_list_patches_each_element() {
    let app = setup_app().await;
    let expected = plain_data(
        &app,
        "{ allForumsConnection(first: 2) { nodes { id name } } }",
    )
    .await;
    let expected = expected["allForumsConnection"]["nodes"]
        .as_array()
        .unwrap()
        .clone();

    let (_, body) = post_query(
        &app,
        r#"{ allForumsConnection(first: 2) { nodes { id ... @defer(label: "names") { name } } } }"#,
    )
    .await;
    let (parts, _) = parse_multipart_body(&body);
    assert_eq!(parts.len(), 2);
    let initial = parts[0]["data"]["allForumsConnection"]["nodes"]
        .as_array()
        .unwrap();
    assert!(initial.iter().all(|n| n.get("name").is_none()));

    let entries = parts[1]["incremental"].as_array().unwrap();
    assert_eq!(entries.len(), expected.len(), "one patch per list element");
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry["label"], "names");
        assert_eq!(entry["path"], json!(["allForumsConnection", "nodes", i]));
        assert_eq!(entry["data"], json!({ "name": expected[i]["name"] }));
        assert_eq!(initial[i]["id"], expected[i]["id"]);
    }
}

#[tokio::test]
async fn test_stream_under_list_streams_each_element() {
    let app = setup_app().await;
    let query = "{ allForumsConnection(first: 2) { nodes { id messagesConnection(first: 3) { nodes { id } } } } }";
    let expected = plain_data(&app, query).await;
    let expected = expected["allForumsConnection"]["nodes"]
        .as_array()
        .unwrap()
        .clone();

    let (_, body) = post_query(
        &app,
        "{ allForumsConnection(first: 2) { nodes { id messagesConnection(first: 3) @stream(initialCount: 1) { nodes { id } } } } }",
    )
    .await;
    let (parts, _) = parse_multipart_body(&body);
    assert_eq!(parts.len(), 2);
    let initial = parts[0]["data"]["allForumsConnection"]["nodes"]
        .as_array()
        .unwrap();
    let entries = parts[1]["incremental"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();

    for (i, forum) in expected.iter().enumerate() {
        let messages = forum["messagesConnection"]["nodes"].as_array().unwrap();
        let first = initial[i]["messagesConnection"]["nodes"]
            .as_array()
            .unwrap();
        assert_eq!(first.as_slice(), &messages[..messages.len().min(1)]);
        let path = json!([
            "allForumsConnection",
            "nodes",
            i,
            "messagesConnection",
            "nodes",
            1
        ]);
        let streamed = entries
            .iter()
            .find(|e| e["path"] == path)
            .map(|e| e["items"].as_array().unwrap().clone())
            .unwrap_or_default();
        assert_eq!(
            streamed.as_slice(),
            &messages[messages.len().min(1)..],
            "forum {i}"
        );
    }
}

#[tokio::test]
async fn test_incremental_initial_payload_answers_typename() {
    let app = setup_app().await;
    let (_, body) = post_query(
        &app,
        "{ __typename allForumsConnection(first: 1) { nodes { id ... @defer { name } } } }",
    )
    .await;
    let (parts, _) = parse_multipart_body(&body);
    assert_eq!(parts[0]["data"]["__typename"], "Query");
    assert!(parts[0]["data"]["allForumsConnection"]["nodes"].is_array());
}
