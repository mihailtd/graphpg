//! Extension integrations (`extensions.integrations`, docs/extensions.md): an integration is on
//! when the schema **uses** its extension, can be switched off, and an unknown name is an error.
//! Run against the test image (docker/test-pg); skipped where an extension is missing.
mod common;
use graphpg::catalog::integrations::Integration;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use tokio_postgres::NoTls;

const EXTENSIONS: &[&str] = &[
    "timescaledb",
    "pg_jsonschema",
    "pgmq",
    "vector",
    "vectorscale",
    "vchord",
    "pgroonga",
];

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Two schemas: `ext_used` uses every extension, `ext_unused` none.
async fn fixture(db: &tokio_postgres::Client) {
    db.batch_execute("SELECT pg_advisory_lock(7461021)")
        .await
        .unwrap();
    db.batch_execute(
        "DROP SCHEMA IF EXISTS ext_used CASCADE; DROP SCHEMA IF EXISTS ext_unused CASCADE;
         CREATE SCHEMA ext_used; CREATE SCHEMA ext_unused;
         CREATE TABLE ext_unused.plain (id int PRIMARY KEY);
         CREATE TABLE ext_used.metrics (time timestamptz NOT NULL, value float8);
         SELECT create_hypertable('ext_used.metrics', by_range('time'));
         CREATE TABLE ext_used.docs (
             id int PRIMARY KEY, body text, embedding vector(3),
             settings jsonb CHECK (jsonb_matches_schema('{\"type\": \"object\"}', settings)));
         CREATE INDEX ON ext_used.docs USING diskann (embedding vector_cosine_ops);
         CREATE INDEX ON ext_used.docs USING vchordrq (embedding vector_l2_ops);
         CREATE INDEX ON ext_used.docs USING pgroonga (body);
         SELECT pgmq.create('ext_test_jobs') WHERE NOT EXISTS (SELECT 1 FROM pgmq.list_queues() WHERE queue_name = 'ext_test_jobs');",
    )
    .await
    .unwrap();
}

async fn active(schema: &str, config: &str) -> Result<Vec<(Integration, String)>, String> {
    let db = client().await;
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config(schema, &Config::from_yaml(config).unwrap())
        .await
        .map_err(|e| e.to_string())?;
    Ok(catalog
        .integrations
        .iter()
        .map(|a| (a.integration, a.detail.clone()))
        .collect())
}

#[tokio::test]
async fn integrations_follow_use_and_can_be_switched_off() {
    if !common::with_extensions(EXTENSIONS).await {
        return;
    }
    let db = client().await;
    fixture(&db).await;

    let used = active("ext_used", "{}").await.unwrap();
    let has = |i: Integration, detail: &str| used.contains(&(i, detail.to_string()));
    assert!(has(Integration::TimescaleDb, "1 hypertable"), "{used:?}");
    assert!(
        has(Integration::PgJsonschema, "1 JSON Schema check"),
        "{used:?}"
    );
    assert!(has(Integration::Pgvectorscale, "1 index"), "{used:?}");
    assert!(has(Integration::VectorChord, "1 index"), "{used:?}");
    assert!(has(Integration::PgRoonga, "1 index"), "{used:?}");
    assert!(
        used.iter().any(|(i, _)| *i == Integration::Pgmq),
        "{used:?}"
    );

    // Installed but not used by this schema: off (pgmq queues are database-wide)
    let unused = active("ext_unused", "{}").await.unwrap();
    for i in [
        Integration::TimescaleDb,
        Integration::PgJsonschema,
        Integration::Pgvectorscale,
        Integration::VectorChord,
        Integration::PgRoonga,
    ] {
        assert!(!unused.iter().any(|(a, _)| *a == i), "{i:?} in {unused:?}");
    }

    // Switched off
    let off = active(
        "ext_used",
        "extensions:\n  integrations:\n    timescaledb: off\n    pgroonga: off\n",
    )
    .await
    .unwrap();
    assert!(!off
        .iter()
        .any(|(i, _)| matches!(i, Integration::TimescaleDb | Integration::PgRoonga)));
    assert!(off.iter().any(|(i, _)| *i == Integration::PgJsonschema));
    db.batch_execute("SELECT pg_advisory_unlock(7461021)")
        .await
        .unwrap();
}

#[test]
fn unknown_integrations_are_errors() {
    let err = Config::from_yaml("extensions:\n  integrations:\n    timescale: off\n")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("unknown integration") && err.contains("timescaledb"),
        "{err}"
    );
}
