mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn setup_test() -> (
    tokio_postgres::Client,
    apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    graphpg::DatabaseCatalog,
) {
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

    (client, schema, catalog)
}

fn server_state(
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: graphpg::DatabaseCatalog,
    pool_size: usize,
) -> Arc<ServerState> {
    server_state_with_debug(schema, catalog, pool_size, DebugOptions::default())
}

fn server_state_with_debug(
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: graphpg::DatabaseCatalog,
    pool_size: usize,
    debug: DebugOptions,
) -> Arc<ServerState> {
    Arc::new(ServerState::new(
        create_pool(common::db_url(), pool_size).expect("Failed to create pool"),
        schema,
        catalog,
        "app_public".to_string(),
        debug,
    ))
}

/// POSTs a GraphQL request body to the router and returns (status, json).
async fn post_graphql(
    app: &axum::Router,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn test_mutation_lifecycle_create_update_delete() {
    let (client, schema, catalog) = setup_test().await;

    // 1. CREATE FORUM
    let create_mutation = r#"
        mutation {
            createForum(input: { name: "Rust Engine Forum" }) {
                forum {
                    id
                    name
                }
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        create_mutation,
        "create.graphql",
    )
    .expect("Validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Planning failed");
    let create_res = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Create result:\n{}",
        serde_json::to_string_pretty(&create_res).unwrap()
    );
    let forum = &create_res["data"]["createForum"]["forum"];
    assert_eq!(forum["name"], "Rust Engine Forum");
    let created_id = forum["id"].as_str().expect("id string").to_string();

    // 2. UPDATE FORUM
    let update_mutation = format!(
        r#"
        mutation {{
            updateForum(id: "{}", input: {{ name: "Rust Engine Forum Updated" }}) {{
                forum {{
                    id
                    name
                }}
            }}
        }}
    "#,
        created_id
    );

    let doc_upd = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        &update_mutation,
        "update.graphql",
    )
    .expect("Validation failed");

    let plan_upd = planner.plan(&doc_upd).expect("Planning failed");
    let update_res = ExecutionEngine::execute(&client, &plan_upd)
        .await
        .expect("Execution failed");

    println!(
        "Update result:\n{}",
        serde_json::to_string_pretty(&update_res).unwrap()
    );
    assert_eq!(
        update_res["data"]["updateForum"]["forum"]["name"],
        "Rust Engine Forum Updated"
    );

    // 3. DELETE FORUM
    let delete_mutation = format!(
        r#"
        mutation {{
            deleteForum(id: "{}") {{
                forum {{
                    id
                }}
            }}
        }}
    "#,
        created_id
    );

    let doc_del = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        &delete_mutation,
        "delete.graphql",
    )
    .expect("Validation failed");

    let plan_del = planner.plan(&doc_del).expect("Planning failed");
    let delete_res = ExecutionEngine::execute(&client, &plan_del)
        .await
        .expect("Execution failed");

    println!(
        "Delete result:\n{}",
        serde_json::to_string_pretty(&delete_res).unwrap()
    );
    assert_eq!(delete_res["data"]["deleteForum"]["forum"]["id"], created_id);

    // 4. VERIFY RECORD IS GONE (Query returns null)
    let verify_query = format!(
        r#"
        query {{
            forum(id: "{}") {{
                id
                name
            }}
        }}
    "#,
        created_id
    );

    let doc_check = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        &verify_query,
        "check.graphql",
    )
    .expect("Validation failed");

    let plan_check = planner.plan(&doc_check).expect("Planning failed");
    let check_res = ExecutionEngine::execute(&client, &plan_check)
        .await
        .expect("Execution failed");

    assert_eq!(check_res["data"]["forum"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_http_health_endpoint() {
    let (_client, schema, catalog) = setup_test().await;

    let state = server_state(schema, catalog, 2);

    let app = create_router(state);

    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["status"], "ok");
    assert_eq!(json["engine"], "graphpg");
}

#[tokio::test]
async fn test_http_graphql_query_execution() {
    let (_client, schema, catalog) = setup_test().await;

    let state = server_state(schema, catalog, 2);

    let app = create_router(state);

    let req_body = serde_json::json!({
        "query": "{ forums { name } }"
    });

    let request = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify Apollo Client 4 compliant Content-Type header
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("application/graphql-response+json"));

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    println!(
        "HTTP GraphQL Response:\n{}",
        serde_json::to_string_pretty(&json).unwrap()
    );

    let forums = json["data"]["forums"].as_array().expect("forums array");
    assert!(!forums.is_empty());
}

