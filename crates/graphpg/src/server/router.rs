//! Assembles the HTTP router from modular route definitions per feature:
//! GraphQL, developer UIs, and health checks, with layered middleware.
use super::bulk::{export_handler, import_handler};
use super::http::{graphql_handler, health_handler, openapi_spec_handler, rest_handler};
use super::ServerState;
use crate::config::Config;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

/// Mounts the GraphQL API routes: `/graphql`, plus `/export` and `/import/{type_name}`.
pub fn graphql_routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/graphql", post(graphql_handler))
        .route("/export", post(export_handler))
        .route("/import/{type_name}", post(import_handler))
}

/// Mounts the REST API routes: `/rest`.
pub fn rest_routes() -> Router<Arc<ServerState>> {
    Router::new().route("/rest", get(rest_handler))
}

/// Mounts the OpenAPI document route: `state.openapi_path`.
pub fn openapi_routes(state: &ServerState) -> Router<Arc<ServerState>> {
    let mut router = Router::new();
    if state.openapi_enabled && state.openapi_spec {
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
    if state.scalar {
        router = router.merge(graphpg_ui::scalar_router());
    }
    router
}

/// Mounts the liveness health check route: `/health`.
pub fn health_routes() -> Router<Arc<ServerState>> {
    Router::new().route("/health", get(health_handler))
}

/// Applies global middleware layers configured by [`Config`].
pub fn apply_layers(router: Router, _config: &Config) -> Router {
    router.layer(CorsLayer::permissive())
}

/// Assembles the complete Axum [`Router`] for the server.
pub fn create_router(state: Arc<ServerState>) -> Router {
    let mut router = Router::new();

    if state.graphql_enabled {
        router = router.merge(graphql_routes());
    }

    if state.openapi_enabled {
        router = router.merge(rest_routes());
        router = router.merge(openapi_routes(&state));
    }

    router = router.merge(health_routes());
    router = router.merge(ui_routes(&state));

    let config = state.config.as_ref().cloned().unwrap_or_default();
    let router = router.with_state(state);
    apply_layers(router, &config)
}
