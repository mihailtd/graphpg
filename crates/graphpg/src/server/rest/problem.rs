//! RFC 9457 `application/problem+json` error responses for the REST API.
//!
//! The `type` member is `about:blank` unless an error was given one (`errors.map`, or a JSON
//! `PTxxx` message): the status code carries the meaning and the stable, machine-readable
//! `code` member (such as `EMAIL_TAKEN`) identifies the problem.
//! Raw PostgreSQL messages never reach the client in production; the client gets a request ID
//! that is also written to the server log together with the real error.

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::catalog::DatabaseCatalog;
use crate::execution::ExecutionError;
use crate::server::auth::AuthError;
use crate::server::sql_errors;

/// RFC 9457 Problem Details object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    pub problem_type: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    #[serde(rename = "requestId", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The safety limit a request exceeded, and what it asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extensions: serde_json::Map<String, serde_json::Value>,
    /// Response headers (such as `WWW-Authenticate`); not part of the body.
    #[serde(skip)]
    pub headers: Vec<(header::HeaderName, header::HeaderValue)>,
}

fn title_for(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "Bad Request",
        StatusCode::UNAUTHORIZED => "Unauthorized",
        StatusCode::FORBIDDEN => "Forbidden",
        StatusCode::NOT_FOUND => "Not Found",
        StatusCode::CONFLICT => "Conflict",
        StatusCode::PRECONDITION_FAILED => "Precondition Failed",
        StatusCode::PAYLOAD_TOO_LARGE => "Content Too Large",
        StatusCode::UNPROCESSABLE_ENTITY => "Unprocessable Content",
        StatusCode::SERVICE_UNAVAILABLE => "Service Unavailable",
        StatusCode::INTERNAL_SERVER_ERROR => "Internal Server Error",
        other => other.canonical_reason().unwrap_or("Error"),
    }
}

impl ProblemDetails {
    pub fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            problem_type: "about:blank".to_string(),
            title: title_for(status).to_string(),
            status: status.as_u16(),
            detail: detail.into(),
            instance: None,
            code: None,
            fields: None,
            request_id: None,
            limit: None,
            actual: None,
            hint: None,
            extensions: serde_json::Map::new(),
            headers: Vec::new(),
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn with_type(mut self, problem_type: impl Into<String>) -> Self {
        self.problem_type = problem_type.into();
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    pub fn with_fields(mut self, fields: Vec<String>) -> Self {
        if !fields.is_empty() {
            self.fields = Some(fields);
        }
        self
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail).with_code("BAD_REQUEST")
    }

