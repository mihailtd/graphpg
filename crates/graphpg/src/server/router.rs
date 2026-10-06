//! Assembles the HTTP router from modular route definitions per feature:
//! GraphQL, developer UIs, and health checks, with layered middleware.
use super::bulk::{export_handler, import_handler};
use super::http::{
    graphql_handler, graphql_sdl_handler, graphql_subgraph_handler, health_handler,
    openapi_spec_handler, rest_handler,
};
use super::rest::{
    rest_aggregate_get_handler, rest_aggregate_post_handler, rest_column_get_handler,
    rest_column_put_handler, rest_create_handler, rest_delete_handler, rest_global_search_handler,
    rest_item_handler, rest_list_handler, rest_rpc_handler, rest_search_handler,
    rest_search_method_handler, rest_update_handler, rest_upsert_handler, GLOBAL_SEARCH_RESOURCE,
    REST_PREFIX, RPC_RESOURCE,
};
use super::ServerState;
use crate::config::models::{CorsConfig, HeaderPreset, HeadersConfig};
use crate::config::Config;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, Method};
use axum::routing::{get, options, post, MethodRouter};
use axum::Router;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

/// Mounts the GraphQL API routes: `/graphql`, plus `/export` and `/import/{type_name}`.
pub fn graphql_routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/graphql", post(graphql_handler))
        .route("/export", post(export_handler))
        // An import streams NDJSON of any size: the body limit does not apply
        .route(
            "/import/{type_name}",
            post(import_handler).layer(DefaultBodyLimit::disable()),
        )
}

/// Mounts the GraphQL SDL routes: `graphql.sdl_path`, and `graphql.subgraph_path` with federation.
pub fn sdl_routes(state: &ServerState) -> Router<Arc<ServerState>> {
    let mut router = Router::new();
    let Some(config) = state.config.as_ref().filter(|c| c.graphql.sdl) else {
        return router;
    };
    router = router.route(&config.graphql.sdl_path, get(graphql_sdl_handler));
    if config.graphql.federation.enabled {
        router = router.route(&config.graphql.subgraph_path, get(graphql_subgraph_handler));
    }
    router
}

/// Mounts the REST API routes:
/// - `/rest`: API root description
/// - `/rest/{resource}`: List (GET) and Create (POST)
/// - `/rest/{resource}/search`: Advanced filter query (POST)
/// - `/rest/{resource}/search/{method}`: Fuzzy, semantic, full-text ... search (POST)
/// - `/rest/{resource}/aggregate`: Aggregates and grouped aggregates (GET, POST)
/// - `/rest/rpc/{function}`: Functions (GET for read-only ones, POST)
/// - `/rest/search`: Search across every searchable resource (GET, POST)
/// - `/rest/{resource}/{key}`: Single item Read (GET), Update (PATCH), Upsert (PUT), Delete (DELETE)
/// - `/rest/{resource}/{key}/{column}`: The bytes of a `bytea` column (GET, PUT)
///
/// Every route answers `OPTIONS` with its methods; `HEAD` answers like `GET` without a body.
pub fn rest_routes() -> Router<Arc<ServerState>> {
    let resource = format!("{REST_PREFIX}/{{resource}}");
    Router::new()
        .route(
            REST_PREFIX,
            get(rest_handler).options(allowed("GET, HEAD, OPTIONS", &[])),
        )
        .route(
            &resource,
            get(rest_list_handler)
                .post(rest_create_handler)
                .options(allowed(
                    "GET, HEAD, POST, OPTIONS",
                    &[("accept-post", WRITE_MEDIA_WITH_CSV)],
                )),
        )
        .route(
            &format!("{resource}/search"),
            post(rest_search_handler).options(allowed("POST, OPTIONS", &[])),
        )
        .route(
            &format!("{resource}/search/{{method}}"),
            post(rest_search_method_handler).options(allowed("POST, OPTIONS", &[])),
        )
        .route(
            &format!("{resource}/aggregate"),
            get(rest_aggregate_get_handler)
                .post(rest_aggregate_post_handler)
                .options(allowed("GET, HEAD, POST, OPTIONS", &[])),
        )
        .route(
            &format!("{REST_PREFIX}/{RPC_RESOURCE}/{{function}}"),
            get(rest_rpc_handler)
                .post(rest_rpc_handler)
                .options(allowed("GET, HEAD, POST, OPTIONS", &[])),
        )
        .route(
            &format!("{REST_PREFIX}/{GLOBAL_SEARCH_RESOURCE}"),
            get(rest_global_search_handler)
                .post(rest_global_search_handler)
                .options(allowed("GET, HEAD, POST, OPTIONS", &[])),
        )
        .route(
            &format!("{resource}/{{key}}"),
            get(rest_item_handler)
                .patch(rest_update_handler)
                .put(rest_upsert_handler)
                .delete(rest_delete_handler)
                .options(allowed(
                    "GET, HEAD, PATCH, PUT, DELETE, OPTIONS",
                    &[("accept-patch", WRITE_MEDIA)],
                )),
        )
        .route(
            &format!("{resource}/{{key}}/{{column}}"),
            get(rest_column_get_handler)
                .put(rest_column_put_handler)
                .options(allowed("GET, HEAD, PUT, OPTIONS", &[])),
        )
}

