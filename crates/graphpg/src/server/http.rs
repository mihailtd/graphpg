use crate::catalog::DatabaseCatalog;
use crate::compiler::{CompiledOperation, CompiledQuery, SqlCompiler};
use crate::execution::{ExecutionEngine, ExecutionError};
use crate::openapi::OpenApiGenerator;
use crate::planner::{PlannerError, QueryPlanner};
use crate::server::cache::RequestCache;
use crate::server::sql_errors;
use apollo_compiler::executable::Operation;
use apollo_compiler::response::JsonMap;
use apollo_compiler::validation::Valid;
use apollo_compiler::{ExecutableDocument, Schema};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use deadpool_postgres::{
    CreatePoolError, ManagerConfig, Pool, PoolConfig, RecyclingMethod, Runtime,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tracing::Instrument;

#[derive(Debug, Default, Deserialize)]
pub struct GraphQLRequest {
    /// The operation's text; absent when the request names a persisted query.
    #[serde(default)]
    pub query: Option<String>,
    #[serde(rename = "operationName")]
    pub operation_name: Option<String>,
    pub variables: Option<serde_json::Value>,
    /// `persistedQuery` (`{ "version": 1, "sha256Hash": ... }`) for persisted queries.
    #[serde(default)]
    pub extensions: Option<serde_json::Value>,
    /// The id of a persisted query in the manifest.
    #[serde(default, rename = "documentId")]
    pub document_id: Option<String>,
    /// What a field error does to the response: `PROPAGATE` (default), `NULL` or `HALT`
    /// (see [`partial`](super::partial)).
    #[serde(default, rename = "onError")]
    pub on_error: Option<String>,
}

/// `GET /graphql`: a query in the URL, its JSON values (`variables`, `extensions`) as text.
#[derive(Debug, Deserialize)]
pub struct GraphQLGetParams {
    pub query: Option<String>,
    #[serde(rename = "operationName")]
    pub operation_name: Option<String>,
    pub variables: Option<String>,
    pub extensions: Option<String>,
    #[serde(rename = "documentId")]
    pub document_id: Option<String>,
    #[serde(rename = "onError")]
    pub on_error: Option<String>,
}

impl GraphQLGetParams {
    fn into_request(self) -> Result<GraphQLRequest, String> {
        let json = |name: &str, text: Option<String>| match text.filter(|t| !t.is_empty()) {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|err| format!("`{name}` is not JSON: {err}")),
        };
        Ok(GraphQLRequest {
            query: self.query,
            operation_name: self.operation_name.filter(|n| !n.is_empty()),
            variables: json("variables", self.variables)?,
            extensions: json("extensions", self.extensions)?,
            document_id: self.document_id,
            on_error: self.on_error,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct GraphQLError {
    pub message: String,
    /// Response path of the field the error belongs to (the root mutation field).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Vec<serde_json::Value>>,
    /// `code` (a `<Type>ErrorCode` value, or `BAD_USER_INPUT`) and details clients act on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

impl GraphQLError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: None,
            extensions: None,
        }
    }
}

/// Errors of an operation planning rejected: one per invalid input value.
pub(crate) fn planner_errors(err: &PlannerError) -> Vec<GraphQLError> {
    match err {
        PlannerError::Input(errors) => errors
            .iter()
            .map(|e| GraphQLError {
                message: e.message.clone(),
                path: None,
                extensions: Some(serde_json::json!({ "code": e.code, "field": e.path })),
            })
            .collect(),
        PlannerError::Limit(limit) => vec![GraphQLError {
            message: limit.to_string(),
            path: None,
            extensions: Some(serde_json::json!({
                "code": limit.code(),
                "limit": limit.limit(),
                "actual": limit.actual(),
            })),
        }],
        other => vec![GraphQLError {
            message: other.to_string(),
            path: None,
            extensions: Some(serde_json::json!({ "code": "BAD_USER_INPUT" })),
        }],
    }
}

/// The error under the mutation fields that wrapped it.
fn root_error(err: &ExecutionError) -> Option<&ExecutionError> {
    let mut root = err;
    while let ExecutionError::Mutation { source, .. } = root {
        root = source;
    }
    Some(root)
}

/// The error of a failed execution: a constraint violation with its code and input fields,
/// or the database's message.
pub(crate) fn execution_error(
    err: &ExecutionError,
    catalog: &DatabaseCatalog,
    mask_errors: bool,
    errors_config: &crate::config::models::ErrorsConfig,
) -> (StatusCode, GraphQLError) {
    let path = match err {
        ExecutionError::Mutation { field, .. } => {
            Some(vec![serde_json::Value::String(field.clone())])
        }
        _ => None,
    };

    if let Some(custom) =
        sql_errors::db_error(err).and_then(|db| sql_errors::resolve(db, errors_config, mask_errors))
    {
        let mut extensions = serde_json::json!({ "code": custom.code });
        if let Some(hint) = custom.hint {
            extensions["hint"] = serde_json::json!(hint);
        }
        return (
            StatusCode::OK,
            GraphQLError {
                message: custom.detail,
                path,
                extensions: Some(extensions),
            },
        );
    }

    if err.is_input_error() {
        return (
            StatusCode::OK,
            GraphQLError {
                message: match root_error(err) {
                    Some(ExecutionError::InvalidInput(message)) => message.clone(),
                    _ => match sql_errors::db_error(err) {
                        Some(db) => db.message().to_string(),
                        None => err.to_string(),
                    },
                },
                path,
                extensions: Some(serde_json::json!({ "code": "BAD_USER_INPUT" })),
            },
        );
    }

    if let Some(ExecutionError::TooManyRows { max, actual }) = root_error(err) {
        return (
            StatusCode::OK,
            GraphQLError {
                message: err.to_string(),
                path,
                extensions: Some(serde_json::json!({
                    "code": "BULK_LIMIT_EXCEEDED",
                    "limit": max,
                    "actual": actual,
                })),
            },
        );
    }

    match err.violation(catalog) {
        Some(v) => {
            let mut extensions = serde_json::json!({
                "code": v.code,
                "type": v.type_name,
                "constraint": v.constraint,
                "fields": v.fields,
            });
            if !v.validation_errors.is_empty() {
                extensions["validationErrors"] = serde_json::json!(v.validation_errors);
            }
            let message = match err {
                ExecutionError::Mutation { field, .. } => {
                    format!(
                        "{} (in mutation field `{field}`; the operation was rolled back)",
                        v.message
                    )
                }
                _ => v.message,
            };
            (
                StatusCode::OK,
                GraphQLError {
                    message,
                    path,
                    extensions: Some(extensions),
                },
            )
        }
        None if mask_errors && err.is_permission_denied() => (
            StatusCode::OK,
            GraphQLError {
                message: "Permission denied".to_string(),
                path,
                extensions: Some(serde_json::json!({ "code": "PERMISSION_DENIED" })),
            },
        ),
        None if err.is_source_unavailable() => {
            tracing::warn!(error = %err, "a foreign data source is unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                GraphQLError {
                    message: "a foreign data source is unavailable".to_string(),
                    path,
                    extensions: Some(serde_json::json!({ "code": "SOURCE_UNAVAILABLE" })),
                },
            )
        }
        None if err.is_connection_lost() => {
            tracing::warn!(error = %err, "the database connection was lost during a request");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                GraphQLError {
                    message: "the database connection was lost while the request ran: retry it"
                        .to_string(),
                    path,
                    extensions: Some(serde_json::json!({ "code": "DATABASE_UNAVAILABLE" })),
                },
            )
        }
        None if err.is_schema_changed() => {
            tracing::warn!(error = %err, "the database schema changed under a request");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                GraphQLError {
                    message: "the database schema changed while the request ran: retry it"
                        .to_string(),
                    path,
                    extensions: Some(serde_json::json!({ "code": "SCHEMA_CHANGED" })),
                },
            )
        }
        None if err.is_timeout() => (
            StatusCode::SERVICE_UNAVAILABLE,
            GraphQLError {
                message: "the request ran longer than the statement timeout and was cancelled"
                    .to_string(),
                path,
                extensions: Some(serde_json::json!({ "code": "STATEMENT_TIMEOUT" })),
            },
        ),
        None => {
            let rejected = matches!(err, ExecutionError::Mutation { source, .. } if matches!(**source, ExecutionError::Rejected(_)));
            let (status, code) = if rejected {
                (StatusCode::OK, "BAD_USER_INPUT")
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_SERVER_ERROR")
            };
            if mask_errors && !rejected {
                // The client cannot act on a database error: it gets an ID to quote, the
                // operator gets the error under that ID
                let request_id = uuid::Uuid::new_v4().to_string();
                tracing::error!(%request_id, code, error = %err, "GraphQL request failed");
                return (
                    status,
                    GraphQLError {
                        message: "An unexpected database error occurred".to_string(),
                        path,
                        extensions: Some(
                            serde_json::json!({ "code": code, "requestId": request_id }),
                        ),
                    },
                );
            }
            (
                status,
                GraphQLError {
                    message: err.to_string(),
                    path,
                    extensions: Some(serde_json::json!({ "code": code })),
                },
            )
        }
    }
}

