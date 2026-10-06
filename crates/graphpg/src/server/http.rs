use crate::catalog::DatabaseCatalog;
use crate::compiler::{CompiledOperation, CompiledQuery, SqlCompiler};
use crate::execution::{ExecutionEngine, ExecutionError};
use crate::openapi::OpenApiGenerator;
use crate::planner::{PlannerError, QueryPlanner};
use crate::server::cache::RequestCache;
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

#[derive(Debug, Deserialize)]
pub struct GraphQLRequest {
    pub query: String,
    #[serde(rename = "operationName")]
    pub operation_name: Option<String>,
    pub variables: Option<serde_json::Value>,
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

/// The error of a failed execution: a constraint violation with its code and input fields,
/// or the database's message.
pub(crate) fn execution_error(
    err: &ExecutionError,
    catalog: &DatabaseCatalog,
    mask_errors: bool,
) -> (StatusCode, GraphQLError) {
    let path = match err {
        ExecutionError::Mutation { field, .. } => {
            Some(vec![serde_json::Value::String(field.clone())])
        }
        _ => None,
    };
    match err.violation(catalog) {
        Some(v) => {
            let extensions = serde_json::json!({
                "code": v.code,
                "type": v.type_name,
                "constraint": v.constraint,
                "fields": v.fields,
            });
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
    /// Cached OpenAPI 3.1 document.
    pub openapi_document: std::sync::RwLock<Arc<serde_json::Value>>,
    /// The engine configuration, if passed to the state.
    pub config: Option<crate::config::Config>,
    /// Runtime mode (dev or prod).
    pub mode: crate::config::Mode,
    /// Answer `__schema` and `__type` (`graphql.introspection`).
    pub introspection: bool,
    /// Hide the database's own message of unexpected failures (`errors.detail: masked`).
    pub mask_errors: bool,
    /// Safety limits (page size, depth, cost, bulk rows, body size, groups): enforced on every
    /// plan, whichever API made it.
    pub limits: crate::config::models::LimitsConfig,
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
        let openapi_doc = OpenApiGenerator::generate_with_schema(
            &catalog,
            &crate::config::Config::default(),
            &default_schema,
            Some(&schema),
        );
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
            openapi_document: std::sync::RwLock::new(Arc::new(openapi_doc)),
            config: None,
            mode: crate::config::Mode::Prod,
            introspection: false,
            mask_errors: true,
            limits: crate::config::models::LimitsConfig::default(),
        }
    }

    /// Sets the safety limits (the defaults of [`LimitsConfig`](crate::config::models::LimitsConfig) otherwise).
    pub fn with_limits(mut self, limits: crate::config::models::LimitsConfig) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the runtime mode.
    pub fn with_mode(mut self, mode: crate::config::Mode) -> Self {
        self.mode = mode;
        self.mask_errors = mode == crate::config::Mode::Prod;
        self
    }

    /// Allows or refuses introspection queries (off by default, like `graphql.introspection`).
    pub fn with_introspection(mut self, on: bool) -> Self {
        self.introspection = on;
        self
    }

    /// Sets how much of an unexpected failure clients see, overriding the mode's default.
    pub fn with_error_detail(mut self, detail: crate::config::models::ErrorDetail) -> Self {
        self.mask_errors = detail == crate::config::models::ErrorDetail::Masked;
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
        self.limits = config.limits.clone();
        let cat = self.current_catalog();
        let doc = OpenApiGenerator::generate_with_schema(
            &cat,
            &config,
            &self.default_schema,
            Some(&self.current_schema()),
        );
        *self.openapi_document.write().unwrap() = Arc::new(doc);
        self.config = Some(config);
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

    pub fn current_openapi_document(&self) -> Arc<serde_json::Value> {
        self.openapi_document.read().unwrap().clone()
    }

    pub fn reload(&self, schema: Valid<Schema>, catalog: DatabaseCatalog) {
        let cfg = self.config.as_ref().cloned().unwrap_or_default();
        let doc = OpenApiGenerator::generate_with_schema(
            &catalog,
            &cfg,
            &self.default_schema,
            Some(&schema),
        );
        *self.openapi_document.write().unwrap() = Arc::new(doc);
        *self.schema.write().unwrap() = Arc::new(schema);
        *self.catalog.write().unwrap() = Arc::new(catalog);
        // Statements prepared, documents validated and operations compiled for the previous
        // schema will not be reused
        self.pool.manager().statement_caches.clear();
        self.cache.clear();
    }
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
    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
}

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
    let res = GraphQLResponse {
        errors: Some(errors),
        data: None,
        extensions,
    };
    (status, graphql_headers(), Json(res)).into_response()
}

pub(crate) async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "engine": "graphpg",
        "version": "0.1.0"
    }))
}

pub(crate) async fn rest_handler() -> impl IntoResponse {
    (StatusCode::OK, "hello world")
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

pub(crate) async fn graphql_handler(
    State(state): State<Arc<ServerState>>,
    request_headers: HeaderMap,
    Json(req): Json<GraphQLRequest>,
) -> Response {
    // Authenticate before anything else is looked at
    let auth_ctx = match state.authenticate(&request_headers).await {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let schema = state.current_schema();
    let catalog = state.current_catalog();

    // 1. Parse and validate the document (once per distinct document)
    let doc = match state.cache.document(&req.query) {
        Some(doc) => doc,
        None => {
            let parsed = tracing::info_span!("graphql.parse").in_scope(|| {
                ExecutableDocument::parse_and_validate(&schema, &req.query, "request.graphql")
                    .map_err(|err| err.to_string())
            });
            match parsed {
                Ok(doc) => {
                    let doc = Arc::new(doc);
                    state.cache.store_document(&req.query, doc.clone());
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
    .with_limits(state.limits.plan_limits());
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
    let mut wal_position: Option<String> = None;
    let mut extensions = serde_json::Map::new();
    if has_concrete {
        // An operation run before with the same variables reuses its compiled statements
        let cached = state.cache.operation(
            &req.query,
            req.operation_name.as_deref(),
            req.variables.as_ref(),
        );
        let cache_status = if cached.is_some() { "hit" } else { "miss" };
        let compiled = match cached {
            Some(compiled) => compiled,
            None => {
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
                let compiled = Arc::new(compiled);
                state.cache.store_operation(
                    &req.query,
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
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    message,
                    extensions_value(&extensions),
                );
            }
        };
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
                let (status, error) = execution_error(&err, &catalog, state.mask_errors);
                return errors_response(status, vec![error], extensions_value(&extensions));
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

    let res = GraphQLResponse {
        errors: None,
        data: Some(serde_json::Value::Object(data)),
        extensions: (!extensions.is_empty()).then_some(serde_json::Value::Object(extensions)),
    };
    let mut headers = graphql_headers();
    if let Some(lsn) = wal_position.and_then(|l| l.parse().ok()) {
        headers.insert(WAL_POSITION_HEADER, lsn);
    }
    (StatusCode::OK, headers, Json(res)).into_response()
}
