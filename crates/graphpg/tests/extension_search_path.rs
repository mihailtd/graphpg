//! Extensions installed in a schema of their own (Supabase's `extensions`) are on the
//! `search_path` of graphpg's connections, so the extension functions, operators and types its
//! statements use unqualified resolve.
mod common;
use graphpg::server::create_pool;
use tokio_postgres::NoTls;

#[tokio::test]
async fn pooled_connections_find_extensions_in_their_own_schema() {
    let (db, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    // fuzzystrmatch, in a schema of its own (no other test uses it)
    db.batch_execute(
        "SELECT pg_advisory_lock(7461025);
         CREATE SCHEMA IF NOT EXISTS ext_home;
         CREATE EXTENSION IF NOT EXISTS fuzzystrmatch SCHEMA ext_home;
         SELECT pg_advisory_unlock(7461025);",
    )
    .await
    .unwrap();
    let schema: String = db
        .query_one(
            "SELECT n.nspname::text FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
             WHERE e.extname = 'fuzzystrmatch'",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    let pool = create_pool(common::db_url(), 1).unwrap();
    let client = pool.get().await.unwrap();
    let path: String = client
        .query_one("SELECT current_setting('search_path')", &[])
        .await
        .unwrap()
        .get(0);
    // Appended: the configured path first, unchanged
    assert!(path.starts_with("\"$user\", public"), "{path}");
    assert!(path.contains(&schema), "{path}");
    let distance: i32 = client
        .query_one("SELECT levenshtein('graphpg', 'graphql')", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(distance, 2);
}
