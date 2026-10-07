//! Safety limits (`limits:`): page size, depth, cost, bulk rows, request body size and
//! `statement_timeout`, enforced on the plan so GraphQL and REST share them.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::models::{LimitsConfig, PageSizeConfig};
use graphpg::config::{Config, Mode};
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_pool_with, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn forum_state(limits: LimitsConfig) -> ServerState {
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
    .with_limits(limits)
}

fn limits() -> LimitsConfig {
    LimitsConfig {
        page_size: PageSizeConfig {
            default: 2,
            max: Some(3),
        },
        depth: Some(3),
        cost: Some(10_000),
        bulk_rows: Some(2),
        groups: Some(2),
        request_body_bytes: Some(2048),
        statement_timeout: None,
        count_threshold: Some(10_000),
        require_filter: false,
    }
}

struct Reply {
    status: StatusCode,
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
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn graphql(app: &axum::Router, query: &str) -> Reply {
    send(app, "POST", "/graphql", Some(json!({ "query": query }))).await
}

fn error_code(reply: &Reply) -> &str {
    reply.body["errors"][0]["extensions"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("{}", reply.body))
}

// ---------------------------------------------------------------------------------------------
// GraphQL
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn graphql_page_sizes_default_and_are_bounded() {
    let app = create_router(Arc::new(forum_state(limits()).await));

    // without `first`, a connection reads the configured default page
    let page = graphql(
        &app,
        "{ forumsConnection(orderBy: [NAME_ASC]) { nodes { name } pageInfo { hasNextPage } } }",
    )
    .await;
    let data = &page.body["data"]["forumsConnection"];
    assert_eq!(data["nodes"].as_array().unwrap().len(), 2, "{}", page.body);
    assert_eq!(data["pageInfo"]["hasNextPage"], true);

    // up to the maximum is fine; more is rejected with the limit named
    let max = graphql(&app, "{ forumsConnection(first: 3) { nodes { name } } }").await;
    assert_eq!(
        max.body["data"]["forumsConnection"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    for query in [
        "{ forumsConnection(first: 4) { nodes { name } } }",
        "{ forumsConnection(last: 1000000) { nodes { name } } }",
        "{ forumsConnection(first: 1) { nodes { messagesConnection(first: 99) { nodes { id } } } } }",
    ] {
        let reply = graphql(&app, query).await;
        assert_eq!(error_code(&reply), "PAGE_SIZE_EXCEEDED", "{query}: {}", reply.body);
        assert_eq!(reply.body["errors"][0]["extensions"]["limit"], 3);
        assert!(reply.body["data"].is_null());
    }

    // plain lists have no page size: they are cut at the maximum
    let list = graphql(&app, "{ forums { name } }").await;
    assert_eq!(list.body["data"]["forums"].as_array().unwrap().len(), 3);
    let cut = create_router(Arc::new(
        forum_state(LimitsConfig {
            page_size: PageSizeConfig {
                default: 1,
                max: Some(1),
            },
            ..limits()
        })
        .await,
    ));
    let list = graphql(&cut, "{ forums { name } }").await;
    assert_eq!(
        list.body["data"]["forums"].as_array().unwrap().len(),
        1,
        "{}",
        list.body
    );
}

#[tokio::test]
async fn graphql_depth_and_cost_are_bounded() {
    let app = create_router(Arc::new(forum_state(limits()).await));

    // forums > messages > author is three levels
    let ok = graphql(&app, "{ forumsConnection(first: 1) { nodes { messagesConnection(first: 1) { nodes { author { username } } } } } }").await;
    assert!(ok.body["errors"].is_null(), "{}", ok.body);

    let deep = graphql(
        &app,
        "{ forumsConnection(first: 1) { nodes { messagesConnection(first: 1) { nodes { author { messagesConnection(first: 1) { nodes { id } } } } } } } }",
    )
    .await;
    assert_eq!(error_code(&deep), "DEPTH_LIMIT_EXCEEDED", "{}", deep.body);
    assert_eq!(deep.body["errors"][0]["extensions"]["limit"], 3);
    assert_eq!(deep.body["errors"][0]["extensions"]["actual"], 4);

    // cost: 3 forums x 3 messages x ... fits 10 000; raise the pages and the nesting and it does not
    let tight = create_router(Arc::new(
        forum_state(LimitsConfig {
            cost: Some(20),
            depth: None,
            page_size: PageSizeConfig {
                default: 2,
                max: Some(100),
            },
            ..limits()
        })
        .await,
    ));
    let pricey = graphql(
        &tight,
        "{ forumsConnection(first: 10) { nodes { messagesConnection(first: 10) { nodes { id } } } } }",
    )
    .await;
    assert_eq!(
        error_code(&pricey),
        "COST_LIMIT_EXCEEDED",
        "{}",
        pricey.body
    );
    assert_eq!(pricey.body["errors"][0]["extensions"]["actual"], 110);
    let cheap = graphql(&tight, "{ forumsConnection(first: 2) { nodes { name } } }").await;
    assert!(cheap.body["errors"].is_null(), "{}", cheap.body);
}

#[tokio::test]
async fn graphql_bulk_writes_are_bounded() {
    let app = create_router(Arc::new(forum_state(limits()).await));
    let three = graphql(
        &app,
        r#"mutation { createForums(inputs: [{ name: "a" }, { name: "b" }, { name: "c" }]) { forums { name } } }"#,
    )
    .await;
    assert_eq!(error_code(&three), "BULK_LIMIT_EXCEEDED", "{}", three.body);
    assert_eq!(three.body["errors"][0]["extensions"]["actual"], 3);
    // nothing was written
    let none = graphql(
        &app,
        r#"{ forumsConnection(filter: { name: { in: ["a", "b", "c"] } }) { totalCount } }"#,
    )
    .await;
    assert_eq!(none.body["data"]["forumsConnection"]["totalCount"], 0);
}

#[tokio::test]
async fn request_bodies_are_bounded_for_both_apis() {
    let app = create_router(Arc::new(forum_state(limits()).await));
    let padding = "x".repeat(4096);
    let graphql_big = send(
        &app,
        "POST",
        "/graphql",
        Some(json!({ "query": "{ forums { name } }", "extensions": { "pad": padding } })),
    )
    .await;
    assert_eq!(graphql_big.status, StatusCode::PAYLOAD_TOO_LARGE);
    let rest_big = send(
        &app,
        "POST",
        "/rest/forums",
        Some(json!({ "name": padding })),
    )
    .await;
    assert_eq!(rest_big.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        send(
            &app,
            "POST",
            "/graphql",
            Some(json!({ "query": "{ forums { name } }" }))
        )
        .await
        .status,
        StatusCode::OK
    );

    // no limit, no problem
    let unbounded = create_router(Arc::new(
        forum_state(LimitsConfig {
            request_body_bytes: None,
            ..limits()
        })
        .await,
    ));
    let big = send(
        &unbounded,
        "POST",
        "/graphql",
        Some(json!({ "query": "{ forums { name } }", "extensions": { "pad": padding } })),
    )
    .await;
    assert_eq!(big.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------------------------
// REST
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn rest_shares_the_limits() {
    let app = create_router(Arc::new(forum_state(limits()).await));

    // default page, and the same maximum
    let page = send(&app, "GET", "/rest/forums?order=name", None).await;
    assert_eq!(page.body.as_array().unwrap().len(), 2, "{}", page.body);
    assert_eq!(
        send(&app, "GET", "/rest/forums?first=3", None).await.status,
        StatusCode::OK
    );
    let over = send(&app, "GET", "/rest/forums?first=4", None).await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST);
    assert_eq!(over.body["code"], "PAGE_SIZE_EXCEEDED");
    assert_eq!(
        (over.body["limit"].as_u64(), over.body["actual"].as_u64()),
        (Some(3), Some(4))
    );
    let search = send(
        &app,
        "POST",
        "/rest/forums/search",
        Some(json!({ "first": 4 })),
    )
    .await;
    assert_eq!(search.body["code"], "PAGE_SIZE_EXCEEDED");

    // include depth is the depth limit: forums > messages > author fits, one more does not
    assert_eq!(
        send(&app, "GET", "/rest/forums?include=messages&first=1", None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "GET",
            "/rest/forums?include=messages.author&first=1",
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    let deep = send(
        &app,
        "GET",
        "/rest/forums?include=messages.author.messages&first=1",
        None,
    )
    .await;
    assert_eq!(
        (deep.status, deep.body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("DEPTH_LIMIT_EXCEEDED"))
    );

    // bulk creates
    let bulk = send(
        &app,
        "POST",
        "/rest/forums",
        Some(json!([{ "name": "a" }, { "name": "b" }, { "name": "c" }])),
    )
    .await;
    assert_eq!(bulk.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(bulk.body["code"], "BULK_LIMIT_EXCEEDED");

    // groups of an aggregate
    let groups = send(&app, "GET", "/rest/messages/aggregate?group=id", None).await;
    if groups.status != StatusCode::OK {
        assert_eq!(groups.body["code"], "TOO_MANY_GROUPS", "{}", groups.body);
    }
}

#[tokio::test]
async fn the_openapi_document_states_the_limits() {
    let state = forum_state(limits()).await;
    let config = Config::from_yaml(
        "limits:\n  page_size: { default: 2, max: 3 }\n  bulk_rows: 2\n  groups: 2\n",
    )
    .unwrap();
    let state = state.with_config(config);
    let doc = (*state.current_openapi_document()).clone();

    let params = doc["paths"]["/rest/forums"]["get"]["parameters"]
        .as_array()
        .unwrap();
    let first = params.iter().find(|p| p["name"] == "first").unwrap();
    assert_eq!(first["schema"]["maximum"], 3);
    assert!(first["description"].as_str().unwrap().contains("default 2"));
    let body = &doc["components"]["schemas"]["forumsSearchRequest"]["properties"];
    assert_eq!(body["first"]["maximum"], 3);
    let bulk = &doc["paths"]["/rest/forums"]["post"]["requestBody"]["content"]["application/json"]
        ["schema"]["oneOf"][1];
    assert_eq!(bulk["maxItems"], 2);
    assert_eq!(
        doc["paths"]["/rest/messages/aggregate"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"]["oneOf"][1]["maxItems"],
        2
    );

    // switched off, the document has no maximum
    let open = forum_state(limits()).await.with_config(
        Config::from_yaml("limits:\n  page_size: { max: off }\n  bulk_rows: off\n").unwrap(),
    );
    let doc = (*open.current_openapi_document()).clone();
    let params = doc["paths"]["/rest/forums"]["get"]["parameters"]
        .as_array()
        .unwrap();
    assert!(
        params.iter().find(|p| p["name"] == "first").unwrap()["schema"]
            .get("maximum")
            .is_none()
    );
    assert_eq!(
        doc["paths"]["/rest/forums"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["oneOf"][1]["maxItems"],
        Value::Null
    );
}

// ---------------------------------------------------------------------------------------------
// statement_timeout
// ---------------------------------------------------------------------------------------------

async fn extras_state(timeout: Option<Duration>) -> ServerState {
    common::ensure_rest_extras_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("rest_extras")
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    ServerState::new(
        create_pool_with(common::db_url(), 2, timeout).unwrap(),
        schema,
        catalog,
        "rest_extras".to_string(),
        DebugOptions::default(),
    )
    .with_mode(Mode::Prod)
}

#[tokio::test]
async fn a_slow_statement_is_cancelled_for_both_apis() {
    let app = create_router(Arc::new(
        extras_state(Some(Duration::from_millis(150))).await,
    ));

    let started = std::time::Instant::now();
    let rest = send(&app, "GET", "/rest/rpc/nap?secs=5", None).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancelled, not finished: {:?}",
        started.elapsed()
    );
    assert_eq!(rest.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rest.body["code"], "STATEMENT_TIMEOUT");
    assert!(
        !rest.body.to_string().contains("canceling statement"),
        "no database message: {}",
        rest.body
    );

    let gql = graphql(&app, "{ nap(secs: 5) }").await;
    assert_eq!(gql.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_code(&gql), "STATEMENT_TIMEOUT");

    // a quick statement still works, on the same pool, and so does a mutation's transaction
    let quick = send(&app, "GET", "/rest/rpc/nap?secs=0", None).await;
    assert_eq!(quick.body, json!(1));
    let write = send(&app, "POST", "/rest/rpc/touch_counters", Some(json!({}))).await;
    assert_eq!(write.status, StatusCode::NO_CONTENT);

    // without a timeout the same call completes
    let patient = create_router(Arc::new(extras_state(None).await));
    assert_eq!(
        send(&patient, "GET", "/rest/rpc/nap?secs=0.3", None)
            .await
            .body,
        json!(1)
    );
}