/// Request media types of a write: JSON, and `multipart/form-data` for files.
const WRITE_MEDIA: &str = "application/json, multipart/form-data";
/// Request media types of a create: those of a write, and CSV rows.
const WRITE_MEDIA_WITH_CSV: &str = "application/json, text/csv, multipart/form-data";

/// The answer to `OPTIONS`: `204` with the methods in `Allow`, and the media types a write
/// takes in `Accept-Post` / `Accept-Patch` (RFC 7694). CORS preflights are answered earlier,
/// by the CORS layer.
fn allowed(
    allow: &'static str,
    media: &'static [(&'static str, &'static str)],
) -> MethodRouter<Arc<ServerState>> {
    options(move || async move {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(axum::http::header::ALLOW, HeaderValue::from_static(allow));
        for (name, value) in media {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        (axum::http::StatusCode::NO_CONTENT, headers)
    })
}

/// Mounts the OpenAPI document route: `state.openapi_path`.
pub fn openapi_routes(state: &ServerState) -> Router<Arc<ServerState>> {
    let mut router = Router::new();
    if state.serves_openapi_document() {
        router = router.route(&state.openapi_path, get(openapi_spec_handler));
    }
    router
}

/// Mounts developer tools: the GraphiQL IDE at `/graphiql` and the Scalar reference at `/scalar`.
pub fn ui_routes(state: &ServerState) -> Router<Arc<ServerState>> {
    let mut router = Router::new();
    if state.graphiql {
        router = router.merge(graphpg_ui::graphiql_router());
    }
    // The reference shows the OpenAPI document: with the API off there is nothing to show
    if state.scalar && state.openapi_enabled {
        router = router.merge(graphpg_ui::scalar_router_for(&state.openapi_path));
    }
    router
}

/// Mounts the liveness health check route: `/health`.
pub fn health_routes() -> Router<Arc<ServerState>> {
    Router::new().route("/health", get(health_handler))
}

/// Applies global middleware layers configured by [`Config`].
pub fn apply_layers(router: Router, config: &Config) -> Router {
    let headers = Arc::new(config.headers.clone());
    let router = router.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let headers = headers.clone();
            async move {
                // The bundled UIs load scripts and styles the strict policy would block
                let ui = is_ui_path(request.uri().path());
                let mut response = next.run(request).await;
                apply_security_headers(&headers, ui, response.headers_mut());
                response
            }
        },
    ));
    match cors_layer(&config.server.cors) {
        Some(cors) => router.layer(cors),
        None => router,
    }
}