#[tokio::test]
async fn test_http_graphql_validation_error() {
    let (_client, schema, catalog) = setup_test().await;

    let state = server_state(schema, catalog, 2);

    let app = create_router(state);

    let req_body = serde_json::json!({
        "query": "{ forums { nonExistentField123 } }"
    });

    let request = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let errors = json["errors"].as_array().expect("errors array");
    assert!(!errors.is_empty());
    assert!(errors[0]["message"]
        .as_str()
        .unwrap()
        .contains("nonExistentField123"));
}

#[tokio::test]
async fn test_http_concurrent_requests_share_pool() {
    let (_client, schema, catalog) = setup_test().await;

    // More in-flight requests than pooled connections: requests must queue for a
    // connection and all complete successfully.
    let app = create_router(server_state(schema, catalog, 2));

    let requests = (0..20).map(|_| {
        let app = app.clone();
        async move {
            let req_body = serde_json::json!({ "query": "{ forums { name messages { body } } }" });
            let request = Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        }
    });

    for (status, json) in spawn_and_join(requests).await {
        assert_eq!(status, StatusCode::OK, "{json}");
        // Other tests in this file create and delete forums concurrently: check the seeded ones.
        let names: Vec<&str> = json["data"]["forums"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        for seeded in ["Cats", "Dogs", "Postgres"] {
            assert!(names.contains(&seeded), "{seeded} missing: {json}");
        }
    }
}

async fn spawn_and_join<F: std::future::Future + Send + 'static>(
    futs: impl Iterator<Item = F>,
) -> Vec<F::Output>
where
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futs.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.expect("request task panicked"));
    }
    out
}