    /// A parameter or body member the API does not know or cannot accept.
    pub fn invalid(detail: impl Into<String>, field: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
            .with_code("INVALID_REQUEST")
            .with_fields(vec![field.into()])
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail).with_code("NOT_FOUND")
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, detail).with_code("FORBIDDEN")
    }

    /// `416`, with the size of the whole in `Content-Range: items */N` (RFC 9110 14.4).
    pub fn range_not_satisfiable(detail: impl Into<String>, total: Option<i64>) -> Self {
        let mut problem =
            Self::new(StatusCode::RANGE_NOT_SATISFIABLE, detail).with_code("RANGE_NOT_SATISFIABLE");
        let size = total.map_or_else(|| "*".to_string(), |n| n.to_string());
        if let Ok(value) = header::HeaderValue::from_str(&format!("items */{size}")) {
            problem.headers.push((header::CONTENT_RANGE, value));
        }
        problem
    }

    pub fn not_acceptable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_ACCEPTABLE, detail).with_code("NOT_ACCEPTABLE")
    }

    pub fn unsupported_media_type(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, detail).with_code("UNSUPPORTED_MEDIA_TYPE")
    }

    pub fn payload_too_large(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, detail).with_code("PAYLOAD_TOO_LARGE")
    }

    /// A safety limit the request exceeds (see [`crate::plan::limits`]).
    pub fn from_limit(error: &crate::plan::LimitError) -> Self {
        let status = match error {
            crate::plan::LimitError::BulkRows { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        };
        let mut problem = Self::new(status, error.to_string()).with_code(error.code());
        problem.limit = Some(error.limit());
        problem.actual = Some(error.actual());
        problem
    }

    pub fn precondition_failed(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::PRECONDITION_FAILED, detail).with_code("STALE_VERSION")
    }

    /// A failure the client cannot fix. The real `error` is logged under a fresh request ID;
    /// in production the client sees only `public`, in dev the real error too.
    pub fn internal(
        status: StatusCode,
        code: &'static str,
        public: &str,
        error: &dyn std::fmt::Display,
        is_prod: bool,
    ) -> Self {
        let request_id = uuid::Uuid::new_v4().to_string();
        tracing::error!(%request_id, code, error = %error, "REST request failed");
        let detail = if is_prod {
            public.to_string()
        } else {
            format!("{public}: {error}")
        };
        Self::new(status, detail)
            .with_code(code)
            .with_request_id(request_id)
    }

    /// An authentication failure as problem+json, keeping the RFC 6750 challenge header.
    pub fn from_auth_error(err: AuthError) -> Self {
        let (status, code) = match &err {
            AuthError::Missing | AuthError::InvalidToken(_) => {
                (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED")
            }
            AuthError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            AuthError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "AUTH_UNAVAILABLE"),
        };
        let message = err.message();
        let mut problem = Self::new(status, message).with_code(code);
        if let Some(www) = err
            .into_response()
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .cloned()
        {
            problem.headers.push((header::WWW_AUTHENTICATE, www));
        }
        problem
    }

    /// Maps an execution failure: typed constraint violations keep their stable code and
    /// message; custom SQL errors keep their status (`PTxxx` messages unmasked, `errors.map`
    /// ones masked); anything else is masked in production.
    pub fn from_execution_error(
        err: &ExecutionError,
        catalog: &DatabaseCatalog,
        is_prod: bool,
        errors_config: &crate::config::models::ErrorsConfig,
    ) -> Self {
        let mut root = err;
        while let ExecutionError::Mutation { source, .. } = root {
            root = source;
        }

        if let Some(db) = sql_errors::db_error(root) {
            if let Some(custom) = sql_errors::resolve(db, errors_config, is_prod) {
                return Self::from_custom(custom);
            }
        }

        if let ExecutionError::TooManyRows { max, actual } = root {
            return Self::from_limit(&crate::plan::LimitError::BulkRows {
                rows: *actual as usize,
                max: *max as usize,
            });
        }

        if let Some(v) = err.violation(catalog) {
            let status = violation_status(&v.code);
            let mut problem = Self::new(status, v.message)
                .with_code(v.code)
                .with_fields(v.fields);
            if is_prod {
                let request_id = uuid::Uuid::new_v4().to_string();
                tracing::debug!(%request_id, code = ?problem.code, "REST request rejected by a constraint");
                problem = problem.with_request_id(request_id);
            }
            return problem;
        }

        if err.is_source_unavailable() {
            tracing::warn!(error = %err, "a foreign data source is unavailable");
            return Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "A foreign data source is unavailable",
            )
            .with_code("SOURCE_UNAVAILABLE");
        }

        if err.is_timeout() {
            return Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The request ran longer than the statement timeout and was cancelled",
            )
            .with_code("STATEMENT_TIMEOUT");
        }

        if is_prod && err.is_permission_denied() {
            return Self::new(StatusCode::FORBIDDEN, "Permission denied")
                .with_code("PERMISSION_DENIED");
        }

        match root {
            ExecutionError::EmptyResult => Self::not_found("No matching record found"),
            ExecutionError::Rejected(msg) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, msg.clone()).with_code("REJECTED")
            }
            ExecutionError::StaleVersion { table, .. } => Self::precondition_failed(format!(
                "The `{table}` row changed since the `If-Match` version was read"
            )),
            _ => Self::internal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_SERVER_ERROR",
                "An unexpected database error occurred",
                err,
                is_prod,
            ),
        }
    }

    fn from_custom(custom: sql_errors::CustomError) -> Self {
        let mut problem = Self::new(custom.status, custom.detail).with_code(custom.code);
        if let Some(title) = custom.title {
            problem = problem.with_title(title);
        }
        if let Some(problem_type) = custom.problem_type {
            problem = problem.with_type(problem_type);
        }
        if let Some(hint) = custom.hint {
            problem = problem.with_hint(hint);
        }
        problem.extensions = custom.extensions;
        if let Some(detail) = custom.db_detail {
            problem
                .extensions
                .entry("databaseDetail")
                .or_insert(serde_json::json!(detail));
        }
        problem
    }
}

