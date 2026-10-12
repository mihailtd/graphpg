//! Telemetry over HTTP: the Prometheus endpoint, and the metrics requests leave in it with
//! bounded labels (route templates, operation names, SQLSTATEs).
mod common;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::{Config, Mode};
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn app(configure: impl FnOnce(&mut Config)) -> axum::Router {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to the test database");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("app_public")
        .await
        .expect("inspect app_public");
    let schema = SchemaBuilder::build(&catalog).expect("build schema");
    let mut config = Config::defaults(Mode::Prod);
    configure(&mut config);
    if config.telemetry.metrics.enabled {
        // What `telemetry::init` does at startup
        graphpg::telemetry::metrics().enable_prometheus();
    }
    let pool = create_pool(common::db_url(), 4).expect("pool");
    graphpg::telemetry::metrics().watch_pool("primary", pool.clone());
    let state = ServerState::new(
        pool,
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    )
    .with_config(config);
    create_router(Arc::new(state))
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, String, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).to_string(),
    )
}

fn metrics_on(config: &mut Config) {
    config.telemetry.metrics.enabled = true;
}

#[tokio::test]
async fn requests_are_measured_by_route_template() {
    let app = app(metrics_on).await;
    assert_eq!(
        call(&app, "GET", "/rest/forums", None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/rest/forums/00000000-0000-0000-0000-000000000000",
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "GET", "/no/such/path", None).await.0,
        StatusCode::NOT_FOUND
    );

    let (status, content_type, text) = call(&app, "GET", "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.starts_with("text/plain; version=0.0.4"),
        "{content_type}"
    );
    assert!(text.contains(
        r#"http_server_request_duration_seconds_count{http_request_method="GET",http_route="/rest/{resource}",http_response_status_code="200"}"#
    ), "{text}");
    assert!(text.contains(
        r#"http_server_request_duration_seconds_count{http_request_method="GET",http_route="/rest/{resource}/{key}",http_response_status_code="404"}"#
    ), "{text}");
    // An unmatched path has no route: its raw path never becomes a label
    assert!(!text.contains("/no/such/path"), "{text}");
    assert!(!text.contains(r#"http_route="/rest/forums""#), "{text}");
    assert!(text.contains(
        r#"db_client_operation_duration_seconds_count{db_system_name="postgresql",graphpg_operation_type="query",error_type=""}"#
    ), "{text}");
    assert!(
        text.contains(r#"graphpg_planning_duration_seconds_count{graphpg_api="rest"}"#),
        "{text}"
    );
    assert!(text.contains(
        r#"db_client_connection_count{db_client_connection_pool_name="primary",db_client_connection_state="idle"}"#
    ), "{text}");
    assert!(
        text.contains(r#"db_client_connection_max{db_client_connection_pool_name="primary"}"#),
        "{text}"
    );
    assert!(text.contains("http_server_active_requests "), "{text}");
}

#[tokio::test]
async fn graphql_operations_are_measured_by_name_and_error() {
    let app = app(metrics_on).await;
    let query = r#"{"query":"query TelemetryForums { forums { name } }"}"#;
    for _ in 0..2 {
        assert_eq!(
            call(&app, "POST", "/graphql", Some(query)).await.0,
            StatusCode::OK
        );
    }
    let invalid = r#"{"query":"query TelemetryBroken { forums { noSuchField } }"}"#;
    call(&app, "POST", "/graphql", Some(invalid)).await;

    let (_, _, text) = call(&app, "GET", "/metrics", None).await;
    assert!(text.contains(
        r#"graphpg_graphql_operation_duration_seconds_count{graphql_operation_type="query",graphql_operation_name="TelemetryForums",error_type=""} 2"#
    ), "{text}");
    // A document that does not validate fails before its operation is known
    assert!(
        text.contains(r#"graphql_operation_name="",error_type="GRAPHQL_ERROR"}"#),
        "{text}"
    );
    assert!(text.contains(r#"graphpg_cache_requests_total{graphpg_cache_name="document",graphpg_cache_result="hit"}"#), "{text}");
    assert!(text.contains(r#"graphpg_cache_requests_total{graphpg_cache_name="operation",graphpg_cache_result="hit"}"#), "{text}");
    assert!(
        text.contains(r#"graphpg_planning_duration_seconds_count{graphpg_api="graphql"}"#),
        "{text}"
    );
}

#[tokio::test]
async fn the_endpoint_follows_the_config() {
    // Off (the prod default)
    let off = app(|_| {}).await;
    assert_eq!(
        call(&off, "GET", "/metrics", None).await.0,
        StatusCode::NOT_FOUND
    );

    // Another path
    let moved = app(|c| {
        metrics_on(c);
        c.telemetry.metrics.path = "/internal/metrics".to_string();
    })
    .await;
    assert_eq!(
        call(&moved, "GET", "/metrics", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&moved, "GET", "/internal/metrics", None).await.0,
        StatusCode::OK
    );

    // A listener of its own: not on the API's port
    let separate = app(|c| {
        metrics_on(c);
        c.telemetry.metrics.listen = Some("127.0.0.1:9464".to_string());
    })
    .await;
    assert_eq!(
        call(&separate, "GET", "/metrics", None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn telemetry_config_is_read_from_yaml() {
    let config = Config::from_yaml(
        r#"
telemetry:
  service_name: orders-api
  logs: { level: 'info,graphpg=debug', format: json }
  metrics: { enabled: true, path: /internal/metrics, listen: '0.0.0.0:9464' }
  traces: { sample_ratio: 0.25 }
  otlp:
    endpoint: http://collector:4318/
    headers: { Authorization: Basic abc }
    logs: false
    interval: 15s
"#,
    )
    .expect("valid config");
    let t = &config.telemetry;
    assert_eq!(t.resolved_service_name(), "orders-api");
    assert_eq!(t.logs.format, graphpg::config::models::LogFormat::Json);
    assert!(t.metrics.enabled);
    assert_eq!(t.metrics.listen.as_deref(), Some("0.0.0.0:9464"));
    assert_eq!(t.traces.sample_ratio, 0.25);
    assert_eq!(
        t.otlp.resolved_endpoint().as_deref(),
        Some("http://collector:4318")
    );
    assert!(t.otlp.traces && t.otlp.metrics && !t.otlp.logs);
    assert_eq!(t.otlp.interval.0, std::time::Duration::from_secs(15));

    // Metrics are on in dev and off in prod unless the file says otherwise
    assert!(Config::defaults(Mode::Dev).telemetry.metrics.enabled);
    assert!(!Config::defaults(Mode::Prod).telemetry.metrics.enabled);

    for (yaml, problem) in [
        (
            "telemetry: { metrics: { path: metrics } }",
            "must start with `/`",
        ),
        (
            "telemetry: { metrics: { listen: ':9464' } }",
            "an address and a port",
        ),
        (
            "telemetry: { traces: { sample_ratio: 2 } }",
            "between 0 and 1",
        ),
        (
            "telemetry: { otlp: { endpoint: 'collector:4318' } }",
            "http:// or https://",
        ),
        ("telemetry: { logs: { format: xml } }", "unknown variant"),
    ] {
        let err = Config::from_yaml(yaml).expect_err(yaml).to_string();
        assert!(err.contains(problem), "{yaml}: {err}");
    }
}
