//! The `postgres_fdw` integration (docs/foreign-data.md): the keys of the remote tables are
//! imported from the remote catalog, so foreign tables need no keys in `graphpg.yaml`. The
//! "remote" side is this database, over a loopback server.
mod common;
use axum::body::Body;
use axum::http::Request;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use graphpg::DatabaseCatalog;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "fdwk_app";

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// The remote tables (keys, a unique constraint, a foreign key), and foreign tables over them
/// without any key: one column renamed locally (`column_name`). The connection returned holds
/// the lock on them until the test drops it.
async fn inspect(config: &Config) -> (DatabaseCatalog, tokio_postgres::Client) {
    let db = client().await;
    db.batch_execute("SELECT pg_advisory_lock(7461026)")
        .await
        .unwrap();
    db.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS postgres_fdw;
         DROP SCHEMA IF EXISTS fdwk_remote CASCADE; DROP SCHEMA IF EXISTS fdwk_app CASCADE;
         DROP SERVER IF EXISTS fdwk_loopback CASCADE;
         CREATE SCHEMA fdwk_remote;
         CREATE TABLE fdwk_remote.customers (id int PRIMARY KEY, code text NOT NULL UNIQUE, name text NOT NULL);
         CREATE TABLE fdwk_remote.orders (
             id int PRIMARY KEY,
             customer_id int NOT NULL REFERENCES fdwk_remote.customers (id),
             total numeric NOT NULL);
         INSERT INTO fdwk_remote.customers VALUES (1, 'ACME', 'Acme'), (2, 'GLOBEX', 'Globex');
         INSERT INTO fdwk_remote.orders VALUES (10, 1, 9.5), (11, 1, 20), (12, 2, 3);
         DO $$ BEGIN
           EXECUTE format('CREATE SERVER fdwk_loopback FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host %L, port %L, dbname %L)',
             split_part(current_setting('unix_socket_directories'), ',', 1), current_setting('port'), current_database());
         END $$;
         CREATE USER MAPPING FOR CURRENT_USER SERVER fdwk_loopback;
         CREATE SCHEMA fdwk_app;
         CREATE FOREIGN TABLE fdwk_app.customers (id int NOT NULL, code text NOT NULL, name text NOT NULL)
           SERVER fdwk_loopback OPTIONS (schema_name 'fdwk_remote', table_name 'customers');
         CREATE FOREIGN TABLE fdwk_app.purchases (
             id int NOT NULL,
             buyer_id int OPTIONS (column_name 'customer_id') NOT NULL,
             total numeric NOT NULL)
           SERVER fdwk_loopback OPTIONS (schema_name 'fdwk_remote', table_name 'orders');",
    )
    .await
    .unwrap();
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config(SCHEMA, config)
        .await
        .unwrap();
    (catalog, db)
}

async fn graphql(catalog: DatabaseCatalog, config: Config, query: &str) -> Value {
    let app = create_router(Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            SchemaBuilder::build(&catalog).unwrap(),
            catalog,
            SCHEMA.to_string(),
            DebugOptions::default(),
        )
        .with_config(config),
    ));
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let res = app.oneshot(request).await.unwrap();
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn keys_come_from_the_remote_catalog() {
    let config = Config::default();
    let (catalog, _lock) = inspect(&config).await;
    let customers = catalog.find_table(SCHEMA, "customers").unwrap();
    assert_eq!(customers.primary_key, ["id"]);
    assert!(
        customers.unique_keys.iter().any(|k| k.columns == ["code"]),
        "{:?}",
        customers.unique_keys
    );
    let purchases = catalog.find_table(SCHEMA, "purchases").unwrap();
    assert_eq!(purchases.primary_key, ["id"]);
    // The remote foreign key, on the local names of both sides
    let fk = &purchases.foreign_keys[0];
    assert_eq!(fk.columns, ["buyer_id"]);
    assert_eq!(
        (fk.foreign_table.as_str(), &fk.foreign_columns[..]),
        ("customers", &["id".to_string()][..])
    );

    // Lookups by key, and relations both ways, with no graphpg.yaml
    let reply = graphql(
        catalog,
        config,
        "{ customer(id: 1) { name purchases { databaseId } } purchase(id: 12) { customer { code } } }",
    )
    .await;
    assert!(reply.get("errors").is_none(), "{reply}");
    let mut ids: Vec<i64> = reply["data"]["customer"]["purchases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["databaseId"].as_i64().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, [10, 11], "{reply}");
    assert_eq!(
        reply["data"]["purchase"]["customer"]["code"], "GLOBEX",
        "{reply}"
    );
}

#[tokio::test]
async fn declared_keys_win_and_the_import_can_be_off() {
    // A key declared in graphpg.yaml stays
    let config = Config::from_yaml("entities:\n  customers:\n    primary_key: [code]\n").unwrap();
    let (catalog, lock) = inspect(&config).await;
    assert_eq!(
        catalog.find_table(SCHEMA, "customers").unwrap().primary_key,
        ["code"]
    );

    drop(lock);

    let config =
        Config::from_yaml("extensions:\n  integrations:\n    postgres_fdw: off\n").unwrap();
    let (catalog, _lock) = inspect(&config).await;
    let purchases = catalog.find_table(SCHEMA, "purchases").unwrap();
    assert!(purchases.primary_key.is_empty());
    assert!(purchases.foreign_keys.is_empty());
}
