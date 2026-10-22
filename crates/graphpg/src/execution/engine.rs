use crate::catalog::pg_array_literal;
use crate::compiler::{
    CompiledMutation, CompiledOperation, CompiledQuery, CompiledStatement, ParamSource,
    SqlCompiler, ValueRef, WriteContext,
};
use crate::plan::OperationPlan;
use std::collections::HashMap;
use thiserror::Error;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Statement};

#[derive(Error, Debug)]
pub enum ExecutionError {
    #[error("PostgreSQL execution error: {}", describe_pg_error(.0))]
    Postgres(#[from] tokio_postgres::Error),

    /// A data-modifying statement failed; `context` tells which input rows it wrote.
    #[error("PostgreSQL execution error: {}", describe_pg_error(.error))]
    Write {
        error: tokio_postgres::Error,
        context: Option<WriteContext>,
    },

    #[error("No data row returned from query")]
    EmptyResult,

    /// A write the input asked for cannot be done (e.g. a row to connect does not exist).
    #[error("{0}")]
    Rejected(String),

    /// An input value PostgreSQL cannot take, found before the statement ran (text with a
    /// NUL character).
    #[error("{0}")]
    InvalidInput(String),

    /// `expectedVersion` did not match: the row changed since that version was read.
    #[error("the `{table}` row changed since `expectedVersion` was read")]
    StaleVersion { schema: String, table: String },

    /// An update or delete by filter affected more rows than `limits.bulk_rows` allows.
    #[error("the operation writes {actual} rows; at most {max} are allowed")]
    TooManyRows { max: u64, actual: u64 },

    #[error("{source} (in mutation field `{field}`; the operation was rolled back)")]
    Mutation {
        field: String,
        #[source]
        source: Box<ExecutionError>,
    },
}

impl ExecutionError {
    /// `error.type` of the failure in metrics and traces: the SQLSTATE of a database error,
    /// else the kind of failure. A bounded set of values.
    pub fn metric_code(&self) -> String {
        match self {
            ExecutionError::Mutation { source, .. } => source.metric_code(),
            ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => {
                match e.as_db_error() {
                    Some(db) => db.code().code().to_string(),
                    None => "connection".to_string(),
                }
            }
            ExecutionError::EmptyResult => "empty_result".to_string(),
            ExecutionError::Rejected(_) => "rejected".to_string(),
            ExecutionError::InvalidInput(_) => "invalid_input".to_string(),
            ExecutionError::StaleVersion { .. } => "stale_version".to_string(),
            ExecutionError::TooManyRows { .. } => "too_many_rows".to_string(),
        }
    }

    /// The client's input is at fault: a value PostgreSQL cannot take, or one only the client
    /// writes that PostgreSQL rejected (a JSON path, a regular expression or a `LIKE` pattern:
    /// graphpg's own SQL has none, they are always bound parameters).
    pub fn is_input_error(&self) -> bool {
        match self {
            ExecutionError::Mutation { source, .. } => source.is_input_error(),
            ExecutionError::InvalidInput(_) => true,
            ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => {
                e.as_db_error().is_some_and(|db| {
                    let code = db.code();
                    *code == SqlState::INVALID_REGULAR_EXPRESSION
                        || *code == SqlState::INVALID_ESCAPE_SEQUENCE
                        // `\u0000` in a JSON value: no `jsonb` (nor text) holds it
                        || *code == SqlState::UNTRANSLATABLE_CHARACTER
                        || ((*code == SqlState::SYNTAX_ERROR
                            || *code == SqlState::INVALID_TEXT_REPRESENTATION)
                            && db.message().contains("jsonpath"))
                })
            }
            _ => false,
        }
    }

    /// The statement ran longer than the connection's `statement_timeout` and was cancelled.
    pub fn is_timeout(&self) -> bool {
        match self {
            ExecutionError::Mutation { source, .. } => source.is_timeout(),
            ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => {
                e.as_db_error().is_some_and(|db| {
                    *db.code() == SqlState::QUERY_CANCELED
                        && db.message().contains("statement timeout")
                })
            }
            _ => false,
        }
    }

