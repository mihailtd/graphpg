mod common;
use axum::http::StatusCode;
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, ServerState};
use http_body_util::BodyExt;
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

async fn post_graphql(
    app: &axum::Router,
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&payload).unwrap(),
        ))
        .unwrap();

    let res = app.clone().oneshot(req).await.expect("handler failed");
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn test_named_fragment_spread() {
    let app = setup_app().await;

    let query = "
        fragment ForumDetails on Forum {
            id
            name
        }
        query {
            forums {
                ...ForumDetails
            }
        }
    ";

    let (status, json) = post_graphql(&app, serde_json::json!({ "query": query })).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["errors"].is_null(), "{json}");
    let forums = json["data"]["forums"].as_array().expect("forums array");
    assert!(!forums.is_empty());
    assert!(forums[0].get("id").is_some());
    assert!(forums[0].get("name").is_some());
}

#[tokio::test]
async fn test_inline_fragment() {
    let app = setup_app().await;

    let query = "
        query {
            forums {
                name
                ... on Forum {
                    id
                }
            }
        }
    ";

    let (status, json) = post_graphql(&app, serde_json::json!({ "query": query })).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["errors"].is_null(), "{json}");
    let forums = json["data"]["forums"].as_array().expect("forums array");
    assert!(!forums.is_empty());
    assert!(forums[0].get("id").is_some());
    assert!(forums[0].get("name").is_some());
}

#[tokio::test]
async fn test_root_query_fragment_spread() {
    let app = setup_app().await;

    let query = "
        fragment RootContent on Query {
            forums {
                name
            }
            users {
                username
            }
        }
        query {
            ...RootContent
        }
    ";

    let (status, json) = post_graphql(&app, serde_json::json!({ "query": query })).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["errors"].is_null(), "{json}");
    assert!(json["data"]["forums"].is_array(), "{json}");
    assert!(json["data"]["users"].is_array(), "{json}");
}

#[tokio::test]
async fn test_query_with_variables() {
    let app = setup_app().await;

    let query = "
        query GetForumById($id: UUID!) {
            forum(id: $id) {
                id
                databaseId
                name
            }
        }
    ";

    let (status, json) = post_graphql(
        &app,
        serde_json::json!({
            "query": query,
            "variables": { "id": "ca700000-0000-0000-0000-000000000ca7" }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["errors"].is_null(), "{json}");
    assert_eq!(
        json["data"]["forum"]["id"],
        "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3"
    );
    assert_eq!(
        json["data"]["forum"]["databaseId"],
        "ca700000-0000-0000-0000-000000000ca7"
    );
    assert_eq!(json["data"]["forum"]["name"], "Cats");
}

#[tokio::test]
async fn test_connection_with_pagination_and_condition_variables() {
    let app = setup_app().await;

    let query = "
        query PaginatedForums($first: Int, $cond: ForumCondition) {
            forumsConnection(first: $first, condition: $cond) {
                totalCount
                nodes {
                    id
                    name
                }
            }
        }
    ";

    let (status, json) = post_graphql(
        &app,
        serde_json::json!({
            "query": query,
            "variables": {
                "first": 1,
                "cond": { "name": "Postgres" }
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["errors"].is_null(), "{json}");
    let conn = &json["data"]["forumsConnection"];
    assert_eq!(conn["totalCount"], 1);
    let nodes = conn["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["name"], "Postgres");
}

#[tokio::test]
async fn test_variable_default_values_fallback() {
    let app = setup_app().await;

    let query = "
        query ForumsWithDefaultLimit($first: Int = 1) {
            forumsConnection(first: $first) {
                nodes {
                    name
                }
            }
        }
    ";

    // Omit variables so $first falls back to default 1
    let (status, json) = post_graphql(&app, serde_json::json!({ "query": query })).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["errors"].is_null(), "{json}");
    let nodes = json["data"]["forumsConnection"]["nodes"]
        .as_array()
        .expect("nodes array");
    assert_eq!(
        nodes.len(),
        1,
        "Default value $first: 1 should limit to 1 node"
    );
}

#[tokio::test]
async fn test_mutation_lifecycle_with_variables() {
    let app = setup_app().await;

    // 1. Create with $input variable
    let create_query = "
        mutation Create($input: CreateForumInput!) {
            createForum(input: $input) {
                forum {
                    id
                    name
                }
            }
        }
    ";
    let (status, json) = post_graphql(
        &app,
        serde_json::json!({
            "query": create_query,
            "variables": {
                "input": { "name": "GraphQL Variables Test" }
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["errors"].is_null(), "{json}");
    let created_id = json["data"]["createForum"]["forum"]["id"]
        .as_str()
        .expect("created id")
        .to_string();
    assert_eq!(
        json["data"]["createForum"]["forum"]["name"],
        "GraphQL Variables Test"
    );

    // 2. Update with $id and $input variables
    let update_query = "
        mutation Update($id: ID!, $input: UpdateForumInput!) {
            updateForum(id: $id, input: $input) {
                forum {
                    id
                    name
                }
            }
        }
    ";
    let (status, json) = post_graphql(
        &app,
        serde_json::json!({
            "query": update_query,
            "variables": {
                "id": created_id,
                "input": { "name": "Updated Variables Forum" }
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["data"]["updateForum"]["forum"]["name"],
        "Updated Variables Forum"
    );

    // 3. Delete with $id variable
    let delete_query = "
        mutation Delete($id: ID!) {
            deleteForum(id: $id) {
                forum {
                    id
                }
            }
        }
    ";
    let (status, json) = post_graphql(
        &app,
        serde_json::json!({
            "query": delete_query,
            "variables": { "id": created_id }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["deleteForum"]["forum"]["id"], created_id);
}
