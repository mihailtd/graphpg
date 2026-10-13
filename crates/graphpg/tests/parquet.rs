//! Parquet files through pg_duckdb (docs/foreign-data.md): loaded into a materialized view,
//! they are served and joined like a table; a plain view over `read_parquet` cannot be served
//! (DuckDB runs the whole statement, and does not parse graphpg's SQL/JSON).
//!
//! Runs against `DUCKDB_DATABASE_URL` (the `pgduckdb/pgduckdb:18` image, a service of the CI
//! test job): pg_duckdb cannot share the main test image, where its threads crash the server
//! alongside pgrx extensions. Skipped without it.
mod common;
use axum::body::Body;
use axum::http::Request;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const CONFIG: &str = "
entities:
  sales:
    primary_key: [id]
    foreign_keys:
      sales_customer:
        columns: [customer_id]
        references: { table: customers }
        field_name: customer
        reverse_field_name: sales
  sales_live:
    primary_key: [id]
";

async fn app() -> Option<axum::Router> {
    let Ok(url) = std::env::var("DUCKDB_DATABASE_URL") else {
        eprintln!("SKIPPED: set DUCKDB_DATABASE_URL to a PostgreSQL with pg_duckdb (docker.io/pgduckdb/pgduckdb:18-v1.1.1)");
        return None;
    };
    let (db, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    db.batch_execute(
        "SELECT pg_advisory_lock(7461027);
         CREATE EXTENSION IF NOT EXISTS pg_duckdb;
         DROP SCHEMA IF EXISTS parquet_app CASCADE;
         CREATE SCHEMA parquet_app;
         SET search_path = parquet_app, public;
         CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
         INSERT INTO customers VALUES (1, 'Acme'), (2, 'Globex');",
    )
    .await
    .unwrap();
    // The file, written by DuckDB
    db.batch_execute(
        "SELECT * FROM duckdb.raw_query($$
           COPY (SELECT * FROM (VALUES (1, 1, 10.5), (2, 1, 20.0), (3, 2, 7.25)) t(id, customer_id, total))
           TO '/tmp/graphpg_sales.parquet' (FORMAT parquet)
         $$);
         CREATE MATERIALIZED VIEW sales AS
           SELECT r['id']::int AS id, r['customer_id']::int AS customer_id, r['total']::float8 AS total
           FROM read_parquet('/tmp/graphpg_sales.parquet') r;
         CREATE UNIQUE INDEX ON sales (id);
         REFRESH MATERIALIZED VIEW CONCURRENTLY sales;
         CREATE VIEW sales_live AS
           SELECT r['id']::int AS id, r['total']::float8 AS total
           FROM read_parquet('/tmp/graphpg_sales.parquet') r;
         RESET search_path;
         SELECT pg_advisory_unlock(7461027);",
    )
    .await
    .unwrap();
    let config = Config::from_yaml(CONFIG).unwrap();
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config("parquet_app", &config)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    Some(create_router(Arc::new(
        ServerState::new(
            create_pool(&url, 2).unwrap(),
            schema,
            catalog,
            "parquet_app".to_string(),
            DebugOptions::default(),
        )
        .with_config(config)
        .with_introspection(true),
    )))
}

async fn graphql(app: &axum::Router, query: &str) -> Value {
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let res = app.clone().oneshot(request).await.unwrap();
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn a_materialized_view_over_parquet_is_served_and_joined() {
    let Some(app) = app().await else {
        return;
    };
    let reply = graphql(
        &app,
        "{ customer(id: 1) { name sales { databaseId total } }
           sale(id: 3) { total customer { name } }
           salesConnection { aggregates { sum { total } } } }",
    )
    .await;
    assert!(reply.get("errors").is_none(), "{reply}");
    let data = &reply["data"];
    let mut totals: Vec<f64> = data["customer"]["sales"]
        .as_array()
        .unwrap_or_else(|| panic!("{reply}"))
        .iter()
        .map(|s| s["total"].as_f64().unwrap())
        .collect();
    totals.sort_by(f64::total_cmp);
    assert_eq!(totals, [10.5, 20.0], "{reply}");
    assert_eq!(data["sale"]["customer"]["name"], "Globex", "{reply}");
    assert_eq!(
        data["salesConnection"]["aggregates"]["sum"]["total"], 37.75,
        "{reply}"
    );

    // A plain view over the file: DuckDB runs the statement and rejects its SQL/JSON (the
    // guide says so; if this starts passing, the guide can relax)
    let reply = graphql(&app, "{ salesLive { total } }").await;
    assert_eq!(
        reply["errors"][0]["extensions"]["code"], "INTERNAL_SERVER_ERROR",
        "{reply}"
    );
}
