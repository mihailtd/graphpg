#![allow(clippy::field_reassign_with_default, clippy::len_zero)] // test setup builds configs field by field

mod common;
use axum::http::header;
use axum::http::StatusCode;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, ServerState};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn setup_multi_schema_db(client: &tokio_postgres::Client) {
    let sql = r#"
        DROP SCHEMA IF EXISTS multi_app CASCADE;
        DROP SCHEMA IF EXISTS multi_auth CASCADE;

        CREATE SCHEMA multi_auth;
        CREATE SCHEMA multi_app;

        -- multi_auth schema
        CREATE TABLE multi_auth.users (
            id serial primary key,
            username text not null unique,
            email text not null
        );

        -- Colliding table in multi_auth
        CREATE TABLE multi_auth.audit_logs (
            id serial primary key,
            action text not null,
            user_id int references multi_auth.users(id)
        );

        -- multi_app schema
        CREATE TABLE multi_app.posts (
            id serial primary key,
            author_id int not null references multi_auth.users(id),
            title text not null,
            content text
        );

        CREATE TABLE multi_app.comments (
            id serial primary key,
            post_id int not null references multi_app.posts(id),
            user_id int not null references multi_auth.users(id),
            content text not null
        );

        -- Colliding table in multi_app
        CREATE TABLE multi_app.audit_logs (
            id serial primary key,
            event text not null,
            post_id int references multi_app.posts(id)
        );

        -- Seed initial data
        INSERT INTO multi_auth.users (username, email) VALUES
            ('alice', 'alice@example.com'),
            ('bob', 'bob@example.com');

        INSERT INTO multi_app.posts (author_id, title, content) VALUES
            (1, 'First Multi-Schema Post', 'Content written by Alice'),
            (2, 'Second Post', 'Content written by Bob');

        INSERT INTO multi_app.comments (post_id, user_id, content) VALUES
            (1, 2, 'Great post Alice! - from Bob'),
            (2, 1, 'Thanks Bob! - from Alice');

        INSERT INTO multi_auth.audit_logs (action, user_id) VALUES
            ('USER_LOGIN', 1);

        INSERT INTO multi_app.audit_logs (event, post_id) VALUES
            ('POST_PUBLISHED', 1);
    "#;
    client
        .batch_execute(sql)
        .await
        .expect("Failed to setup multi-schema database");
}

static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn ensure_multi_schema_db(client: &tokio_postgres::Client) {
    INIT.get_or_init(|| async {
        setup_multi_schema_db(client).await;
    })
    .await;
}

async fn setup_app() -> axum::Router {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    ensure_multi_schema_db(&client).await;

    let inspector = CatalogInspector::new(&client);
    let mut config = Config::default();
    config.database.schemas = vec!["multi_auth".to_string(), "multi_app".to_string()];

    let catalog = inspector
        .inspect_schemas_with_config(&["multi_auth", "multi_app"], &config)
        .await
        .expect("Failed to inspect multiple schemas");

    let schema = SchemaBuilder::build(&catalog).expect("Failed to build valid GraphQL schema");

    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema,
        catalog,
        "multi_auth".to_string(),
        Default::default(),
    ));

    create_router(state)
}

