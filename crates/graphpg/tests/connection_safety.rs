//! Pooled connections go back to the pool idle: a task that panics inside a transaction does
//! not hand that transaction (a request's role and claims) to the next checkout.
mod common;
use graphpg::execution::engine::PanicSafe;
use graphpg::server::create_pool;

#[tokio::test]
async fn a_connection_dropped_by_a_panic_is_discarded() {
    let pool = create_pool(common::db_url(), 1).expect("pool");
    let client = pool.get().await.expect("connection");
    client
        .batch_execute("BEGIN; SELECT set_config('request.jwt.claim.sub', 'alice', true)")
        .await
        .unwrap();
    let client = PanicSafe::new(client);
    let task = tokio::spawn(async move {
        let _held = client;
        panic!("a bug while the request's transaction is open");
    });
    assert!(task.await.unwrap_err().is_panic());

    let next = pool.get().await.expect("a connection");
    let row = next
        .query_one("SELECT current_setting('request.jwt.claim.sub', true)", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, Option<String>>(0).unwrap_or_default(), "");
}

/// A request whose connection the database ends (a restart, a failover) is answered as
/// `DATABASE_UNAVAILABLE` (503, retryable), not as an internal error.
#[tokio::test]
async fn a_request_whose_connection_ends_is_unavailable() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use graphpg::catalog::CatalogInspector;
    use graphpg::config::Config;
    use graphpg::schema::SchemaBuilder;
    use graphpg::server::{create_router, ServerState};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    let (c, connection) = tokio_postgres::connect(common::db_url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    c.batch_execute(
        "DROP SCHEMA IF EXISTS conn_lost CASCADE; CREATE SCHEMA conn_lost; \
         CREATE FUNCTION conn_lost.slow_answer() RETURNS int LANGUAGE sql STABLE \
         AS 'SELECT pg_sleep(2); SELECT 42';",
    )
    .await
    .unwrap();
    let config = Config::from_yaml("database:\n  schemas: [conn_lost]\n").unwrap();
    let catalog = CatalogInspector::new(&c)
        .inspect_schema_with_config("conn_lost", &config)
        .await
        .unwrap();
    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).unwrap(),
        SchemaBuilder::build(&catalog).unwrap(),
        catalog,
        "conn_lost".to_string(),
        Default::default(),
    ));
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"{ slowAnswer }"}"#))
        .unwrap();
    let response = tokio::spawn(create_router(state).oneshot(request));

    // The database ends the request's connection while it runs
    let running = "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                   WHERE query LIKE '%conn_lost%slow_answer%' AND pid <> pg_backend_pid()";
    let mut terminated = 0;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        terminated = c.query(running, &[]).await.unwrap().len();
        if terminated > 0 {
            break;
        }
    }
    assert!(terminated > 0, "the request never ran");

    let response = response.await.unwrap().unwrap();
    let status = response.status();
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body["errors"][0]["extensions"]["code"], "DATABASE_UNAVAILABLE",
        "{body}"
    );
    c.batch_execute("DROP SCHEMA conn_lost CASCADE")
        .await
        .unwrap();
}
