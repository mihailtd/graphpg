//! Where NULLs go in an order: `<NAME>_ASC_NULLS_FIRST` / `<NAME>_DESC_NULLS_LAST` (GraphQL),
//! `order=rank:nullsfirst` (REST). Keyset pagination stays exact in every placement: paging
//! forward or backward yields PostgreSQL's own `ORDER BY`, no row skipped or repeated.
mod common;
use axum::body::Body;
use axum::http::Request;
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "null_ordering";

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn app() -> axum::Router {
    static FIXTURE: OnceCell<()> = OnceCell::const_new();
    FIXTURE
        .get_or_init(|| async {
            client()
                .await
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};
                     CREATE TABLE {SCHEMA}.items (id int PRIMARY KEY, rank int, label text NOT NULL);
                     INSERT INTO {SCHEMA}.items
                     SELECT i, CASE WHEN i % 3 = 0 THEN NULL ELSE i % 4 END, 'item ' || i
                     FROM generate_series(1, 11) i;"
                ))
                .await
                .unwrap();
        })
        .await;
    let client = client().await;
    let catalog = CatalogInspector::new(&client)
        .inspect_schema(SCHEMA)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    create_router(Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            schema,
            catalog,
            SCHEMA.to_string(),
            DebugOptions::default(),
        )
        .with_introspection(true),
    ))
}

/// The ids in PostgreSQL's order.
async fn expected(order: &str) -> Vec<i64> {
    client()
        .await
        .query(
            &format!("SELECT id FROM {SCHEMA}.items ORDER BY {order}"),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| i64::from(r.get::<_, i32>(0)))
        .collect()
}

async fn graphql(app: &axum::Router, query: &str) -> Value {
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let res = app.clone().oneshot(request).await.unwrap();
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(body.get("errors").is_none(), "{query}: {body}");
    body["data"].clone()
}

/// Every id, paging forward by `size` through `orderBy`.
async fn page_forward(app: &axum::Router, order: &str, size: usize) -> Vec<i64> {
    let (mut ids, mut after) = (Vec::new(), String::new());
    loop {
        let cursor = if after.is_empty() {
            String::new()
        } else {
            format!(", after: \"{after}\"")
        };
        let data = graphql(
            app,
            &format!("{{ itemsConnection(first: {size}, orderBy: [{order}]{cursor}) {{ edges {{ cursor node {{ databaseId }} }} pageInfo {{ hasNextPage }} }} }}"),
        )
        .await;
        let c = &data["itemsConnection"];
        for edge in c["edges"].as_array().unwrap() {
            ids.push(edge["node"]["databaseId"].as_i64().unwrap());
            after = edge["cursor"].as_str().unwrap().to_string();
        }
        if c["pageInfo"]["hasNextPage"] != true {
            return ids;
        }
    }
}

/// Every id, paging backward by `size` from the end, put back in order.
async fn page_backward(app: &axum::Router, order: &str, size: usize) -> Vec<i64> {
    let (mut pages, mut before) = (Vec::new(), String::new());
    loop {
        let cursor = if before.is_empty() {
            String::new()
        } else {
            format!(", before: \"{before}\"")
        };
        let data = graphql(
            app,
            &format!("{{ itemsConnection(last: {size}, orderBy: [{order}]{cursor}) {{ edges {{ cursor node {{ databaseId }} }} pageInfo {{ hasPreviousPage }} }} }}"),
        )
        .await;
        let c = &data["itemsConnection"];
        let edges = c["edges"].as_array().unwrap();
        let page: Vec<i64> = edges
            .iter()
            .map(|e| e["node"]["databaseId"].as_i64().unwrap())
            .collect();
        if let Some(first) = edges.first() {
            before = first["cursor"].as_str().unwrap().to_string();
        }
        pages.push(page);
        if c["pageInfo"]["hasPreviousPage"] != true {
            return pages.into_iter().rev().flatten().collect();
        }
    }
}

#[tokio::test]
async fn every_null_placement_pages_exactly_both_ways() {
    let app = app().await;
    for (graphql_order, sql_order) in [
        ("RANK_ASC, ID_ASC", "rank ASC, id ASC"),
        ("RANK_DESC, ID_ASC", "rank DESC, id ASC"),
        (
            "RANK_ASC_NULLS_FIRST, ID_ASC",
            "rank ASC NULLS FIRST, id ASC",
        ),
        (
            "RANK_DESC_NULLS_LAST, ID_ASC",
            "rank DESC NULLS LAST, id ASC",
        ),
        (
            "RANK_DESC_NULLS_LAST, ID_DESC",
            "rank DESC NULLS LAST, id DESC",
        ),
        (
            "RANK_ASC_NULLS_FIRST, ID_DESC",
            "rank ASC NULLS FIRST, id DESC",
        ),
    ] {
        let want = expected(sql_order).await;
        for size in [1, 2, 4] {
            assert_eq!(
                page_forward(&app, graphql_order, size).await,
                want,
                "forward {graphql_order} by {size}"
            );
            assert_eq!(
                page_backward(&app, graphql_order, size).await,
                want,
                "backward {graphql_order} by {size}"
            );
        }
    }
}

#[tokio::test]
async fn only_nullable_options_offer_a_placement() {
    let app = app().await;
    let data = graphql(
        &app,
        r#"{ __type(name: "ItemOrderBy") { enumValues { name } } }"#,
    )
    .await;
    let names: Vec<&str> = data["__type"]["enumValues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"RANK_ASC_NULLS_FIRST"), "{names:?}");
    assert!(names.contains(&"RANK_DESC_NULLS_LAST"), "{names:?}");
    // The defaults keep their names; a column without NULLs has no placement
    assert!(!names.contains(&"RANK_ASC_NULLS_LAST"), "{names:?}");
    assert!(!names.contains(&"LABEL_ASC_NULLS_FIRST"), "{names:?}");
}

async fn rest(app: &axum::Router, uri: &str) -> (u16, Value, axum::http::HeaderMap) {
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (res.status().as_u16(), res.headers().clone());
    let body = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes())
        .unwrap_or(Value::Null);
    (status, body, headers)
}

#[tokio::test]
async fn rest_places_nulls_and_pages_with_them() {
    let app = app().await;
    for (order, sql_order) in [
        ("rank:nullsfirst,id", "rank ASC NULLS FIRST, id"),
        ("-rank:nullslast,id", "rank DESC NULLS LAST, id"),
        ("rank:desc:nullslast,-id", "rank DESC NULLS LAST, id DESC"),
        ("RANK_ASC_NULLS_FIRST,ID_ASC", "rank ASC NULLS FIRST, id"),
    ] {
        let want = expected(sql_order).await;
        let (mut ids, mut uri) = (Vec::new(), format!("/rest/items?order={order}&first=3"));
        loop {
            let (status, body, headers) = rest(&app, &uri).await;
            assert_eq!(status, 200, "{uri}: {body}");
            ids.extend(
                body.as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].as_i64().unwrap()),
            );
            // Follow `Link: <…>; rel="next"`, as a client does
            // (URLs may hold commas: split on the `<…>` brackets, not on `,`)
            let next = headers
                .get_all("link")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split('<').skip(1))
                .find(|l| l.contains("rel=\"next\""))
                .and_then(|l| l.split_once('>'))
                .map(|(url, _)| url.to_string());
            match next {
                Some(next) => uri = next,
                None => break,
            }
        }
        assert_eq!(ids, want, "{order}");
    }
    let (status, _, _) = rest(&app, "/rest/items?order=natural:nullsfirst").await;
    assert_eq!(status, 400);
}
