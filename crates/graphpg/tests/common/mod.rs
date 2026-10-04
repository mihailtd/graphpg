//! Shared helpers for the integration tests.
//!
//! The tests need a PostgreSQL 18 database. Point them at one with `TEST_DATABASE_URL`
//! (default `postgres://postgres:postgres@localhost:5432/graphpg_test`; `docker compose up -d`
//! in the repository root starts a matching one). Each test file creates the schema it needs.
#![allow(dead_code)]

use std::sync::OnceLock;
use tokio_postgres::NoTls;

const DEFAULT_URL: &str = "postgres://postgres:postgres@localhost:5432/graphpg_test";
const FORUM_SQL: &str = include_str!("../fixtures/forum.sql");
const INTERFACES_SQL: &str = include_str!("../fixtures/interfaces_and_unions.sql");

/// Connection string of the test database.
pub fn db_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()))
}

/// Create the `app_public` forum fixture (users, forums, messages) unless it already exists.
pub async fn ensure_forum_fixture() {
    ensure_fixture("app_public", FORUM_SQL).await;
}

/// Create the `interfaces_and_unions` fixture (single-table, class-table and union polymorphism).
pub async fn ensure_interfaces_fixture() {
    ensure_fixture("interfaces_and_unions", INTERFACES_SQL).await;
}

/// Run `sql` unless `schema` already exists. Safe to call from tests running in parallel:
/// creation is serialised by an advisory lock.
async fn ensure_fixture(schema: &str, sql: &str) {
    let (client, connection) = tokio_postgres::connect(db_url(), NoTls)
        .await
        .expect("Failed to connect to the test database (set TEST_DATABASE_URL)");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client.batch_execute("SELECT pg_advisory_lock(7461001)").await.expect("advisory lock");
    let exists: bool = client
        .query_one("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)", &[&schema])
        .await
        .expect("fixture check")
        .get(0);
    if !exists {
        client.batch_execute(sql).await.unwrap_or_else(|e| panic!("Failed to create the {schema} fixture: {e}"));
    }
    client.batch_execute("SELECT pg_advisory_unlock(7461001)").await.expect("advisory unlock");
}