#[derive(Debug, Serialize)]
pub struct GraphQLResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<GraphQLError>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

/// Opt-in diagnostics added to each response's `extensions`.
/// These expose generated SQL to every client — enable for local development only.
#[derive(Debug, Clone, Copy, Default)]
pub struct DebugOptions {
    /// Include the compiled SQL and its parameters (`extensions.sql`).
    pub sql: bool,
    /// Include the `EXPLAIN ANALYZE` plan (`extensions.explain`). Implies `sql`.
    pub explain: bool,
}

impl From<crate::config::DebugConfig> for DebugOptions {
    fn from(c: crate::config::DebugConfig) -> Self {
        Self {
            sql: c.sql,
            explain: c.explain,
        }
    }
}

pub struct ServerState {
    pub pool: Pool,
    pub schema: std::sync::RwLock<Arc<Valid<Schema>>>,
    pub catalog: std::sync::RwLock<Arc<DatabaseCatalog>>,
    pub default_schema: String,
    pub debug: DebugOptions,
    /// Validated documents and compiled operations of requests seen before.
    pub cache: RequestCache,
    /// Tag statements with sqlcommenter comments naming the GraphQL operation (on by
    /// default; `SQL_COMMENTS=0` turns it off).
    pub sql_comments: bool,
    /// Read replicas queries may run on (mutations run on `pool`, the primary).
    pub replicas: Vec<Replica>,
    next_replica: std::sync::atomic::AtomicUsize,
    /// Authenticates requests (OAuth 2.0 bearer tokens); `None` when authentication is off.
    pub auth: Option<Arc<super::Authenticator>>,
    /// Serve the GraphiQL IDE at `/graphiql` (development and pre-production only).
    pub graphiql: bool,
    /// Serve the Scalar OpenAPI UI at `/scalar` (development and pre-production only).
    pub scalar: bool,
    /// Serve the GraphQL API at `/graphql`, `/export` and `/import/...`.
    pub graphql_enabled: bool,
    /// Serve the REST API and OpenAPI document at `/rest`.
    pub openapi_enabled: bool,
    /// Serve the OpenAPI 3.1 document at `openapi_path` (default `/openapi.json`).
    pub openapi_spec: bool,
    /// Path to serve the OpenAPI document at (defaults to `/openapi.json`).
    pub openapi_path: String,
    /// The OpenAPI 3.1 document, built when first asked for (on a large schema it takes
    /// seconds and tens of megabytes, which a server that does not serve it never pays).
    openapi_document: std::sync::Mutex<Option<Arc<serde_json::Value>>>,
    /// The settings a reload can change while serving ([`ServerState::settings`]).
    settings: std::sync::RwLock<Arc<Settings>>,
    /// Runtime mode (dev or prod).
    pub mode: crate::config::Mode,
    /// Answer `__schema` and `__type` (`graphql.introspection`).
    pub introspection: bool,
    /// Request rates per client (`limits.rate_limit`); `None` when off.
    pub rate_limiter: Option<Arc<super::RateLimiter>>,
    /// Persisted queries and the allow-list (`graphql.persisted_queries`); `None` when off.
    pub persisted: Option<Arc<super::PersistedQueries>>,
    /// Live queries (`subscriptions.enabled`); `None` when off. Its re-runs start with
    /// [`Hub::start`](super::subscriptions::Hub::start).
    pub subscriptions: Option<Arc<super::subscriptions::Hub>>,
    /// Where requests that find the schema out of date ask for a reload (set by the reload
    /// listener; see [`ServerState::schema_out_of_date`]).
    pub reload_requests: std::sync::OnceLock<tokio::sync::mpsc::Sender<&'static str>>,
    /// Schemas swapped in since startup ([`ServerState::reload`]).
    pub reloads: std::sync::atomic::AtomicU64,
}

/// The settings of a [`ServerState`] that a reload with a changed configuration swaps in, with
/// the schema (dev mode's watcher, see [`ServerState::reload_with_config`]): requests read
/// them as they start. The others are fixed when the server starts.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The engine configuration, if passed to the state.
    pub config: Option<crate::config::Config>,
    /// Safety limits (page size, depth, cost, bulk rows, groups): enforced on every plan,
    /// whichever API made it. `request_body_bytes` and `rate_limit` are read at startup.
    pub limits: crate::config::models::LimitsConfig,
    /// Custom error configurations and detail level.
    pub errors: crate::config::models::ErrorsConfig,
    /// Hide the database's own message of unexpected failures (`errors.detail: masked`).
    pub mask_errors: bool,
}

/// A read replica: used for queries while it is a standby (`pg_is_in_recovery()`).
pub struct Replica {
    pub pool: Pool,
    pub standby: std::sync::atomic::AtomicBool,
}

/// Header of a response to a mutation: the primary's WAL position after it committed.
pub const WAL_POSITION_HEADER: &str = "x-wal-position";
/// Request header: queries run on a replica only once it has replayed this WAL position
/// (a client's own writes, from [`WAL_POSITION_HEADER`]).
pub const MIN_WAL_POSITION_HEADER: &str = "x-min-wal-position";

