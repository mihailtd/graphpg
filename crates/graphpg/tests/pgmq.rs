//! The `pgmq` integration (docs/extensions.md): the queues named in `extensions.pgmq.queues`
//! are served as functions of their own (send, read, delete, archive, pop, visibility,
//! metrics), over GraphQL and REST. Runs on the test image; skipped without pgmq.
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

/// The (empty) schema served, created once: concurrent `CREATE SCHEMA IF NOT EXISTS` collide.
async fn schema() {
    static ONCE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    ONCE.get_or_init(|| async {
        client()
            .await
            .batch_execute("CREATE SCHEMA IF NOT EXISTS pgmq_api")
            .await
            .unwrap();
    })
    .await;
}

/// A fresh queue `queue`, and the API serving it (`None` without pgmq).
async fn setup(queue: &str) -> Option<axum::Router> {
    if !common::with_extensions(&["pgmq"]).await {
        return None;
    }
    schema().await;
    let db = client().await;
    db.batch_execute(&format!(
        "SELECT pgmq.drop_queue('{queue}') FROM pgmq.list_queues() WHERE queue_name = '{queue}';
         SELECT pgmq.create('{queue}');"
    ))
    .await
    .unwrap();
    let config =
        Config::from_yaml(&format!("extensions:\n  pgmq:\n    queues: [{queue}]\n")).unwrap();
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config("pgmq_api", &config)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog)
        .unwrap_or_else(|e| panic!("{e}\n{}", SchemaBuilder::generate_sdl(&catalog)));
    Some(create_router(Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            schema,
            catalog,
            "pgmq_api".to_string(),
            DebugOptions::default(),
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
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(body.get("errors").is_none(), "{query}: {body}");
    body["data"].clone()
}

#[tokio::test]
async fn a_queue_is_sent_read_acknowledged_and_measured() {
    let Some(app) = setup("api_jobs").await else {
        return;
    };
    let sent = graphql(
        &app,
        r#"mutation { sendApiJobsMessage(message: { task: "a" }) }"#,
    )
    .await;
    let first = sent["sendApiJobsMessage"][0].clone();
    assert!(first.is_string() || first.is_number(), "{sent}");
    let batch = graphql(
        &app,
        r#"mutation { sendApiJobsMessages(messages: [{ task: "b" }, { task: "c" }]) }"#,
    )
    .await;
    assert_eq!(batch["sendApiJobsMessages"].as_array().unwrap().len(), 2);
    // Delayed: not visible yet
    graphql(
        &app,
        r#"mutation { sendApiJobsMessage(message: { task: "later" }, delay: 3600) }"#,
    )
    .await;

    let read = graphql(
        &app,
        "mutation { readApiJobsMessages(visibilityTimeout: 60, count: 10) { msgId readCt message } }",
    )
    .await;
    let messages = read["readApiJobsMessages"].as_array().unwrap();
    let tasks: Vec<&str> = messages
        .iter()
        .map(|m| m["message"]["task"].as_str().unwrap())
        .collect();
    assert_eq!(tasks, ["a", "b", "c"], "{read}");
    assert!(messages.iter().all(|m| m["readCt"] == 1));
    // Hidden from other readers for the visibility timeout
    let again = graphql(
        &app,
        "mutation { readApiJobsMessages(visibilityTimeout: 60, count: 10) { msgId } }",
    )
    .await;
    assert_eq!(again["readApiJobsMessages"], json!([]));

    // Acknowledge: delete one, archive another
    let ids: Vec<String> = messages.iter().map(|m| m["msgId"].to_string()).collect();
    let deleted = graphql(
        &app,
        &format!(
            "mutation {{ deleteApiJobsMessages(messageIds: [{}]) }}",
            ids[0]
        ),
    )
    .await;
    assert_eq!(
        deleted["deleteApiJobsMessages"].as_array().unwrap().len(),
        1
    );
    let archived = graphql(
        &app,
        &format!(
            "mutation {{ archiveApiJobsMessages(messageIds: [{}]) }}",
            ids[1]
        ),
    )
    .await;
    assert_eq!(
        archived["archiveApiJobsMessages"].as_array().unwrap().len(),
        1
    );

    // Metrics only read: a query. Left: the third and the delayed one
    let metrics = graphql(&app, "{ apiJobsQueueMetrics { queueLength } }").await;
    assert_eq!(
        metrics["apiJobsQueueMetrics"]["queueLength"]
            .to_string()
            .trim_matches('"'),
        "2",
        "{metrics}"
    );
}

#[tokio::test]
async fn rest_serves_the_same_operations() {
    let Some(app) = setup("rest_jobs").await else {
        return;
    };
    let call = |name: &str, args: Value| {
        Request::post(format!("/rest/rpc/{name}"))
            .header("content-type", "application/json")
            .body(Body::from(json!({ "args": args }).to_string()))
            .unwrap()
    };
    let res = app
        .clone()
        .oneshot(call(
            "sendRestJobsMessage",
            json!({ "message": { "n": 1 } }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let res = app
        .clone()
        .oneshot(call(
            "readRestJobsMessages",
            json!({ "visibilityTimeout": 30, "count": 5 }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let rows = body.as_array().unwrap_or_else(|| panic!("{body}"));
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["message"], json!({ "n": 1 }), "{body}");
}

#[tokio::test]
async fn queues_are_opt_in_and_must_exist() {
    if !common::with_extensions(&["pgmq"]).await {
        return;
    }
    schema().await;
    let db = client().await;
    // None named: no queue fields
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config("pgmq_api", &Config::default())
        .await
        .unwrap();
    assert!(!catalog
        .functions
        .iter()
        .any(|f| !f.fixed_arguments.is_empty()));
    // A queue that does not exist
    let err = CatalogInspector::new(&db)
        .inspect_schema_with_config(
            "pgmq_api",
            &Config::from_yaml("extensions:\n  pgmq:\n    queues: [no_such_queue]\n").unwrap(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no queue `no_such_queue`"), "{err}");
}
