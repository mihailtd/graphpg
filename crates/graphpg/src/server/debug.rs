//! Development-only endpoints (`dev.debug`).
use super::http::ServerState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct StatementsQuery {
    /// Operations to report, the most expensive first (default 20).
    top: Option<usize>,
}

/// `GET /debug/statements` (`dev.debug.statements`): the most expensive operations so far, from
/// `pg_stat_statements`, with the indexes their plans lack, checked with hypopg when installed.
/// The report of `graphpg advise --statements`, as JSON.
pub async fn statements_handler(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<StatementsQuery>,
) -> Response {
    let client = match state.pool.get().await {
        Ok(client) => client,
        Err(err) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    };
    let catalog = state.current_catalog();
    let top = query.top.unwrap_or(20).clamp(1, 500);
    match crate::catalog::statements::top_statements(&client, &catalog, top).await {
        Ok(report) => Json(report).into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}
