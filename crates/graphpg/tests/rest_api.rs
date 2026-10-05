//! REST API behaviour: strict input handling, raw keys, opt-in counts, searches, writes with
//! row versions, RFC 9457 errors, safety limits, and the OpenAPI document describing it all.
mod common;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::models::{AuthConfig, RolesConfig};
use graphpg::config::Mode;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, Authenticator, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const CATS: &str = "ca700000-0000-0000-0000-000000000ca7";

async fn state() -> ServerState {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to the test database");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("app_public")
        .await
        .expect("inspect app_public");
    let schema = SchemaBuilder::build(&catalog).expect("build schema");
    ServerState::new(
        create_pool(common::db_url(), 4).expect("pool"),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    )
}

async fn app() -> axum::Router {
    create_router(Arc::new(state().await))
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    #[track_caller]
    fn assert_problem(&self, status: StatusCode, code: &str) -> &Value {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(
            self.header("content-type"),
            Some("application/problem+json")
        );
        assert_eq!(self.body["type"], "about:blank");
        assert_eq!(self.body["status"], status.as_u16());
        assert_eq!(self.body["code"], code, "{}", self.body);
        &self.body
    }
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
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
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        headers,
        body,
    }
}

async fn get(app: &axum::Router, uri: &str) -> Reply {
    call(app, "GET", uri, &[], None).await
}

fn names(reply: &Reply) -> Vec<&str> {
    reply
        .body
        .as_array()
        .expect("an array of rows")
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect()
}

/// Removes a forum a test created, whatever happened.
async fn delete_forum(app: &axum::Router, id: &str) {
    call(app, "DELETE", &format!("/rest/forums/{id}"), &[], None).await;
}