    /// The database refused the statement for lack of privilege, or a Row-Level Security
    /// policy rejected a row (SQLSTATE `42501`).
    pub fn is_permission_denied(&self) -> bool {
        match self {
            ExecutionError::Mutation { source, .. } => source.is_permission_denied(),
            ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => {
                e.code() == Some(&SqlState::INSUFFICIENT_PRIVILEGE)
            }
            _ => false,
        }
    }

    /// A failure of the data a statement read (a bad value, a function raising, a privilege
    /// or policy, a foreign source that is down), as opposed to one of the database or the
    /// request as a whole (a timeout, resources, a crash): only the first kind can belong to
    /// one field.
    pub fn is_data_error(&self) -> bool {
        let ExecutionError::Postgres(e) = self else {
            return false;
        };
        let Some(db) = e.as_db_error() else {
            return false;
        };
        let class = &db.code().code()[..2];
        !matches!(class, "40" | "53" | "54" | "57" | "58" | "XX")
    }

    /// A foreign data wrapper could not reach its remote server: a *database* error of
    /// class `08` (connection exception). graphpg's own connection failing is never one (it
    /// is an I/O error, not something the server reports), so this one names a source.
    pub fn is_source_unavailable(&self) -> bool {
        let e = match self {
            ExecutionError::Mutation { source, .. } => return source.is_source_unavailable(),
            ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => e,
            _ => return false,
        };
        e.as_db_error()
            .is_some_and(|db| db.code().code().starts_with("08"))
    }

    /// The connection to the database broke while the operation ran: closed, or ended by the
    /// server shutting down or restarting (SQLSTATE `57P01`-`57P03`). Whether a write that
    /// was committing took effect is unknown.
    pub fn is_connection_lost(&self) -> bool {
        let e = match self {
            ExecutionError::Mutation { source, .. } => return source.is_connection_lost(),
            ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => e,
            _ => return false,
        };
        if e.is_closed() {
            return true;
        }
        match e.as_db_error() {
            Some(db) => {
                let code = db.code();
                *code == SqlState::ADMIN_SHUTDOWN
                    || *code == SqlState::CRASH_SHUTDOWN
                    || *code == SqlState::CANNOT_CONNECT_NOW
            }
            // An I/O failure of graphpg's own connection
            None => std::error::Error::source(e).is_some_and(|s| s.is::<std::io::Error>()),
        }
    }

    /// The statement names a column, table, function or type the database no longer has (or
    /// no longer has as the statement expects): the schema changed after the server last read
    /// it, and requests compiled from that reading fail until it reads it again.
    pub fn is_schema_changed(&self) -> bool {
        let e = match self {
            ExecutionError::Mutation { source, .. } => return source.is_schema_changed(),
            ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => e,
            _ => return false,
        };
        e.as_db_error().is_some_and(|db| {
            let code = db.code();
            *code == SqlState::UNDEFINED_COLUMN
                || *code == SqlState::UNDEFINED_TABLE
                || *code == SqlState::UNDEFINED_FUNCTION
                || *code == SqlState::UNDEFINED_OBJECT
                // A prepared statement whose table changed shape
                || (*code == SqlState::FEATURE_NOT_SUPPORTED
                    && db.message().contains("cached plan must not change result type"))
                // A column whose type changed, read into a row of its old type
                || (*code == SqlState::DATATYPE_MISMATCH
                    && db.message().starts_with("ROW() column has type"))
        })
    }

