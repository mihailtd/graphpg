//! The `timescaledb` integration (docs/extensions.md): hypertables' estimated counts from
//! TimescaleDB (their own statistics are their chunks'), what hypertables and continuous
//! aggregates are, and advice. Runs on the test image; skipped without TimescaleDB.
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
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "tsdb";

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// `None` (skip) without TimescaleDB.
async fn fixture() -> Option<()> {
    static FIXTURE: OnceCell<bool> = OnceCell::const_new();
    let ok = *FIXTURE
        .get_or_init(|| async {
            if !common::with_extensions(&["timescaledb"]).await {
                return false;
            }
            client()
                .await
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};
                     CREATE TABLE {SCHEMA}.readings (device int NOT NULL, time timestamptz NOT NULL, value float8);
                     SELECT create_hypertable('{SCHEMA}.readings', by_range('time', interval '1 day'));
                     INSERT INTO {SCHEMA}.readings
                       SELECT i % 10, timestamptz '2026-01-01' + (i || ' minutes')::interval, i
                       FROM generate_series(1, 100000) i;
                     ANALYZE {SCHEMA}.readings;
                     CREATE MATERIALIZED VIEW {SCHEMA}.readings_hourly WITH (timescaledb.continuous) AS
                       SELECT time_bucket('1 hour', time) AS bucket, device, avg(value) AS avg_value
                       FROM {SCHEMA}.readings GROUP BY 1, 2 WITH NO DATA;"
                ))
                .await
                .unwrap();
            true
        })
        .await;
    ok.then_some(())
}

async fn app(config: &str) -> axum::Router {
    let config = Config::from_yaml(config).unwrap();
    let catalog = CatalogInspector::new(&client().await)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    create_router(Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            schema,
            catalog,
            SCHEMA.to_string(),
            DebugOptions {
                sql: true,
                explain: false,
            },
        )
        .with_config(config)
        .with_introspection(true),
    ))
}

async fn graphql(app: &axum::Router, query: &str) -> Value {
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let res = app.clone().oneshot(request).await.unwrap();
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

/// The readings' primary key, so connections have stable cursors.
const KEYED: &str = "entities:\n  readings:\n    primary_key: [device, time]\n";

#[tokio::test]
async fn hypertable_estimates_come_from_timescaledb() {
    if fixture().await.is_none() {
        return;
    }
    let app = app(KEYED).await;
    let reply = graphql(
        &app,
        "{ readingsConnection { totalCount(estimate: true) } exact: readingsConnection { totalCount } }",
    )
    .await;
    let estimate = reply["data"]["readingsConnection"]["totalCount"]
        .as_i64()
        .unwrap();
    assert!((95_000..=105_000).contains(&estimate), "{reply}");
    assert_eq!(reply["data"]["exact"]["totalCount"], 100_000);
    assert!(
        reply["extensions"]["sql"]
            .to_string()
            .contains("approximate_row_count"),
        "{}",
        reply["extensions"]["sql"]
    );

    // REST's planned count too
    let res = app
        .clone()
        .oneshot(
            Request::get("/rest/readings?first=1")
                .header("prefer", "count=planned")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let total: i64 = res.headers()["x-total-count"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((95_000..=105_000).contains(&total), "{total}");
}

#[tokio::test]
async fn hypertables_and_continuous_aggregates_are_described() {
    if fixture().await.is_none() {
        return;
    }
    let on = app(KEYED).await;
    let reply = graphql(
        &on,
        r#"{ h: __type(name: "Reading") { description } c: __type(name: "ReadingsHourly") { description } }"#,
    )
    .await;
    let hypertable = reply["data"]["h"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        hypertable.contains("TimescaleDB hypertable, partitioned by `time`, in chunks of 1 day"),
        "{reply}"
    );
    let aggregate = reply["data"]["c"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(aggregate.contains("continuous aggregate"), "{reply}");

    // Off: a table like any other, counted from table statistics or exactly
    let off = app(&format!(
        "{KEYED}extensions:\n  integrations:\n    timescaledb: off\n"
    ))
    .await;
    let reply = graphql(
        &off,
        r#"{ __type(name: "Reading") { description } readingsConnection { totalCount(estimate: true) } }"#,
    )
    .await;
    assert!(reply["data"]["__type"]["description"].is_null(), "{reply}");
    assert!(!reply["extensions"]["sql"]
        .to_string()
        .contains("approximate_row_count"));
}

#[tokio::test]
async fn advice_for_a_hypertable_without_a_key_names_its_time_column() {
    if fixture().await.is_none() {
        return;
    }
    let db = client().await;
    let inspector = CatalogInspector::new(&db);
    let catalog = inspector.inspect_schema(SCHEMA).await.unwrap();
    let advice = inspector.advise(SCHEMA, &catalog).await.unwrap();
    assert!(
        advice.iter().any(|a| a
            .message
            .contains("hypertable `readings` has no primary key")
            && a.message.contains("must include `time`")),
        "{advice:#?}"
    );
}