/// The CORS layer for `server.cors`, or none when no origin is allowed.
fn cors_layer(cors: &CorsConfig) -> Option<CorsLayer> {
    if !cors.enabled() {
        return None;
    }
    let any = cors.origins.iter().any(|o| o == "*");
    let origins = if any {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(
            cors.origins
                .iter()
                .filter_map(|o| HeaderValue::from_str(o).ok()),
        )
    };
    let methods: Vec<Method> = if cors.methods.is_empty() {
        ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"]
            .iter()
            .map(|m| Method::from_bytes(m.as_bytes()).expect("valid method"))
            .collect()
    } else {
        cors.methods
            .iter()
            .filter_map(|m| Method::from_bytes(m.to_ascii_uppercase().as_bytes()).ok())
            .collect()
    };
    let headers: Vec<HeaderName> = if cors.headers.is_empty() {
        ["authorization", "content-type", "accept", "prefer"]
            .iter()
            .map(|h| HeaderName::from_static(h))
            .collect()
    } else {
        cors.headers
            .iter()
            .filter_map(|h| HeaderName::from_bytes(h.as_bytes()).ok())
            .collect()
    };
    let mut layer = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(AllowMethods::list(methods))
        .allow_headers(AllowHeaders::list(headers))
        .allow_credentials(cors.credentials);
    if let Some(secs) = cors.max_age {
        layer = layer.max_age(Duration::from_secs(secs));
    }
    Some(layer)
}

fn is_ui_path(path: &str) -> bool {
    ["/graphiql", "/scalar"]
        .iter()
        .any(|ui| path == *ui || path.starts_with(&format!("{ui}/")))
}

/// Assembles the complete Axum [`Router`] for the server.
pub fn create_router(state: Arc<ServerState>) -> Router {
    let mut router = Router::new();
    // `limits.request_body_bytes` for every request body
    let body_limit = match state.limits.request_body_bytes {
        Some(bytes) => DefaultBodyLimit::max(bytes),
        None => DefaultBodyLimit::disable(),
    };

    if state.graphql_enabled {
        router = router.merge(graphql_routes().layer(body_limit));
        router = router.merge(sdl_routes(&state));
    }

    if state.openapi_enabled {
        router = router.merge(rest_routes().layer(body_limit));
        router = router.merge(openapi_routes(&state));
    }

    router = router.merge(health_routes());
    router = router.merge(ui_routes(&state));

    let config = state.config.as_ref().cloned().unwrap_or_default();
    let router = router.with_state(state);
    apply_layers(router, &config)
}

/// Sets the headers of `config.preset`, then the overrides. Headers a handler already set stay.
fn apply_security_headers(config: &HeadersConfig, ui: bool, out: &mut axum::http::HeaderMap) {
    let preset: &[(&str, &str)] = match config.preset {
        HeaderPreset::Off => &[],
        HeaderPreset::Relaxed => &[
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
        ],
        HeaderPreset::Strict => &[
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            (
                "strict-transport-security",
                "max-age=31536000; includeSubDomains",
            ),
            ("x-frame-options", "DENY"),
            (
                "content-security-policy",
                "default-src 'none'; frame-ancestors 'none'",
            ),
            ("cross-origin-opener-policy", "same-origin"),
        ],
    };
    for (name, value) in preset {
        if ui && matches!(*name, "content-security-policy" | "x-frame-options") {
            continue;
        }
        if !out.contains_key(*name) {
            out.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
    }
    for (name, value) in &config.set {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        if value.is_empty() {
            out.remove(&name);
        } else if let Ok(value) = HeaderValue::from_str(value) {
            out.insert(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_sets_and_overrides_replace_or_remove() {
        let mut config = HeadersConfig::default();
        config
            .set
            .insert("x-frame-options".into(), "SAMEORIGIN".into());
        config
            .set
            .insert("strict-transport-security".into(), String::new());
        let mut out = axum::http::HeaderMap::new();
        apply_security_headers(&config, false, &mut out);
        assert_eq!(out["x-frame-options"], "SAMEORIGIN");
        assert!(!out.contains_key("strict-transport-security"));
        assert_eq!(out["x-content-type-options"], "nosniff");
    }

    #[test]
    fn off_sets_nothing() {
        let config = HeadersConfig {
            preset: HeaderPreset::Off,
            ..Default::default()
        };
        let mut out = axum::http::HeaderMap::new();
        apply_security_headers(&config, false, &mut out);
        assert!(out.is_empty());
    }
}
