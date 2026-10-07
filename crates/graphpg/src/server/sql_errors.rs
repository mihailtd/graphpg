//! Errors the database author chose the shape of: PostgREST-style `PTxxx` SQLSTATEs raised from
//! SQL, and the `errors.map` entries of the configuration. REST and GraphQL read them here, so
//! the same SQL error says the same thing over both.
//!
//! A `PTxxx` error is written by the author of the function, so it is never masked. A mapped
//! error is a built-in failure (`23505`, a constraint) given a status and a code: its message
//! is the database's own, so it is masked in production like any other.

use axum::http::StatusCode;
use tokio_postgres::error::DbError;

use crate::config::models::ErrorsConfig;
use crate::execution::ExecutionError;

/// A database error with the shape the deployment or the SQL author gave it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CustomError {
    /// HTTP status (REST only: GraphQL answers `200`). Always 4xx or 5xx.
    pub status: StatusCode,
    pub title: Option<String>,
    pub problem_type: Option<String>,
    pub code: String,
    pub detail: String,
    pub hint: Option<String>,
    /// The database's `DETAIL` line (`Key (email)=(…) already exists`).
    pub db_detail: Option<String>,
    /// Further members of a JSON message, REST only.
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

/// The database error at the root of a failed execution.
pub(crate) fn db_error(err: &ExecutionError) -> Option<&DbError> {
    let mut root = err;
    while let ExecutionError::Mutation { source, .. } = root {
        root = source;
    }
    match root {
        ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => e.as_db_error(),
        _ => None,
    }
}

/// The custom shape of a database error, if it has one. A mapped constraint name wins over a
/// mapped SQLSTATE, which wins over the `PTxxx` convention. `masked`: production, where a
/// mapped error says only what its title says.
pub(crate) fn resolve(db: &DbError, config: &ErrorsConfig, masked: bool) -> Option<CustomError> {
    let sqlstate = db.code().code();
    let mapped = db
        .constraint()
        .and_then(|c| config.map.get(c))
        .or_else(|| config.map.get(sqlstate));
    if let Some(cfg) = mapped {
        let status = cfg
            .status
            .and_then(|s| StatusCode::from_u16(s).ok())
            .filter(|s| s.as_u16() >= 400)
            .unwrap_or(StatusCode::BAD_REQUEST);
        return Some(CustomError {
            status,
            title: cfg.title.clone(),
            problem_type: cfg.problem_type.clone(),
            code: cfg.code.clone().unwrap_or_else(|| sqlstate.to_string()),
            detail: if masked {
                cfg.title
                    .clone()
                    .or_else(|| status.canonical_reason().map(str::to_string))
                    .unwrap_or_else(|| "Error".to_string())
            } else {
                db.message().to_string()
            },
            hint: db.hint().filter(|_| !masked).map(str::to_string),
            db_detail: db.detail().filter(|_| !masked).map(str::to_string),
            extensions: serde_json::Map::new(),
        });
    }
    pt_error(db, sqlstate)
}

/// `PT` and three digits: the digits are the HTTP status. Anything below 400 or beyond 599 is a
/// `400`: an error never answers with a success or a redirect.
fn pt_error(db: &DbError, sqlstate: &str) -> Option<CustomError> {
    let digits = sqlstate.strip_prefix("PT")?;
    if digits.len() != 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let status = digits
        .parse::<u16>()
        .ok()
        .filter(|s| (400..=599).contains(s))
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::BAD_REQUEST);
    let hint = db.hint().map(str::to_string);
    let mut error = CustomError {
        status,
        title: None,
        problem_type: None,
        code: hint.clone().unwrap_or_else(|| sqlstate.to_string()),
        detail: db.message().to_string(),
        hint,
        db_detail: db.detail().map(str::to_string),
        extensions: serde_json::Map::new(),
    };
    // A JSON message sets the members itself
    if let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str(db.message()) {
        let mut take = |key: &str| match map.remove(key) {
            Some(serde_json::Value::String(s)) => Some(s),
            Some(other) => {
                map.insert(key.to_string(), other);
                None
            }
            None => None,
        };
        let message = take("message");
        let detail = take("detail");
        if let Some(d) = detail.or(message) {
            error.detail = d;
        }
        error.title = take("title");
        error.problem_type = take("type");
        if let Some(code) = take("code") {
            error.code = code;
        }
        if let Some(hint) = take("hint") {
            error.hint = Some(hint);
        }
        map.remove("status");
        error.extensions = map;
    }
    Some(error)
}
