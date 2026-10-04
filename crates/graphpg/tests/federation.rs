mod common;
use axum::http::header;
use axum::http::StatusCode;
use graphpg::catalog::CatalogInspector;
use graphpg::config::{EngineConfig, FederationConfig};
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, ServerState};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;


async fn setup_app(federation_enabled: bool) -> axum::Router {
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
    let mut config = EngineConfig::default();
    if federation_enabled {
        config.federation = FederationConfig {
            enabled: true,
            version: Some("v2.3".to_string()),
        };
    }

    let catalog = inspector
        .inspect_schema_with_config("app_public", &config)
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

#[tokio::test]
async fn test_federation_subgraph_sdl_schema_export() {
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
    let mut config = EngineConfig::default();
    config.federation = FederationConfig {
        enabled: true,
        version: Some("v2.3".to_string()),
    };

    let catalog = inspector
        .inspect_schema_with_config("app_public", &config)
        .await
        .expect("Failed to inspect schema with federation config");

    let subgraph_sdl = SchemaBuilder::generate_federation_subgraph_sdl(&catalog);

    // 1. Must declare schema link to federation v2.3 spec
    assert!(
        subgraph_sdl.contains("extend schema\n  @link(url: \"https://specs.apollo.dev/federation/v2.3\", import: [\"@key\", \"@shareable\"])"),
        "Subgraph SDL should extend schema with @link directive"
    );

    // 2. Must decorate entity types with @key
    assert!(
        subgraph_sdl.contains("type User implements Node @key(fields: \"id\")")
            || subgraph_sdl.contains("type User @key(fields: \"id\")"),
        "User entity should have @key(fields: \"id\") directive"
    );
    assert!(
        subgraph_sdl.contains("type Forum implements Node @key(fields: \"id\")")
            || subgraph_sdl.contains("type Forum @key(fields: \"id\")"),
        "Forum entity should have @key(fields: \"id\") directive"
    );

    // 3. Subgraph SDL must NOT contain federation internal types
    assert!(!subgraph_sdl.contains("scalar _Any"), "Subgraph SDL must not define scalar _Any");
    assert!(!subgraph_sdl.contains("scalar _FieldSet"), "Subgraph SDL must not define scalar _FieldSet");
    assert!(!subgraph_sdl.contains("type _Service"), "Subgraph SDL must not define type _Service");
    assert!(!subgraph_sdl.contains("union _Entity"), "Subgraph SDL must not define union _Entity");

    // 4. Query in Subgraph SDL must NOT contain _service or _entities
    assert!(!subgraph_sdl.contains("_service: _Service!"), "Subgraph Query must not contain _service");
    assert!(!subgraph_sdl.contains("_entities(representations:"), "Subgraph Query must not contain _entities");
}

#[tokio::test]
async fn test_federation_runtime_schema_directives_and_types() {
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
    let mut config = EngineConfig::default();
    config.federation = FederationConfig {
        enabled: true,
        version: Some("v2.3".to_string()),
    };

    let catalog = inspector
        .inspect_schema_with_config("app_public", &config)
        .await
        .expect("Failed to inspect schema with federation config");

    let runtime_sdl = SchemaBuilder::generate_sdl(&catalog);

    // Runtime SDL must define Apollo Federation types for local engine validation
    assert!(runtime_sdl.contains("scalar _Any"));
    assert!(runtime_sdl.contains("scalar _FieldSet"));
    assert!(runtime_sdl.contains("directive @key"));
    assert!(runtime_sdl.contains("directive @shareable"));
    assert!(runtime_sdl.contains("type _Service {\n  sdl: String!\n}"));
    assert!(runtime_sdl.contains("union _Entity"));
    assert!(runtime_sdl.contains("  _service: _Service!\n"));
    assert!(runtime_sdl.contains("  _entities(representations: [_Any!]!): [_Entity]!\n"));

    // Verify it compiles and validates with apollo-compiler
    let valid_schema = SchemaBuilder::build(&catalog).expect("Runtime schema must validate cleanly");
    let schema_ref = valid_schema.as_ref();
    assert!(schema_ref.get_union("_Entity").is_some());
    assert!(schema_ref.get_object("_Service").is_some());
}

#[tokio::test]
async fn test_query_service_sdl_via_http() {
    let app = setup_app(true).await;

    let request_body = json!({
        "query": "query { _service { sdl } }"
    });

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(res.get("errors").is_none(), "Errors: {:?}", res.get("errors"));
    let sdl = res["data"]["_service"]["sdl"].as_str().expect("sdl must be a string");
    assert!(sdl.contains("https://specs.apollo.dev/federation/v2.3"));
    assert!(sdl.contains("@key(fields: \"id\")"));
}

#[tokio::test]
async fn test_query_entities_single_and_batch_lookups() {
    let app = setup_app(true).await;

    // 1. Fetch real existing IDs from the database
    let seed_request = json!({
        "query": "query { users { id username } forums { id name } }"
    });

    let seed_res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&seed_request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    let body = seed_res.into_body().collect().await.unwrap().to_bytes();
    let seed_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id = seed_json["data"]["users"][0]["id"].as_str().unwrap().to_string();
    let expected_username = seed_json["data"]["users"][0]["username"].as_str().unwrap().to_string();
    let forum_id = seed_json["data"]["forums"][0]["id"].as_str().unwrap().to_string();
    let expected_forum_name = seed_json["data"]["forums"][0]["name"].as_str().unwrap().to_string();

    // 2. Query _entities with representations as variables (Apollo Router pattern)
    let entities_query = r#"
        query LookupEntities($reps: [_Any!]!) {
            _entities(representations: $reps) {
                __typename
                ... on User {
                    id
                    username
                }
                ... on Forum {
                    id
                    name
                }
            }
        }
    "#;

    let reps = json!([
        { "__typename": "User", "id": user_id },
        { "__typename": "Forum", "id": forum_id },
        { "__typename": "User", "id": user_id }
    ]);

    let request_body = json!({
        "query": entities_query,
        "variables": { "reps": reps }
    });

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(res.get("errors").is_none(), "Errors: {:?}", res.get("errors"));
    let entities = res["data"]["_entities"].as_array().expect("_entities must be an array");
    assert_eq!(entities.len(), 3);

    // Verify 1st entity (User)
    assert_eq!(entities[0]["__typename"], "User");
    assert_eq!(entities[0]["id"], user_id);
    assert_eq!(entities[0]["username"], expected_username);

    // Verify 2nd entity (Forum)
    assert_eq!(entities[1]["__typename"], "Forum");
    assert_eq!(entities[1]["id"], forum_id);
    assert_eq!(entities[1]["name"], expected_forum_name);

    // Verify 3rd entity (User repeated)
    assert_eq!(entities[2]["__typename"], "User");
    assert_eq!(entities[2]["id"], user_id);
    assert_eq!(entities[2]["username"], expected_username);
}

#[tokio::test]
async fn test_query_entities_batch_beyond_function_argument_limit() {
    let app = setup_app(true).await;
    let post = |body: serde_json::Value| {
        axum::http::Request::builder()
            .method("POST")
            .uri("/graphql")
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };

    let seed = app.clone().oneshot(post(json!({ "query": "query { users { id } }" }))).await.unwrap();
    let seed: serde_json::Value = serde_json::from_slice(&seed.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let user_id = seed["data"]["users"][0]["id"].as_str().unwrap().to_string();

    // Routers batch more representations than PostgreSQL's 100 function arguments
    let reps: Vec<serde_json::Value> = (0..150).map(|_| json!({ "__typename": "User", "id": user_id })).collect();
    let request = json!({
        "query": "query($reps: [_Any!]!) { _entities(representations: $reps) { ... on User { id } } }",
        "variables": { "reps": reps }
    });
    let response = app.oneshot(post(request)).await.unwrap();
    let res: serde_json::Value = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert!(res.get("errors").is_none(), "Errors: {:?}", res.get("errors"));
    let entities = res["data"]["_entities"].as_array().expect("_entities must be an array");
    assert_eq!(entities.len(), 150);
    assert!(entities.iter().all(|e| e["id"] == user_id));
}

#[tokio::test]
async fn test_query_entities_missing_or_invalid_id_returns_null() {
    let app = setup_app(true).await;

    // Fetch one valid user ID
    let seed_request = json!({ "query": "query { users { id } }" });
    let seed_res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&seed_request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = seed_res.into_body().collect().await.unwrap().to_bytes();
    let seed_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let valid_user_id = seed_json["data"]["users"][0]["id"].as_str().unwrap().to_string();

    let entities_query = r#"
        query LookupEntities($reps: [_Any!]!) {
            _entities(representations: $reps) {
                __typename
                ... on User {
                    id
                    username
                }
            }
        }
    "#;

    let reps = json!([
        { "__typename": "User", "id": valid_user_id },
        { "__typename": "User", "id": "00000000-0000-0000-0000-000000000000" }, // Non-existent ID
        { "__typename": "UnknownType", "id": valid_user_id },                   // Unknown type
        { "__typename": "User", "id": "invalid-uuid-string" }                   // Malformed UUID
    ]);

    let request_body = json!({
        "query": entities_query,
        "variables": { "reps": reps }
    });

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(res.get("errors").is_none(), "Errors: {:?}", res.get("errors"));
    let entities = res["data"]["_entities"].as_array().expect("_entities array");
    assert_eq!(entities.len(), 4);

    // 1st is valid
    assert_eq!(entities[0]["__typename"], "User");
    assert_eq!(entities[0]["id"], valid_user_id);

    // 2nd, 3rd, 4th must all be null according to Apollo Federation spec
    assert!(entities[1].is_null(), "Non-existent entity should return null");
    assert!(entities[2].is_null(), "Unknown type should return null");
    assert!(entities[3].is_null(), "Malformed ID should return null");
}

#[tokio::test]
async fn test_query_entities_relay_global_id_support() {
    let app = setup_app(true).await;

    // Fetch one valid user ID and decode Relay ID
    let seed_request = json!({ "query": "query { users { id username } }" });
    let seed_res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&seed_request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = seed_res.into_body().collect().await.unwrap().to_bytes();
    let seed_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let relay_global_id = seed_json["data"]["users"][0]["id"].as_str().unwrap().to_string();
    let expected_username = seed_json["data"]["users"][0]["username"].as_str().unwrap().to_string();

    let (_, raw_uuid) = SchemaBuilder::decode_global_id(&relay_global_id).expect("valid Relay Global ID");

    let entities_query = r#"
        query LookupEntities($reps: [_Any!]!) {
            _entities(representations: $reps) {
                __typename
                ... on User {
                    id
                    username
                }
            }
        }
    "#;

    let reps = json!([
        { "__typename": "User", "id": relay_global_id },
        { "__typename": "User", "id": raw_uuid }
    ]);

    let request_body = json!({
        "query": entities_query,
        "variables": { "reps": reps }
    });

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(res.get("errors").is_none(), "Errors: {:?}", res.get("errors"));
    let entities = res["data"]["_entities"].as_array().expect("_entities array");
    assert_eq!(entities.len(), 2);
    assert_eq!(entities[0]["__typename"], "User");
    assert_eq!(entities[0]["username"], expected_username);
    assert_eq!(entities[1]["__typename"], "User");
    assert_eq!(entities[1]["username"], expected_username);
}

#[tokio::test]
async fn test_query_entities_nested_selections() {
    let app = setup_app(true).await;

    // Fetch user that has messages or forums
    let seed_request = json!({ "query": "query { users { id } }" });
    let seed_res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&seed_request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = seed_res.into_body().collect().await.unwrap().to_bytes();
    let seed_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id = seed_json["data"]["users"][0]["id"].as_str().unwrap().to_string();

    let entities_query = r#"
        query LookupUserWithMessages($reps: [_Any!]!) {
            _entities(representations: $reps) {
                __typename
                ... on User {
                    id
                    username
                    messages {
                        id
                        body
                    }
                }
            }
        }
    "#;

    let reps = json!([
        { "__typename": "User", "id": user_id }
    ]);

    let request_body = json!({
        "query": entities_query,
        "variables": { "reps": reps }
    });

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(res.get("errors").is_none(), "Errors: {:?}", res.get("errors"));
    let entities = res["data"]["_entities"].as_array().expect("_entities array");
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0]["__typename"], "User");
    assert!(entities[0]["messages"].is_array(), "messages should be an array");
}

#[tokio::test]
async fn test_non_federated_mode_remains_unpolluted() {
    let app = setup_app(false).await;

    // In non-federated mode, _service query should fail schema validation
    let request_body = json!({
        "query": "query { _service { sdl } }"
    });

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