    /// A failure that running the operation again may not hit: a serialization failure
    /// (under `REPEATABLE READ` or `SERIALIZABLE`) or a deadlock.
    pub fn is_transient(&self) -> bool {
        let code = match self {
            ExecutionError::Mutation { source, .. } => return source.is_transient(),
            ExecutionError::Postgres(e) | ExecutionError::Write { error: e, .. } => e.code(),
            _ => None,
        };
        code.is_some_and(|c| {
            *c == SqlState::T_R_SERIALIZATION_FAILURE || *c == SqlState::T_R_DEADLOCK_DETECTED
        })
    }
}

/// Times an operation is attempted when it keeps failing with a serialization failure or
/// deadlock (it then fails with the last error).
pub const MAX_ATTEMPTS: u32 = 10;

/// Pause before attempt `attempt + 1`: exponential from 2 ms up to 256 ms, with jitter so
/// competing transactions spread out.
fn retry_delay(attempt: u32) -> std::time::Duration {
    let base = 2u64 << attempt.min(7);
    let jitter = (uuid::Uuid::new_v4().as_u128() % 1000) as u64;
    std::time::Duration::from_micros(base * 1000 / 2 + base * jitter / 2)
}

/// `tokio_postgres::Error`'s Display is just "db error" for server errors; surface the
/// server's message (and detail/hint when present) instead.
fn describe_pg_error(err: &tokio_postgres::Error) -> String {
    match err.as_db_error() {
        Some(db) => {
            let mut msg = format!("{} ({})", db.message(), db.code().code());
            if let Some(detail) = db.detail() {
                msg.push_str(&format!(" — {}", detail));
            }
            if let Some(hint) = db.hint() {
                msg.push_str(&format!(" Hint: {}", hint));
            }
            msg
        }
        None => err.to_string(),
    }
}

/// Prepared statements cached per pooled connection by default (`database.statement_cache`).
pub const MAX_CACHED_STATEMENTS: usize = 512;

static STATEMENT_CACHE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(MAX_CACHED_STATEMENTS);

/// Sets how many prepared statements each pooled connection caches (`database.statement_cache`;
/// 0: none). Each one holds a plan in its PostgreSQL backend: with many distinct operations on
/// a wide schema, a smaller cache keeps the backends' memory down.
pub fn set_statement_cache(size: usize) {
    STATEMENT_CACHE.store(size, std::sync::atomic::Ordering::Relaxed);
}

/// A connection statements are prepared on: plain clients prepare each time, pooled clients
/// through their per-connection statement cache.
pub trait Connection: Sync {
    fn client(&self) -> &Client;

    fn prepare(
        &self,
        sql: &str,
    ) -> impl std::future::Future<Output = Result<Statement, tokio_postgres::Error>> + Send;

    /// Whether a transaction may be open on the connection (then the engine nests in it with
    /// a savepoint instead of committing or rolling back the caller's transaction).
    fn may_be_in_transaction(&self) -> bool {
        true
    }
}

impl Connection for Client {
    fn client(&self) -> &Client {
        self
    }

    async fn prepare(&self, sql: &str) -> Result<Statement, tokio_postgres::Error> {
        Client::prepare(self, sql).await
    }
}

/// Repeated SQL skips PostgreSQL's parse/analyze/plan phase, which for nested JSON queries
/// often costs more than execution itself. The cache is per connection and bounded: it is
/// cleared once it holds `database.statement_cache` statements. Values, page sizes and offsets
/// are bound as parameters, so only distinct operations (and which arguments they pass)
/// produce distinct SQL.
impl Connection for deadpool_postgres::ClientWrapper {
    fn client(&self) -> &Client {
        self
    }

    async fn prepare(&self, sql: &str) -> Result<Statement, tokio_postgres::Error> {
        let limit = STATEMENT_CACHE.load(std::sync::atomic::Ordering::Relaxed);
        if limit == 0 {
            return Client::prepare(self, sql).await;
        }
        if self.statement_cache.size() >= limit {
            self.statement_cache.clear();
        }
        self.prepare_cached(sql).await
    }

    /// Pooled connections are handed out idle.
    fn may_be_in_transaction(&self) -> bool {
        false
    }
}

/// A pooled connection discarded, not returned to the pool, when dropped by a panicking task:
/// it may be inside a transaction holding a request's role and claims, which the next
/// request on it would run in.
pub struct PanicSafe(Option<deadpool_postgres::Object>);

impl PanicSafe {
    pub fn new(client: deadpool_postgres::Object) -> Self {
        Self(Some(client))
    }

    pub fn into_inner(mut self) -> deadpool_postgres::Object {
        self.0.take().expect("a connection until dropped")
    }
}

impl std::ops::Deref for PanicSafe {
    type Target = deadpool_postgres::ClientWrapper;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("a connection until dropped")
    }
}

