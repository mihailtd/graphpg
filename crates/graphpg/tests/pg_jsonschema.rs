//! The `pg_jsonschema` integration (docs/extensions.md): a `CHECK (jsonb_matches_schema(...))`
//! types its column in the OpenAPI document; violations get the typed CHECK code. Runs on the
//! test image; skipped without pg_jsonschema.
mod common;
use axum::body::Body;
use axum::http::Request;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::openapi::OpenApiGenerator;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SETTINGS_SCHEMA: &str = r#"{"type": "object", "properties": {"theme": {"enum": ["light", "dark"]}, "size": {"type": "integer", "minimum": 1}}, "required": ["theme"], "additionalProperties": false}"#;

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup(config: &str) -> Option<(Config, graphpg::DatabaseCatalog, axum::Router)> {
    if !common::with_extensions(&["pg_jsonschema"]).await {
        return None;
    }
    let db = client().await;
    db.batch_execute("SELECT pg_advisory_lock(7461022)")
        .await
        .unwrap();
    db.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS jsonschema_test CASCADE; CREATE SCHEMA jsonschema_test;
         CREATE TABLE jsonschema_test.profiles (
             id int PRIMARY KEY,
             settings jsonb NOT NULL CHECK (jsonb_matches_schema('{SETTINGS_SCHEMA}', settings)),
             extra jsonb CHECK (jsonb_matches_schema('{{\"type\": \"array\"}}', extra)));"
    ))
    .await
    .unwrap();
    let config = Config::from_yaml(config).unwrap();
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config("jsonschema_test", &config)
        .await
        .unwrap();
    db.batch_execute("SELECT pg_advisory_unlock(7461022)")
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    let app = create_router(Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            schema,
            catalog.clone(),
            "jsonschema_test".to_string(),
            DebugOptions::default(),
        )
        .with_config(config.clone()),
    ));
    Some((config, catalog, app))
}

#[tokio::test]
async fn json_schema_checks_type_their_columns() {
    let Some((config, catalog, app)) = setup("{}").await else {
        return;
    };
    let doc = OpenApiGenerator::generate_for(&catalog, &config, "jsonschema_test");
    let props = &doc["components"]["schemas"]["profiles"]["properties"];
    let expected: Value = serde_json::from_str(SETTINGS_SCHEMA).unwrap();
    assert_eq!(
        props["settings"]["type"], expected["type"],
        "{}",
        props["settings"]
    );
    assert_eq!(props["settings"]["properties"], expected["properties"]);
    assert_eq!(props["settings"]["required"], json!(["theme"]));
    // Nullable: NULL passes the check
    assert_eq!(
        props["extra"]["anyOf"],
        json!([{ "type": "array" }, { "type": "null" }]),
        "{}",
        props["extra"]
    );

    // A write the schema rejects: the typed CHECK code, over both APIs
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "query": r#"mutation { createProfile(input: { id: 1, settings: { theme: "blue" } }) { profile { id } } }"# }).to_string(),
        ))
        .unwrap();
    let body: Value = serde_json::from_slice(
        &app.clone()
            .oneshot(request)
            .await
            .unwrap()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(
        body["errors"][0]["extensions"]["code"], "SETTINGS_INVALID",
        "{body}"
    );
    let res = app
        .clone()
        .oneshot(
            Request::post("/rest/profiles")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "id": 2, "settings": { "size": 3 } }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 422);
    let problem: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(problem["code"], "SETTINGS_INVALID", "{problem}");
}

#[tokio::test]
async fn off_leaves_json_untyped() {
    let Some((config, catalog, _)) =
        setup("extensions:\n  integrations:\n    pg_jsonschema: off\n").await
    else {
        return;
    };
    let doc = OpenApiGenerator::generate_for(&catalog, &config, "jsonschema_test");
    let settings = &doc["components"]["schemas"]["profiles"]["properties"]["settings"];
    assert!(settings.get("properties").is_none(), "{settings}");
}
