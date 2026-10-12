//! The span and metrics of every HTTP request, and the Prometheus endpoint.
use super::metrics::{method_label, metrics};
use axum::extract::{MatchedPath, Request};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::time::Instant;
use tracing::Instrument;

/// Runs the request in an `http.request` server span (`GET /rest/{resource}`), continuing the
/// caller's trace when it sends a `traceparent`, and records `http.server.request.duration`.
/// It runs after routing, so the route is the matched template, never the raw path.
pub async fn track(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = method_label(request.method());
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string())
        .unwrap_or_default();
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let name = if route.is_empty() {
        method.to_string()
    } else {
        format!("{method} {route}")
    };
    let span = tracing::info_span!(
        "http.request",
        otel.name = %name,
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.request.method = method,
        http.route = %route,
        url.path = %request.uri().path(),
        http.response.status_code = tracing::field::Empty,
        request_id = %request_id,
        trace_id = tracing::field::Empty,
    );
    #[cfg(feature = "otel")]
    if let Some(trace_id) = super::otel::continue_trace(&span, request.headers()) {
        span.record("trace_id", trace_id.as_str());
    }

    let active = metrics().active_request();
    let response = next.run(request).instrument(span.clone()).await;
    drop(active);

    let status = response.status().as_u16();
    span.record("http.response.status_code", status);
    if response.status().is_server_error() {
        span.record("otel.status_code", "ERROR");
    }
    metrics().http_request(method, &route, status, start.elapsed().as_secs_f64());
    response
}

/// `GET <telemetry.metrics.path>`: every metric in the Prometheus text format.
pub async fn metrics_handler() -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        metrics().render(),
    )
        .into_response()
}