#[tokio::test]
async fn test_multi_schema_smart_naming_and_sdl() {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    ensure_multi_schema_db(&client).await;

    let inspector = CatalogInspector::new(&client);
    let mut config = Config::default();
    config.database.schemas = vec!["multi_auth".to_string(), "multi_app".to_string()];

    let catalog = inspector
        .inspect_schemas_with_config(&["multi_auth", "multi_app"], &config)
        .await
        .expect("Failed to inspect multiple schemas");

    let sdl = SchemaBuilder::generate_sdl(&catalog);
    println!("=== GENERATED MULTI-SCHEMA SDL ===\n{}", sdl);

    // 1. Non-colliding types MUST remain unprefixed
    assert!(sdl.contains("type User"), "User should be unprefixed");
    assert!(sdl.contains("type Post"), "Post should be unprefixed");
    assert!(sdl.contains("type Comment"), "Comment should be unprefixed");

    // 2. Query root fields for non-colliding types MUST remain unprefixed
    assert!(
        sdl.contains("allUsersConnection"),
        "allUsersConnection query field should be unprefixed"
    );
    assert!(
        sdl.contains("allPostsConnection"),
        "allPostsConnection query field should be unprefixed"
    );

    // 3. Colliding types MUST be prefixed with their schema name
    assert!(
        sdl.contains("type MultiAuthAuditLog"),
        "multi_auth.audit_logs should be prefixed as MultiAuthAuditLog"
    );
    assert!(
        sdl.contains("type MultiAppAuditLog"),
        "multi_app.audit_logs should be prefixed as MultiAppAuditLog"
    );
    assert!(
        !sdl.contains("type AuditLog\n"),
        "Unprefixed type AuditLog must NOT exist due to collision"
    );

    // 4. Query root fields for colliding types MUST be prefixed
    assert!(
        sdl.contains("multiAuthAllAuditLogsConnection"),
        "Colliding query field should be prefixed as multiAuthAllAuditLogsConnection"
    );
    assert!(
        sdl.contains("multiAppAllAuditLogsConnection"),
        "Colliding query field should be prefixed as multiAppAllAuditLogsConnection"
    );
}

#[tokio::test]
async fn test_multi_schema_cross_schema_fk_query() {
    let app = setup_app().await;

    let query = r#"
        query {
            posts {
                databaseId
                title
                user {
                    databaseId
                    username
                    email
                }
            }
        }
    "#;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({ "query": query })).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        res.get("errors").is_none(),
        "Unexpected errors: {:?}",
        res.get("errors")
    );
    let posts = res["data"]["posts"].as_array().expect("posts array");
    assert!(posts.len() >= 2);

    let first = posts
        .iter()
        .find(|p| p["title"] == "First Multi-Schema Post")
        .expect("first post");
    assert_eq!(first["title"], "First Multi-Schema Post");
    assert_eq!(first["user"]["username"], "alice");
    assert_eq!(first["user"]["email"], "alice@example.com");

    let second = posts
        .iter()
        .find(|p| p["title"] == "Second Post")
        .expect("second post");
    assert_eq!(second["title"], "Second Post");
    assert_eq!(second["user"]["username"], "bob");
    assert_eq!(second["user"]["email"], "bob@example.com");
}

#[tokio::test]
async fn test_multi_schema_cross_schema_reverse_relation() {
    let app = setup_app().await;

    let query = r#"
        query {
            users {
                username
                posts {
                    title
                }
            }
        }
    "#;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({ "query": query })).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        res.get("errors").is_none(),
        "Unexpected errors: {:?}",
        res.get("errors")
    );
    let users = res["data"]["users"].as_array().expect("users array");
    assert!(users.len() >= 2);

    let alice = users
        .iter()
        .find(|u| u["username"] == "alice")
        .expect("alice");
    let alice_posts = alice["posts"].as_array().expect("alice posts");
    assert!(alice_posts.len() >= 1);
    assert!(alice_posts
        .iter()
        .any(|p| p["title"] == "First Multi-Schema Post"));

    let bob = users.iter().find(|u| u["username"] == "bob").expect("bob");
    let bob_posts = bob["posts"].as_array().expect("bob posts");
    assert!(bob_posts.len() >= 1);
    assert!(bob_posts.iter().any(|p| p["title"] == "Second Post"));
}