#[tokio::test]
async fn test_http_introspection_and_mixed_root_fields() {
    let (_client, schema, catalog) = setup_test().await;
    let app = create_router(server_state(schema, catalog, 2));

    let (status, json) = post_graphql(
        &app,
        serde_json::json!({
            "query": "query IntrospectionQuery { __schema { queryType { name } types { name } } }",
            "operationName": "IntrospectionQuery"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["__schema"]["queryType"]["name"], "Query");
    let types = json["data"]["__schema"]["types"].as_array().unwrap();
    assert!(types.iter().any(|t| t["name"] == "Forum"), "{json}");

    // Introspection and database fields in one operation, merged in selection order
    let (status, json) = post_graphql(
        &app,
        serde_json::json!({ "query": "{ __typename forums { name } forumType: __type(name: \"Forum\") { name } }" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let keys: Vec<_> = json["data"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["__typename", "forums", "forumType"]);
    assert_eq!(json["data"]["__typename"], "Query");
    assert_eq!(json["data"]["forumType"]["name"], "Forum");
    // Other tests of this file create (and delete) forums concurrently: the fixture's are there
    let names: Vec<&str> = json["data"]["forums"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    for name in ["Cats", "Dogs", "Postgres"] {
        assert!(names.contains(&name), "{names:?}");
    }
}

#[tokio::test]
async fn test_http_operation_name_selects_operation() {
    let (_client, schema, catalog) = setup_test().await;
    let app = create_router(server_state(schema, catalog, 2));

    let query = "query A { forums { name } } query B { users { username } }";
    let (status, json) = post_graphql(
        &app,
        serde_json::json!({ "query": query, "operationName": "B" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["data"]["users"].is_array(), "{json}");
    assert!(json["data"].get("forums").is_none(), "{json}");
}

#[tokio::test]
async fn test_http_debug_sql_extension() {
    let (_client, schema, catalog) = setup_test().await;

    // Off by default
    let app = create_router(server_state(schema.clone(), catalog.clone(), 2));
    let (_, json) = post_graphql(&app, serde_json::json!({ "query": "{ forums { name } }" })).await;
    assert!(json.get("extensions").is_none(), "{json}");

    let debug = DebugOptions {
        sql: true,
        explain: false,
    };
    let app = create_router(server_state_with_debug(schema, catalog, 2, debug));
    let (status, json) = post_graphql(
        &app,
        serde_json::json!({ "query": "{ message(id: \"ca70ca70-0000-0000-0000-cec111a0ca70\") { body forum { name } } }" }),
    )
    .await;
    println!("{}", serde_json::to_string_pretty(&json).unwrap());
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["message"]["forum"]["name"], "Cats");

    let sql = json["extensions"]["sql"]["query"]
        .as_str()
        .expect("sql.query");
    // Pretty-printed for humans, so compare with whitespace collapsed
    let collapsed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        collapsed.contains("LEFT JOIN \"app_public\".\"forums\""),
        "{sql}"
    );
    assert!(sql.contains('\n'), "SQL should be pretty-printed: {sql}");
    assert_eq!(
        json["extensions"]["sql"]["params"],
        serde_json::json!(["ca70ca70-0000-0000-0000-cec111a0ca70"])
    );
    assert!(json["extensions"].get("explain").is_none(), "{json}");
}

#[tokio::test]
async fn test_http_debug_explain_extension() {
    let (client, schema, catalog) = setup_test().await;
    let debug = DebugOptions {
        sql: false,
        explain: true,
    };
    let app = create_router(server_state_with_debug(schema, catalog, 2, debug));

    let (status, json) =
        post_graphql(&app, serde_json::json!({ "query": "{ forums { name } }" })).await;
    println!("{}", serde_json::to_string_pretty(&json).unwrap());
    assert_eq!(status, StatusCode::OK, "{json}");
    // explain implies sql
    assert!(json["extensions"]["sql"]["query"].is_string(), "{json}");
    let explain = json["extensions"]["explain"]
        .as_array()
        .expect("explain lines");
    let text = explain
        .iter()
        .filter_map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Execution Time"), "{text}");
    assert!(text.contains("Serialization"), "{text}");

    // EXPLAIN ANALYZE runs in a rolled-back transaction: a mutation is applied exactly once
    let name = format!("explain-once-{}", uuid::Uuid::new_v4());
    let (status, json) = post_graphql(
        &app,
        serde_json::json!({ "query": format!("mutation {{ createForum(input: {{ name: \"{name}\" }}) {{ forum {{ id }} }} }}") }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["extensions"]["explain"].is_array(), "{json}");

    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM app_public.forums WHERE name = $1",
            &[&name],
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute("DELETE FROM app_public.forums WHERE name = $1", &[&name])
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_graphiql_page_served() {
    let (_client, schema, catalog) = setup_test().await;
    // Off by default: a development tool
    let off = create_router(server_state(schema.clone(), catalog.clone(), 1));
    let request = Request::builder()
        .method("GET")
        .uri("/graphiql")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        off.clone().oneshot(request).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let request_js = Request::builder()
        .method("GET")
        .uri("/graphiql/graphiql.js")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        off.oneshot(request_js).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    let state = ServerState::new(
        create_pool(common::db_url(), 1).expect("pool"),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    );
    let app = create_router(Arc::new(state.with_graphiql(true)));

    let request = Request::builder()
        .method("GET")
        .uri("/graphiql")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("<title>graphpg · GraphiQL</title>"));
    assert!(
        !html.contains("esm.sh"),
        "GraphiQL must not reference external CDNs like esm.sh: {html}"
    );

    // Extract the hashed CSS and JS links from the HTML
    let css_start = html.find("/graphiql/graphiql.").expect("css link");
    let css_end = html[css_start..].find(".css").expect(".css") + css_start + 4;
    let css_path = &html[css_start..css_end];

    let js_start = html.rfind("/graphiql/graphiql.").expect("js link");
    let js_end = html[js_start..].find(".js").expect(".js") + js_start + 3;
    let js_path = &html[js_start..js_end];

    // Vendored static assets are embedded and served directly by the server
    for (asset_path, expected_mime) in [
        (js_path, "application/javascript"),
        (css_path, "text/css"),
        ("/graphiql/editor.worker.js", "application/javascript"),
        ("/graphiql/graphql.worker.js", "application/javascript"),
        ("/graphiql/json.worker.js", "application/javascript"),
    ] {
        let req = Request::builder()
            .method("GET")
            .uri(asset_path)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "Failed for asset {asset_path}"
        );
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.starts_with(expected_mime),
            "Expected {expected_mime} for {asset_path}, got {ct}"
        );
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert!(!bytes.is_empty(), "Asset {asset_path} is empty");
    }
}

#[tokio::test]
async fn test_scalar_page_served() {
    let (_client, schema, catalog) = setup_test().await;

    // Off by default: a development tool
    let off = create_router(server_state(schema.clone(), catalog.clone(), 1));
    let request = Request::builder()
        .method("GET")
        .uri("/scalar")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        off.clone().oneshot(request).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let request_js = Request::builder()
        .method("GET")
        .uri("/scalar/scalar.js")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        off.oneshot(request_js).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    let state = ServerState::new(
        create_pool(common::db_url(), 1).expect("pool"),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    );
    let app = create_router(Arc::new(state.with_scalar(true)));

    let request = Request::builder()
        .method("GET")
        .uri("/scalar")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Scalar"), "{html}");
    assert!(html.contains("/scalar/scalar.js"), "{html}");
    assert!(html.contains("/openapi.json"), "{html}");
    // Verify agent is disabled as requested
    assert!(
        html.contains("\"disabled\":true") || html.contains("\"disabled\": true"),
        "{html}"
    );

    // Also test trailing slash
    let request_slash = Request::builder()
        .method("GET")
        .uri("/scalar/")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request_slash).await.unwrap().status(),
        StatusCode::OK
    );

    // Vendored static asset scalar.js is embedded and served directly by the server
    let req = Request::builder()
        .method("GET")
        .uri("/scalar/scalar.js")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("application/javascript"), "{ct}");
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert!(!bytes.is_empty(), "scalar.js is empty");
}

#[tokio::test]
async fn test_graphql_disabled_returns_404() {
    let (_client, schema, catalog) = setup_test().await;
    let state = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).expect("Failed to create pool"),
            schema,
            catalog,
            "app_public".to_string(),
            DebugOptions::default(),
        )
        .with_graphql_enabled(false),
    );
    let app = create_router(state);

    // /graphql returns 404
    let req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"query": "{ __typename }"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // /export returns 404
    let req = Request::builder()
        .method("POST")
        .uri("/export")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"query": "{ __typename }"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // /import/User returns 404
    let req = Request::builder()
        .method("POST")
        .uri("/import/User")
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from("{}"))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // /health still returns 200
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_rest_endpoint_enabled_and_disabled() {
    let (_client, schema, catalog) = setup_test().await;
    let state_enabled = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema.clone(),
        catalog.clone(),
        "app_public".to_string(),
        DebugOptions::default(),
    ));
    let app_enabled = create_router(state_enabled);

    // GET /rest returns 200 OK with "hello world"
    let req = Request::builder()
        .method("GET")
        .uri("/rest")
        .body(Body::empty())
        .unwrap();
    let res = app_enabled.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"hello world");

    // When openapi_enabled is false, GET /rest returns 404
    let state_disabled = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).expect("Failed to create pool"),
            schema,
            catalog,
            "app_public".to_string(),
            DebugOptions::default(),
        )
        .with_openapi_enabled(false),
    );
    let app_disabled = create_router(state_disabled);
    let req = Request::builder()
        .method("GET")
        .uri("/rest")
        .body(Body::empty())
        .unwrap();
    let res = app_disabled.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_rest_list_resource_endpoint() {
    let (_client, schema, catalog) = setup_test().await;
    let state_enabled = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema.clone(),
        catalog.clone(),
        "app_public".to_string(),
        DebugOptions::default(),
    ));
    let app = create_router(state_enabled);

    // 1. GET /rest/forums returns 200 with JSON array of plain rows (no Relay envelope)
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json; charset=utf-8"
    );
    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("Valid JSON");
    let rows = json.as_array().expect("Expected JSON array of rows");
    assert!(!rows.is_empty(), "Forums should not be empty");

    let forum_names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    for expected in ["Cats", "Dogs", "Postgres"] {
        assert!(
            forum_names.contains(&expected),
            "Missing forum {expected} in {forum_names:?}"
        );
    }
    // Verify plain row fields without Relay envelope:
    assert!(rows[0].get("id").is_some());
    assert!(rows[0].get("name").is_some());
    assert!(rows[0].get("node").is_none());
    assert!(rows[0].get("edges").is_none());
    assert!(rows[0].get("pageInfo").is_none());

    // 2. GET /rest/users returns 200 with JSON array of users
    let req = Request::builder()
        .method("GET")
        .uri("/rest/users")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("Valid JSON");
    assert!(json.is_array());

    // 3. GET /rest/nonexistent returns 404
    let req = Request::builder()
        .method("GET")
        .uri("/rest/nonexistent")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // 4. When openapi_enabled is false, GET /rest/forums returns 404
    let state_disabled = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).expect("Failed to create pool"),
            schema,
            catalog,
            "app_public".to_string(),
            DebugOptions::default(),
        )
        .with_openapi_enabled(false),
    );
    let app_disabled = create_router(state_disabled);
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums")
        .body(Body::empty())
        .unwrap();
    let res = app_disabled.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_openapi_spec_serving_and_content_negotiation() {
    let (_client, schema, catalog) = setup_test().await;

    // 1. Spec enabled (e.g. dev mode)
    let state_enabled = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).expect("Failed to create pool"),
            schema.clone(),
            catalog.clone(),
            "app_public".to_string(),
            DebugOptions::default(),
        )
        .with_openapi_spec(true),
    );
    let app_enabled = create_router(state_enabled.clone());

    // GET /openapi.json returns 200 with JSON by default
    let req = Request::builder()
        .method("GET")
        .uri("/openapi.json")
        .body(Body::empty())
        .unwrap();
    let res = app_enabled.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json; charset=utf-8"
    );
    let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json_doc: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("Valid JSON OpenAPI document");
    assert_eq!(json_doc["openapi"], "3.1.0");
    assert_eq!(json_doc["info"]["title"], "graphpg REST API");
    assert!(json_doc["paths"]["/rest"].is_object());
    assert!(json_doc["components"]["schemas"].is_object());

    // GET /openapi.json with Accept: application/yaml returns YAML
    let req = Request::builder()
        .method("GET")
        .uri("/openapi.json")
        .header(header::ACCEPT, "application/yaml")
        .body(Body::empty())
        .unwrap();
    let res = app_enabled.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/yaml; charset=utf-8"
    );
    let yaml_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let yaml_doc: serde_json::Value =
        serde_yaml::from_slice(&yaml_bytes).expect("Valid YAML OpenAPI document");
    assert_eq!(yaml_doc["openapi"], "3.1.0");

    // 2. Spec disabled returns 404
    let state_disabled = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).expect("Failed to create pool"),
            schema.clone(),
            catalog.clone(),
            "app_public".to_string(),
            DebugOptions::default(),
        )
        .with_openapi_spec(false),
    );
    let app_disabled = create_router(state_disabled);
    let req = Request::builder()
        .method("GET")
        .uri("/openapi.json")
        .body(Body::empty())
        .unwrap();
    let res = app_disabled.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // 3. Custom path: /spec
    let state_custom = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).expect("Failed to create pool"),
            schema.clone(),
            catalog.clone(),
            "app_public".to_string(),
            DebugOptions::default(),
        )
        .with_openapi_spec(true)
        .with_openapi_path("/spec".to_string()),
    );
    let app_custom = create_router(state_custom);
    let req = Request::builder()
        .method("GET")
        .uri("/spec")
        .body(Body::empty())
        .unwrap();
    let res = app_custom.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let req = Request::builder()
        .method("GET")
        .uri("/openapi.json")
        .body(Body::empty())
        .unwrap();
    let res = app_custom.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // 4. Reload updates the OpenAPI document
    let mut modified_catalog = catalog.clone();
    modified_catalog.tables.clear();
    state_enabled.reload(schema, modified_catalog);
    let updated_doc = state_enabled.current_openapi_document();
    assert!(
        updated_doc["components"]["schemas"]
            .as_object()
            .unwrap()
            .is_empty(),
        "Schemas should be empty after reloading with catalog without tables"
    );
}