impl Drop for PanicSafe {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            if std::thread::panicking() {
                drop(deadpool_postgres::Object::take(client));
            }
        }
    }
}

const SAVEPOINT: &str = "_graphql_operation";

/// The transaction statements run in: one of the engine's own, or a savepoint inside a
/// transaction the caller opened.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Transaction,
    Savepoint,
}

impl Scope {
    async fn begin<C: Connection>(conn: &C) -> Result<Self, tokio_postgres::Error> {
        let client = conn.client();
        if !conn.may_be_in_transaction() {
            client.batch_execute("BEGIN").await?;
            return Ok(Self::Transaction);
        }
        // SAVEPOINT fails outside a transaction block: then start one
        match client
            .batch_execute(&format!("SAVEPOINT {SAVEPOINT}"))
            .await
        {
            Ok(()) => Ok(Self::Savepoint),
            Err(err) if err.code() == Some(&SqlState::NO_ACTIVE_SQL_TRANSACTION) => {
                client.batch_execute("BEGIN").await?;
                Ok(Self::Transaction)
            }
            Err(err) => Err(err),
        }
    }

    async fn commit(self, client: &Client) -> Result<(), tokio_postgres::Error> {
        match self {
            Self::Transaction => client.batch_execute("COMMIT").await,
            Self::Savepoint => {
                client
                    .batch_execute(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                    .await
            }
        }
    }

    async fn rollback(self, client: &Client) -> Result<(), tokio_postgres::Error> {
        match self {
            Self::Transaction => client.batch_execute("ROLLBACK").await,
            Self::Savepoint => {
                client
                    .batch_execute(&format!(
                        "ROLLBACK TO SAVEPOINT {SAVEPOINT}; RELEASE SAVEPOINT {SAVEPOINT}"
                    ))
                    .await
            }
        }
    }
}

pub struct ExecutionEngine;

/// What the writes of a mutation field returned so far.
#[derive(Default)]
struct Written {
    /// Returned columns (as text) of each write node's row.
    nodes: HashMap<usize, HashMap<String, Option<String>>>,
    /// Rows of `collect` writes, in order.
    returned: Vec<Vec<Option<String>>>,
}

impl Written {
    /// `statement` with its deferred parameters filled in.
    fn resolve(
        &self,
        statement: &CompiledStatement,
        node_paths: &[String],
    ) -> Result<CompiledQuery, ExecutionError> {
        let value = |v: &ValueRef| -> Result<Option<String>, ExecutionError> {
            match v {
                ValueRef::Literal(text) => Ok(text.clone()),
                ValueRef::Node { node, column } => match self.nodes.get(node).and_then(|row| row.get(column)) {
                    Some(text) => Ok(text.clone()),
                    None if statement.strict => Err(ExecutionError::Rejected(format!(
                        "`{}` was not written (a trigger skipped it?), and rows nested under it need its values",
                        node_paths.get(*node).map(String::as_str).unwrap_or("?")
                    ))),
                    None => Ok(None),
                },
            }
        };
        let array = |items: Vec<Option<String>>| {
            let items = items
                .into_iter()
                .map(|t| t.map_or(serde_json::Value::Null, serde_json::Value::String))
                .collect();
            match pg_array_literal(&serde_json::Value::Array(items)) {
                serde_json::Value::String(literal) => literal,
                other => other.to_string(),
            }
        };
        let mut query = statement.query.clone();
        for (i, source) in &statement.deferred {
            query.params[*i] = match source {
                ParamSource::Value(v) => value(v)?.ok_or_else(|| {
                    ExecutionError::Rejected(
                        "a nested row refers to a NULL value of the row it belongs to".to_string(),
                    )
                })?,
                ParamSource::Array(items) => {
                    array(items.iter().map(value).collect::<Result<_, _>>()?)
                }
                ParamSource::Returned { column } => array(
                    self.returned
                        .iter()
                        .map(|row| row.get(*column).cloned().flatten())
                        .collect(),
                ),
            };
        }
        Ok(query)
    }
}

impl ExecutionEngine {
    /// Executes a planned operation and returns `{"data": ...}`.
    pub async fn execute(
        client: &Client,
        plan: &OperationPlan,
    ) -> Result<serde_json::Value, ExecutionError> {
        let compiled = SqlCompiler::compile_operation(plan);
        let data = Self::run(client, &compiled, None).await?;
        Ok(serde_json::json!({ "data": data }))
    }

