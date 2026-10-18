//! `Range: items=A-B` on lists: `206 Partial Content`, `Content-Range`, `416`, and how the total
//! is known only when `Prefer: count=exact` asks for it.
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
    let mut limits = graphpg::config::models::LimitsConfig::default();
    if threshold.is_some() {
        limits.count_threshold = threshold;
    }
    let state = state.with_limits(limits);
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

#[tokio::test]
async fn a_range_is_a_page_with_a_content_range() {
    let env = env().await;
    env.client
        .batch_execute(
            "DELETE FROM rest_extras.people WHERE id BETWEEN 9301 AND 9305;
             INSERT INTO rest_extras.people VALUES (9301,'a'),(9302,'b'),(9303,'c'),(9304,'d'),(9305,'e');",
        )
        .await
        .unwrap();
    let list = "/rest/people?filter[id][gte]=9301&filter[id][lte]=9305&order=id&fields=id";
    let ids = |r: &Reply| -> Vec<i64> {
        r.json()
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect()
    };

    // every list says it takes ranges
    let plain = env.call("GET", list, &[], vec![]).await;
    assert_eq!(plain.status, StatusCode::OK);
    assert_eq!(plain.header("accept-ranges"), "items");
    assert!(plain.header("content-range").is_empty());

    // the total is `*` unless a count is asked for
    let first = env
        .call("GET", list, &[("range", "items=0-1")], vec![])
        .await;
    assert_eq!(
        first.status,
        StatusCode::PARTIAL_CONTENT,
        "{}",
        first.text()
    );
    assert_eq!(ids(&first), [9301, 9302]);
    assert_eq!(first.header("content-range"), "items 0-1/*");
    let counted = env
        .call(
            "GET",
            list,
            &[("range", "items=1-2"), ("prefer", "count=exact")],
            vec![],
        )
        .await;
    assert_eq!(ids(&counted), [9302, 9303]);
    assert_eq!(counted.header("content-range"), "items 1-2/5");

    // an open end runs to the last row
    let rest = env
        .call(
            "GET",
            list,
            &[("range", "items=3-"), ("prefer", "count=exact")],
            vec![],
        )
        .await;
    assert_eq!(ids(&rest), [9304, 9305]);
    assert_eq!(rest.header("content-range"), "items 3-4/5");
    assert_eq!(rest.status, StatusCode::PARTIAL_CONTENT);

    // the whole list is a `200`
    let all = env
        .call(
            "GET",
            list,
            &[("range", "items=0-99"), ("prefer", "count=exact")],
            vec![],
        )
        .await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(ids(&all).len(), 5);
    assert_eq!(all.header("content-range"), "items 0-4/5");

    // HEAD carries the same headers
    let head = env
        .call("HEAD", list, &[("range", "items=0-1")], vec![])
        .await;
    assert_eq!(head.status, StatusCode::PARTIAL_CONTENT);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-range"), "items 0-1/*");

    // ranges that are ignored: another unit, a suffix
    for ignored in ["bytes=0-1", "items=-2"] {
        let res = env.call("GET", list, &[("range", ignored)], vec![]).await;
        assert_eq!(res.status, StatusCode::OK, "{ignored}");
        assert_eq!(ids(&res).len(), 5);
    }

    env.client
        .batch_execute("DELETE FROM rest_extras.people WHERE id BETWEEN 9301 AND 9305")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_range_that_cannot_be_served_is_a_416() {
    let env = env().await;
    env.client
        .batch_execute(
            "DELETE FROM rest_extras.people WHERE id BETWEEN 9311 AND 9312;
             INSERT INTO rest_extras.people VALUES (9311,'a'),(9312,'b');",
        )
        .await
        .unwrap();
    let list = "/rest/people?filter[id][gte]=9311&filter[id][lte]=9312&order=id";
    // past the end: the size of the whole is in `Content-Range`
    let past = env
        .call("GET", list, &[("range", "items=10-12")], vec![])
        .await;
    assert_eq!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(past.header("content-range"), "items */2");
    assert!(past
        .header("content-type")
        .starts_with("application/problem+json"));
    // it ends before it starts
    let backwards = env
        .call("GET", list, &[("range", "items=5-2")], vec![])
        .await;
    assert_eq!(backwards.status, StatusCode::RANGE_NOT_SATISFIABLE);
    // it does not mix with the other ways to page
    let mixed = env
        .call(
            "GET",
            &format!("{list}&first=1"),
            &[("range", "items=0-0")],
            vec![],
        )
        .await;
    assert_eq!(mixed.status, StatusCode::BAD_REQUEST);
    env.client
        .batch_execute("DELETE FROM rest_extras.people WHERE id BETWEEN 9311 AND 9312")
        .await
        .unwrap();
}