impl ServerState {
    pub fn new(
        pool: Pool,
        schema: Valid<Schema>,
        catalog: DatabaseCatalog,
        default_schema: String,
        debug: DebugOptions,
    ) -> Self {
        Self {
            pool,
            schema: std::sync::RwLock::new(Arc::new(schema)),
            catalog: std::sync::RwLock::new(Arc::new(catalog)),
            default_schema,
            debug,
            cache: RequestCache::default(),
            sql_comments: true,
            replicas: Vec::new(),
            next_replica: std::sync::atomic::AtomicUsize::new(0),
            auth: None,
            graphiql: false,
            scalar: false,
            graphql_enabled: true,
            openapi_enabled: true,
            openapi_spec: false,
            openapi_path: "/openapi.json".to_string(),
            openapi_document: std::sync::Mutex::new(None),
            settings: std::sync::RwLock::new(Arc::new(Settings {
                mask_errors: true,
                ..Settings::default()
            })),
            mode: crate::config::Mode::Prod,
            introspection: false,
            rate_limiter: None,
            persisted: None,
            subscriptions: None,
            reload_requests: std::sync::OnceLock::new(),
            reloads: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Sets the safety limits (the defaults of [`LimitsConfig`](crate::config::models::LimitsConfig) otherwise).
    /// A request failed on a column, table, function or type the database no longer has (see
    /// `ExecutionError::is_schema_changed`): read the database again, as `NOTIFY` does, so a
    /// server that does not watch DDL (prod) stops compiling requests from the old schema.
    pub fn schema_out_of_date(&self) {
        if let Some(tx) = self.reload_requests.get() {
            let _ = tx.try_send(super::reload::OUT_OF_DATE);
        }
    }

    pub fn with_limits(mut self, limits: crate::config::models::LimitsConfig) -> Self {
        self.rate_limiter = super::RateLimiter::new(&limits.rate_limit);
        self.settings_mut().limits = limits;
        self
    }

    /// The settings to change while building the state.
    fn settings_mut(&mut self) -> &mut Settings {
        Arc::make_mut(self.settings.get_mut().unwrap())
    }

    /// The current settings: those of the configuration last loaded.
    pub fn settings(&self) -> Arc<Settings> {
        self.settings.read().unwrap().clone()
    }

    /// Accepts persisted queries (see [`PersistedQueries`](super::PersistedQueries)), or not
    /// (`None`, the default).
    pub fn with_persisted_queries(
        mut self,
        persisted: Option<Arc<super::PersistedQueries>>,
    ) -> Self {
        self.persisted = persisted;
        self
    }

    /// Sets custom error configurations.
    pub fn with_errors(mut self, errors: crate::config::models::ErrorsConfig) -> Self {
        self.settings_mut().errors = errors;
        self
    }

    /// Sets the runtime mode.
    pub fn with_mode(mut self, mode: crate::config::Mode) -> Self {
        self.mode = mode;
        self.settings_mut().mask_errors = mode == crate::config::Mode::Prod;
        self
    }

    /// Allows or refuses introspection queries (off by default, like `graphql.introspection`).
    pub fn with_introspection(mut self, on: bool) -> Self {
        self.introspection = on;
        self
    }

    /// Sets how much of an unexpected failure clients see, overriding the mode's default.
    pub fn with_error_detail(mut self, detail: crate::config::models::ErrorDetail) -> Self {
        self.settings_mut().mask_errors = detail == crate::config::models::ErrorDetail::Masked;
        self
    }

    /// Returns true if running in production mode.
    pub fn is_production(&self) -> bool {
        self.mode == crate::config::Mode::Prod
    }

    /// Authenticates requests with `auth` (see [`Authenticator::new`](super::Authenticator::new)).
    pub fn with_auth(mut self, auth: Option<Arc<super::Authenticator>>) -> Self {
        self.auth = auth;
        self
    }

    /// Adds read replicas (standbys are detected by [`refresh_replicas`](Self::refresh_replicas)).
    pub fn with_replicas(mut self, pools: Vec<Pool>) -> Self {
        self.replicas = pools
            .into_iter()
            .map(|pool| Replica {
                pool,
                standby: std::sync::atomic::AtomicBool::new(false),
            })
            .collect();
        self
    }

    /// Checks which replicas are standbys (`pg_is_in_recovery()`); unreachable ones are not
    /// used until a later check finds them again.
    pub async fn refresh_replicas(&self) {
        for replica in &self.replicas {
            let standby = match replica.pool.get().await {
                Ok(client) => client
                    .query_one("SELECT pg_is_in_recovery()", &[])
                    .await
                    .map(|r| r.get::<_, bool>(0))
                    .unwrap_or(false),
                Err(_) => false,
            };
            replica
                .standby
                .store(standby, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A connection for a query: a standby replica (round-robin) that has replayed
    /// `min_lsn` when given, else the primary. Also says which it is.
    pub async fn read_pool(
        &self,
        min_lsn: Option<&str>,
    ) -> Result<(deadpool_postgres::Object, &'static str), String> {
        use std::sync::atomic::Ordering;
        let n = self.replicas.len();
        let start = if n == 0 {
            0
        } else {
            self.next_replica.fetch_add(1, Ordering::Relaxed) % n
        };
        for i in 0..n {
            let replica = &self.replicas[(start + i) % n];
            if !replica.standby.load(Ordering::Relaxed) {
                continue;
            }
            let Ok(client) = replica.pool.get().await else {
                replica.standby.store(false, Ordering::Relaxed);
                continue;
            };
            let caught_up = match min_lsn {
                None => true,
                Some(lsn) => client
                    .query_one(
                        "SELECT coalesce(pg_last_wal_replay_lsn() >= $1::text::pg_lsn, false)",
                        &[&lsn],
                    )
                    .await
                    .map(|r| r.get::<_, bool>(0))
                    .unwrap_or(false),
            };
            if caught_up {
                return Ok((client, "replica"));
            }
        }
        self.pool
            .get()
            .await
            .map(|c| (c, "primary"))
            .map_err(|err| format!("Database unavailable: {err}"))
    }

    /// Serves the GraphiQL IDE at `/graphiql`, or not (the default).
    pub fn with_graphiql(mut self, on: bool) -> Self {
        self.graphiql = on;
        self
    }

    /// Whether the OpenAPI document is served at [`openapi_path`](Self::with_openapi_path): when
    /// the REST API is on and either `openapi.spec` or the Scalar reference (which shows the
    /// document) is.
    pub fn serves_openapi_document(&self) -> bool {
        self.openapi_enabled && (self.openapi_spec || self.scalar)
    }

    /// Serves the Scalar OpenAPI UI at `/scalar`, or not (the default).
    pub fn with_scalar(mut self, on: bool) -> Self {
        self.scalar = on;
        self
    }

    /// Turns sqlcommenter tags on or off.
    pub fn with_sql_comments(mut self, on: bool) -> Self {
        self.sql_comments = on;
        self
    }

    /// Serves the GraphQL API at `/graphql`, `/export` and `/import/...`, or not (the default is on).
    pub fn with_graphql_enabled(mut self, on: bool) -> Self {
        self.graphql_enabled = on;
        self
    }

    /// Serves the REST API and OpenAPI document at `/rest`, or not (the default is on).
    pub fn with_openapi_enabled(mut self, on: bool) -> Self {
        self.openapi_enabled = on;
        self
    }

    /// Alias for [`with_openapi_enabled`](Self::with_openapi_enabled).
    pub fn with_rest_enabled(self, on: bool) -> Self {
        self.with_openapi_enabled(on)
    }

    /// Serves the OpenAPI 3.1 document at [`openapi_path`](Self::with_openapi_path), or not (the default).
    pub fn with_openapi_spec(mut self, on: bool) -> Self {
        self.openapi_spec = on;
        self
    }

    /// Sets the path to serve the OpenAPI document at (e.g. `/openapi.json`).
    pub fn with_openapi_path(mut self, mut path: String) -> Self {
        if !path.starts_with('/') {
            path = format!("/{path}");
        }
        self.openapi_path = path;
        self
    }

    /// Configures the server state from [`Config`](crate::config::Config).
    pub fn with_config(mut self, config: crate::config::Config) -> Self {
        self.graphql_enabled = config.graphql.enabled;
        self.openapi_enabled = config.openapi.enabled;
        self.openapi_spec = config.openapi.spec;
        self.openapi_path = if config.openapi.path.starts_with('/') {
            config.openapi.path.clone()
        } else {
            format!("/{}", config.openapi.path)
        };
        self.graphiql = config.graphql.ui;
        self.scalar = config.openapi.ui;
        self.sql_comments = config.database.sql_comments;
        self.rate_limiter = super::RateLimiter::new(&config.limits.rate_limit);
        self.subscriptions = config
            .subscriptions
            .enabled
            .then(|| super::subscriptions::Hub::new(config.subscriptions.clone()));
        *self.openapi_document.lock().unwrap() = None;
        let settings = self.settings_mut();
        settings.limits = config.limits.clone();
        settings.errors = config.errors.clone();
        settings.config = Some(config);
        self
    }

    /// Uses `doc` as the OpenAPI document (one already generated for this schema and
    /// configuration, by the pinned-schema check at startup).
    pub fn with_openapi_document(self, doc: serde_json::Value) -> Self {
        *self.openapi_document.lock().unwrap() = Some(Arc::new(doc));
        self
    }

    /// The role and settings a request runs with: `None` when authentication is off.
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<super::AuthSessionContext>, super::AuthError> {
        match &self.auth {
            Some(auth) => auth.authenticate(headers).await.map(Some),
            None => Ok(None),
        }
    }

    pub fn current_schema(&self) -> Arc<Valid<Schema>> {
        self.schema.read().unwrap().clone()
    }

    pub fn current_catalog(&self) -> Arc<DatabaseCatalog> {
        self.catalog.read().unwrap().clone()
    }

    /// The OpenAPI document of the current schema, generated on the first call after a start
    /// or a reload.
    pub fn current_openapi_document(&self) -> Arc<serde_json::Value> {
        let mut doc = self.openapi_document.lock().unwrap();
        if let Some(doc) = &*doc {
            return doc.clone();
        }
        let config = self.settings().config.clone().unwrap_or_default();
        let generated = Arc::new(OpenApiGenerator::generate_with_schema(
            &self.current_catalog(),
            &config,
            &self.default_schema,
            Some(&self.current_schema()),
        ));
        *doc = Some(generated.clone());
        generated
    }

    /// Swaps in a schema read with a changed configuration, and the settings of that
    /// configuration a running server can change ([`Settings`]). Returns the settings that
    /// changed but are fixed until a restart, by their path in the configuration
    /// (`server.port`, `limits.rate_limit`).
    pub fn reload_with_config(
        &self,
        schema: Valid<Schema>,
        catalog: DatabaseCatalog,
        config: crate::config::Config,
    ) -> Vec<String> {
        let previous = self.settings();
        let fixed = previous
            .config
            .as_ref()
            .map(|before| fixed_settings_changed(before, &config))
            .unwrap_or_default();
        let detail_changed = previous
            .config
            .as_ref()
            .is_some_and(|before| before.errors.detail != config.errors.detail);
        let settings = Settings {
            mask_errors: if detail_changed {
                config.errors.detail == crate::config::models::ErrorDetail::Masked
            } else {
                previous.mask_errors
            },
            limits: config.limits.clone(),
            errors: config.errors.clone(),
            config: Some(config),
        };
        *self.settings.write().unwrap() = Arc::new(settings);
        self.reload(schema, catalog);
        fixed
    }

    pub fn reload(&self, schema: Valid<Schema>, catalog: DatabaseCatalog) {
        if let Some(persisted) = &self.persisted {
            for problem in persisted.check(&schema) {
                tracing::warn!("persisted query manifest: {problem}");
            }
        }
        *self.schema.write().unwrap() = Arc::new(schema);
        *self.catalog.write().unwrap() = Arc::new(catalog);
        self.reloads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self.openapi_document.lock().unwrap() = None;
        // Statements prepared, documents validated and operations compiled for the previous
        // schema will not be reused
        self.pool.manager().statement_caches.clear();
        self.cache.clear();
        if let Some(hub) = &self.subscriptions {
            hub.schema_changed();
        }
    }
}

/// The settings a running server cannot change (the listener, connections, authentication,
/// layers and background tasks built at startup), as paths in the configuration.
const FIXED_SETTINGS: &[&str] = &[
    "/server",
    "/database/pool_size",
    "/database/statement_cache",
    "/database/sql_comments",
    "/auth",
    "/debug/sql",
    "/debug/explain",
    "/graphql/enabled",
    "/graphql/ui",
    "/graphql/introspection",
    "/graphql/sdl",
    "/graphql/sdl_path",
    "/graphql/subgraph_path",
    "/graphql/federation",
    "/graphql/persisted_queries",
    "/openapi/enabled",
    "/openapi/spec",
    "/openapi/path",
    "/openapi/ui",
    "/headers",
    "/limits/request_body_bytes",
    "/limits/rate_limit",
    "/limits/statement_timeout",
    "/subscriptions",
    "/events",
    "/telemetry",
    "/reload",
    "/watch",
];

/// The [`FIXED_SETTINGS`] that differ between two configurations, as dotted paths.
fn fixed_settings_changed(
    before: &crate::config::Config,
    after: &crate::config::Config,
) -> Vec<String> {
    let (Ok(before), Ok(after)) = (serde_json::to_value(before), serde_json::to_value(after))
    else {
        return Vec::new();
    };
    FIXED_SETTINGS
        .iter()
        .filter(|path| before.pointer(path) != after.pointer(path))
        .map(|path| path[1..].replace('/', "."))
        .collect()
}

/// Creates a PostgreSQL connection pool. Connections are opened lazily on first use.
pub fn create_pool(database_url: &str, max_size: usize) -> Result<Pool, CreatePoolError> {
    create_pool_with(database_url, max_size, None)
}

/// Like [`create_pool`], with a `statement_timeout` for every connection: a statement running
/// longer is cancelled (see [`ExecutionError::is_timeout`]).
pub fn create_pool_with(
    database_url: &str,
    max_size: usize,
    statement_timeout: Option<std::time::Duration>,
) -> Result<Pool, CreatePoolError> {
    let mut cfg = deadpool_postgres::Config::new();
    cfg.url = Some(database_url.to_string());
    if let Some(timeout) = statement_timeout {
        cfg.options = Some(format!("-c statement_timeout={}", timeout.as_millis()));
    }
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    cfg.pool = Some(PoolConfig::new(max_size));
    cfg.builder(NoTls)
        .map_err(CreatePoolError::Config)?
        .runtime(Runtime::Tokio1)
        .post_create(deadpool_postgres::Hook::async_fn(|client, _| {
            Box::pin(async move {
                client
                    .batch_execute(EXTENSION_SEARCH_PATH)
                    .await
                    .map_err(deadpool_postgres::HookError::Backend)
            })
        }))
        .build()
        .map_err(CreatePoolError::Build)
}

/// Run on every new connection: the schemas of the installed extensions not on its
/// `search_path` go at its end. graphpg's statements call extensions' functions, operators and
/// types unqualified (`word_similarity`, `<=>`, `::vector`, `ST_DWithin`), and installations
/// often keep extensions in a schema of their own (Supabase: `extensions`). Appended, they
/// shadow nothing; set once per connection, they cost requests nothing.
const EXTENSION_SEARCH_PATH: &str = "
    SELECT set_config('search_path', concat_ws(', ', nullif(current_setting('search_path'), ''), missing), false)
    FROM (
      SELECT string_agg(DISTINCT quote_ident(n.nspname), ', ') AS missing
      FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
      WHERE n.nspname <> ALL (current_schemas(true))
    ) m
    WHERE missing IS NOT NULL";

fn graphql_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/graphql-response+json; charset=utf-8"
            .parse()
            .unwrap(),
    );
    headers
}

fn error_response(
    status: StatusCode,
    message: String,
    extensions: Option<serde_json::Value>,
) -> Response {
    errors_response(status, vec![GraphQLError::new(message)], extensions)
}

fn errors_response(
    status: StatusCode,
    errors: Vec<GraphQLError>,
    extensions: Option<serde_json::Value>,
) -> Response {
    let code = errors
        .first()
        .and_then(|e| e.extensions.as_ref())
        .and_then(|ext| ext.get("code"))
        .and_then(|code| code.as_str())
        .unwrap_or("GRAPHQL_ERROR")
        .to_string();
    let res = GraphQLResponse {
        errors: Some(errors),
        data: None,
        extensions,
    };
    let mut response = (status, graphql_headers(), Json(res)).into_response();
    response.extensions_mut().insert(ErrorCode(code));
    response
}

pub(crate) async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "engine": "graphpg",
        "version": "0.1.0"
    }))
}