#[tokio::test]
async fn test_multi_schema_colliding_entities_execution() {
    let app = setup_app().await;

    let query = r#"
        query {
            multiAuthAuditLogs {
                databaseId
                action
            }
            multiAppAuditLogs {
                databaseId
                event
            }
        }
    "#;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({ "query": query })).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        res.get("errors").is_none(),
        "Unexpected errors: {:?}",
        res.get("errors")
    );

    let auth_logs = res["data"]["multiAuthAuditLogs"]
        .as_array()
        .expect("auth logs");
    assert_eq!(auth_logs.len(), 1);
    assert_eq!(auth_logs[0]["action"], "USER_LOGIN");

    let app_logs = res["data"]["multiAppAuditLogs"]
        .as_array()
        .expect("app logs");
    assert_eq!(app_logs.len(), 1);
    assert_eq!(app_logs[0]["event"], "POST_PUBLISHED");
}

#[tokio::test]
async fn test_multi_schema_mutation() {
    let app = setup_app().await;

    let query = r#"
        mutation {
            createPost(input: { authorId: 1, title: "Third Multi-Schema Post", content: "Brand new content" }) {
                post {
                    databaseId
                    title
                    user {
                        username
                    }
                }
            }
        }
    "#;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({ "query": query })).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        res.get("errors").is_none(),
        "Unexpected errors: {:?}",
        res.get("errors")
    );
    let post = &res["data"]["createPost"]["post"];
    assert_eq!(post["title"], "Third Multi-Schema Post");
    assert_eq!(post["user"]["username"], "alice");
}

#[tokio::test]
async fn test_multi_schema_relay_node() {
    let app = setup_app().await;

    // Encode global ID for User 1 (in multi_auth) and Post 1 (in multi_app)
    let user_global_id = SchemaBuilder::encode_global_id("User", "1");
    let post_global_id = SchemaBuilder::encode_global_id("Post", "1");

    let query = format!(
        r#"
        query {{
            userNode: node(id: "{user_global_id}") {{
                id
                ... on User {{
                    username
                    email
                }}
            }}
            postNode: node(id: "{post_global_id}") {{
                id
                ... on Post {{
                    title
                    user {{
                        username
                    }}
                }}
            }}
        }}
        "#
    );

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({ "query": query })).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        res.get("errors").is_none(),
        "Unexpected errors: {:?}",
        res.get("errors")
    );
    assert_eq!(res["data"]["userNode"]["username"], "alice");
    assert_eq!(res["data"]["postNode"]["title"], "First Multi-Schema Post");
    assert_eq!(res["data"]["postNode"]["user"]["username"], "alice");
}

#[tokio::test]
async fn test_multi_schema_federation_entities() {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    ensure_multi_schema_db(&client).await;

    let inspector = CatalogInspector::new(&client);
    let mut config = Config::default();
    config.database.schemas = vec!["multi_auth".to_string(), "multi_app".to_string()];
    config.graphql.federation.enabled = true;

    let catalog = inspector
        .inspect_schemas_with_config(&["multi_auth", "multi_app"], &config)
        .await
        .expect("Failed to inspect multiple schemas");

    let schema = SchemaBuilder::build(&catalog).expect("Failed to build valid GraphQL schema");

    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema,
        catalog,
        "multi_auth".to_string(),
        Default::default(),
    ));

    let app = create_router(state);

    let query = r#"
        query ($representations: [_Any!]!) {
            _entities(representations: $representations) {
                ... on User {
                    __typename
                    id
                    username
                }
                ... on Post {
                    __typename
                    id
                    title
                    user {
                        username
                    }
                }
            }
        }
    "#;

    let variables = json!({
        "representations": [
            { "__typename": "User", "id": 1 },
            { "__typename": "Post", "id": 2 }
        ]
    });

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({ "query": query, "variables": variables })).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        res.get("errors").is_none(),
        "Unexpected errors: {:?}",
        res.get("errors")
    );
    let entities = res["data"]["_entities"].as_array().expect("entities array");
    assert_eq!(entities.len(), 2);
    assert_eq!(entities[0]["username"], "alice");
    assert_eq!(entities[1]["title"], "Second Post");
    assert_eq!(entities[1]["user"]["username"], "bob");
}
