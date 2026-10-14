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

    // The dev-mode watcher's check: operations at or over a mean time
    let slow = graphpg::catalog::statements::slow_operations(&db, &catalog, 0.0)
        .await
        .unwrap();
    assert!(
        slow.iter()
            .any(|(o, _, calls)| o == &format!("query {by_status}") && *calls == 5),
        "{slow:?}"
    );
    let none = graphpg::catalog::statements::slow_operations(&db, &catalog, 1e9)
        .await
        .unwrap();
    assert!(none.is_empty(), "{none:?}");

    // The lookup by key uses the primary key: nothing to suggest
    let fast = &report.operations[position(by_id)];
    assert!(fast.suggestions.is_empty(), "{fast:#?}");
}

/// In dev mode the report is served at `/debug/statements`; prod has no such route.
#[tokio::test]
async fn dev_mode_serves_the_report() {
    if !common::with_extensions(&["pg_stat_statements"]).await {
        return;
    }
    let catalog_for = |mode| async move {
        let config = Config::parse("", mode, &|_| None).unwrap();
        let catalog = CatalogInspector::new(&client().await)
            .inspect_schema_with_config("public", &config)
            .await
            .unwrap();
        create_router(Arc::new(
            ServerState::new(
                create_pool(common::db_url(), 1).unwrap(),
                SchemaBuilder::build(&catalog).unwrap(),
                catalog,
                "public".to_string(),
                DebugOptions::default(),
            )
            .with_config(config)
            .with_mode(mode),
        ))
    };
    let get = |app: axum::Router| async move {
        let res = app
            .oneshot(
                Request::get("/debug/statements?top=5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, body)
    };

    let (status, body) = get(catalog_for(graphpg::config::Mode::Dev).await).await;
    let report: Value = serde_json::from_slice(&body).unwrap();
    if status == axum::http::StatusCode::SERVICE_UNAVAILABLE {
        // pg_stat_statements created but not preloaded: the reason, not a failure
        assert!(
            report["error"]
                .as_str()
                .unwrap()
                .contains("pg_stat_statements"),
            "{report}"
        );
        return;
    }
    assert_eq!(status, axum::http::StatusCode::OK, "{report}");
    assert!(
        report["operations"].as_array().unwrap().len() <= 5,
        "{report}"
    );
    assert!(report.get("hypopg").is_some(), "{report}");

    let (status, _) = get(catalog_for(graphpg::config::Mode::Prod).await).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}
