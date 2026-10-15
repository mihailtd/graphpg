//! Dev mode reloads the schema when the catalog changes. Without a superuser there is no DDL
//! event trigger, and the catalog fingerprint, polled, is what finds a change: every kind of
//! change the API is built from must change it.
mod common;
use graphpg::server::watcher::catalog_fingerprint;
use tokio_postgres::NoTls;

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("connect to the test database");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[tokio::test]
async fn every_kind_of_ddl_changes_the_catalog_fingerprint() {
    let c = client().await;
    c.batch_execute(
        "DROP SCHEMA IF EXISTS watch_fixture CASCADE;
         CREATE SCHEMA watch_fixture;
         CREATE TABLE watch_fixture.things (id int PRIMARY KEY, name text);
         CREATE TYPE watch_fixture.mood AS ENUM ('happy');
         CREATE FUNCTION watch_fixture.answer() RETURNS int LANGUAGE sql STABLE AS 'SELECT 42';",
    )
    .await
    .unwrap();
    let schemas = vec!["watch_fixture".to_string()];
    let mut last = catalog_fingerprint(&c, &schemas).await.unwrap();
    assert!(!last.is_empty());
    assert_eq!(
        last,
        catalog_fingerprint(&c, &schemas).await.unwrap(),
        "stable"
    );

    for ddl in [
        "ALTER TABLE watch_fixture.things ADD COLUMN size int",
        "ALTER TABLE watch_fixture.things RENAME COLUMN size TO weight",
        "ALTER TABLE watch_fixture.things ALTER COLUMN weight TYPE bigint",
        "ALTER TABLE watch_fixture.things ALTER COLUMN name SET NOT NULL",
        "ALTER TABLE watch_fixture.things ADD CONSTRAINT positive CHECK (weight > 0)",
        "CREATE UNIQUE INDEX ON watch_fixture.things (name)",
        "COMMENT ON COLUMN watch_fixture.things.name IS 'What it is called'",
        "COMMENT ON TABLE watch_fixture.things IS '@name Item'",
        "ALTER TYPE watch_fixture.mood ADD VALUE 'sad'",
        "CREATE OR REPLACE FUNCTION watch_fixture.answer() RETURNS int LANGUAGE sql STABLE AS 'SELECT 43'",
        "ALTER TABLE watch_fixture.things ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY everyone ON watch_fixture.things USING (true)",
        "GRANT SELECT ON watch_fixture.things TO PUBLIC",
        "CREATE VIEW watch_fixture.names AS SELECT name FROM watch_fixture.things",
        "DROP VIEW watch_fixture.names",
    ] {
        c.batch_execute(ddl).await.expect(ddl);
        let now = catalog_fingerprint(&c, &schemas).await.unwrap();
        assert_ne!(now, last, "not seen: {ddl}");
        last = now;
    }

    // A change elsewhere is not one of these schemas'
    c.batch_execute("CREATE TABLE IF NOT EXISTS public.watch_elsewhere (id int)")
        .await
        .unwrap();
    assert_eq!(catalog_fingerprint(&c, &schemas).await.unwrap(), last);
    c.batch_execute("DROP TABLE public.watch_elsewhere")
        .await
        .unwrap();
}

/// The watcher's connection failing (a database restart) leaves it polling, then listening
/// again: it does not reload over and over, and it still finds changes.
#[tokio::test]
async fn the_watcher_survives_losing_its_connection() {
    use graphpg::catalog::CatalogInspector;
    use graphpg::config::{Config, Mode};
    use graphpg::schema::SchemaBuilder;
    use graphpg::server::{create_pool, start_schema_watcher, ServerState, WatcherOptions};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    let c = client().await;
    c.batch_execute(
        "DROP SCHEMA IF EXISTS watch_lost CASCADE; CREATE SCHEMA watch_lost; \
         CREATE TABLE watch_lost.things (id int PRIMARY KEY);",
    )
    .await
    .unwrap();
    // Reloads export the schema: to a directory of the test's own
    let dir = std::env::temp_dir().join(format!("watch_lost_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = |file: &str| dir.join(file).display().to_string();
    let yaml = format!(
        "database:\n  schemas: [watch_lost]\nexport:\n  graphql: {}\n  subgraph: {}\n  openapi: {}\n  config_schema: {}\n",
        out("schema.graphql"),
        out("subgraph.graphql"),
        out("openapi.json"),
        out("graphpg.schema.json"),
    );
    let yaml = yaml.as_str();
    let config_path = dir.join("graphpg.yaml");
    std::fs::write(&config_path, yaml).unwrap();
    let config = Config::from_yaml(yaml).unwrap();
    let catalog = CatalogInspector::new(&c)
        .inspect_schema_with_config("watch_lost", &config)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    let state = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            schema,
            catalog,
            "watch_lost".to_string(),
            Default::default(),
        )
        .with_config(config),
    );
    start_schema_watcher(
        WatcherOptions {
            database_url: common::db_url().to_string(),
            mode: Mode::Dev,
            config_path: Some(config_path.clone()),
        },
        state.clone(),
    );
    // Its listener, once connected, is cut off
    let listening = "SELECT pid FROM pg_stat_activity WHERE query LIKE '%LISTEN graphpg_watch%' \
                     AND pid <> pg_backend_pid()";
    let mut terminated = 0;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        terminated = c
            .query(
                &format!("SELECT pg_terminate_backend(pid) FROM ({listening}) l"),
                &[],
            )
            .await
            .unwrap()
            .len();
        if terminated > 0 {
            break;
        }
    }
    assert!(terminated > 0, "the watcher never listened");

    let before = state.reloads.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let reloads = state.reloads.load(Ordering::Relaxed) - before;
    assert!(reloads <= 1, "{reloads} reloads without a change");

    c.batch_execute("CREATE TABLE watch_lost.more (id int PRIMARY KEY)")
        .await
        .unwrap();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if state
            .current_catalog()
            .find_table("watch_lost", "more")
            .is_some()
        {
            c.batch_execute("DROP SCHEMA watch_lost CASCADE")
                .await
                .unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }
    panic!("the change was not found after the watcher lost its connection");
}