#[tokio::test]
async fn test_rest_reads_filtering() {
    let (_client, schema, catalog) = setup_test().await;
    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    ));
    let app = create_router(state);

    // 1. Equal filter: filter[name][eq]=Cats
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?filter[name][eq]=Cats")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Cats");

    // 2. Pattern filter: filter[name][like]=%Cat%
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?filter[name][like]=%25Cat%25")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Cats");

    // 3. In-list filter: filter[name][in]=Cats,Dogs
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?filter[name][in]=Cats,Dogs")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 2);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.contains(&"Cats"));
    assert!(names.contains(&"Dogs"));
}

#[tokio::test]
async fn test_rest_reads_ordering() {
    let (_client, schema, catalog) = setup_test().await;
    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    ));
    let app = create_router(state);

    // 1. Order by name:desc
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?order=name:desc")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.len() >= 3);
    assert!(
        names
            .windows(2)
            .all(|w| w[0].to_lowercase() >= w[1].to_lowercase()),
        "Expected descending order: {names:?}"
    );

    // 2. Order by -name (unary minus shorthand)
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?order=-name")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.len() >= 3);
    assert!(
        names
            .windows(2)
            .all(|w| w[0].to_lowercase() >= w[1].to_lowercase()),
        "Expected descending order: {names:?}"
    );

    // 3. Order by name:asc
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?order=name:asc")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.len() >= 3);
    assert!(
        names
            .windows(2)
            .all(|w| w[0].to_lowercase() <= w[1].to_lowercase()),
        "Expected ascending order: {names:?}"
    );
}

