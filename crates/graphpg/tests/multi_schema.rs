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

        -- Written to by the REST tests
        CREATE TABLE multi_app.scratch (id serial primary key, note text not null);
        CREATE TABLE multi_auth.scratch (id serial primary key, note text not null);

        -- The same function name in both schemas
        CREATE FUNCTION multi_auth.whoami() RETURNS text LANGUAGE sql STABLE AS $$ SELECT 'auth' $$;
        CREATE FUNCTION multi_app.whoami() RETURNS text LANGUAGE sql STABLE AS $$ SELECT 'app' $$;
        CREATE FUNCTION multi_app.only_in_app() RETURNS int LANGUAGE sql STABLE AS $$ SELECT 7 $$;

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

async fn rest(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    let body = match body {
        Some(body) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(body.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, json)
}

#[tokio::test]
async fn test_rest_paths_name_the_schema() {
    let app = setup_app().await;

    // `/rest/audit_logs` is the default schema's, each schema's own is at its path
    let (status, _, rows) = rest(&app, "GET", "/rest/audit_logs", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[0]["action"], "USER_LOGIN");
    let (_, _, rows) = rest(&app, "GET", "/rest/multi_auth/audit_logs", None).await;
    assert_eq!(rows[0]["action"], "USER_LOGIN");
    let (status, _, rows) = rest(&app, "GET", "/rest/multi_app/audit_logs", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[0]["event"], "POST_PUBLISHED");

    // A single row, a search and an aggregate
    let (status, _, row) = rest(&app, "GET", "/rest/multi_app/audit_logs/1", None).await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["event"], "POST_PUBLISHED");
    let (status, _, rows) = rest(
        &app,
        "POST",
        "/rest/multi_app/audit_logs/search",
        Some(json!({ "filter": { "event": { "eq": "POST_PUBLISHED" } } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let (status, _, _) = rest(&app, "GET", "/rest/multi_app/audit_logs/aggregate", None).await;
    assert_eq!(status, StatusCode::OK);

    // A resource of another schema is not there, nor is a schema that was not configured
    let (status, _, problem) = rest(&app, "GET", "/rest/multi_app/users", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(problem["instance"], "/rest/multi_app/users");
    let (status, _, _) = rest(&app, "GET", "/rest/injection_attempt/users", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = rest(&app, "GET", "/rest/public/users", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Cross-schema embedding: a post of one schema, its author of the other
    let (status, _, rows) = rest(
        &app,
        "GET",
        "/rest/multi_app/posts?include=author&order=id",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[0]["author"]["username"], "alice");
}

#[tokio::test]
async fn test_rest_writes_under_a_schema() {
    let app = setup_app().await;
    let (status, headers, row) = rest(
        &app,
        "POST",
        "/rest/multi_app/scratch",
        Some(json!({ "note": "written" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    let id = row["id"].as_i64().unwrap();
    assert_eq!(
        headers[header::LOCATION],
        format!("/rest/multi_app/scratch/{id}").as_str()
    );
    let (status, _, row) = rest(
        &app,
        "PATCH",
        &format!("/rest/multi_app/scratch/{id}"),
        Some(json!({ "note": "renamed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["note"], "renamed");
    // The same key in the default schema's table is another row
    let (status, _, _) = rest(&app, "GET", &format!("/rest/multi_auth/scratch/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = rest(
        &app,
        "DELETE",
        &format!("/rest/multi_app/scratch/{id}"),
        None,
    )
    .await;
    assert!(status.is_success(), "{status}");
}

#[tokio::test]
async fn test_rest_functions_under_a_schema() {
    let app = setup_app().await;
    // The default schema's `whoami` at the unqualified path, each schema's at its own
    let (_, _, body) = rest(&app, "GET", "/rest/rpc/whoami", None).await;
    assert_eq!(body, json!("auth"));
    let (_, _, body) = rest(&app, "GET", "/rest/multi_auth/rpc/whoami", None).await;
    assert_eq!(body, json!("auth"));
    let (status, _, body) = rest(&app, "GET", "/rest/multi_app/rpc/whoami", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!("app"));
    let (_, _, body) = rest(&app, "GET", "/rest/multi_app/rpc/only_in_app", None).await;
    assert_eq!(body, json!(7));
    // A function of another schema is not there
    let (status, _, _) = rest(&app, "GET", "/rest/multi_auth/rpc/only_in_app", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn collect(value: &serde_json::Value, refs: &mut Vec<String>, ids: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(name) = s.strip_prefix("#/components/schemas/") {
                refs.push(name.to_string());
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect(v, refs, ids)),
        serde_json::Value::Object(map) => {
            for (key, v) in map {
                match (key.as_str(), v) {
                    ("operationId", serde_json::Value::String(id)) => ids.push(id.clone()),
                    _ => collect(v, refs, ids),
                }
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn test_openapi_has_a_fixed_path_per_schema() {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    ensure_multi_schema_db(&client).await;
    let mut config = Config::default();
    config.database.schemas = vec!["multi_auth".to_string(), "multi_app".to_string()];
    let catalog = CatalogInspector::new(&client)
        .inspect_schemas_with_config(&["multi_auth", "multi_app"], &config)
        .await
        .unwrap();
    let graphql = SchemaBuilder::build(&catalog).unwrap();
    let doc = graphpg::openapi::OpenApiGenerator::generate_with_schema(
        &catalog,
        &config,
        "multi_auth",
        Some(&graphql),
    );

    let paths = doc["paths"].as_object().unwrap();
    for path in [
        "/rest/audit_logs",
        "/rest/multi_auth/audit_logs",
        "/rest/multi_app/audit_logs",
        "/rest/multi_app/audit_logs/{key}",
        "/rest/multi_app/audit_logs/aggregate",
        "/rest/multi_app/posts",
        "/rest/rpc/whoami",
        "/rest/multi_auth/rpc/whoami",
        "/rest/multi_app/rpc/whoami",
        "/rest/multi_app/rpc/only_in_app",
    ] {
        assert!(paths.contains_key(path), "no {path} in {:?}", paths.keys());
    }
    // Fixed paths, not a template over the schema; a schema's own are its own
    assert!(paths.keys().all(|p| !p.contains("{schema}")));
    assert!(!paths.contains_key("/rest/multi_auth/rpc/only_in_app"));
    assert!(!paths.contains_key("/rest/multi_app/users"));
    assert!(!paths.contains_key("/rest/multi_app/search"));

    // Every operation has an id of its own, and every reference leads to a component
    let (mut refs, mut uses) = (Vec::new(), Vec::new());
    collect(&doc, &mut refs, &mut uses);
    // (`operationId` also names the operation a link leads to)
    let ids: Vec<String> = paths
        .values()
        .flat_map(|item| item.as_object().unwrap().values())
        .filter_map(|op| op["operationId"].as_str().map(String::from))
        .collect();
    for used in &uses {
        assert!(
            ids.contains(used),
            "a link leads to a missing operation {used}"
        );
    }
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "operation ids repeat");
    let components = doc["components"]["schemas"].as_object().unwrap();
    for name in refs {
        assert!(components.contains_key(&name), "dangling reference {name}");
    }
    assert!(components.contains_key("multi_app.audit_logs"));
    assert!(components.contains_key("multi_auth.audit_logs"));
}