pub(crate) async fn live_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok"
    }))
}

pub(crate) async fn ready_handler(State(state): State<Arc<ServerState>>) -> Response {
    let check = match state.pool.get().await {
        Ok(client) => client
            .query_one("SELECT 1", &[])
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Err(err) => Err(err.to_string()),
    };
    match check {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready" })),
        )
            .into_response(),
        Err(err) => {
            let error = if state.settings().mask_errors {
                "Database unavailable".to_string()
            } else {
                format!("Database unavailable: {err}")
            };
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "status": "not_ready",
                    "error": error
                })),
            )
                .into_response()
        }
    }
}

/// `GET /rest`: what the REST API serves, as links: its resources (tables and views), its
/// functions, and the OpenAPI document describing them when it is served. Authenticated like
/// every REST route: an invalid token, or none without an anonymous role, is refused.
pub(crate) async fn rest_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Response {
    if !state.openapi_enabled {
        return super::rest::ProblemDetails::not_found("The REST API is disabled").into_response();
    }
    if let Err(err) = state.authenticate(&headers).await {
        return super::rest::ProblemDetails::from_auth_error(err).into_response();
    }
    let catalog = state.current_catalog();
    let path_of = |schema: &str, name: &str| {
        if schema == state.default_schema {
            format!("{}/{name}", super::rest::REST_PREFIX)
        } else {
            format!("{}/{schema}/{name}", super::rest::REST_PREFIX)
        }
    };
    let resources: Vec<serde_json::Value> = catalog
        .tables
        .iter()
        .filter(|t| {
            let ops = &t.operations;
            ops.list || ops.read || ops.create || ops.update || ops.delete
        })
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "schema": t.schema,
                "type": t.type_name,
                "path": path_of(&t.schema, &t.name),
            })
        })
        .collect();
    let functions: Vec<serde_json::Value> =
        super::rest::rpc_functions(&catalog, &state.default_schema, None)
            .into_iter()
            .map(|f| {
                let path = if f.schema == state.default_schema {
                    format!("{}/rpc/{}", super::rest::REST_PREFIX, f.name)
                } else {
                    format!("{}/{}/rpc/{}", super::rest::REST_PREFIX, f.schema, f.name)
                };
                serde_json::json!({ "name": f.name, "schema": f.schema, "path": path })
            })
            .collect();
    let mut index = serde_json::json!({ "resources": resources, "functions": functions });
    if state.serves_openapi_document() {
        index["openapi"] = serde_json::json!(state.openapi_path);
    }
    Json(index).into_response()
}