#[tokio::test]
async fn test_rest_reads_pagination_and_link_headers() {
    let (_client, schema, catalog) = setup_test().await;
    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    ));
    let app = create_router(state);

    // 1. First page: ?filter[name][in]=Cats,Dogs,Postgres&first=2&order=name:asc
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?filter[name][in]=Cats,Dogs,Postgres&first=2&order=name:asc")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("x-total-count").unwrap(), "3");

    let link_header = res
        .headers()
        .get(header::LINK)
        .expect("Link header expected")
        .to_str()
        .unwrap()
        .to_string();
    assert!(link_header.contains("rel=\"next\""));
    assert!(link_header.contains("after="));
    assert!(
        link_header.contains("filter%5Bname%5D%5Bin%5D=Cats%2CDogs%2CPostgres")
            || link_header.contains("filter[name][in]=Cats,Dogs,Postgres")
    );

    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "Cats");
    assert_eq!(rows[1]["name"], "Dogs");

    // Extract next URL from link header: <URI>; rel="next"
    let next_uri = link_header
        .split(';')
        .next()
        .unwrap()
        .trim_matches(|c| c == '<' || c == '>');

    // 2. Fetch second page using next URL
    let req = Request::builder()
        .method("GET")
        .uri(next_uri)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("x-total-count").unwrap(), "3");
    let link_header_page2 = res
        .headers()
        .get(header::LINK)
        .expect("Link header expected on page 2")
        .to_str()
        .unwrap()
        .to_string();
    assert!(link_header_page2.contains("rel=\"prev\""));
    assert!(link_header_page2.contains("before="));

    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Postgres");

    // 3. Offset pagination: ?filter[name][in]=Cats,Dogs,Postgres&limit=1&offset=1&order=name:asc
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?filter[name][in]=Cats,Dogs,Postgres&limit=1&offset=1&order=name:asc")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Dogs");
}