// ---------------------------------------------------------------------------------------------
// Strict input: a typo is a 400, never an unfiltered read or a no-op write
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reads_reject_what_they_do_not_understand() {
    let app = app().await;
    for uri in [
        "/rest/forums?filtr[name][eq]=Cats",
        "/rest/forums?filter=notjson",
        "/rest/forums?filter=[1,2]",
        "/rest/forums?filter[name][eqq]=Cats",
        "/rest/forums?filter[nope][eq]=Cats",
        "/rest/forums?filter[archivedAt][isNull]=maybe",
        "/rest/forums?order=bogus",
        "/rest/forums?order=name:sideways",
        "/rest/forums?first=0",
        "/rest/forums?first=1001",
        "/rest/forums?last=1001",
        "/rest/forums?first=x",
        "/rest/forums?offset=-1",
        "/rest/forums?limit=5",
        "/rest/forums?select=id",
        "/rest/forums?orderBy=name",
        "/rest/forums?fields=nope",
        "/rest/forums?include=nope",
        "/rest/forums?include=messagesConnection",
        "/rest/forums?include=messages.forum.messages.forum",
        "/rest/forums?include=a,b,c,d,e,f,g,h,i,j,k",
        // a single item takes only `fields` and `include`
        &format!("/rest/forums/{CATS}?filter[name][eq]=Dogs"),
        &format!("/rest/forums/{CATS}?first=1"),
        &format!("/rest/forums/{CATS}?whatever=1"),
    ] {
        let reply = get(&app, uri).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            reply.body
        );
        assert_eq!(
            reply.header("content-type"),
            Some("application/problem+json"),
            "{uri}"
        );
        assert_eq!(reply.body["type"], "about:blank");
    }

    // the same filter spelled correctly narrows the rows, and the largest page is allowed
    let reply = get(&app, "/rest/forums?filter[name][eq]=Cats&first=1000").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(names(&reply), ["Cats"]);
    // nested embedding within the depth limit
    let reply = get(&app, "/rest/forums?include=messages.author&first=1").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn filter_through_relations_and_combined_embedding() {
    let app = app().await;
    // a to-many relation filter, spelled with deepObject brackets
    let reply = get(
        &app,
        "/rest/forums?filter[messages][some][body][like]=%25&fields=name",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    // includes that share a prefix are merged into one embedded relation
    let reply = get(
        &app,
        "/rest/forums?include=messages,messages.author&first=1",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body[0]["messages"].is_array());
}

#[tokio::test]
async fn writes_reject_unknown_and_malformed_bodies() {
    let app = app().await;

    for (method, uri, body) in [
        (
            "POST",
            "/rest/forums".to_string(),
            json!({ "name": "X", "bogus": 1 }),
        ),
        ("POST", "/rest/forums".to_string(), json!([])),
        ("POST", "/rest/forums".to_string(), json!(5)),
        ("POST", "/rest/forums".to_string(), json!("text")),
        (
            "POST",
            "/rest/forums".to_string(),
            json!([{ "name": "X" }, 3]),
        ),
        (
            "POST",
            "/rest/forums".to_string(),
            json!([{ "name": "X", "bogus": 1 }]),
        ),
        (
            "PATCH",
            format!("/rest/forums/{CATS}"),
            json!({ "nme": "x" }),
        ),
        ("PATCH", format!("/rest/forums/{CATS}"), json!({})),
        ("PATCH", format!("/rest/forums/{CATS}"), json!([1])),
        // the key of a row cannot be changed through its URL
        (
            "PATCH",
            format!("/rest/forums/{CATS}"),
            json!({ "id": "bbbbbbbb-0000-0000-0000-000000000002" }),
        ),
        (
            "PUT",
            format!("/rest/forums/{CATS}"),
            json!({ "name": "Cats", "bogus": true }),
        ),
        (
            "PUT",
            format!("/rest/forums/{CATS}"),
            json!({ "id": "bbbbbbbb-0000-0000-0000-000000000002", "name": "Moved" }),
        ),
    ] {
        let reply = call(&app, method, &uri, &[], Some(body.clone())).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{method} {uri} {body}: {}",
            reply.body
        );
        assert_eq!(reply.body["type"], "about:blank");
    }
    let written = get(&app, "/rest/forums?filter[name][in]=X,Moved,Cats").await;
    assert_eq!(names(&written), ["Cats"], "nothing was written");

    // the unknown member is named
    let reply = call(
        &app,
        "POST",
        "/rest/forums",
        &[],
        Some(json!({ "name": "X", "bogus": 1 })),
    )
    .await;
    assert_eq!(reply.body["code"], "INVALID_REQUEST");
    assert_eq!(reply.body["fields"], json!(["body.bogus"]));

    // malformed JSON and an empty body
    for body in ["{not json", ""] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/rest/forums")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn bulk_create_is_bounded() {
    let app = app().await;
    let rows: Vec<Value> = (0..1001)
        .map(|i| json!({ "name": format!("bulk {i}") }))
        .collect();
    let reply = call(&app, "POST", "/rest/forums", &[], Some(Value::Array(rows))).await;
    reply.assert_problem(StatusCode::PAYLOAD_TOO_LARGE, "BULK_LIMIT_EXCEEDED");

    let oversized = "x".repeat(2 * 1024 * 1024);
    let reply = call(
        &app,
        "POST",
        "/rest/forums",
        &[],
        Some(json!({ "name": oversized })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
}

// ---------------------------------------------------------------------------------------------
// Keys: rows carry the real key, not the Relay global ID
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn rows_carry_the_raw_primary_key() {
    let app = app().await;
    let reply = get(&app, "/rest/forums?filter[name][eq]=Cats").await;
    assert_eq!(reply.body[0]["id"], CATS);

    // the key addresses the row, raw or as the global ID of the same table
    let global = graphpg::schema::SchemaBuilder::encode_global_id("Forum", CATS);
    for key in [CATS, global.as_str()] {
        let reply = get(&app, &format!("/rest/forums/{key}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{key}");
        assert_eq!(reply.body["id"], CATS);
    }
    // a global ID of another type is not this table's key
    let other = graphpg::schema::SchemaBuilder::encode_global_id("Message", CATS);
    assert_eq!(
        get(&app, &format!("/rest/forums/{other}")).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&app, "/rest/forums/not-a-uuid").await.status,
        StatusCode::BAD_REQUEST
    );
    get(&app, "/rest/forums/aaaaaaaa-0000-0000-0000-00000000dead")
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");

    // an embedded relation's rows carry raw keys too
    let reply = get(&app, "/rest/forums?filter[name][eq]=Cats&include=messages").await;
    if let Some(message) = reply.body[0]["messages"].as_array().and_then(|m| m.first()) {
        assert!(uuid_like(message["id"].as_str().unwrap()), "{message}");
    }
}

fn uuid_like(s: &str) -> bool {
    s.len() == 36 && s.matches('-').count() == 4
}

// ---------------------------------------------------------------------------------------------
// Counts, pagination and search
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn total_count_is_opt_in() {
    let app = app().await;
    let fixture = "filter[name][in]=Cats,Dogs,Postgres";
    let plain = get(&app, &format!("/rest/forums?first=1&{fixture}")).await;
    assert_eq!(plain.status, StatusCode::OK);
    assert!(
        plain.header("x-total-count").is_none(),
        "no count unless asked for"
    );

    let counted = call(
        &app,
        "GET",
        &format!("/rest/forums?first=1&{fixture}"),
        &[("prefer", "count=exact")],
        None,
    )
    .await;
    assert_eq!(counted.header("x-total-count"), Some("3"));
    assert_eq!(counted.header("preference-applied"), Some("count=exact"));
    assert_eq!(counted.body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn search_takes_all_options_in_the_body() {
    let app = app().await;

    // options without a filter
    let reply = call(&app, "POST", "/rest/forums/search", &[], Some(json!({ "filter": { "name": { "in": ["Cats", "Dogs", "Postgres"] } }, "first": 1, "order": "name:desc" }))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(names(&reply), ["Postgres"]);

    // no body is the whole collection
    let reply = call(&app, "POST", "/rest/forums/search", &[], None).await;
    assert!(reply.body.as_array().unwrap().len() >= 3);

    // anything else is a 400
    for body in [
        json!([{ "name": { "eq": "Cats" } }]),
        json!({ "name": { "eq": "Cats" } }),
        json!({ "filter": { "name": { "eq": "Cats" } }, "orderBy": ["NAME_DESC"] }),
        json!({ "filter": { "name": { "eqq": "Cats" } } }),
        json!({ "filter": { "nope": { "eq": "Cats" } } }),
        json!({ "filter": "name" }),
        json!({ "first": "10" }),
        json!({ "first": 1001 }),
        json!({ "order": "bogus" }),
    ] {
        let reply = call(&app, "POST", "/rest/forums/search", &[], Some(body.clone())).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
    }
    let reply = call(&app, "POST", "/rest/forums/search?first=1", &[], None).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_pages_with_cursor_headers_that_keep_the_filter() {
    let app = app().await;
    let first = call(
        &app,
        "POST",
        "/rest/forums/search",
        &[],
        Some(json!({ "filter": { "name": { "like": "%o%" } }, "order": "name", "first": 1 })),
    )
    .await;
    assert_eq!(names(&first), ["Dogs"]);
    assert!(
        first.header("link").is_none(),
        "a search has no GET link: the filter is in the body"
    );
    let cursor = first
        .header("x-next-cursor")
        .expect("next cursor")
        .to_string();

    let second = call(
        &app,
        "POST",
        "/rest/forums/search",
        &[],
        Some(json!({ "filter": { "name": { "like": "%o%" } }, "order": "name", "first": 1, "after": cursor })),
    )
    .await;
    assert_eq!(
        names(&second),
        ["Postgres"],
        "the second page is still filtered"
    );
    assert!(second.header("x-next-cursor").is_none());
    assert!(second.header("x-previous-cursor").is_some());
}

#[tokio::test]
async fn relation_filters_work_in_search() {
    let app = app().await;
    let reply = call(
        &app,
        "POST",
        "/rest/forums/search",
        &[],
        Some(json!({ "filter": { "messages": { "none": { "body": { "like": "%" } } } }, "fields": ["name"] })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

// ---------------------------------------------------------------------------------------------
// Writes: row versions, upserts, nested keys, Prefer, Location
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn single_row_lifecycle_with_row_versions() {
    let app = app().await;

    let created = call(
        &app,
        "POST",
        "/rest/forums",
        &[],
        Some(json!({ "name": "Lifecycle" })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let id = created.body["id"].as_str().unwrap().to_string();
    assert!(uuid_like(&id), "the created row carries its raw key: {id}");
    assert_eq!(
        created.header("location"),
        Some(format!("/rest/forums/{id}").as_str())
    );
    let etag1 = created.header("etag").expect("etag").to_string();

    let item = format!("/rest/forums/{id}");
    // PATCH at the current version
    let updated = call(
        &app,
        "PATCH",
        &item,
        &[("if-match", &etag1)],
        Some(json!({ "name": "Lifecycle 2" })),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.body["name"], "Lifecycle 2");
    let etag2 = updated.header("etag").unwrap().to_string();
    assert_ne!(etag1, etag2);

    // the old version is stale for PATCH, PUT and DELETE
    for method in ["PATCH", "PUT", "DELETE"] {
        let body = (method != "DELETE").then(|| json!({ "name": "Stale" }));
        let reply = call(&app, method, &item, &[("if-match", &etag1)], body).await;
        reply.assert_problem(StatusCode::PRECONDITION_FAILED, "STALE_VERSION");
    }
    assert_eq!(
        get(&app, &item).await.body["name"],
        "Lifecycle 2",
        "stale writes changed nothing"
    );

    // weak tags and `*` are understood; two tags are not
    let weak = format!("W/{etag2}");
    let ok = call(
        &app,
        "PUT",
        &item,
        &[("if-match", &weak)],
        Some(json!({ "name": "Lifecycle 3" })),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    let star = call(
        &app,
        "PATCH",
        &item,
        &[("if-match", "*")],
        Some(json!({ "name": "Lifecycle 4" })),
    )
    .await;
    assert_eq!(star.status, StatusCode::OK);
    let two = call(
        &app,
        "PATCH",
        &item,
        &[("if-match", "\"1\", \"2\"")],
        Some(json!({ "name": "x" })),
    )
    .await;
    assert_eq!(two.status, StatusCode::BAD_REQUEST);

    // a representation read from the API can be sent back: the global ID of the key is accepted
    let global = graphpg::schema::SchemaBuilder::encode_global_id("Forum", &id);
    let back = call(
        &app,
        "PATCH",
        &item,
        &[],
        Some(json!({ "id": global, "name": "Lifecycle 5" })),
    )
    .await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);
    assert_eq!(back.body["id"], id);

    // minimal returns, and says so
    let minimal = call(
        &app,
        "PATCH",
        &item,
        &[("prefer", "return=minimal")],
        Some(json!({ "name": "Lifecycle 6" })),
    )
    .await;
    assert_eq!(minimal.status, StatusCode::NO_CONTENT);
    assert_eq!(minimal.header("preference-applied"), Some("return=minimal"));
    assert!(minimal.header("etag").is_some());

    let deleted = call(&app, "DELETE", &item, &[], None).await;
    assert_eq!(deleted.status, StatusCode::OK);
    call(&app, "DELETE", &item, &[], None)
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
    call(&app, "PATCH", &item, &[], Some(json!({ "name": "gone" })))
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
}

#[tokio::test]
async fn put_inserts_then_updates_and_the_url_wins() {
    let app = app().await;
    let id = "aaaaaaaa-0000-0000-0000-0000000000a1";
    let item = format!("/rest/forums/{id}");
    delete_forum(&app, id).await;

    let first = call(&app, "PUT", &item, &[], Some(json!({ "name": "Put 1" }))).await;
    assert_eq!(
        first.status,
        StatusCode::OK,
        "PUT answers 200 whether it inserted or updated: {}",
        first.body
    );
    assert_eq!(first.body["id"], id);
    let second = call(
        &app,
        "PUT",
        &item,
        &[],
        Some(json!({ "id": id, "name": "Put 2" })),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(get(&app, &item).await.body["name"], "Put 2");
    // `If-Match` demands an existing row: without one it is a 404, not an insert
    let missing = "/rest/forums/aaaaaaaa-0000-0000-0000-0000000000a2";
    let reply = call(
        &app,
        "PUT",
        missing,
        &[("if-match", "\"1\"")],
        Some(json!({ "name": "x" })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    delete_forum(&app, id).await;
}

#[tokio::test]
async fn bulk_create_returns_rows_with_raw_keys() {
    let app = app().await;
    let reply = call(
        &app,
        "POST",
        "/rest/forums",
        &[],
        Some(json!([{ "name": "Bulk A" }, { "name": "Bulk B" }])),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    let rows = reply.body.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(uuid_like(row["id"].as_str().unwrap()), "{row}");
        delete_forum(&app, row["id"].as_str().unwrap()).await;
    }
    let minimal = call(
        &app,
        "POST",
        "/rest/forums",
        &[("prefer", "return=minimal")],
        Some(json!([{ "name": "Bulk C" }])),
    )
    .await;
    assert_eq!(minimal.status, StatusCode::NO_CONTENT);
    assert_eq!(minimal.header("preference-applied"), Some("return=minimal"));
    let found = call(
        &app,
        "POST",
        "/rest/forums/search",
        &[],
        Some(json!({ "filter": { "name": { "eq": "Bulk C" } } })),
    )
    .await;
    for row in found.body.as_array().unwrap() {
        delete_forum(&app, row["id"].as_str().unwrap()).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn constraint_violations_keep_their_typed_code() {
    let app = app().await;
    let reply = call(&app, "POST", "/rest/forums", &[], Some(json!({}))).await;
    let body = reply.assert_problem(StatusCode::UNPROCESSABLE_ENTITY, "NAME_REQUIRED");
    assert_eq!(body["fields"], json!(["input.name"]));
    // masked in prod: a request ID, never the database's message
    assert!(body["requestId"].is_string());
    assert!(!body["detail"].as_str().unwrap().contains("violates"));
    assert_eq!(body["instance"], "/rest/forums");
}

#[tokio::test]
async fn unavailable_database_is_masked_in_prod_and_detailed_in_dev() {
    let mk = |mode| async move {
        let mut state = state().await;
        state.pool = create_pool("postgres://nobody:secret@127.0.0.1:1/nowhere", 1).unwrap();
        create_router(Arc::new(state.with_mode(mode)))
    };
    let prod = call(&mk(Mode::Prod).await, "GET", "/rest/forums", &[], None).await;
    let body = prod.assert_problem(StatusCode::SERVICE_UNAVAILABLE, "DATABASE_UNAVAILABLE");
    assert_eq!(body["detail"], "The database is unavailable");
    assert!(body["requestId"].is_string());
    assert!(!body.to_string().contains("127.0.0.1"));

    let dev = call(&mk(Mode::Dev).await, "GET", "/rest/forums", &[], None).await;
    let body = dev.assert_problem(StatusCode::SERVICE_UNAVAILABLE, "DATABASE_UNAVAILABLE");
    assert!(body["detail"].as_str().unwrap().len() > "The database is unavailable".len());
}

#[tokio::test]
async fn authentication_failures_are_problem_json_with_a_challenge() {
    let auth = AuthConfig {
        enabled: true,
        issuer: Some("https://issuer.test".to_string()),
        audience: vec!["graphpg".to_string()],
        roles: RolesConfig {
            authenticated: Some("postgres".to_string()),
            anonymous: None,
        },
        ..AuthConfig::default()
    };
    let authenticator =
        Authenticator::new(&auth, Some("a-test-secret-of-sufficient-length-0123456789"))
            .await
            .expect("valid auth configuration");
    let app = create_router(Arc::new(state().await.with_auth(authenticator)));

    for (method, uri) in [
        ("GET", "/rest/forums"),
        ("POST", "/rest/forums"),
        ("GET", "/rest/forums/x"),
        ("DELETE", "/rest/forums/x"),
    ] {
        let reply = call(&app, method, uri, &[], None).await;
        reply.assert_problem(StatusCode::UNAUTHORIZED, "UNAUTHENTICATED");
        assert!(reply.header("www-authenticate").is_some(), "{method} {uri}");
    }
    let reply = call(
        &app,
        "GET",
        "/rest/forums",
        &[("authorization", "Bearer garbage")],
        None,
    )
    .await;
    reply.assert_problem(StatusCode::UNAUTHORIZED, "UNAUTHENTICATED");
}

#[tokio::test]
async fn disabled_api_and_unknown_resources_are_problem_json() {
    let disabled = create_router(Arc::new(state().await.with_openapi_enabled(false)));
    assert_eq!(
        call(&disabled, "GET", "/rest/forums", &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let app = app().await;
    get(&app, "/rest/nothing_here")
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
    call(&app, "POST", "/rest/nothing_here", &[], Some(json!({})))
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
}

// ---------------------------------------------------------------------------------------------
// The OpenAPI document describes what the API does
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn openapi_matches_the_behaviour() {
    let state = state().await;
    let doc = (*state.current_openapi_document()).clone();
    let app = create_router(Arc::new(state));

    let forums = &doc["components"]["schemas"]["forums"];
    // rows are keyed by field name, and the key is a uuid (which is what rows carry)
    assert_eq!(forums["properties"]["id"]["format"], "uuid");
    assert!(forums["properties"].get("archivedAt").is_some(), "{forums}");
    assert!(forums["properties"].get("archived_at").is_none());
    assert!(forums["required"]
        .as_array()
        .unwrap()
        .contains(&json!("id")));

    // list parameters: the grammar the parser implements, and nothing else
    let params = doc["paths"]["/rest/forums"]["get"]["parameters"]
        .as_array()
        .unwrap();
    let mut documented: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
    documented.sort_unstable();
    assert_eq!(
        documented,
        [
            "Prefer", "after", "before", "fields", "filter", "first", "include", "last", "offset",
            "order"
        ]
    );
    let first = params.iter().find(|p| p["name"] == "first").unwrap();
    assert_eq!(first["schema"]["maximum"], 1000);
    let fields = params.iter().find(|p| p["name"] == "fields").unwrap();
    assert_eq!(
        fields["schema"]["items"]["enum"],
        json!(["id", "name", "archivedAt"])
    );
    // every documented parameter really is accepted
    for uri in [
        "/rest/forums?filter[name][eq]=Cats&order=-name&first=1&offset=0&fields=name&include=messages&last=1",
    ] {
        let status = get(&app, uri).await.status;
        assert_ne!(status, StatusCode::NOT_FOUND, "{uri}");
    }

    // headers: the count is documented as opt-in, and a list has no ETag
    let ok = &doc["paths"]["/rest/forums"]["get"]["responses"]["200"];
    assert!(ok["headers"].get("X-Total-Count").is_some());
    assert!(ok["headers"].get("ETag").is_none());

    // PUT answers 200 only; bulk create documents its array response
    let put = &doc["paths"]["/rest/forums/{key}"]["put"]["responses"];
    assert!(put.get("200").is_some() && put.get("201").is_none());
    let created = &doc["paths"]["/rest/forums"]["post"]["responses"]["201"]["content"]
        ["application/json"]["schema"];
    assert!(created["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["type"] == "array"));
    assert!(doc["paths"]["/rest/forums"]["post"]["responses"]
        .get("413")
        .is_some());

    // the search body is the options object, with the filter inside it
    let search = &doc["paths"]["/rest/forums/search"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"];
    assert_eq!(search["$ref"], "#/components/schemas/forumsSearchRequest");
    let request = &doc["components"]["schemas"]["forumsSearchRequest"];
    assert_eq!(request["additionalProperties"], false);
    for member in [
        "filter", "order", "first", "last", "after", "before", "offset", "fields", "include",
    ] {
        assert!(request["properties"].get(member).is_some(), "{member}");
    }
    let search_headers =
        &doc["paths"]["/rest/forums/search"]["post"]["responses"]["200"]["headers"];
    assert!(search_headers.get("X-Next-Cursor").is_some() && search_headers.get("Link").is_none());

    // relation filters are described
    let filter = &doc["components"]["schemas"]["forumsFilter"]["properties"];
    let messages = &filter["messages"]["properties"];
    for op in ["some", "every", "none"] {
        assert_eq!(
            messages[op]["$ref"], "#/components/schemas/messagesFilter",
            "{op}"
        );
    }
    assert!(doc["components"]["schemas"]["forumsFilter"]["properties"]["and"].is_object());

    // the bodies are the GraphQL input types, nested writes included
    let resolve = |schema: &Value| -> Value {
        match schema["$ref"].as_str() {
            Some(r) => doc["components"]["schemas"]
                [r.strip_prefix("#/components/schemas/").unwrap()]
            .clone(),
            None => schema.clone(),
        }
    };
    let update = resolve(&doc["components"]["schemas"]["forumsUpdateInput"]);
    assert_eq!(update["additionalProperties"], false);
    assert!(update["properties"].get("name").is_some());
    assert_eq!(
        update["properties"]["archivedAt"]["type"],
        json!(["string", "null"])
    );
    let create = resolve(&doc["components"]["schemas"]["forumsCreateInput"]);
    assert_eq!(create["required"], json!(["name"]));
    // a relation takes the nested write inputs
    let create_children = resolve(&create["properties"]["messages"]);
    let create_children = resolve(&create_children["anyOf"][0].clone());
    for member in ["create", "connect", "connectOrCreate"] {
        assert!(
            create_children["properties"].get(member).is_some(),
            "{member} in {create_children}"
        );
    }
    assert!(
        create_children["properties"].get("sync").is_none(),
        "sync is for updates"
    );
    let update_children = resolve(&resolve(&update["properties"]["messages"])["anyOf"][0].clone());
    for member in [
        "create",
        "connect",
        "disconnect",
        "set",
        "connectOrCreate",
        "update",
        "delete",
        "sync",
    ] {
        assert!(
            update_children["properties"].get(member).is_some(),
            "{member} in {update_children}"
        );
    }
    // ... which nest again, by reference (`Input.CreateMessageInput` refers back to forums)
    let child = resolve(&update_children["properties"]["create"]["items"].clone());
    assert!(child["properties"].get("body").is_some(), "{child}");
    assert_eq!(child["required"], json!(["body"]));
    assert!(child["properties"].get("forum").is_some());

    // an update can answer with the row as it was: documented as a preference and a schema
    let patch = &doc["paths"]["/rest/forums/{key}"]["patch"];
    let prefer = patch["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Prefer")
        .unwrap();
    assert!(prefer["description"].as_str().unwrap().contains("previous"));
    let ok = &patch["responses"]["200"]["content"]["application/json"]["schema"]["oneOf"];
    assert_eq!(ok[1]["$ref"], "#/components/schemas/forumsWithPrevious");
    let wrapper = &doc["components"]["schemas"]["forumsWithPrevious"];
    assert_eq!(wrapper["required"], json!(["row", "previous"]));
    let delete = &doc["paths"]["/rest/forums/{key}"]["delete"]["responses"]["200"]["content"]
        ["application/json"]["schema"];
    assert!(
        delete.get("oneOf").is_none(),
        "a delete has no previous row"
    );

    // problem responses are on every operation, `type` is about:blank
    for (path, item) in doc["paths"].as_object().unwrap() {
        if !path.starts_with("/rest/") {
            continue;
        }
        for (method, op) in item.as_object().unwrap() {
            for status in ["400", "401", "403", "500"] {
                assert!(
                    op["responses"].get(status).is_some(),
                    "{method} {path} lacks {status}"
                );
            }
        }
    }
    assert!(
        doc["components"]["schemas"]["ProblemDetails"]["properties"]["type"]["description"]
            .as_str()
            .unwrap()
            .contains("about:blank")
    );
}

#[tokio::test]
async fn omitted_operations_are_neither_served_nor_described() {
    let mut config = graphpg::config::Config::default();
    config.entities.insert(
        "forums".to_string(),
        graphpg::config::models::EntityConfig {
            omit_operations: vec![graphpg::config::models::Operation::Delete],
            ..Default::default()
        },
    );
    let base = state().await;
    let catalog = base.current_catalog();
    let doc = graphpg::openapi::OpenApiGenerator::generate_for(&catalog, &config, "app_public");
    assert!(doc["paths"]["/rest/forums/{key}"].get("delete").is_none());
    assert!(doc["paths"]["/rest/forums/{key}"].get("patch").is_some());

    let app = create_router(Arc::new(base.with_config(config)));
    let reply = call(&app, "DELETE", &format!("/rest/forums/{CATS}"), &[], None).await;
    reply.assert_problem(StatusCode::FORBIDDEN, "FORBIDDEN");
}