fn sdl_response(sdl: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        sdl,
    )
        .into_response()
}

/// `graphql.sdl_path`: the GraphQL SDL of the catalog the server is running on.
pub(crate) async fn graphql_sdl_handler(State(state): State<Arc<ServerState>>) -> Response {
    sdl_response(crate::schema::SchemaBuilder::generate_sdl(
        &state.current_catalog(),
    ))
}

/// `graphql.subgraph_path`: the Apollo Federation subgraph SDL.
pub(crate) async fn graphql_subgraph_handler(State(state): State<Arc<ServerState>>) -> Response {
    sdl_response(
        crate::schema::SchemaBuilder::generate_federation_subgraph_sdl(&state.current_catalog()),
    )
}

pub(crate) async fn openapi_spec_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !state.serves_openapi_document() {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }
    let doc = state.current_openapi_document();
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    let wants_yaml = accept.contains("application/yaml")
        || accept.contains("text/yaml")
        || accept.contains("application/x-yaml");

    if wants_yaml {
        match serde_yaml::to_string(&*doc) {
            Ok(yaml) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
                yaml,
            )
                .into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!("Failed to serialize OpenAPI YAML: {err}"),
            )
                .into_response(),
        }
    } else {
        match serde_json::to_string(&*doc) {
            Ok(json_str) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                json_str,
            )
                .into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!("Failed to serialize OpenAPI JSON: {err}"),
            )
                .into_response(),
        }
    }
}

/// Pretty-prints generated SQL for humans (the executed statement is unchanged).
fn format_sql(sql: &str) -> String {
    let options = sqlformat::FormatOptions {
        dialect: sqlformat::Dialect::PostgreSql,
        joins_as_top_level: true,
        max_inline_arguments: Some(80),
        ..Default::default()
    };
    sqlformat::format(sql, &sqlformat::QueryParams::None, &options)
}

/// `extensions.sql`: the statement of a query, or the statements of each mutation field.
pub(crate) fn debug_sql(compiled: &CompiledOperation) -> serde_json::Value {
    let statement =
        |q: &CompiledQuery| serde_json::json!({ "query": format_sql(&q.sql), "params": q.params });
    match compiled {
        CompiledOperation::Query(q) => statement(q),
        CompiledOperation::Mutation(mutations) => serde_json::json!({
            "mutations": mutations
                .iter()
                .map(|m| serde_json::json!({
                    "field": m.field_alias,
                    "writes": m.writes.iter().map(|w| statement(&w.statement.query)).collect::<Vec<_>>(),
                    "payload": statement(&m.payload.query),
                }))
                .collect::<Vec<_>>()
        }),
    }
}

/// Response key of a root field (its alias, or its name).
pub(crate) fn response_key(field: &apollo_compiler::executable::Field) -> &str {
    field.alias.as_ref().unwrap_or(&field.name).as_str()
}

/// Resolves `__schema` / `__type` / root `__typename` from the GraphQL schema.
/// Whether the selection reads `__schema` or `__type` at the root, in a fragment or not.
/// (`__typename` is not introspection: clients add it to every query.)
fn asks_introspection(
    doc: &apollo_compiler::ExecutableDocument,
    selections: &apollo_compiler::executable::SelectionSet,
) -> bool {
    use apollo_compiler::executable::Selection;
    selections
        .selections
        .iter()
        .any(|selection| match selection {
            Selection::Field(f) => matches!(f.name.as_str(), "__schema" | "__type"),
            Selection::InlineFragment(f) => asks_introspection(doc, &f.selection_set),
            Selection::FragmentSpread(f) => doc
                .fragments
                .get(&f.fragment_name)
                .is_some_and(|fragment| asks_introspection(doc, &fragment.selection_set)),
        })
}

pub(crate) fn execute_introspection(
    schema: &Valid<Schema>,
    doc: &Valid<ExecutableDocument>,
    operation: &Operation,
    variables: Option<&serde_json::Value>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let variables: JsonMap = match variables {
        Some(v) if !v.is_null() => {
            serde_json::from_value(v.clone()).map_err(|e| format!("Invalid variables: {e}"))?
        }
        _ => JsonMap::new(),
    };
    let to_message =
        |err: apollo_compiler::request::RequestError| err.to_graphql_error(&doc.sources).message;

    apollo_compiler::introspection::check_max_depth(doc, operation).map_err(to_message)?;
    let variables = apollo_compiler::request::coerce_variable_values(schema, operation, &variables)
        .map_err(to_message)?;
    let response = apollo_compiler::introspection::partial_execute(
        schema,
        &schema.implementers_map(),
        doc,
        operation,
        &variables,
    )
    .map_err(to_message)?;

    if let Some(err) = response.errors.first() {
        return Err(err.message.clone());
    }
    match response.data.map(serde_json::to_value) {
        Some(Ok(serde_json::Value::Object(map))) => Ok(map),
        _ => Ok(serde_json::Map::new()),
    }
}

