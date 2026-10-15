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

/// Debug values (`debug.sql`, `debug.explain`) too large for a response header, kept for
/// `GET /debug/sql/{id}`: the most recent ones only.
static OVERSIZED: std::sync::Mutex<std::collections::VecDeque<(String, serde_json::Value)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// How many oversized debug values are kept.
const KEPT: usize = 50;

/// Keeps `value` for `GET /debug/sql/{id}` and returns its id.
pub fn keep_oversized(value: serde_json::Value) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut kept = OVERSIZED.lock().unwrap();
    if kept.len() >= KEPT {
        kept.pop_front();
    }
    kept.push_back((id.clone(), value));
    id
}

/// `GET /debug/sql/{id}` (`debug.sql` or `debug.explain`, dev only): statements or plans of a
/// REST request that were too large for its `X-Debug-SQL` / `X-Debug-Explain` header.
pub async fn oversized_handler(axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    let kept = OVERSIZED.lock().unwrap();
    match kept.iter().find(|(k, _)| *k == id) {
        Some((_, value)) => Json(value.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such debug value (only the last 50 are kept)" })),
        )
            .into_response(),
    }
}
