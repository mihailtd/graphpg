//! Writes to a collection: update and delete by filter, bulk upsert, `missing`, and JSON Patch.
mod common;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

/// The tests share one table: one at a time.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Env {
    app: axum::Router,
    client: tokio_postgres::Client,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn count(&self) -> &str {
        self.headers
            .get("x-affected-count")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

async fn env(bulk_rows: Option<usize>) -> Env {
    env_with(bulk_rows, false).await
}

async fn env_with(bulk_rows: Option<usize>, require_filter: bool) -> Env {
    let guard = LOCK.lock().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE SCHEMA IF NOT EXISTS rest_coll;
             DROP TABLE IF EXISTS rest_coll.items;
             CREATE TABLE rest_coll.items (
                 id int PRIMARY KEY,
                 sku text NOT NULL UNIQUE,
                 name text NOT NULL,
                 note text DEFAULT 'none',
                 qty int DEFAULT 5,
                 settings jsonb
             );
             INSERT INTO rest_coll.items (id, sku, name, note, qty) VALUES
                 (1, 'T-1', 'one', 'a', 1), (2, 'T-2', 'two', 'a', 2),
                 (3, 'T-3', 'three', 'b', 3), (4, 'X-4', 'four', 'b', 4);",
        )
        .await
        .unwrap();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("rest_coll")
        .await
        .unwrap();
    let graphql = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        graphql,
        catalog,
        "rest_coll".to_string(),
        DebugOptions::default(),
    )
    .with_mode(graphpg::config::Mode::Dev);
    let mut state = state;
    if bulk_rows.is_some() {
        state.limits.bulk_rows = bulk_rows;
    }
    state.limits.require_filter = require_filter;
    Env {
        app: create_router(Arc::new(state)),
        client,
        _guard: guard,
    }
}

