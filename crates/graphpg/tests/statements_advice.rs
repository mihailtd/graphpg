//! `graphpg advise --statements` (docs/extensions.md): graphpg's operations, the most
//! expensive first, from `pg_stat_statements`, with the indexes their plans lack, checked with
//! hypopg. Runs on the test image; skipped without the extensions.
mod common;
use axum::body::Body;
use axum::http::Request;
use graphpg::catalog::statements::{top_statements, StatementsError};
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
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
async fn slow_operations_first_with_the_index_they_lack() {
    if !common::with_extensions(&["pg_stat_statements", "hypopg"]).await {
        return;
    }
    let db = client().await;
    // Statements of earlier runs out (pg_stat_statements identifies relations by name)
    db.batch_execute(
        "SELECT pg_stat_statements_reset();
         DROP SCHEMA IF EXISTS stmt_advice CASCADE; CREATE SCHEMA stmt_advice;
         CREATE TABLE stmt_advice.events (id int PRIMARY KEY, status text NOT NULL, payload text);
         INSERT INTO stmt_advice.events
           SELECT i, 'status ' || (i % 500), repeat('x', 50) FROM generate_series(1, 200000) i;
         ANALYZE stmt_advice.events;",
    )
    .await
    .unwrap();
    let config = Config::default();
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config("stmt_advice", &config)
        .await
        .unwrap();
    let app = create_router(Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            SchemaBuilder::build(&catalog).unwrap(),
            catalog.clone(),
            "stmt_advice".to_string(),
            DebugOptions::default(),
        )
        .with_config(config),
    ));

    let (by_status, by_id) = ("StmtAdviceByStatus", "StmtAdviceById");
    // Counting the rows of a filter on an unindexed column (slow), and a lookup by key (fast)
    for i in 0..5 {
        let reply = graphql(
            &app,
            &format!(
                "query {by_status} {{ eventsConnection(filter: {{ status: {{ equalTo: \"status {i}\" }} }}, first: 5) {{ totalCount nodes {{ id }} }} }}"
            ),
        )
        .await;
        assert!(reply.get("errors").is_none(), "{reply}");
        let reply = graphql(
            &app,
            &format!("query {by_id} {{ event(id: {}) {{ id }} }}", i + 1),
        )
        .await;
        assert!(reply.get("errors").is_none(), "{reply}");
    }

    let report = match top_statements(&db, &catalog, 10_000).await {
        Ok(report) => report,
        Err(StatementsError::Unreadable(err)) => {
            eprintln!("SKIPPED: pg_stat_statements is not readable: {err}");
            return;
        }
        Err(err) => panic!("{err}"),
    };
    assert!(report.hypopg);
    let position = |name: &str| {
        report
            .operations
            .iter()
            .position(|o| o.operation == format!("query {name}"))
            .unwrap_or_else(|| panic!("{name} not in {:#?}", report.operations))
    };
    let (slow, fast) = (position(by_status), position(by_id));
    assert!(slow < fast, "{:#?}", report.operations);

    let slow = &report.operations[slow];
    assert_eq!(slow.calls, 5);
    let suggestion = slow
        .suggestions
        .iter()
        .find(|s| s.sql.contains("stmt_advice"))
        .unwrap_or_else(|| panic!("{slow:#?}"));
    assert!(
        suggestion
            .sql
            .starts_with("CREATE INDEX ON \"stmt_advice\".\"events\" (\"status\""),
        "{suggestion:?}"
    );
    let with_index = suggestion.cost_with_index.unwrap();
    assert!(with_index < suggestion.cost / 2.0, "{suggestion:?}");

    // The lookup by key uses the primary key: nothing to suggest
    let fast = &report.operations[position(by_id)];
    assert!(fast.suggestions.is_empty(), "{fast:#?}");
}
