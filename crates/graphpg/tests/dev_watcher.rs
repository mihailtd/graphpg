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