/// HTTP status of a typed constraint code.
pub fn violation_status(code: &str) -> StatusCode {
    match code {
        "STALE_VERSION" => StatusCode::PRECONDITION_FAILED,
        c if c.ends_with("_NOT_FOUND") => StatusCode::NOT_FOUND,
        c if c.ends_with("_TAKEN")
            || c.ends_with("_CONFLICT")
            || matches!(
                c,
                "UNIQUE_VIOLATION" | "PRIMARY_KEY_VIOLATION" | "FOREIGN_KEY_VIOLATION"
            ) =>
        {
            StatusCode::CONFLICT
        }
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

impl IntoResponse for ProblemDetails {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            headers.insert(name.clone(), value.clone());
        }
        let mut response = (status, headers, Json(self)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_use_about_blank_and_keep_the_code() {
        let p = ProblemDetails::new(StatusCode::CONFLICT, "taken").with_code("EMAIL_TAKEN");
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["type"], "about:blank");
        assert_eq!(json["code"], "EMAIL_TAKEN");
        assert_eq!(json["status"], 409);
        assert!(json.get("headers").is_none());
    }

    #[test]
    fn violation_codes_map_to_statuses() {
        assert_eq!(violation_status("EMAIL_TAKEN"), StatusCode::CONFLICT);
        assert_eq!(
            violation_status("STALE_VERSION"),
            StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(violation_status("AUTHOR_NOT_FOUND"), StatusCode::NOT_FOUND);
        assert_eq!(
            violation_status("NAME_REQUIRED"),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn internal_errors_are_masked_in_prod() {
        let err = "relation \"secret\" does not exist";
        let prod = ProblemDetails::internal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_SERVER_ERROR",
            "unexpected",
            &err,
            true,
        );
        assert_eq!(prod.detail, "unexpected");
        assert!(prod.request_id.is_some());
        let dev = ProblemDetails::internal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_SERVER_ERROR",
            "unexpected",
            &err,
            false,
        );
        assert!(dev.detail.contains("secret"));
    }

    #[test]
    fn custom_pt_and_mapped_errors_serialise_properly() {
        // Plain PT error
        let mut p = ProblemDetails::new(StatusCode::PAYMENT_REQUIRED, "Payment required")
            .with_code("PT402")
            .with_hint("Upgrade subscription");
        p.extensions
            .insert("extra".into(), serde_json::json!("val"));
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["status"], 402);
        assert_eq!(json["code"], "PT402");
        assert_eq!(json["hint"], "Upgrade subscription");
        assert_eq!(json["extra"], "val");

        // Custom mapped error
        let p2 = ProblemDetails::new(StatusCode::CONFLICT, "Duplicate record")
            .with_code("CUSTOM_CONFLICT")
            .with_type("https://example.com/errors/conflict")
            .with_title("Custom Conflict");
        let json2 = serde_json::to_value(&p2).unwrap();
        assert_eq!(json2["status"], 409);
        assert_eq!(json2["code"], "CUSTOM_CONFLICT");
        assert_eq!(json2["type"], "https://example.com/errors/conflict");
        assert_eq!(json2["title"], "Custom Conflict");
    }
}
