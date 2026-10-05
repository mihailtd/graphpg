//! Assembles the HTTP router from modular route definitions per feature:
//! GraphQL, developer UIs, and health checks, with layered middleware.
use super::bulk::{export_handler, import_handler};
use super::http::{graphql_handler, health_handler, openapi_spec_handler, rest_handler};
use super::rest::{
    rest_create_handler, rest_delete_handler, rest_item_handler, rest_list_handler,
    rest_search_handler, rest_update_handler, rest_upsert_handler, MAX_BODY_BYTES, REST_PREFIX,
};
use super::ServerState;
use crate::config::Config;
use axum::extract::DefaultBodyLimit;
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

/// Mounts the REST API routes:
/// - `/rest`: API root description
/// - `/rest/{resource}`: List (GET) and Create (POST)
/// - `/rest/{resource}/search`: Advanced filter query (POST)
/// - `/rest/{resource}/{key}`: Single item Read (GET), Update (PATCH), Upsert (PUT), Delete (DELETE)
pub fn rest_routes() -> Router<Arc<ServerState>> {
    let resource = format!("{REST_PREFIX}/{{resource}}");
    Router::new()
        .route(REST_PREFIX, get(rest_handler))
        .route(&resource, get(rest_list_handler).post(rest_create_handler))
        .route(&format!("{resource}/search"), post(rest_search_handler))
        .route(
            &format!("{resource}/{{key}}"),
            get(rest_item_handler)
                .patch(rest_update_handler)
                .put(rest_upsert_handler)
                .delete(rest_delete_handler),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
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
