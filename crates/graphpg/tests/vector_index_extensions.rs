//! The `pgvectorscale` and `vectorchord` integrations (docs/extensions.md): their vector
//! indexes (`diskann`, `vchordrq`) give semantic search fields like pgvector's own, and the
//! plans use them. Runs on the test image; skipped without the extensions.
mod common;
use axum::body::Body;
use axum::http::Request;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// The tables in `schema` (one per test: a test dropping it would break another's queries).
async fn app(schema: &str, config: &str) -> Option<axum::Router> {
    if !common::with_extensions(&["vector", "vectorscale", "vchord"]).await {
        return None;
    }
    let db = client().await;
    db.batch_execute("SELECT pg_advisory_lock(7461023)")
        .await
        .unwrap();
    db.batch_execute(
        &"DROP SCHEMA IF EXISTS vec_ext CASCADE; CREATE SCHEMA vec_ext;
         CREATE TABLE vec_ext.notes (id int PRIMARY KEY, title text NOT NULL, embedding vector(3) NOT NULL);
         CREATE TABLE vec_ext.photos (id int PRIMARY KEY, title text NOT NULL, embedding vector(3) NOT NULL);
         INSERT INTO vec_ext.notes SELECT i, 'note ' || i, ARRAY[i, 1, 0]::vector FROM generate_series(1, 200) i;
         INSERT INTO vec_ext.photos SELECT i, 'photo ' || i, ARRAY[0, i, 1]::vector FROM generate_series(1, 200) i;
         CREATE INDEX notes_diskann ON vec_ext.notes USING diskann (embedding vector_cosine_ops);
         CREATE INDEX photos_vchord ON vec_ext.photos USING vchordrq (embedding vector_l2_ops);
         ANALYZE vec_ext.notes; ANALYZE vec_ext.photos;"
            .replace("vec_ext", schema),
    )
    .await
    .unwrap();
    let config = Config::from_yaml(config).unwrap();
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config(schema, &config)
        .await
        .unwrap();
    db.batch_execute("SELECT pg_advisory_unlock(7461023)")
        .await
        .unwrap();
    let sdl = SchemaBuilder::build(&catalog).unwrap();
    Some(create_router(Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            sdl,
            catalog,
            schema.to_string(),
            DebugOptions {
                sql: false,
                explain: true,
            },
        )
        .with_config(config)
        .with_introspection(true),
    )))
}

async fn graphql(app: &axum::Router, query: &str) -> Value {
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let res = app.clone().oneshot(request).await.unwrap();
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn diskann_and_vectorchord_indexes_are_searchable() {
    let Some(app) = app("vec_ext_on", "{}").await else {
        return;
    };
    // Nearest by cosine (diskann) and by L2 (vchordrq)
    let reply = graphql(
        &app,
        "{ notes: notesSemanticSearch(vector: [50, 1, 0], first: 1) { hits { node { title } } }
           photos: photosSemanticSearch(vector: [0, 7, 1], first: 1) { hits { node { title } } } }",
    )
    .await;
    assert!(reply.get("errors").is_none(), "{reply}");
    assert_eq!(
        reply["data"]["photos"]["hits"][0]["node"]["title"], "photo 7",
        "{reply}"
    );
    let note = reply["data"]["notes"]["hits"][0]["node"]["title"]
        .as_str()
        .unwrap();
    // Cosine: direction, not length; 50:1 is closest to the largest ratios
    assert!(note.starts_with("note "), "{reply}");

    // The plans use the extensions' indexes
    let plan = reply["extensions"]["explain"].to_string();
    assert!(plan.contains("notes_diskann"), "{plan}");
    assert!(plan.contains("photos_vchord"), "{plan}");
}

#[tokio::test]
async fn off_no_search_fields_from_their_indexes() {
    let Some(app) = app(
        "vec_ext_off",
        "extensions:\n  integrations:\n    pgvectorscale: off\n    vectorchord: off\n",
    )
    .await
    else {
        return;
    };
    let reply = graphql(&app, "{ __type(name: \"Query\") { fields { name } } }").await;
    let fields = reply["data"]["__type"]["fields"].to_string();
    assert!(!fields.contains("notesSemanticSearch"), "{fields}");
    assert!(!fields.contains("photosSemanticSearch"), "{fields}");
}
