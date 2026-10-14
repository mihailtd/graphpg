//! The message queue recipe of docs/extensions.md: a function of your own records the job and
//! sends its pgmq message in one transaction, and graphpg serves it like any function. Runs on
//! the test image; skipped without pgmq.
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

const SCHEMA: &str = "queue_recipe";
const QUEUE: &str = "recipe_exports";

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Messages in the queue.
async fn queued(db: &tokio_postgres::Client) -> i64 {
    db.query_one(&format!("SELECT count(*) FROM pgmq.q_{QUEUE}"), &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn a_function_of_your_own_enqueues_with_the_job() {
    if !common::with_extensions(&["pgmq"]).await {
        return;
    }
    let db = client().await;
    db.batch_execute("SELECT pg_advisory_lock(7461028)")
        .await
        .unwrap();
    // The recipe, verbatim but for the names
    db.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};
         SELECT pgmq.drop_queue('{QUEUE}') FROM pgmq.list_queues() WHERE queue_name = '{QUEUE}';
         SELECT pgmq.create('{QUEUE}');
         CREATE TABLE {SCHEMA}.exports (
             id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             format text NOT NULL CHECK (format IN ('csv', 'json')),
             status text NOT NULL DEFAULT 'queued',
             created_at timestamptz NOT NULL DEFAULT now()
         );
         CREATE FUNCTION {SCHEMA}.request_export(requested_format text) RETURNS {SCHEMA}.exports
         LANGUAGE plpgsql VOLATILE AS $$
         DECLARE
             job {SCHEMA}.exports;
         BEGIN
             INSERT INTO {SCHEMA}.exports (format) VALUES (requested_format) RETURNING * INTO job;
             PERFORM pgmq.send('{QUEUE}', jsonb_build_object('export_id', job.id));
             RETURN job;
         END $$;"
    ))
    .await
    .unwrap();
    let config = Config::default();
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap();
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
    let graphql = |query: &'static str| {
        let app = app.clone();
        async move {
            let request = Request::post("/graphql")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "query": query }).to_string()))
                .unwrap();
            let res = app.oneshot(request).await.unwrap();
            serde_json::from_slice::<Value>(&res.into_body().collect().await.unwrap().to_bytes())
                .unwrap()
        }
    };

    let reply =
        graphql("mutation { requestExport(requestedFormat: \"csv\") { databaseId status } }").await;
    assert!(reply.get("errors").is_none(), "{reply}");
    let job = &reply["data"]["requestExport"];
    assert_eq!(job["status"], "queued", "{reply}");
    // The worker's side: the message names the job
    let message: Value = db
        .query_one(
            &format!("SELECT message FROM pgmq.read('{QUEUE}', 30, 1)"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    // (the API serves `bigint` as a string)
    assert_eq!(
        message["export_id"].to_string(),
        job["databaseId"].as_str().unwrap(),
        "{message}"
    );

    // A request the rules reject enqueues nothing: the job and its message are one transaction
    let before = queued(&db).await;
    let reply =
        graphql("mutation { requestExport(requestedFormat: \"pdf\") { databaseId } }").await;
    assert!(reply.get("errors").is_some(), "{reply}");
    assert_eq!(queued(&db).await, before);
}
