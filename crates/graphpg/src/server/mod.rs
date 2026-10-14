pub mod auth;
pub mod bulk;
pub mod cache;
pub mod debug;
pub mod http;
pub mod incremental;
pub mod partial;
pub mod persisted;
pub mod rate_limit;
pub mod reload;
pub mod rest;
pub mod router;
pub(crate) mod sql_errors;
pub mod subscriptions;
pub mod watcher;

pub use auth::{AuthError, AuthSessionContext, Authenticator};
pub use http::{
    create_pool, create_pool_with, DebugOptions, GraphQLRequest, GraphQLResponse, Replica,
    ServerState, MIN_WAL_POSITION_HEADER, WAL_POSITION_HEADER,
};
pub use persisted::PersistedQueries;
pub use rate_limit::RateLimiter;
pub use rest::{rest_list_handler, rest_search_handler, ProblemDetails, RestQuery};
pub use router::{
    apply_layers, create_router, graphql_routes, health_routes, metrics_routes, openapi_routes,
    rest_routes, ui_routes,
};
pub use watcher::{start_schema_watcher, WatcherOptions};