    /// Runs a compiled operation and returns its `data` object.
    ///
    /// A query is one statement. Mutation fields run one after another — each sees what the
    /// previous ones wrote — in a single transaction that is rolled back if any of them fails
    /// (a savepoint, when the caller has a transaction open on the connection).
    ///
    /// With `explain`, the `EXPLAIN (ANALYZE, ...)` plan of every statement is appended to it.
    /// ANALYZE really executes a statement, so data-modifying ones are explained in a
    /// rolled-back savepoint (or transaction) and applied exactly once (sequences still advance).
    ///
    /// Transactions run at the database's `default_transaction_isolation` (set per database
    /// or role). An operation failing with a serialization failure or a deadlock is run
    /// again, up to [`MAX_ATTEMPTS`] times, when it ran in a transaction of its own (inside a
    /// caller's transaction, the caller has to retry).
    pub async fn run<C: Connection>(
        conn: &C,
        compiled: &CompiledOperation,
        explain: Option<&mut Vec<String>>,
    ) -> Result<serde_json::Value, ExecutionError> {
        Self::run_with_auth(conn, compiled, explain, None).await
    }

    /// Runs a compiled operation with an optional authenticated session context.
    ///
    /// When `auth_ctx` is present, queries and mutations run in a transaction whose role and
    /// settings it sets first (see [`AuthSessionContext::apply`]). Inside a transaction the
    /// caller opened, those stay in effect until that transaction ends.
    ///
    /// [`AuthSessionContext::apply`]: crate::server::AuthSessionContext::apply
    pub async fn run_with_auth<C: Connection>(
        conn: &C,
        compiled: &CompiledOperation,
        explain: Option<&mut Vec<String>>,
        auth_ctx: Option<&crate::server::AuthSessionContext>,
    ) -> Result<serde_json::Value, ExecutionError> {
        use tracing::Instrument as _;
        let kind = match compiled {
            CompiledOperation::Query(_) => "query",
            CompiledOperation::Mutation(_) => "mutation",
        };
        let span = tracing::info_span!(
            "db.operation",
            // `{db.system.name}`: the conventions' name when there is no single statement
            otel.name = "postgresql",
            otel.kind = "client",
            otel.status_code = tracing::field::Empty,
            db.system.name = "postgresql",
            // The older name, which Tempo's service graphs look for
            db.system = "postgresql",
            graphpg.operation.type = kind,
            error.type = tracing::field::Empty,
        );
        let start = std::time::Instant::now();
        let result = Self::run_with_retries(conn, compiled, explain, auth_ctx)
            .instrument(span.clone())
            .await;
        let error = result.as_ref().err().map(ExecutionError::metric_code);
        if let Some(code) = &error {
            span.record("error.type", code.as_str());
            span.record("otel.status_code", "ERROR");
        }
        crate::telemetry::metrics().db_operation(
            kind,
            error.as_deref(),
            start.elapsed().as_secs_f64(),
        );
        result
    }