/// The code of a GraphQL response's first error (`extensions.code`), for the metrics.
#[derive(Clone)]
struct ErrorCode(String);

/// What the metrics say of a GraphQL operation, learnt while answering it.
struct OperationMetric {
    kind: &'static str,
    name: String,
}

pub(crate) async fn graphql_handler(
    State(state): State<Arc<ServerState>>,
    request_headers: HeaderMap,
    extensions: axum::http::Extensions,
    Json(req): Json<GraphQLRequest>,
) -> Response {
    let peer = peer_of(&extensions);
    let sse = super::subscriptions::transport::wants_only_sse(&request_headers);
    let response = answer_graphql(state, request_headers, req, false, peer).await;
    if sse {
        super::subscriptions::transport::json_as_sse(response).await
    } else {
        response
    }
}

/// The address of the connection a request came on, when the server records it.
fn peer_of(extensions: &axum::http::Extensions) -> Option<std::net::IpAddr> {
    extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip())
}

/// `GET /graphql` with subscriptions on: a WebSocket upgrade (`graphql-transport-ws`), or a
/// query in the URL as without them.
pub(crate) async fn graphql_get_or_upgrade(
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Response {
    use axum::extract::FromRequestParts;
    let (mut parts, _body) = request.into_parts();
    let peer = peer_of(&parts.extensions);
    let upgrade = parts
        .headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if let (true, Some(hub)) = (upgrade, state.subscriptions.clone()) {
        return match axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &state)
            .await
        {
            Ok(ws) => {
                super::subscriptions::transport::websocket(state, hub, ws, parts.headers, peer)
            }
            Err(rejection) => rejection.into_response(),
        };
    }
    let params = match axum::extract::Query::<GraphQLGetParams>::try_from_uri(&parts.uri) {
        Ok(axum::extract::Query(params)) => params,
        Err(rejection) => return rejection.into_response(),
    };
    let sse = super::subscriptions::transport::wants_only_sse(&parts.headers);
    let response = match params.into_request() {
        Ok(req) => answer_graphql(state, parts.headers, req, true, peer).await,
        Err(message) => error_response(StatusCode::BAD_REQUEST, message, None),
    };
    if sse {
        super::subscriptions::transport::json_as_sse(response).await
    } else {
        response
    }
}

/// Answers `req` with an authenticated context (a WebSocket connection's): the response's
/// status and its JSON.
pub(crate) async fn execute_json(
    state: Arc<ServerState>,
    auth_ctx: Option<super::AuthSessionContext>,
    req: GraphQLRequest,
    peer: Option<std::net::IpAddr>,
) -> (StatusCode, serde_json::Value) {
    let mut metric = OperationMetric {
        kind: "",
        name: String::new(),
    };
    let response = run_authenticated(
        state,
        HeaderMap::new(),
        req,
        false,
        &mut metric,
        auth_ctx,
        peer,
    )
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| {
            serde_json::json!({ "errors": [{ "message": "the response is not JSON (@defer and @stream need HTTP)" }] })
        });
    (status, body)
}

/// `GET /graphql`: queries only (a mutation is `405`), so they can be cached by URL, which
/// persisted queries keep short.
pub(crate) async fn graphql_get_handler(
    State(state): State<Arc<ServerState>>,
    request_headers: HeaderMap,
    extensions: axum::http::Extensions,
    axum::extract::Query(params): axum::extract::Query<GraphQLGetParams>,
) -> Response {
    let peer = peer_of(&extensions);
    let sse = super::subscriptions::transport::wants_only_sse(&request_headers);
    let response = match params.into_request() {
        Ok(req) => answer_graphql(state, request_headers, req, true, peer).await,
        Err(message) => error_response(StatusCode::BAD_REQUEST, message, None),
    };
    if sse {
        super::subscriptions::transport::json_as_sse(response).await
    } else {
        response
    }
}

async fn answer_graphql(
    state: Arc<ServerState>,
    request_headers: HeaderMap,
    req: GraphQLRequest,
    get: bool,
    peer: Option<std::net::IpAddr>,
) -> Response {
    let start = std::time::Instant::now();
    let mut metric = OperationMetric {
        kind: "",
        name: String::new(),
    };
    let response = run_graphql(state, request_headers, req, get, &mut metric, peer).await;
    let error = match response.extensions().get::<ErrorCode>() {
        Some(code) => Some(code.0.clone()),
        None => (!response.status().is_success()).then(|| response.status().as_u16().to_string()),
    };
    crate::telemetry::metrics().graphql_operation(
        metric.kind,
        &metric.name,
        error.as_deref(),
        start.elapsed().as_secs_f64(),
    );
    response
}