#[tokio::test]
async fn test_rest_reads_projection_and_embedding() {
    let (_client, schema, catalog) = setup_test().await;
    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    ));
    let app = create_router(state);

    // 1. Column projection: ?fields=name
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?fields=name")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(!rows.is_empty());
    assert!(rows[0].get("name").is_some());
    assert!(rows[0].get("id").is_none());

    // 2. Unknown field in ?fields=invalid returns 400
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?fields=invalid_col")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // 3. Relation embedding: ?include=messages
    let req = Request::builder()
        .method("GET")
        .uri("/rest/forums?include=messages")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(!rows.is_empty());
    assert!(rows[0].get("messages").is_some());
    assert!(rows[0]["messages"].is_array());
}

#[tokio::test]
async fn test_rest_reads_search_post() {
    let (_client, schema, catalog) = setup_test().await;
    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).expect("Failed to create pool"),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    ));
    let app = create_router(state);

    // 1. POST /rest/forums/search with {"filter": {"name": {"eq": "Cats"}}}
    let search_body = serde_json::json!({
        "filter": {
            "name": { "eq": "Cats" }
        }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/rest/forums/search")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&search_body).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Cats");

    // 2. POST /rest/forums/search with raw or filter body
    let search_body = serde_json::json!({
        "or": [
            { "name": { "like": "%Cat%" } },
            { "name": { "like": "%Dog%" } }
        ]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/rest/forums/search")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&search_body).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 2);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.contains(&"Cats"));
    assert!(names.contains(&"Dogs"));

    // 3. POST /rest/forums/search with filter, orderBy, and first
    let search_body = serde_json::json!({
        "filter": {
            "name": { "like": "%o%" }
        },
        "orderBy": ["NAME_DESC"],
        "first": 1
    });
    let req = Request::builder()
        .method("POST")
        .uri("/rest/forums/search")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&search_body).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Postgres");
}
