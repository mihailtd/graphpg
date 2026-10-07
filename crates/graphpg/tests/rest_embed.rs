//! Controls inside an embedded relation: `fields[posts]`, `filter[posts][…]`, `order[posts]`,
//! `first[posts]`, `offset[posts]`, `inner` and `anti`, and ordering by a to-one relation's column.
#![allow(dead_code)]
mod common;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

struct Env {
    app: axum::Router,
    client: tokio_postgres::Client,
}

async fn env() -> Env {
    env_with_threshold(None).await
}

async fn env_with_threshold(threshold: Option<u64>) -> Env {
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
    let graphql = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        graphql,
        catalog,
        "rest_extras".to_string(),
        DebugOptions::default(),
    );
    let mut state = state;
    if threshold.is_some() {
        state.limits.count_threshold = threshold;
    }
    Env {
        app: create_router(Arc::new(state)),
        client,
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("{}", self.text()))
    }
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

impl Env {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let res = self
            .app
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }
}

fn names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_string())
        .collect()
}

/// The tests share the rows they seed: one at a time.
static SEEDED: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn seed(env: &Env) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = SEEDED.lock().await;
    env.client
        .batch_execute(
            "DELETE FROM rest_extras.sailors WHERE id BETWEEN 7101 AND 7110;
             DELETE FROM rest_extras.crews WHERE id BETWEEN 7101 AND 7103;
             INSERT INTO rest_extras.crews VALUES (7101,'alpha'),(7102,'bravo'),(7103,'charlie');
             INSERT INTO rest_extras.sailors VALUES
               (7101,7101,'ann'),(7102,7101,'bob'),(7103,7101,'cy'),(7104,7102,'dee'),(7105,7102,'bea');",
        )
        .await
        .unwrap();
    guard
}

#[tokio::test]
async fn an_embedded_relation_takes_its_own_controls() {
    let env = env().await;
    let _rows = seed(&env).await;
    let base = "/rest/crews?filter[id][gte]=7101&filter[id][lte]=7103&order=id&fields=name&include=sailors";

    // A sparse fieldset of the embedded rows
    let r = env
        .call("GET", &format!("{base}&fields[sailors]=name"), &[], vec![])
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let rows = r.json();
    assert_eq!(rows[0]["sailors"][0], serde_json::json!({ "name": "ann" }));
    assert_eq!(rows[2]["sailors"], serde_json::json!([]));

    // A filter keeps every parent and only the matching embedded rows
    let r = env
        .call(
            "GET",
            &format!("{base}&filter[sailors][name][startsWith]=b&fields[sailors]=name"),
            &[],
            vec![],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let rows = r.json();
    assert_eq!(rows.as_array().unwrap().len(), 3);
    assert_eq!(names(&rows[0]["sailors"]), ["bob"]);
    assert_eq!(names(&rows[1]["sailors"]), ["bea"]);

    // Order, a page of them
    let r = env
        .call(
            "GET",
            &format!("{base}&order[sailors]=-name&first[sailors]=2&fields[sailors]=name"),
            &[],
            vec![],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(names(&r.json()[0]["sailors"]), ["cy", "bob"]);
    let r = env
        .call("GET", &format!("{base}&order[sailors]=name&first[sailors]=1&offset[sailors]=1&fields[sailors]=name"), &[], vec![])
        .await;
    assert_eq!(names(&r.json()[0]["sailors"]), ["bob"]);

    // A single row takes them too
    let r = env
        .call(
            "GET",
            "/rest/crews/7101?include=sailors&order[sailors]=-name&fields[sailors]=name",
            &[],
            vec![],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(names(&r.json()["sailors"]), ["cy", "bob", "ann"]);

    // And a search body
    let r = env
        .call(
            "POST",
            "/rest/crews/search",
            &[("content-type", "application/json")],
            serde_json::json!({
                "filter": { "id": { "eq": 7101 } },
                "include": ["sailors"],
                "embed": { "sailors": { "fields": ["name"], "order": "-name", "first": 2 } }
            })
            .to_string()
            .into_bytes(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(names(&r.json()[0]["sailors"]), ["cy", "bob"]);
}

#[tokio::test]
async fn inner_and_anti_keep_parents_by_their_embedded_rows() {
    let env = env().await;
    let _rows = seed(&env).await;
    let base = "/rest/crews?filter[id][gte]=7101&filter[id][lte]=7103&order=id&fields=name&include=sailors&fields[sailors]=name";
    let crews = |r: &Reply| -> Vec<String> { names(&r.json()) };

    let r = env
        .call("GET", &format!("{base}&inner=sailors"), &[], vec![])
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(crews(&r), ["alpha", "bravo"]);

    // Inner on a filtered relation: parents with a matching row
    let r = env
        .call(
            "GET",
            &format!("{base}&inner=sailors&filter[sailors][name][eq]=bea"),
            &[],
            vec![],
        )
        .await;
    assert_eq!(crews(&r), ["bravo"]);
    assert_eq!(names(&r.json()[0]["sailors"]), ["bea"]);

    let r = env
        .call("GET", &format!("{base}&anti=sailors"), &[], vec![])
        .await;
    assert_eq!(crews(&r), ["charlie"]);

    // The parent filters through `some` still work, next to embedded ones
    let r = env
        .call(
            "GET",
            &format!("{base}&filter[sailors][some][name][eq]=cy"),
            &[],
            vec![],
        )
        .await;
    assert_eq!(crews(&r), ["alpha"]);
    assert_eq!(r.json()[0]["sailors"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn controls_need_their_relation_and_the_right_kind() {
    let env = env().await;
    let _rows = seed(&env).await;
    for uri in [
        // not in `include`
        "/rest/crews?first[sailors]=1",
        "/rest/crews?fields[sailors]=name",
        // unknown relation, unknown column, bad values
        "/rest/crews?include=sailors&first[nope]=1",
        "/rest/crews?include=sailors&fields[sailors]=nope",
        "/rest/crews?include=sailors&order[sailors]=nope",
        "/rest/crews?include=sailors&filter[sailors][nope][eq]=1",
        "/rest/crews?include=sailors&first[sailors]=0",
        "/rest/crews?inner=nope",
        // to-one relations have no rows to page
        "/rest/sailors?include=crew&first[crew]=1",
    ] {
        let r = env.call("GET", uri, &[], vec![]).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}: {}", r.text());
    }
    // `inner` is for lists
    let r = env
        .call("GET", "/rest/crews/7101?inner=sailors", &[], vec![])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rows_are_ordered_by_a_column_of_a_to_one_relation() {
    let env = env().await;
    let _rows = seed(&env).await;
    let uri = "/rest/sailors?filter[id][gte]=7101&filter[id][lte]=7105&fields=name&order=crew.name:desc,name";
    let r = env.call("GET", uri, &[], vec![]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(names(&r.json()), ["bea", "dee", "ann", "bob", "cy"]);
    let r = env
        .call("GET", "/rest/sailors?order=crew.nope", &[], vec![])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}
