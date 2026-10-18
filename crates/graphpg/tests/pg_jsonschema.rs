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

const ADDRESS_SCHEMA: &str = r##"{"$schema": "http://json-schema.org/draft-07/schema#", "definitions": {"Line": {"type": "string", "minLength": 1}}, "type": "object", "properties": {"lines": {"type": "array", "items": {"$ref": "#/definitions/Line"}}}}"##;
const CONTACT_SCHEMA: &str = r#"{"type": "object", "required": ["system"], "properties": {"system": {"enum": ["phone", "email"]}}}"#;

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
             extra jsonb CHECK (jsonb_matches_schema('{{\"type\": \"array\"}}', extra)));
         CREATE DOMAIN jsonschema_test.contact_point AS jsonb
             CHECK (jsonb_matches_schema('{CONTACT_SCHEMA}', VALUE));
         CREATE TABLE jsonschema_test.notes (
             id int PRIMARY KEY,
             profile_id int REFERENCES jsonschema_test.profiles,
             tags jsonb NOT NULL CHECK (jsonb_matches_schema('{{\"type\": \"array\", \"items\": {{\"type\": \"string\"}}}}', tags)),
             address jsonb CHECK (jsonb_matches_schema('{ADDRESS_SCHEMA}', address)),
             meta jsonb CHECK (meta IS NULL OR jsonb_matches_schema('{{\"type\": \"object\"}}', meta)),
             contact jsonschema_test.contact_point);"
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

#[tokio::test]
async fn write_bodies_of_the_served_document_keep_the_json_schema() {
    // #215: the served document (and the exported one) describes bodies by the GraphQL input
    // types; their JSON columns keep the schema, nested inputs included
    let Some((_, _, app)) = setup("openapi:\n  spec: true\n").await else {
        return;
    };
    let res = app
        .oneshot(Request::get("/openapi.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let doc: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let schemas = &doc["components"]["schemas"];
    let expected: Value = serde_json::from_str(SETTINGS_SCHEMA).unwrap();
    for input in ["Input.CreateProfileInput", "Input.UpdateProfileInput"] {
        let settings = &schemas[input]["properties"]["settings"];
        assert_eq!(
            settings["properties"], expected["properties"],
            "{input}: {settings}"
        );
        let extra = &schemas[input]["properties"]["extra"];
        assert_eq!(
            extra["anyOf"],
            json!([{ "type": "array" }, { "type": "null" }]),
            "{input}: {extra}"
        );
    }
    let tags = &schemas["Input.CreateNoteInput"]["properties"]["tags"];
    assert_eq!(tags["items"], json!({ "type": "string" }), "{tags}");
    assert_eq!(
        schemas["profilesCreateInput"]["$ref"],
        "#/components/schemas/Input.CreateProfileInput"
    );
}

async fn served(app: &axum::Router) -> Value {
    let res = app
        .clone()
        .oneshot(Request::get("/openapi.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn schemas_that_refer_to_themselves_and_domains_are_components() {
    let Some((_, _, app)) = setup("openapi:\n  spec: true\n").await else {
        return;
    };
    let doc = served(&app).await;
    let schemas = &doc["components"]["schemas"];
    // A draft-07 schema with definitions: a component its references point into
    let address = &schemas["Json.notes.address"];
    assert_eq!(
        address["properties"]["lines"]["items"]["$ref"],
        "#/components/schemas/Json.notes.address/definitions/Line",
        "{address}"
    );
    assert_eq!(address["definitions"]["Line"]["minLength"], 1);
    assert!(address.get("$schema").is_none());
    let row = &schemas["notes"]["properties"];
    assert_eq!(
        row["address"]["anyOf"][0]["$ref"], "#/components/schemas/Json.notes.address",
        "{}",
        row["address"]
    );
    // A domain's schema: one component, which its columns refer to, in rows and in bodies
    let contact = &schemas["Json.ContactPoint"];
    assert_eq!(contact["required"], json!(["system"]), "{contact}");
    assert_eq!(
        row["contact"]["anyOf"][0]["$ref"], "#/components/schemas/Json.ContactPoint",
        "{}",
        row["contact"]
    );
    let input = &schemas["Input.CreateNoteInput"]["properties"];
    assert_eq!(
        input["contact"]["anyOf"][0]["$ref"],
        "#/components/schemas/Json.ContactPoint"
    );
    // `meta IS NULL OR` the call: typed all the same
    assert_eq!(
        row["meta"]["anyOf"],
        json!([{ "type": "object" }, { "type": "null" }]),
        "{}",
        row["meta"]
    );
    // Every reference of the document resolves
    let text = doc.to_string();
    for (at, _) in text.match_indices("\"$ref\":\"#/") {
        let rest = &text[at + 8..];
        let pointer = &rest[1..rest.find('"').unwrap()];
        assert!(doc.pointer(pointer).is_some(), "dangling: {pointer}");
    }
}

#[tokio::test]
async fn a_rejected_value_says_why() {
    let Some((_, _, app)) = setup("{}").await else {
        return;
    };
    // GraphQL: the check's code, and what the schema rejects where
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "query": r#"mutation { createProfile(input: { id: 11, settings: { theme: "blue", size: 0 } }) { profile { id } } }"# }).to_string(),
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
    let extensions = &body["errors"][0]["extensions"];
    assert_eq!(extensions["code"], "SETTINGS_INVALID", "{body}");
    let errors = extensions["validationErrors"]
        .as_array()
        .expect("validationErrors");
    let paths: Vec<&str> = errors.iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert!(
        paths.contains(&"/theme") && paths.contains(&"/size"),
        "{errors:?}"
    );

    // REST: a bulk create, the second row failing: its path and why
    let res = app
        .clone()
        .oneshot(
            Request::post("/rest/profiles")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!([
                        { "id": 12, "settings": { "theme": "light" } },
                        { "id": 13, "settings": { "theme": "light", "extra": true } }
                    ])
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 422);
    let problem: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(problem["code"], "SETTINGS_INVALID", "{problem}");
    assert_eq!(
        problem["fields"],
        json!(["inputs[1].settings"]),
        "{problem}"
    );
    let errors = problem["validationErrors"]
        .as_array()
        .expect("validationErrors");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["path"], "", "{errors:?}");
    assert!(
        errors[0]["message"].as_str().unwrap().contains("extra"),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_value_its_domain_rejects_is_typed() {
    // A check of a domain is reported without a table nor a column: the write says which
    let Some((_, _, app)) = setup("{}").await else {
        return;
    };
    for (body, field) in [
        (
            json!({ "id": 21, "tags": [], "contact": { "system": "fax" } }),
            "input.contact",
        ),
        (
            json!([{ "id": 22, "tags": [], "contact": { "system": "phone" } }, { "id": 23, "tags": [], "contact": { "system": "fax" } }]),
            "inputs[1].contact",
        ),
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::post("/rest/notes")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 422);
        let problem: Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(problem["code"], "CONTACT_INVALID", "{problem}");
        assert_eq!(problem["fields"], json!([field]), "{problem}");
        assert_eq!(
            problem["validationErrors"][0]["path"], "/system",
            "{problem}"
        );
    }
    // and listed among the table's codes
    let sdl = graphpg::schema::SchemaBuilder::generate_sdl(&setup("{}").await.unwrap().1);
    let codes = &sdl[sdl.find("enum NoteErrorCode").unwrap()..];
    assert!(
        codes[..codes.find('}').unwrap()].contains("CONTACT_INVALID"),
        "{codes}"
    );
}
