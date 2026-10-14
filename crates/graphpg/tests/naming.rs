//! Type and field names derived from table names: English singulars (`people` is a `Person`,
//! `episodes_of_care` an `EpisodeOfCare`), and names of many rows that differ from those of one
//! when a noun is its own plural (`evidence`, `series`).
mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use tokio_postgres::NoTls;

const FIXTURE: &str = r#"
DROP SCHEMA IF EXISTS naming_fixture CASCADE;
CREATE SCHEMA naming_fixture;
CREATE TABLE naming_fixture.people (id int PRIMARY KEY, name text);
CREATE TABLE naming_fixture.episodes_of_care (id int PRIMARY KEY,
    person_id int REFERENCES naming_fixture.people);
CREATE TABLE naming_fixture.subscription_statuses (id int PRIMARY KEY);
CREATE TABLE naming_fixture.addresses (id int PRIMARY KEY);
CREATE TABLE naming_fixture.evidences (id int PRIMARY KEY);
CREATE TABLE naming_fixture.series (id int PRIMARY KEY);
"#;

async fn sdl() -> String {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("connect to the test database");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(&format!(
            "SELECT pg_advisory_lock(7461002); {FIXTURE} SELECT pg_advisory_unlock(7461002);"
        ))
        .await
        .expect("naming fixture");
    let config = Config {
        database: graphpg::config::DatabaseConfig {
            schemas: vec!["naming_fixture".to_string()],
            ..Default::default()
        },
        ..Config::default()
    };
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config("naming_fixture", &config)
        .await
        .expect("inspect naming_fixture");
    SchemaBuilder::generate_sdl(&catalog)
}

#[tokio::test]
async fn types_are_the_english_singular_of_their_tables() {
    let sdl = sdl().await;
    for expected in [
        "type Person implements Node",
        "type EpisodeOfCare implements Node",
        "type SubscriptionStatus implements Node",
        "type Address implements Node",
        "type Evidence implements Node",
        "type Series implements Node",
        "  person(",
        "  episodeOfCare(",
        "  createPeople(",
        "  createEpisodesOfCare(",
        "  createSubscriptionStatuses(",
        // Their own plural: the table's name, else `List`
        "  createEvidence(",
        "  createEvidences(",
        "  createSeries(",
        "  createSeriesList(",
    ] {
        assert!(sdl.contains(expected), "missing `{expected}`");
    }
}
