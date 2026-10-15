//! Schema reload without a restart (`reload:`): on `NOTIFY graphpg_reload` and on request,
//! refused when the new schema fails a startup check (pinned schemas).
mod common;
use axum::body::Body;
use axum::http::Request;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::reload::{reload_schema, start_reload_listener};
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "reload_test";

async fn connect() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// A fresh schema with one table; held by an advisory lock until the client drops (the
/// tests change the same schema).
async fn fresh() -> tokio_postgres::Client {
    let client = connect().await;
    client
        .batch_execute("SELECT pg_advisory_lock(7461019)")
        .await
        .unwrap();
    client
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA}; \
             CREATE TABLE {SCHEMA}.books (id int PRIMARY KEY, title text NOT NULL); \
             INSERT INTO {SCHEMA}.books VALUES (1, 'Dune');"
        ))
        .await
        .unwrap();
    client
}

async fn state(config: &str) -> Arc<ServerState> {
    let client = connect().await;
    let config = Config::from_yaml(config).unwrap();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            schema,
            catalog,
            SCHEMA.to_string(),
            DebugOptions::default(),
        )
        .with_config(config),
    )
}

async fn graphql(state: &Arc<ServerState>, query: &str) -> Value {
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let res = create_router(state.clone()).oneshot(request).await.unwrap();
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

const CONFIG: &str = "database:\n  schemas: [reload_test]\nexport:\n  pin: off\n";

#[tokio::test]
async fn reload_reads_the_database_again() {
    let db = fresh().await;
    let state = state(CONFIG).await;
    db.batch_execute(&format!(
        "CREATE TABLE {SCHEMA}.authors (id int PRIMARY KEY, name text NOT NULL); \
         INSERT INTO {SCHEMA}.authors VALUES (1, 'Herbert');"
    ))
    .await
    .unwrap();
    let before = graphql(&state, "{ authors { name } }").await;
    assert!(before["errors"].is_array(), "{before}");

    reload_schema(&state).await.unwrap();
    let after = graphql(&state, "{ authors { name } books { title } }").await;
    assert_eq!(
        after["data"],
        json!({ "authors": [{ "name": "Herbert" }], "books": [{ "title": "Dune" }] })
    );
}

#[tokio::test]
async fn notify_reloads() {
    let db = fresh().await;
    let state = state(CONFIG).await;
    start_reload_listener(
        common::db_url().to_string(),
        "graphpg_reload_test".to_string(),
        state.clone(),
    );
    // The listener connects in the background: notify until the reload shows
    db.batch_execute(&format!(
        "CREATE TABLE {SCHEMA}.shelves (id int PRIMARY KEY); INSERT INTO {SCHEMA}.shelves VALUES (7);"
    ))
    .await
    .unwrap();
    for _ in 0..50 {
        db.batch_execute("NOTIFY graphpg_reload_test")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let reply = graphql(&state, "{ shelves { databaseId } }").await;
        if reply["data"]["shelves"] == json!([{ "databaseId": 7 }]) {
            return;
        }
    }
    panic!("NOTIFY graphpg_reload_test did not reload the schema");
}

#[tokio::test]
async fn a_reload_that_drifts_from_pinned_files_is_refused() {
    let db = fresh().await;
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    let pinned = dir.join("reload_pinned.graphql");
    let config = format!(
        "database:\n  schemas: [reload_test]\nexport:\n  pin: fail\n  graphql: {}\n  openapi: {}\n",
        pinned.display(),
        dir.join("reload_pinned_missing.json").display()
    );
    let state = state(&config).await;
    // Pin what is served now
    std::fs::write(
        &pinned,
        SchemaBuilder::generate_sdl(&state.current_catalog()),
    )
    .unwrap();
    reload_schema(&state).await.expect("no drift yet");

    db.batch_execute(&format!("ALTER TABLE {SCHEMA}.books ADD COLUMN isbn text"))
        .await
        .unwrap();
    let err = reload_schema(&state).await.unwrap_err().to_string();
    assert!(err.contains("pinned"), "{err}");
    // Still serving the schema it had
    let reply = graphql(&state, "{ books { isbn } }").await;
    assert!(reply["errors"].is_array(), "{reply}");
    let reply = graphql(&state, "{ books { title } }").await;
    assert_eq!(reply["data"]["books"], json!([{ "title": "Dune" }]));
}

#[test]
fn channel_names_are_checked() {
    let err = Config::from_yaml("reload:\n  channel: 'drop table; x'\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("reload.channel"), "{err}");
}

/// No `database.schemas`: a reload reads the default schema (`app_public`), as startup does,
/// not none at all.
#[tokio::test]
async fn reload_without_configured_schemas_reads_the_default_one() {
    common::ensure_forum_fixture().await;
    let client = connect().await;
    let config = Config::from_yaml("export:\n  pin: off\n").unwrap();
    assert!(config.database.schemas.is_empty());
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config("app_public", &config)
        .await
        .unwrap();
    let state = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            SchemaBuilder::build(&catalog).unwrap(),
            catalog,
            "app_public".to_string(),
            DebugOptions::default(),
        )
        .with_config(config),
    );
    reload_schema(&state).await.unwrap();
    let catalog = state.current_catalog();
    assert!(catalog.find_table("app_public", "forums").is_some());
}

/// Without DDL watching (prod), a request compiled from a schema the database no longer has
/// fails as `SCHEMA_CHANGED` (retryable, not an internal error), and the server reads the
/// database again by itself.
#[tokio::test]
async fn requests_that_find_the_schema_out_of_date_reload_it() {
    let db = fresh().await;
    let state = state(CONFIG).await;
    start_reload_listener(
        common::db_url().to_string(),
        "graphpg_reload_stale_test".to_string(),
        state.clone(),
    );
    db.batch_execute(&format!(
        "ALTER TABLE {SCHEMA}.books RENAME COLUMN title TO name"
    ))
    .await
    .unwrap();

    let stale = graphql(&state, "{ books { title } }").await;
    assert_eq!(
        stale["errors"][0]["extensions"]["code"], "SCHEMA_CHANGED",
        "{stale}"
    );
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let reply = graphql(&state, "{ books { name } }").await;
        if reply["data"]["books"] == json!([{ "name": "Dune" }]) {
            return;
        }
    }
    panic!("the server did not read the changed schema again");
}

/// Reading the catalog while DDL runs: a column dropped after the reading's snapshot is still
/// in the catalog it reads, but no longer has privileges (`has_column_privilege` is null), which
/// must not crash the reading.
#[tokio::test]
async fn reading_the_catalog_during_ddl_does_not_panic() {
    let db = connect().await;
    db.batch_execute(
        "DROP SCHEMA IF EXISTS ddl_race CASCADE; CREATE SCHEMA ddl_race; \
         CREATE TABLE ddl_race.t (id int PRIMARY KEY, gone int);",
    )
    .await
    .unwrap();
    let reader = connect().await;
    reader
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT 1")
        .await
        .unwrap();
    db.batch_execute("ALTER TABLE ddl_race.t DROP COLUMN gone")
        .await
        .unwrap();

    let config = Config::from_yaml("database:\n  schemas: [ddl_race]\n").unwrap();
    let inspector = CatalogInspector::new(&reader);
    let reading =
        std::panic::AssertUnwindSafe(inspector.inspect_schema_with_config("ddl_race", &config));
    let outcome = futures::FutureExt::catch_unwind(reading).await;
    assert!(outcome.is_ok(), "reading the catalog panicked");
    reader.batch_execute("ROLLBACK").await.unwrap();
    db.batch_execute("DROP SCHEMA ddl_race CASCADE")
        .await
        .unwrap();
}