async fn run_graphql(
    state: Arc<ServerState>,
    request_headers: HeaderMap,
    req: GraphQLRequest,
    get: bool,
    metric: &mut OperationMetric,
    peer: Option<std::net::IpAddr>,
) -> Response {
    // Authenticate before anything else is looked at
    let auth_ctx = match state.authenticate(&request_headers).await {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    run_authenticated(state, request_headers, req, get, metric, auth_ctx, peer).await
}

async fn run_authenticated(
    state: Arc<ServerState>,
    request_headers: HeaderMap,
    req: GraphQLRequest,
    get: bool,
    metric: &mut OperationMetric,
    auth_ctx: Option<super::AuthSessionContext>,
    peer: Option<std::net::IpAddr>,
) -> Response {
    // The text of the operation: sent, or persisted and named by hash or id
    let query = match super::persisted::resolve(
        state.persisted.as_deref(),
        req.query.as_deref(),
        req.extensions.as_ref(),
        req.document_id.as_deref(),
    ) {
        Ok(query) => query,
        Err(rejection) => {
            return errors_response(
                rejection.status,
                vec![GraphQLError {
                    message: rejection.message,
                    path: None,
                    extensions: Some(serde_json::json!({ "code": rejection.code })),
                }],
                None,
            )
        }
    };
    let schema = state.current_schema();
    let catalog = state.current_catalog();

    let on_error = match super::partial::ErrorBehavior::parse(req.on_error.as_deref()) {
        Ok(behavior) => behavior,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message, None),
    };

    // 1. Parse and validate the document (once per distinct document)
    let cached_doc = state.cache.document(&query);
    crate::telemetry::metrics().cache("document", cached_doc.is_some());
    let doc = match cached_doc {
        Some(doc) => doc,
        None => {
            let parsed = tracing::info_span!("graphql.parse").in_scope(|| {
                ExecutableDocument::parse_and_validate(&schema, &*query, "request.graphql")
                    .map_err(|err| err.to_string())
            });
            match parsed {
                Ok(doc) => {
                    let doc = Arc::new(doc);
                    state.cache.store_document(&query, doc.clone());
                    doc
                }
                Err(message) => return error_response(StatusCode::BAD_REQUEST, message, None),
            }
        }
    };
    let operation = match doc.operations.get(req.operation_name.as_deref()) {
        Ok(op) => op,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                err.to_graphql_error(&doc.sources).message,
                None,
            )
        }
    };
    let operation_label = operation
        .name
        .as_ref()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    metric.kind = match operation.operation_type {
        apollo_compiler::ast::OperationType::Query => "query",
        apollo_compiler::ast::OperationType::Mutation => "mutation",
        apollo_compiler::ast::OperationType::Subscription => "subscription",
    };
    metric.name = operation_label.clone();

    let is_subscription =
        operation.operation_type == apollo_compiler::ast::OperationType::Subscription;
    let stream_format = super::subscriptions::transport::stream_format(&request_headers);
    let sse_get = is_subscription
        && stream_format == Some(super::subscriptions::transport::StreamFormat::Sse);
    if get && operation.operation_type != apollo_compiler::ast::OperationType::Query && !sse_get {
        let mut response = error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("a {} cannot be sent with GET: use POST", metric.kind),
            None,
        );
        response
            .headers_mut()
            .insert(header::ALLOW, axum::http::HeaderValue::from_static("POST"));
        return response;
    }

    if !state.introspection && asks_introspection(&doc, &operation.selection_set) {
        return errors_response(
            StatusCode::OK,
            vec![GraphQLError {
                message: "GraphQL introspection is disabled".to_string(),
                path: None,
                extensions: Some(serde_json::json!({ "code": "INTROSPECTION_DISABLED" })),
            }],
            None,
        );
    }

    // Live queries: a stream of results, as long as the client keeps it open
    if is_subscription {
        let Some(hub) = state.subscriptions.clone() else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "subscriptions are off (`subscriptions.enabled`)".to_string(),
                None,
            );
        };
        let Some(format) = stream_format else {
            let mut response = error_response(
                StatusCode::NOT_ACCEPTABLE,
                "a subscription is a stream: send it with `Accept: text/event-stream` (GraphQL over SSE) or `multipart/mixed;subscriptionSpec=\"1.0\"`, or over a WebSocket (graphql-transport-ws)".to_string(),
                None,
            );
            response
                .extensions_mut()
                .insert(ErrorCode("NOT_ACCEPTABLE".to_string()));
            return response;
        };
        let compiled = match super::subscriptions::compile(
            &state,
            &query,
            &doc,
            operation,
            req.operation_name.as_deref(),
            req.variables.as_ref(),
        ) {
            Ok(compiled) => compiled,
            Err(rejection) => {
                let status = if rejection.status == StatusCode::OK {
                    StatusCode::BAD_REQUEST
                } else {
                    rejection.status
                };
                return errors_response(status, rejection.errors, None);
            }
        };
        let client = super::subscriptions::client_key(auth_ctx.as_ref(), peer);
        return super::subscriptions::transport::http_stream(
            state.clone(),
            hub,
            compiled,
            auth_ctx,
            client,
            format,
        )
        .await;
    }

    // Incremental Delivery (@defer and @stream) over HTTP multipart streaming
    if super::incremental::has_active_defer_or_stream(&doc, operation, req.variables.as_ref()) {
        if let Some(inc_plan) =
            super::incremental::analyze_incremental(&doc, operation, req.variables.as_ref())
        {
            return super::incremental::execute_incremental_stream(
                state,
                doc,
                inc_plan,
                req.variables,
                operation_label,
                auth_ctx,
                request_headers
                    .get(MIN_WAL_POSITION_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
            )
            .await;
        }
    }

    let planner = QueryPlanner::with_context(
        &catalog,
        &state.default_schema,
        Some(&**doc),
        req.variables.as_ref(),
    )
    .with_limits(state.settings().limits.plan_limits());
    let target_type = match operation.operation_type {
        apollo_compiler::ast::OperationType::Query => "Query",
        apollo_compiler::ast::OperationType::Mutation => "Mutation",
        apollo_compiler::ast::OperationType::Subscription => "Subscription",
    };
    let root_fields = planner.flatten_selections(&operation.selection_set, Some(target_type));
    let has_introspection = root_fields.iter().any(|f| f.name.starts_with("__"));
    let has_concrete = root_fields.iter().any(|f| !f.name.starts_with("__"));

    // 2. Introspection meta-fields are answered from the schema, without touching the DB
    let mut introspection_data = serde_json::Map::new();
    if has_introspection && operation.operation_type.is_query() {
        match execute_introspection(&schema, &doc, operation, req.variables.as_ref()) {
            Ok(data) => introspection_data = data,
            Err(message) => return error_response(StatusCode::BAD_REQUEST, message, None),
        }
    }

    // 3. Plan and compile the database-backed fields into a single SQL statement
    let mut sql_data = serde_json::Map::new();
    // Root fields that failed on their own (see `partial`), and whether `data` is then null
    let mut field_errors: Vec<(StatusCode, GraphQLError)> = Vec::new();
    let mut data_null = false;
    let mut wal_position: Option<String> = None;
    let mut extensions = serde_json::Map::new();
    if has_concrete {
        // An operation run before with the same variables reuses its compiled statements
        let cached = state.cache.operation(
            &query,
            req.operation_name.as_deref(),
            req.variables.as_ref(),
        );
        let cache_status = if cached.is_some() { "hit" } else { "miss" };
        crate::telemetry::metrics().cache("operation", cached.is_some());
        let compiled = match cached {
            Some(compiled) => compiled,
            None => {
                let planning = std::time::Instant::now();
                let span =
                    tracing::info_span!("graphql.plan", graphql.operation = %operation_label);
                let plan = match span.in_scope(|| planner.plan_operation(operation)) {
                    Ok(p) => p,
                    Err(err) => return errors_response(StatusCode::OK, planner_errors(&err), None),
                };
                let mut compiled = tracing::info_span!("graphql.compile")
                    .in_scope(|| SqlCompiler::compile_operation(&plan));
                if state.sql_comments {
                    let kind = if operation.operation_type.is_mutation() {
                        "mutation"
                    } else {
                        "query"
                    };
                    compiled.tag(&[
                        ("application", "graphpg"),
                        ("graphql_operation", &operation_label),
                        ("graphql_type", kind),
                    ]);
                }
                crate::telemetry::metrics().planning("graphql", planning.elapsed().as_secs_f64());
                let compiled = Arc::new(compiled);
                state.cache.store_operation(
                    &query,
                    req.operation_name.as_deref(),
                    req.variables.as_ref(),
                    compiled.clone(),
                );
                compiled
            }
        };

        if state.debug.sql || state.debug.explain {
            extensions.insert("sql".to_string(), debug_sql(&compiled));
            extensions.insert("cache".to_string(), serde_json::json!(cache_status));
        }
        let extensions_value = |ext: &serde_json::Map<String, serde_json::Value>| {
            (!ext.is_empty()).then(|| serde_json::Value::Object(ext.clone()))
        };

        // 4. Execute on a pooled connection (returned to the pool on drop): queries on a read
        // replica when there is one, mutations on the primary
        let is_mutation = matches!(*compiled, CompiledOperation::Mutation(_));
        let acquired = if is_mutation || state.debug.explain {
            state
                .pool
                .get()
                .await
                .map(|c| (c, "primary"))
                .map_err(|err| format!("Database unavailable: {err}"))
        } else {
            let min_lsn = request_headers
                .get(MIN_WAL_POSITION_HEADER)
                .and_then(|v| v.to_str().ok());
            state.read_pool(min_lsn).await
        };
        let (client, database) = match acquired {
            Ok(acquired) => acquired,
            Err(message) => {
                tracing::error!(error = %message, "failed to acquire database connection");
                return errors_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    vec![GraphQLError {
                        message,
                        path: None,
                        extensions: Some(serde_json::json!({ "code": "DATABASE_UNAVAILABLE" })),
                    }],
                    extensions_value(&extensions),
                );
            }
        };
        let client = crate::execution::engine::PanicSafe::new(client);
        if state.debug.sql || state.debug.explain {
            extensions.insert("database".to_string(), serde_json::json!(database));
        }

        // The operation runs to its end (commit or rollback) even when the client goes away:
        // dropped mid-transaction, the connection would go back to the pool still inside it,
        // with the request's role and claims
        let span = tracing::info_span!("graphql.execute", graphql.operation = %operation_label);
        let (explain_on, track_wal) = (
            state.debug.explain,
            is_mutation && !state.replicas.is_empty(),
        );
        let task = {
            let compiled = compiled.clone();
            let auth_ctx = auth_ctx.clone();
            async move {
                let mut explain = explain_on.then(Vec::new);
                let result = ExecutionEngine::run_with_auth(
                    &*client,
                    &compiled,
                    explain.as_mut(),
                    auth_ctx.as_ref(),
                )
                .await;
                // Where the primary's WAL is after the write, for reads of it from replicas
                let mut wal = None;
                if track_wal && result.is_ok() {
                    if let Ok(row) = client
                        .query_one("SELECT pg_current_wal_lsn()::text", &[])
                        .await
                    {
                        wal = row.get::<_, Option<String>>(0);
                    }
                }
                (result, explain, wal)
            }
        };
        let (result, explain, wal) = match tokio::spawn(task.instrument(span)).await {
            Ok(done) => done,
            Err(err) => {
                tracing::error!(error = %err, "the operation's task failed");
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the operation failed".to_string(),
                    extensions_value(&extensions),
                );
            }
        };
        wal_position = wal;
        if let Some(lines) = explain {
            extensions.insert("explain".to_string(), serde_json::json!(lines));
        }
        match result {
            Ok(serde_json::Value::Object(map)) => sql_data = map,
            Ok(_) => {}
            Err(err) => {
                if err.is_schema_changed() {
                    state.schema_out_of_date();
                }
                // A query failing because of its data: which root fields fail, and the
                // others' data. The request as a whole fails otherwise
                let roots: Vec<&apollo_compiler::executable::Field> = {
                    let mut seen = std::collections::HashSet::new();
                    root_fields
                        .iter()
                        .copied()
                        .filter(|f| !f.name.starts_with("__") && seen.insert(response_key(f)))
                        .collect()
                };
                // `Err`: the request fails with the operation's error
                let apart = if is_mutation
                    || on_error == super::partial::ErrorBehavior::Halt
                    || !err.is_data_error()
                {
                    Err(err)
                } else if roots.len() == 1 {
                    Ok(vec![Err(err)])
                } else {
                    let min_lsn = request_headers
                        .get(MIN_WAL_POSITION_HEADER)
                        .and_then(|v| v.to_str().ok());
                    let aliases: Vec<&str> = roots.iter().map(|f| response_key(f)).collect();
                    run_roots_apart(
                        &state,
                        &planner,
                        operation,
                        &aliases,
                        auth_ctx.clone(),
                        min_lsn,
                        &operation_label,
                    )
                    .await
                    .ok_or(err)
                };
                let results = match apart {
                    Ok(results) => results,
                    Err(err) => {
                        let settings = state.settings();
                        let (status, error) =
                            execution_error(&err, &catalog, settings.mask_errors, &settings.errors);
                        return errors_response(status, vec![error], extensions_value(&extensions));
                    }
                };
                for (field, result) in roots.iter().zip(results) {
                    let key = response_key(field);
                    match result {
                        Ok(serde_json::Value::Object(mut map)) => {
                            if let Some(value) = map.remove(key) {
                                sql_data.insert(key.to_string(), value);
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            let settings = state.settings();
                            let (status, mut error) = execution_error(
                                &err,
                                &catalog,
                                settings.mask_errors,
                                &settings.errors,
                            );
                            error.path = Some(vec![serde_json::json!(key)]);
                            sql_data.insert(key.to_string(), serde_json::Value::Null);
                            data_null |= super::partial::nulls_data(
                                on_error,
                                field.definition.ty.is_non_null(),
                            );
                            field_errors.push((status, error));
                        }
                    }
                }
            }
        }
    }

    // 5. Merge both partial results in the order the root fields were selected
    let mut data = serde_json::Map::new();
    for field in root_fields {
        let key = response_key(field);
        if data.contains_key(key) {
            continue;
        }
        if let Some(v) = introspection_data
            .remove(key)
            .or_else(|| sql_data.remove(key))
        {
            data.insert(key.to_string(), v);
        }
    }
    for (k, v) in sql_data {
        data.entry(k).or_insert(v);
    }

    // With data, the response is a success whatever failed in it (GraphQL over HTTP); with
    // none, it has the status of its first error
    let status = match field_errors.first() {
        Some((status, _)) if data_null => *status,
        _ => StatusCode::OK,
    };
    let first_code = field_errors.first().and_then(|(_, e)| {
        e.extensions
            .as_ref()
            .and_then(|ext| ext.get("code"))
            .and_then(|c| c.as_str())
            .map(str::to_string)
    });
    let res = GraphQLResponse {
        errors: (!field_errors.is_empty())
            .then(|| field_errors.into_iter().map(|(_, e)| e).collect()),
        data: Some(if data_null {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(data)
        }),
        extensions: (!extensions.is_empty()).then_some(serde_json::Value::Object(extensions)),
    };
    let mut headers = graphql_headers();
    if let Some(lsn) = wal_position.and_then(|l| l.parse().ok()) {
        headers.insert(WAL_POSITION_HEADER, lsn);
    }
    let mut response = (status, headers, Json(res)).into_response();
    if let Some(code) = first_code {
        response.extensions_mut().insert(ErrorCode(code));
    }
    response
}