#[tokio::test]
async fn planned_and_estimated_counts_over_rest() {
    let env = env().await;
    env.client
        .batch_execute(
            "DELETE FROM rest_extras.people WHERE id BETWEEN 9321 AND 9325;
             INSERT INTO rest_extras.people VALUES (9321,'a'),(9322,'b'),(9323,'c'),(9324,'d'),(9325,'e');
             ANALYZE rest_extras.people;",
        )
        .await
        .unwrap();
    let uri = "/rest/people?filter[id][gte]=9321&filter[id][lte]=9325&order=id";

    // 1. count=planned returns estimate and headers
    let planned = env
        .call("GET", uri, &[("prefer", "count=planned")], vec![])
        .await;
    assert_eq!(planned.status, StatusCode::OK);
    assert_eq!(planned.header("preference-applied"), "count=planned");
    assert_eq!(planned.header("x-total-count-estimate"), "true");
    let total: i64 = planned
        .header("x-total-count")
        .parse()
        .expect("total count is integer");
    assert!(total > 0);

    // 2. count=planned with Range sets Content-Range with estimated total
    let range_planned = env
        .call(
            "GET",
            uri,
            &[("range", "items=0-1"), ("prefer", "count=planned")],
            vec![],
        )
        .await;
    assert_eq!(range_planned.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(range_planned.header("preference-applied"), "count=planned");
    assert_eq!(range_planned.header("x-total-count-estimate"), "true");
    assert!(range_planned
        .header("content-range")
        .starts_with("items 0-1/"));
    assert!(!range_planned.header("content-range").ends_with("/*"));

    // 3. count=estimated below threshold (5 rows <= 10,000 threshold) returns exact count
    let estimated_below = env
        .call("GET", uri, &[("prefer", "count=estimated")], vec![])
        .await;
    assert_eq!(estimated_below.status, StatusCode::OK);
    assert_eq!(
        estimated_below.header("preference-applied"),
        "count=estimated"
    );
    // below threshold: exact count, so x-total-count-estimate is not set
    assert_eq!(estimated_below.header("x-total-count-estimate"), "");
    assert_eq!(estimated_below.header("x-total-count"), "5");

    // 4. count=exact returns exact count without estimate header
    let exact = env
        .call("GET", uri, &[("prefer", "count=exact")], vec![])
        .await;
    assert_eq!(exact.status, StatusCode::OK);
    assert_eq!(exact.header("preference-applied"), "count=exact");
    assert_eq!(exact.header("x-total-count-estimate"), "");
    assert_eq!(exact.header("x-total-count"), "5");

    env.client
        .batch_execute("DELETE FROM rest_extras.people WHERE id BETWEEN 9321 AND 9325")
        .await
        .unwrap();
}

#[tokio::test]
async fn estimated_count_above_the_threshold_is_the_planner_estimate() {
    let env = env_with_threshold(Some(1)).await;
    env.client
        .batch_execute(
            "DELETE FROM rest_extras.people WHERE id BETWEEN 9331 AND 9335;
             INSERT INTO rest_extras.people VALUES (9331,'a'),(9332,'b'),(9333,'c'),(9334,'d'),(9335,'e');
             ANALYZE rest_extras.people;",
        )
        .await
        .unwrap();
    let uri = "/rest/people?filter[id][gte]=9331&filter[id][lte]=9335&order=id&first=2";

    // The estimate is above the threshold of 1: it is the answer, and says so
    let estimated = env
        .call("GET", uri, &[("prefer", "count=estimated")], vec![])
        .await;
    assert_eq!(estimated.status, StatusCode::OK);
    assert_eq!(estimated.header("preference-applied"), "count=estimated");
    assert_eq!(estimated.header("x-total-count-estimate"), "true");
    assert!(estimated.header("x-total-count").parse::<i64>().unwrap() > 1);

    // A range past the end names the exact size, whichever count was asked for
    let past = env
        .call(
            "GET",
            "/rest/people?filter[id][gte]=9331&filter[id][lte]=9335&order=id",
            &[("range", "items=100-"), ("prefer", "count=planned")],
            vec![],
        )
        .await;
    assert_eq!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(past.header("content-range"), "items */5");

    env.client
        .batch_execute("DELETE FROM rest_extras.people WHERE id BETWEEN 9331 AND 9335")
        .await
        .unwrap();
}