impl Env {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        extra: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            builder = builder.header("content-type", ct);
        }
        for (k, v) in extra {
            builder = builder.header(*k, *v);
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let res = self
            .app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn json(&self, method: &str, uri: &str, body: Option<Value>) -> Reply {
        self.call(method, uri, Some("application/json"), &[], body)
            .await
    }

    /// `(id, sku, name, note, qty)` of every row.
    async fn rows(&self) -> Vec<(i32, String, String, Option<String>, Option<i32>)> {
        self.client
            .query(
                "SELECT id, sku, name, note, qty FROM rest_coll.items ORDER BY id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
            .collect()
    }
}

#[tokio::test]
async fn patch_updates_the_rows_a_filter_matches() {
    let env = env(None).await;
    let r = env
        .json(
            "PATCH",
            "/rest/items?filter[sku][startsWith]=T-&filter[note][eq]=a",
            Some(json!({ "qty": 9 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.count(), "2");
    let mut ids: Vec<i64> = r
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, [1, 2]);
    let qty: Vec<_> = env.rows().await.into_iter().map(|r| r.4.unwrap()).collect();
    assert_eq!(qty, [9, 9, 3, 4]);

    // A merge patch is the same body, and `return=minimal` has none
    let r = env
        .call(
            "PATCH",
            "/rest/items?filter[id][eq]=3",
            Some("application/merge-patch+json"),
            &[("prefer", "return=minimal")],
            Some(json!({ "note": null })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(r.count(), "1");
    assert_eq!(env.rows().await[2].3, None);

    // A filter that matches nothing changes nothing
    let r = env
        .json(
            "PATCH",
            "/rest/items?filter[sku][eq]=nope",
            Some(json!({ "qty": 1 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.count(), "0");
    assert_eq!(r.body, json!([]));
}

#[tokio::test]
async fn a_write_without_a_filter_must_say_so() {
    let env = env(None).await;
    for method in ["PATCH", "DELETE"] {
        let r = env
            .json(method, "/rest/items", Some(json!({ "qty": 1 })))
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{method}: {}", r.body);
        assert_eq!(r.body["code"], "FILTER_REQUIRED");
    }
    assert_eq!(env.rows().await.len(), 4);

    let r = env
        .json(
            "PATCH",
            "/rest/items?unfiltered=true",
            Some(json!({ "qty": 0 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.count(), "4");
    let r = env
        .json("DELETE", "/rest/items?unfiltered=true", None)
        .await;
    assert_eq!((r.status, r.count()), (StatusCode::OK, "4"));
    assert!(env.rows().await.is_empty());

    // What a write does not take is an error, not ignored
    for uri in [
        "/rest/items?filter[id][eq]=1&order=id",
        "/rest/items?filter[id][eq]=1&first=1",
        "/rest/items?filter[id][eq]=1&include=x",
        "/rest/items?filter[id][eq]=1&nope=1",
        "/rest/items?filter[id][nope]=1",
        "/rest/items?unfiltered=maybe",
    ] {
        let r = env.json("DELETE", uri, None).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}: {}", r.body);
    }
}

#[tokio::test]
async fn delete_removes_the_rows_a_filter_matches() {
    let env = env(None).await;
    let r = env
        .json("DELETE", "/rest/items?filter[note][eq]=b", None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.count(), "2");
    assert_eq!(r.body.as_array().unwrap().len(), 2);
    assert_eq!(env.rows().await.len(), 2);
    let r = env
        .call(
            "DELETE",
            "/rest/items?filter[id][eq]=1",
            None,
            &[("prefer", "return=minimal")],
            None,
        )
        .await;
    assert_eq!((r.status, r.count()), (StatusCode::NO_CONTENT, "1"));
}

#[tokio::test]
async fn a_write_by_filter_is_bounded_by_the_bulk_limit() {
    let env = env(Some(3)).await;
    for (method, body) in [("PATCH", Some(json!({ "qty": 0 }))), ("DELETE", None)] {
        let r = env.json(method, "/rest/items?unfiltered=true", body).await;
        assert_eq!(
            r.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{method}: {}",
            r.body
        );
        assert_eq!(r.body["code"], "BULK_LIMIT_EXCEEDED");
        assert_eq!(
            (r.body["limit"].clone(), r.body["actual"].clone()),
            (json!(3), json!(4))
        );
    }
    // Rolled back: nothing changed
    let rows = env.rows().await;
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].4, Some(1));
    // Within the limit it goes through
    let r = env
        .json("DELETE", "/rest/items?filter[sku][startsWith]=T-", None)
        .await;
    assert_eq!((r.status, r.count()), (StatusCode::OK, "3"));
}

#[tokio::test]
async fn bulk_upsert_merges_or_ignores_by_a_declared_key() {
    let env = env(None).await;
    let rows = json!([
        { "id": 1, "sku": "T-1", "name": "ONE", "qty": 11 },
        { "id": 10, "sku": "T-10", "name": "ten", "qty": 10 }
    ]);
    // The primary key by default; a merge updates the columns the rows set
    let r = env.json("PUT", "/rest/items", Some(rows.clone())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.count(), "2");
    let all = env.rows().await;
    assert_eq!(
        (all[0].2.as_str(), all[0].4, all[0].3.as_deref()),
        ("ONE", Some(11), Some("a"))
    );
    assert_eq!(all.last().unwrap().1, "T-10");

    // Only the columns `update` names
    let r = env
        .json(
            "PUT",
            "/rest/items?conflict=sku&update=name",
            Some(json!([{ "id": 2, "sku": "T-2", "name": "TWO", "qty": 99 }])),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let two = &env.rows().await[1];
    assert_eq!((two.2.as_str(), two.4), ("TWO", Some(2)));

    // `ignore` leaves the existing row alone, and inserts the new ones
    let r = env
        .json(
            "PUT",
            "/rest/items?conflict=sku&resolution=ignore",
            Some(json!([
                { "id": 3, "sku": "T-3", "name": "THREE" },
                { "id": 20, "sku": "T-20", "name": "twenty" }
            ])),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let all = env.rows().await;
    assert_eq!(all[2].2, "three");
    assert!(all.iter().any(|r| r.1 == "T-20"));

    // The key must be a unique key; the body an array within the limit
    for (uri, body) in [
        (
            "/rest/items?conflict=name",
            json!([{ "id": 1, "sku": "z", "name": "n" }]),
        ),
        (
            "/rest/items?conflict=sku&update=nope",
            json!([{ "id": 1, "sku": "z", "name": "n" }]),
        ),
        (
            "/rest/items?resolution=ignore&update=name",
            json!([{ "id": 1, "sku": "z", "name": "n" }]),
        ),
        ("/rest/items?resolution=maybe", json!([])),
        ("/rest/items?nope=1", json!([])),
        ("/rest/items", json!([])),
        ("/rest/items", json!({ "id": 1 })),
    ] {
        let r = env.json("PUT", uri, Some(body)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}: {}", r.body);
    }
}

#[tokio::test]
async fn missing_columns_become_their_default_or_null() {
    let env = env(None).await;
    // Rows setting different columns: a missing one is the column's default
    let r = env
        .json(
            "POST",
            "/rest/items",
            Some(json!([
                { "id": 5, "sku": "m-5", "name": "five", "note": "set" },
                { "id": 6, "sku": "m-6", "name": "six", "qty": 60 }
            ])),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let all = env.rows().await;
    assert_eq!((all[4].3.as_deref(), all[4].4), (Some("set"), Some(5)));
    assert_eq!((all[5].3.as_deref(), all[5].4), (Some("none"), Some(60)));

    // `missing=null`: NULL in every row, for every column some row sets
    let r = env
        .json(
            "POST",
            "/rest/items?missing=null",
            Some(json!([
                { "id": 7, "sku": "m-7", "name": "seven", "note": "set" },
                { "id": 8, "sku": "m-8", "name": "eight", "qty": 80 }
            ])),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let all = env.rows().await;
    assert_eq!((all[6].3.as_deref(), all[6].4), (Some("set"), None));
    assert_eq!((all[7].3.as_deref(), all[7].4), (None, Some(80)));

    // The same choice for an upsert
    let r = env
        .json(
            "PUT",
            "/rest/items?missing=null",
            Some(json!([{ "id": 1, "sku": "T-1", "name": "uno" }, { "id": 2, "sku": "T-2", "name": "dos", "qty": 22 }])),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let all = env.rows().await;
    assert_eq!(all[0].4, None);
    assert_eq!(all[1].4, Some(22));

    let r = env
        .json(
            "POST",
            "/rest/items?missing=maybe",
            Some(json!([{ "id": 9, "sku": "m", "name": "n" }])),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_row_takes_a_json_patch() {
    let env = env(None).await;
    let patch = |ops: Value| async {
        env.call(
            "PATCH",
            "/rest/items/1",
            Some("application/json-patch+json"),
            &[],
            Some(ops),
        )
        .await
    };
    let r = patch(json!([
        { "op": "test", "path": "/name", "value": "one" },
        { "op": "replace", "path": "/name", "value": "uno" },
        { "op": "remove", "path": "/note" },
        { "op": "add", "path": "/settings", "value": { "theme": "dark" } },
        { "op": "add", "path": "/settings/size", "value": 3 }
    ]))
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["name"], "uno");
    assert_eq!(r.body["note"], Value::Null);
    assert_eq!(r.body["settings"], json!({ "theme": "dark", "size": 3 }));
    let row = &env.rows().await[0];
    assert_eq!((row.2.as_str(), row.3.clone()), ("uno", None));

    // A patch that changes nothing
    let r = patch(json!([{ "op": "replace", "path": "/name", "value": "uno" }])).await;
    assert_eq!(
        (r.status, r.body["name"].clone()),
        (StatusCode::OK, json!("uno"))
    );

    // One that does not apply is a 422; the row stays as it was
    let r = patch(json!([
        { "op": "replace", "path": "/name", "value": "changed" },
        { "op": "test", "path": "/name", "value": "nope" }
    ]))
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert_eq!(r.body["code"], "PATCH_FAILED");
    assert_eq!(env.rows().await[0].2, "uno");
    let r = patch(json!([{ "op": "replace", "path": "/nope", "value": 1 }])).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    // Changing the key through its own URL is refused like any other write
    let r = patch(json!([{ "op": "replace", "path": "/id", "value": 5 }])).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    // No such row
    let r = env
        .call(
            "PATCH",
            "/rest/items/404",
            Some("application/json-patch+json"),
            &[],
            Some(json!([])),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // A JSON Patch is for a row, not a collection
    let r = env
        .call(
            "PATCH",
            "/rest/items?filter[id][eq]=1",
            Some("application/json-patch+json"),
            &[],
            Some(json!([])),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{}", r.body);
}

#[tokio::test]
async fn options_name_the_methods_and_patch_formats() {
    let env = env(None).await;
    let r = env.call("OPTIONS", "/rest/items", None, &[], None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let allow = r.headers["allow"].to_str().unwrap();
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        assert!(allow.contains(method), "{allow}");
    }
    let r = env.call("OPTIONS", "/rest/items/1", None, &[], None).await;
    assert!(r.headers["accept-patch"]
        .to_str()
        .unwrap()
        .contains("application/json-patch+json"));
}

#[tokio::test]
async fn graphql_writes_by_filter_have_the_same_bound() {
    let env = env(Some(3)).await;
    let graphql = |query: &str| {
        let query = query.to_string();
        let env = &env;
        async move {
            env.json("POST", "/graphql", Some(json!({ "query": query })))
                .await
        }
    };
    let r = graphql("mutation { deleteItems(where: {}) { affectedCount } }").await;
    assert_eq!(
        r.body["errors"][0]["extensions"]["code"], "BULK_LIMIT_EXCEEDED",
        "{}",
        r.body
    );
    assert_eq!(r.body["errors"][0]["extensions"]["actual"], 4);
    assert_eq!(env.rows().await.len(), 4);

    // Without `affectedCount` selected the check still runs, and the answer has no trace of it
    let r = graphql("mutation { deleteItems(where: {}) { __typename } }").await;
    assert_eq!(
        r.body["errors"][0]["extensions"]["code"], "BULK_LIMIT_EXCEEDED",
        "{}",
        r.body
    );
    let r = graphql("mutation { updateItems(where: { sku: { startsWith: \"T-\" } }, set: { qty: 7 }) { __typename } }").await;
    assert_eq!(
        r.body["data"]["updateItems"],
        json!({ "__typename": "UpdateItemsPayload" }),
        "{}",
        r.body
    );
}

#[tokio::test]
async fn graphql_can_require_a_condition_for_writes_by_where() {
    let env = env_with(None, true).await;
    let graphql = |query: &str| {
        let query = query.to_string();
        let env = &env;
        async move {
            env.json("POST", "/graphql", Some(json!({ "query": query })))
                .await
        }
    };
    for query in [
        "mutation { deleteItems(where: {}) { affectedCount } }",
        "mutation { updateItems(where: {}, set: { qty: 0 }) { affectedCount } }",
    ] {
        let r = graphql(query).await;
        assert!(
            r.body["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("unfiltered"),
            "{}",
            r.body
        );
    }
    assert_eq!(env.rows().await.len(), 4);
    // With a condition, or when meant
    let r = graphql("mutation { updateItems(where: { sku: { startsWith: \"T-\" } }, set: { qty: 7 }) { affectedCount } }").await;
    assert_eq!(
        r.body["data"]["updateItems"]["affectedCount"], 3,
        "{}",
        r.body
    );
    let r = graphql(
        "mutation { updateItems(where: {}, set: { qty: 8 }, unfiltered: true) { affectedCount } }",
    )
    .await;
    assert_eq!(
        r.body["data"]["updateItems"]["affectedCount"], 4,
        "{}",
        r.body
    );
    // REST has its own guard, and says so with `unfiltered=true`
    let r = env.json("DELETE", "/rest/items", None).await;
    assert_eq!(r.body["code"], "FILTER_REQUIRED");
    let r = env
        .json("DELETE", "/rest/items?unfiltered=true", None)
        .await;
    assert_eq!((r.status, r.count()), (StatusCode::OK, "4"));
}