/// Runs the root fields `aliases` of a query one by one in one snapshot (see
/// [`partial`](super::partial)): each one's data or error, in order. `None` when they cannot
/// be run (no connection, the transaction failed).
async fn run_roots_apart(
    state: &ServerState,
    planner: &QueryPlanner<'_>,
    operation: &Operation,
    aliases: &[&str],
    auth_ctx: Option<super::AuthSessionContext>,
    min_lsn: Option<&str>,
    operation_label: &str,
) -> Option<Vec<Result<serde_json::Value, ExecutionError>>> {
    let plan = planner.plan_operation(operation).ok()?;
    let queries: Vec<CompiledQuery> = aliases
        .iter()
        .map(|alias| {
            let mut compiled =
                CompiledOperation::Query(SqlCompiler::compile(&plan.only_root(alias)));
            if state.sql_comments {
                compiled.tag(&[
                    ("application", "graphpg"),
                    ("graphql_operation", operation_label),
                    ("graphql_type", "query"),
                    ("graphql_field", alias),
                ]);
            }
            match compiled {
                CompiledOperation::Query(query) => query,
                CompiledOperation::Mutation(_) => unreachable!("a query compiles to a query"),
            }
        })
        .collect();
    let (client, _) = state.read_pool(min_lsn).await.ok()?;
    let client = crate::execution::engine::PanicSafe::new(client);
    let span = tracing::info_span!("graphql.execute_apart", graphql.operation = %operation_label);
    // Run to the end even when the client goes away (see `run_graphql`)
    let task =
        async move { ExecutionEngine::run_apart(&*client, &queries, auth_ctx.as_ref()).await };
    match tokio::spawn(task.instrument(span)).await {
        Ok(Ok(results)) => Some(results),
        Ok(Err(err)) => {
            if err.is_schema_changed() {
                state.schema_out_of_date();
            }
            tracing::warn!(error = %err, "running the root fields apart failed");
            None
        }
        Err(_) => None,
    }
}