    /// [`run_with_auth`](Self::run_with_auth), run again after a transient failure.
    async fn run_with_retries<C: Connection>(
        conn: &C,
        compiled: &CompiledOperation,
        mut explain: Option<&mut Vec<String>>,
        auth_ctx: Option<&crate::server::AuthSessionContext>,
    ) -> Result<serde_json::Value, ExecutionError> {
        let mut attempt = 1;
        loop {
            let (result, owned) =
                Self::run_once(conn, compiled, explain.as_deref_mut(), auth_ctx).await;
            match result {
                Err(err) if owned && err.is_transient() && attempt < MAX_ATTEMPTS => {
                    tracing::debug!(attempt, error = %err, "retrying the operation after a transient failure");
                    if let Some(lines) = explain.as_deref_mut() {
                        lines.clear();
                    }
                    tokio::time::sleep(retry_delay(attempt)).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// One attempt of [`run`](Self::run); also tells whether it ran in a transaction of its
    /// own (which the failure rolled back, so the operation can be run again).
    async fn run_once<C: Connection>(
        conn: &C,
        compiled: &CompiledOperation,
        mut explain: Option<&mut Vec<String>>,
        auth_ctx: Option<&crate::server::AuthSessionContext>,
    ) -> (Result<serde_json::Value, ExecutionError>, bool) {
        match compiled {
            CompiledOperation::Query(query) => {
                let owned = !conn.may_be_in_transaction();
                // An authenticated query needs a transaction for its role and settings
                let scope = match auth_ctx {
                    Some(_) => match Scope::begin(conn).await {
                        Ok(scope) => Some(scope),
                        Err(err) => return (Err(err.into()), false),
                    },
                    None => None,
                };
                let result = async {
                    if let Some(auth) = auth_ctx {
                        auth.apply(conn).await?;
                    }
                    if let Some(lines) = explain.as_deref_mut() {
                        // What EXPLAIN ANALYZE executes is rolled back
                        let plan = match scope {
                            Some(_) => Self::explain_rolled_back(conn.client(), query).await,
                            None => {
                                let explain_scope = Scope::begin(conn).await?;
                                let plan = Self::explain(conn.client(), query).await;
                                explain_scope.rollback(conn.client()).await?;
                                plan
                            }
                        };
                        lines.extend(plan?);
                    }
                    Self::query_data(conn, query).await
                }
                .await;
                let Some(scope) = scope else {
                    return (result, owned);
                };
                let ended = match &result {
                    Ok(_) => scope.commit(conn.client()).await,
                    Err(_) => scope.rollback(conn.client()).await,
                };
                (
                    result.and_then(|data| ended.map(|()| data).map_err(Into::into)),
                    owned,
                )
            }
            CompiledOperation::Mutation(mutations) => {
                let scope = match Scope::begin(conn).await {
                    Ok(scope) => scope,
                    Err(err) => return (Err(err.into()), false),
                };
                let owned = matches!(scope, Scope::Transaction);
                if let Some(auth) = auth_ctx {
                    if let Err(err) = auth.apply(conn).await {
                        let _ = scope.rollback(conn.client()).await;
                        return (Err(err), owned);
                    }
                }
                let mut data = serde_json::Map::new();
                for mutation in mutations {
                    match Self::run_mutation(conn, mutation, explain.as_deref_mut()).await {
                        Ok(value) => {
                            data.insert(mutation.field_alias.clone(), value);
                        }
                        Err(err) => {
                            if let Err(rollback) = scope.rollback(conn.client()).await {
                                return (Err(rollback.into()), false);
                            }
                            let err = ExecutionError::Mutation {
                                field: mutation.field_alias.clone(),
                                source: Box::new(err),
                            };
                            return (Err(err), owned);
                        }
                    }
                }
                // A serialization failure can surface at commit, which ends the transaction
                (
                    scope
                        .commit(conn.client())
                        .await
                        .map(|()| serde_json::Value::Object(data))
                        .map_err(Into::into),
                    owned,
                )
            }
        }
    }

    /// One mutation field inside the operation's transaction: its writes in order, each
    /// taking the values earlier ones returned (keys of rows created through nested
    /// relations), then the payload query with the keys and row count the writes returned.
    async fn run_mutation<C: Connection>(
        conn: &C,
        mutation: &CompiledMutation,
        mut explain: Option<&mut Vec<String>>,
    ) -> Result<serde_json::Value, ExecutionError> {
        let client = conn.client();
        if let Some(lines) = explain.as_deref_mut() {
            lines.push(format!("-- {}", mutation.field_alias));
        }

        let mut written = Written::default();
        let mut affected: u64 = 0;
        for write in &mutation.writes {
            let query = written.resolve(&write.statement, &mutation.node_paths)?;
            if let Some(lines) = explain.as_deref_mut() {
                lines.extend(Self::explain_rolled_back(client, &query).await?);
            }
            let failed = |error| ExecutionError::Write {
                error,
                context: write.statement.context.clone(),
            };
            Self::check_params(&query.params)?;
            let statement = conn.prepare(&query.sql).await?;
            let params = Self::param_refs(&query.params);
            let count = if write.returning.is_empty() && !write.collect {
                client.execute(&statement, &params).await.map_err(failed)? as usize
            } else {
                let rows = client.query(&statement, &params).await.map_err(failed)?;
                let texts = |row: &tokio_postgres::Row| {
                    (0..row.len())
                        .map(|c| row.get::<_, Option<String>>(c))
                        .collect::<Vec<_>>()
                };
                if write.guard
                    && rows
                        .iter()
                        .any(|row| row.get::<_, Option<String>>(0).as_deref() != Some("true"))
                {
                    let context = write.statement.context.as_ref();
                    return Err(ExecutionError::StaleVersion {
                        schema: context.map(|c| c.schema.clone()).unwrap_or_default(),
                        table: context.map(|c| c.table.clone()).unwrap_or_default(),
                    });
                }
                if write.collect {
                    written.returned.extend(rows.iter().map(texts));
                }
                // One row per node, in order (a row a trigger skipped leaves its nodes unknown)
                if rows.len() == write.row_nodes.len() {
                    for (row, &node) in rows.iter().zip(&write.row_nodes) {
                        written.nodes.insert(
                            node,
                            write.returning.iter().cloned().zip(texts(row)).collect(),
                        );
                    }
                }
                rows.len()
            };
            if let Some(message) = &write.expect_rows {
                if count != write.row_nodes.len() {
                    return Err(ExecutionError::Rejected(message.clone()));
                }
            }
            if write.counts {
                affected += count as u64;
            }
        }

        let mut payload = written.resolve(&mutation.payload, &mutation.node_paths)?;
        if let Some(i) = mutation.count_param {
            payload.params[i] = affected.to_string();
        }
        if let Some(lines) = explain {
            // Without writes the payload statement is the data-modifying one
            let plan = if mutation.writes.is_empty() {
                Self::explain_rolled_back(client, &payload).await?
            } else {
                Self::explain(client, &payload).await?
            };
            lines.extend(plan);
        }
        let data = match Self::query_data(conn, &payload).await {
            Err(ExecutionError::Postgres(error)) if mutation.payload.context.is_some() => {
                return Err(ExecutionError::Write {
                    error,
                    context: mutation.payload.context.clone(),
                })
            }
            other => other?,
        };
        let Some(limit) = &mutation.affected_limit else {
            return Ok(data);
        };
        let mut data = data;
        let actual = data.get(&limit.alias).and_then(serde_json::Value::as_u64);
        if let Some(actual) = actual.filter(|actual| *actual > limit.max) {
            return Err(ExecutionError::TooManyRows {
                max: limit.max,
                actual,
            });
        }
        if limit.hidden {
            if let Some(object) = data.as_object_mut() {
                object.remove(&limit.alias);
            }
        }
        Ok(data)
    }

    /// The plan of a data-modifying statement, explained (and so executed) in a savepoint
    /// that is rolled back.
    async fn explain_rolled_back(
        client: &Client,
        compiled: &CompiledQuery,
    ) -> Result<Vec<String>, ExecutionError> {
        client.batch_execute("SAVEPOINT _explain").await?;
        let result = Self::explain(client, compiled).await;
        client
            .batch_execute("ROLLBACK TO SAVEPOINT _explain")
            .await?;
        result
    }

    /// Runs query statements one after another and returns each one's `data` or error, in
    /// order: one failing does not stop the others. They run in one `REPEATABLE READ`,
    /// `READ ONLY` transaction, so all of them read the same snapshot, each in a savepoint
    /// of its own (inside a transaction the caller opened, in its snapshot). Fails as a whole
    /// only when the transaction does.
    pub async fn run_apart<C: Connection>(
        conn: &C,
        queries: &[CompiledQuery],
        auth_ctx: Option<&crate::server::AuthSessionContext>,
    ) -> Result<Vec<Result<serde_json::Value, ExecutionError>>, ExecutionError> {
        const FIELD: &str = "_graphql_field";
        let client = conn.client();
        let scope = Scope::begin(conn).await?;
        let run: Result<_, ExecutionError> = async {
            if scope == Scope::Transaction {
                client
                    .batch_execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
                    .await?;
            }
            if let Some(auth) = auth_ctx {
                auth.apply(conn).await?;
            }
            let mut results = Vec::with_capacity(queries.len());
            for query in queries {
                client.batch_execute(&format!("SAVEPOINT {FIELD}")).await?;
                let result = Self::query_data(conn, query).await;
                let end = match result {
                    Ok(_) => format!("RELEASE SAVEPOINT {FIELD}"),
                    Err(_) => format!("ROLLBACK TO SAVEPOINT {FIELD}; RELEASE SAVEPOINT {FIELD}"),
                };
                client.batch_execute(&end).await?;
                results.push(result);
            }
            Ok(results)
        }
        .await;
        match run {
            Ok(results) => {
                scope.commit(client).await?;
                Ok(results)
            }
            Err(err) => {
                let _ = scope.rollback(client).await;
                Err(err)
            }
        }
    }

    /// Runs a statement returning one row with a `data` column, its estimates first.
    pub(crate) async fn query_data<C: Connection>(
        conn: &C,
        query: &CompiledQuery,
    ) -> Result<serde_json::Value, ExecutionError> {
        Self::check_params(&query.params)?;
        let params = Self::resolved_params(conn.client(), query).await?;
        let statement = conn.prepare(&query.sql).await?;
        let row = conn
            .client()
            .query_one(&statement, &Self::param_refs(&params))
            .await?;
        // A function returning SQL NULL (e.g. a write refused for a stale revision) is JSON null, not a panic.
        Ok(row
            .get::<_, Option<serde_json::Value>>("data")
            .unwrap_or(serde_json::Value::Null))
    }

    /// The parameters of `query`, its row count estimates filled in.
    async fn resolved_params<'q>(
        client: &Client,
        query: &'q CompiledQuery,
    ) -> Result<std::borrow::Cow<'q, Vec<String>>, ExecutionError> {
        let mut params = std::borrow::Cow::Borrowed(&query.params);
        for estimate in &query.estimates {
            let rows = Self::estimate_rows(client, &estimate.query).await?;
            params.to_mut()[estimate.param] = rows.to_string();
        }
        Ok(params)
    }

    /// The planner's estimate of the rows a query returns (`EXPLAIN`, not run).
    async fn estimate_rows(client: &Client, query: &CompiledQuery) -> Result<u64, ExecutionError> {
        let sql = format!("EXPLAIN (FORMAT JSON) {}", query.sql);
        let row = client
            .query_one(&sql, &Self::param_refs(&query.params))
            .await?;
        let plan: serde_json::Value = row.get(0);
        Ok(plan[0]["Plan"]["Plan Rows"].as_f64().unwrap_or(0.0).round() as u64)
    }

    /// The `EXPLAIN (ANALYZE, BUFFERS, SERIALIZE, MEMORY)` plan of a statement, as text lines.
    /// Callers roll back what ANALYZE executes.
    pub async fn explain(
        client: &Client,
        compiled: &CompiledQuery,
    ) -> Result<Vec<String>, ExecutionError> {
        let explain_sql = format!(
            "EXPLAIN (ANALYZE, BUFFERS, SERIALIZE, MEMORY, FORMAT TEXT) {}",
            compiled.sql.trim_end().trim_end_matches(';')
        );
        let params = Self::resolved_params(client, compiled).await?;
        let rows = client
            .query(&explain_sql, &Self::param_refs(&params))
            .await?;
        Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
    }

    /// Refuses text PostgreSQL cannot hold: a NUL character, which no text type (nor `jsonb`)
    /// accepts. Without the check, the database's encoding error would be a server error.
    pub(crate) fn check_params(params: &[String]) -> Result<(), ExecutionError> {
        if params.iter().any(|p| p.contains('\0')) {
            return Err(ExecutionError::InvalidInput(
                "a text value contains the NUL character (\\u0000), which PostgreSQL cannot store"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn param_refs(params: &[String]) -> Vec<&(dyn ToSql + Sync)> {
        params.iter().map(|s| s as &(dyn ToSql + Sync)).collect()
    }
}
