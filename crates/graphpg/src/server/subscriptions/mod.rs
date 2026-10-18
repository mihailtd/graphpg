//! Live queries (see `docs/subscriptions.md`; decision record in issue #27): GraphQL
//! subscriptions whose result is sent again whenever it changes.
//!
//! A subscription's root field is one of the `Query` type's: it is planned and compiled as a
//! query is, and checked against the same limits. The [`Hub`] holds an instance's
//! subscriptions and runs them again every `subscriptions.interval`, sending the results that
//! changed. Nothing in the database tells it what changed, so any change is seen, whatever
//! made it: a function, a foreign table, `now()`, a policy reading another table.
//!
//! - Subscriptions compiled to the same SQL and run with the same claims are one statement:
//!   the query `LATERAL`-joined to `unnest()` of their parameters (all text) and of the hash
//!   of the result each was last sent; only the results whose hash changed come back (see
//!   [`batch`]). Identical subscriptions share one row.
//! - Claims are left out of that grouping when they cannot change the result: everything the
//!   statement reads is a table without Row-Level Security, and it calls no function. Then
//!   the subscriptions of every user are one statement.
//! - Each claims' statements run inside a savepoint, rolled back after them, so the next
//!   claims start from a clean transaction (and a failure stays in its savepoint). A round
//!   uses `subscriptions.concurrency` connections, on a replica when there is one.
//!
//! The transports (`graphql-transport-ws`, GraphQL over SSE, multipart HTTP) are in
//! [`transport`].
pub mod batch;
pub mod transport;

use crate::compiler::{CompiledOperation, CompiledQuery, SqlCompiler};
use crate::config::models::{SubscriptionSource, SubscriptionsConfig};
use crate::execution::{ExecutionEngine, ExecutionError};
use crate::planner::QueryPlanner;
use crate::server::http::{execution_error, planner_errors, GraphQLError, ServerState};
use crate::server::AuthSessionContext;
use apollo_compiler::executable::Operation;
use apollo_compiler::validation::Valid;
use apollo_compiler::ExecutableDocument;
use axum::http::StatusCode;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
use tokio::sync::mpsc;

/// Results queued for a subscription's client at most; while its queue is full, a changed
/// result is sent at a later round instead (a slow client gets the latest one, not each one).
const QUEUE: usize = 4;

/// Statements a transaction of a round runs at most, so a snapshot is not held for long (on
/// a replica, a long one holds back the replay of what it still sees).
const STATEMENTS_PER_TRANSACTION: usize = 200;

const SAVEPOINT: &str = "_graphpg_subscription";

/// Why a subscription was not started: an HTTP status and GraphQL errors.
#[derive(Debug)]
pub struct Rejection {
    pub status: StatusCode,
    pub errors: Vec<GraphQLError>,
}

impl Rejection {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            errors: vec![GraphQLError {
                message: message.into(),
                path: None,
                extensions: Some(serde_json::json!({ "code": code })),
            }],
        }
    }
}

/// An open subscription: its results, each a GraphQL response (`{"data": ...}`, or `data`
/// null with `errors`). Dropping it ends the subscription.
pub struct Subscription {
    pub updates: mpsc::Receiver<serde_json::Value>,
    /// When the access token it runs with expires (seconds since the Unix epoch).
    pub expires_at: Option<u64>,
    _guard: Guard,
}

/// Removes its subscription from the hub when dropped.
struct Guard {
    hub: Weak<Hub>,
    id: u64,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.upgrade() {
            hub.remove(self.id);
        }
    }
}

struct Entry {
    query: Arc<CompiledQuery>,
    auth: Option<AuthSessionContext>,
    /// Who counts against `max_per_subject`.
    client: String,
    /// Hash of the result last sent.
    last_hash: Option<String>,
    tx: mpsc::Sender<serde_json::Value>,
    /// Its first result was run (until then, rounds leave it out: it would be sent twice).
    ready: bool,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<u64, Entry>,
    per_client: HashMap<String, usize>,
}

/// An instance's subscriptions, run again every `subscriptions.interval`.
pub struct Hub {
    config: SubscriptionsConfig,
    registry: Mutex<Registry>,
    next_id: AtomicU64,
    /// Whether claims can change the result of a statement, by its SQL (see
    /// [`batch::CLAIMS_FREE_SQL`]); emptied when the schema is reloaded.
    claims_free: Mutex<HashMap<String, bool>>,
}

/// One statement of a round: a compiled query run for one or more subscriptions, after the
/// claims it is run with.
struct Job {
    settings: Option<Arc<AuthSessionContext>>,
    query: Arc<CompiledQuery>,
    rows: Vec<Row>,
}

/// One set of parameters of a job, and the subscriptions it is for (which were all sent the
/// same result last).
struct Row {
    params: Vec<String>,
    last_hash: Option<String>,
    ids: Vec<u64>,
}

/// What a run of a row found.
enum Outcome {
    /// A result differing from the last one sent: its hash and the response to send.
    Changed(String, serde_json::Value),
    Unchanged,
    /// A failure that may not last (a lost connection, a conflict with recovery on a
    /// replica): nothing is sent, the next round tries again.
    Retry,
}

impl Hub {
    pub fn new(config: SubscriptionsConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            registry: Mutex::new(Registry::default()),
            next_id: AtomicU64::new(1),
            claims_free: Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &SubscriptionsConfig {
        &self.config
    }

    /// Subscriptions open now.
    pub fn len(&self) -> usize {
        self.registry.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The schema was reloaded: what reads which tables, and whether they have Row-Level
    /// Security, is looked up again.
    pub fn schema_changed(&self) {
        self.claims_free.lock().unwrap().clear();
    }

    /// Runs the subscriptions again every `interval`, until the server state is gone.
    pub fn start(self: &Arc<Self>, state: &Arc<ServerState>) {
        let hub = Arc::downgrade(self);
        let state = Arc::downgrade(state);
        let period = self.config.interval.0;
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                let (Some(hub), Some(state)) = (hub.upgrade(), state.upgrade()) else {
                    return;
                };
                if hub.is_empty() {
                    continue;
                }
                let start = Instant::now();
                hub.round(&state).await;
                crate::telemetry::metrics().subscription_round(start.elapsed().as_secs_f64());
            }
        });
    }

    /// Starts a subscription to `query` (compiled from a subscription operation), run with
    /// `auth`, for `client`. Its first result is run now, and queued before it returns
    /// (unless the database cannot be reached: then the first round sends it).
    pub async fn subscribe(
        self: &Arc<Self>,
        state: &ServerState,
        query: Arc<CompiledQuery>,
        auth: Option<AuthSessionContext>,
        client: String,
    ) -> Result<Subscription, Rejection> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, updates) = mpsc::channel(QUEUE);
        {
            // Counted now, so concurrent subscribes cannot pass the limits together
            let mut registry = self.registry.lock().unwrap();
            if registry.entries.len() >= self.config.max_per_instance {
                return Err(Rejection::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "TOO_MANY_SUBSCRIPTIONS",
                    format!(
                        "this server has {} subscriptions open, the most it allows (`subscriptions.max_per_instance`)",
                        self.config.max_per_instance
                    ),
                ));
            }
            let count = registry.per_client.entry(client.clone()).or_default();
            if *count >= self.config.max_per_subject {
                return Err(Rejection::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "TOO_MANY_SUBSCRIPTIONS",
                    format!(
                        "you have {} subscriptions open, the most allowed (`subscriptions.max_per_subject`)",
                        self.config.max_per_subject
                    ),
                ));
            }
            *count += 1;
            registry.entries.insert(
                id,
                Entry {
                    query: query.clone(),
                    auth: auth.clone(),
                    client,
                    last_hash: None,
                    tx: tx.clone(),
                    ready: false,
                },
            );
        }
        crate::telemetry::metrics().subscription_active(1);
        let subscription = Subscription {
            updates,
            expires_at: auth.as_ref().and_then(|a| a.expires_at),
            _guard: Guard {
                hub: Arc::downgrade(self),
                id,
            },
        };

        // The first result, with the subscription's own claims
        let job = Job {
            settings: auth.map(Arc::new),
            query: query.clone(),
            rows: vec![Row {
                params: query.params.clone(),
                last_hash: None,
                ids: vec![id],
            }],
        };
        if let Some(results) = self.run_jobs(state, vec![job]).await {
            self.deliver(results);
        }
        if let Some(entry) = self.registry.lock().unwrap().entries.get_mut(&id) {
            entry.ready = true;
        }
        Ok(subscription)
    }

    fn remove(&self, id: u64) {
        let mut registry = self.registry.lock().unwrap();
        if let Some(entry) = registry.entries.remove(&id) {
            crate::telemetry::metrics().subscription_active(-1);
            if let Some(count) = registry.per_client.get_mut(&entry.client) {
                *count -= 1;
                if *count == 0 {
                    registry.per_client.remove(&entry.client);
                }
            }
        }
    }

    /// One round: every subscription run again, the changed results sent.
    async fn round(&self, state: &ServerState) {
        self.learn_claims_free(state).await;
        let jobs = self.jobs();
        if let Some(results) = self.run_jobs(state, jobs).await {
            self.deliver(results);
        }
    }

    /// Looks up, for the statements not seen before, whether claims can change their result.
    async fn learn_claims_free(&self, state: &ServerState) {
        let unknown: Vec<String> = {
            let registry = self.registry.lock().unwrap();
            let known = self.claims_free.lock().unwrap();
            let mut unknown: Vec<String> = registry
                .entries
                .values()
                .filter(|e| e.auth.is_some() && !known.contains_key(&e.query.sql))
                .map(|e| e.query.sql.clone())
                .collect();
            unknown.sort();
            unknown.dedup();
            unknown
        };
        if unknown.is_empty() {
            return;
        }
        let Ok(client) = self.connection(state).await else {
            return;
        };
        for sql in unknown {
            let (schemas, names): (Vec<String>, Vec<String>) =
                batch::qualified_names(&sql).into_iter().unzip();
            let free = client
                .query_one(batch::CLAIMS_FREE_SQL, &[&schemas, &names])
                .await
                .map(|row| row.get::<_, bool>(0));
            match free {
                Ok(free) => {
                    self.claims_free.lock().unwrap().insert(sql, free);
                }
                Err(err) => {
                    tracing::debug!(error = %err, "subscriptions: could not look up the tables a statement reads");
                    return;
                }
            }
        }
    }

    /// The round's statements: subscriptions grouped by compiled SQL and claims (the role
    /// alone when claims cannot change the result), in batches of `subscriptions.batch`.
    fn jobs(&self) -> Vec<Job> {
        // Subscriptions by (parameters, last hash), of a group keyed by (claims, SQL)
        type Rows = HashMap<(Vec<String>, Option<String>), Vec<u64>>;
        type Key = (Option<Vec<(String, String)>>, String);
        let registry = self.registry.lock().unwrap();
        let claims_free = self.claims_free.lock().unwrap();
        let mut groups: HashMap<Key, (Arc<CompiledQuery>, Rows)> = HashMap::new();
        for (id, entry) in registry.entries.iter().filter(|(_, e)| e.ready) {
            let settings = entry.auth.as_ref().map(|auth| {
                if claims_free.get(&entry.query.sql) == Some(&true) {
                    auth.settings
                        .iter()
                        .filter(|(name, _)| name == "role")
                        .cloned()
                        .collect()
                } else {
                    auth.settings.clone()
                }
            });
            let (_, rows) = groups
                .entry((settings, entry.query.sql.clone()))
                .or_insert_with(|| (entry.query.clone(), HashMap::new()));
            rows.entry((entry.query.params.clone(), entry.last_hash.clone()))
                .or_default()
                .push(*id);
        }
        let mut keyed: Vec<_> = groups.into_iter().collect();
        // Statements of the same claims next to each other: they are set once
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        let batch = if self.config.batch == 0 {
            1
        } else {
            self.config.batch
        };
        let mut jobs = Vec::new();
        for ((settings, _), (query, rows)) in keyed {
            let settings = settings.map(|settings| {
                Arc::new(AuthSessionContext {
                    settings,
                    ..Default::default()
                })
            });
            let rows: Vec<Row> = rows
                .into_iter()
                .map(|((params, last_hash), ids)| Row {
                    params,
                    last_hash,
                    ids,
                })
                .collect();
            // A query with row count estimates binds them per run: not batched
            let size = if query.estimates.is_empty() { batch } else { 1 };
            let mut rows = rows.into_iter().peekable();
            while rows.peek().is_some() {
                jobs.push(Job {
                    settings: settings.clone(),
                    query: query.clone(),
                    rows: rows.by_ref().take(size).collect(),
                });
            }
        }
        jobs
    }

    /// A connection subscriptions run on: a replica when there is one (`source: replica`).
    async fn connection(&self, state: &ServerState) -> Result<deadpool_postgres::Object, String> {
        match self.config.source {
            SubscriptionSource::Replica => state.read_pool(None).await.map(|(c, _)| c),
            SubscriptionSource::Primary => state
                .pool
                .get()
                .await
                .map_err(|err| format!("Database unavailable: {err}")),
        }
    }

    /// Runs `jobs` on up to `concurrency` connections; the subscriptions of each row and what
    /// was found. `None` when no connection could be had.
    async fn run_jobs(
        &self,
        state: &ServerState,
        jobs: Vec<Job>,
    ) -> Option<Vec<(Vec<u64>, Outcome)>> {
        if jobs.is_empty() {
            return Some(Vec::new());
        }
        // Contiguous shares, so statements of the same claims stay together
        let lanes = self.config.concurrency.max(1).min(jobs.len());
        let per_lane = jobs.len().div_ceil(lanes);
        let mut shares: Vec<Vec<Job>> = Vec::new();
        let mut jobs = jobs.into_iter().peekable();
        while jobs.peek().is_some() {
            shares.push(jobs.by_ref().take(per_lane).collect());
        }
        let runs = shares.into_iter().map(|share| async move {
            let client = match self.connection(state).await {
                Ok(client) => client,
                Err(err) => {
                    tracing::warn!(error = %err, "subscriptions: no database connection for this round");
                    return None;
                }
            };
            Some(run_share(state, &client, share).await)
        });
        let results: Vec<_> = futures::future::join_all(runs).await;
        if results.iter().all(Option::is_none) {
            return None;
        }
        Some(results.into_iter().flatten().flatten().collect())
    }

    /// Sends the changed results, and remembers them as the last sent.
    fn deliver(&self, results: Vec<(Vec<u64>, Outcome)>) {
        let mut registry = self.registry.lock().unwrap();
        let (mut data, mut errors) = (0, 0);
        for (ids, outcome) in results {
            let Outcome::Changed(hash, response) = outcome else {
                continue;
            };
            let is_error = response.get("errors").is_some();
            for id in ids {
                let Some(entry) = registry.entries.get_mut(&id) else {
                    continue;
                };
                match entry.tx.try_send(response.clone()) {
                    Ok(()) => {
                        entry.last_hash = Some(hash.clone());
                        if is_error {
                            errors += 1;
                        } else {
                            data += 1;
                        }
                    }
                    // A slow client: sent at a later round (the latest result then)
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                    // Its client is gone (the guard removes it too)
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
        }
        let metrics = crate::telemetry::metrics();
        metrics.subscription_results("data", data);
        metrics.subscription_results("error", errors);
    }
}

/// Runs `jobs` on `client`, in transactions of at most [`STATEMENTS_PER_TRANSACTION`]
/// statements; each claims' statements inside the savepoint, rolled back after them.
async fn run_share(
    state: &ServerState,
    client: &deadpool_postgres::Object,
    jobs: Vec<Job>,
) -> Vec<(Vec<u64>, Outcome)> {
    let mut out = Vec::new();
    let mut queue: std::collections::VecDeque<Job> = jobs.into();
    while !queue.is_empty() {
        if let Err(err) = client
            .batch_execute(&format!("BEGIN; SAVEPOINT {SAVEPOINT}"))
            .await
        {
            tracing::warn!(error = %err, "subscriptions: cannot start a transaction");
            out.extend(queue.drain(..).flat_map(retry_all));
            break;
        }
        // The claims set in the savepoint now
        let mut current: Option<Arc<AuthSessionContext>> = None;
        let mut statements = 0;
        let mut broken = false;
        while let Some(job) = queue.pop_front() {
            if statements >= STATEMENTS_PER_TRANSACTION {
                queue.push_front(job);
                break;
            }
            statements += 1;
            let same_claims = match (&current, &job.settings) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a.settings == b.settings,
                (None, None) => true,
                _ => false,
            };
            if !same_claims {
                if current.is_some() {
                    if let Err(err) = rollback_to_savepoint(client).await {
                        tracing::warn!(error = %err, "subscriptions: cannot reset the claims");
                        queue.push_front(job);
                        broken = true;
                        break;
                    }
                    current = None;
                }
                if let Some(settings) = &job.settings {
                    if let Err(err) = settings.apply(&**client).await {
                        tracing::warn!(error = %err, "subscriptions: cannot set the claims");
                        out.extend(retry_all(job));
                        let _ = rollback_to_savepoint(client).await;
                        continue;
                    }
                    current = Some(settings.clone());
                }
            }
            match run_job(client, &job).await {
                Ok(results) => out.extend(results),
                Err(err) => {
                    // The savepoint is rolled back: the claims are set again for what follows
                    let reset = rollback_to_savepoint(client).await;
                    current = None;
                    if is_retryable(&err) {
                        out.extend(retry_all(job));
                    } else if job.rows.len() > 1 {
                        // Which subscription fails: each row on its own
                        for row in job.rows {
                            queue.push_front(Job {
                                settings: job.settings.clone(),
                                query: job.query.clone(),
                                rows: vec![row],
                            });
                        }
                    } else {
                        let row = &job.rows[0];
                        out.push((row.ids.clone(), failure(state, &err, row)));
                    }
                    if reset.is_err() {
                        broken = true;
                        break;
                    }
                }
            }
        }
        let end = if broken { "ROLLBACK" } else { "COMMIT" };
        if client.batch_execute(end).await.is_err() {
            // The connection is not reused (deadpool checks it) and the rest is tried later
            out.extend(queue.drain(..).flat_map(retry_all));
            break;
        }
    }
    out
}

async fn rollback_to_savepoint(
    client: &deadpool_postgres::Object,
) -> Result<(), tokio_postgres::Error> {
    client
        .batch_execute(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
        .await
}

fn retry_all(job: Job) -> Vec<(Vec<u64>, Outcome)> {
    job.rows
        .into_iter()
        .map(|r| (r.ids, Outcome::Retry))
        .collect()
}

/// Runs one statement: batched, or (a query with row count estimates) on its own.
async fn run_job(
    client: &deadpool_postgres::Object,
    job: &Job,
) -> Result<Vec<(Vec<u64>, Outcome)>, ExecutionError> {
    if !job.query.estimates.is_empty() {
        let mut out = Vec::new();
        for row in &job.rows {
            let mut query = (*job.query).clone();
            query.params = row.params.clone();
            let data = ExecutionEngine::query_data(&**client, &query).await?;
            let response = serde_json::json!({ "data": data });
            let hash = hash_of(&response);
            let outcome = if row.last_hash.as_deref() == Some(hash.as_str()) {
                Outcome::Unchanged
            } else {
                Outcome::Changed(hash, response)
            };
            out.push((row.ids.clone(), outcome));
        }
        return Ok(out);
    }
    let sql = batch::batched_sql(&job.query.sql, job.query.params.len());
    let statement = crate::execution::engine::Connection::prepare(&**client, &sql).await?;
    let index: Vec<i64> = (0..job.rows.len() as i64).collect();
    let hashes: Vec<Option<&str>> = job.rows.iter().map(|r| r.last_hash.as_deref()).collect();
    let columns: Vec<Vec<&str>> = (0..job.query.params.len())
        .map(|k| job.rows.iter().map(|r| r.params[k].as_str()).collect())
        .collect();
    let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&index, &hashes];
    for column in &columns {
        params.push(column);
    }
    let rows = client.query(&statement, &params).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let i: i64 = row.get(0);
        let hash: String = row.get(1);
        let data: Option<serde_json::Value> = row.get(2);
        let Some(source) = job.rows.get(i as usize) else {
            continue;
        };
        let outcome = match data {
            Some(data) => Outcome::Changed(hash, serde_json::json!({ "data": data })),
            None => Outcome::Unchanged,
        };
        out.push((source.ids.clone(), outcome));
    }
    Ok(out)
}

/// A failure that the next round may not meet: no answer from the database, a conflict
/// with recovery on a replica, a serialization failure or deadlock, a shutdown, resources.
fn is_retryable(err: &ExecutionError) -> bool {
    let ExecutionError::Postgres(e) = err else {
        return false;
    };
    match e.as_db_error() {
        None => true,
        Some(db) => {
            matches!(&db.code().code()[..2], "40" | "53" | "57" | "58" | "08") && !err.is_timeout()
        }
    }
}

/// The response of a failing row: `data` null and the error, as a query's would be. Its
/// hash is the error's, so the same failure is sent once.
fn failure(state: &ServerState, err: &ExecutionError, row: &Row) -> Outcome {
    let catalog = state.current_catalog();
    let settings = state.settings();
    let (_, error) = execution_error(err, &catalog, settings.mask_errors, &settings.errors);
    let response = serde_json::json!({ "data": null, "errors": [error] });
    // A masked message names a new request id each time: the database's error is hashed
    let hash = hash_of(&serde_json::json!({ "error": err.to_string() }));
    if row.last_hash.as_deref() == Some(hash.as_str()) {
        Outcome::Unchanged
    } else {
        Outcome::Changed(hash, response)
    }
}

fn hash_of(value: &serde_json::Value) -> String {
    let digest = Sha256::digest(value.to_string().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Plans and compiles a subscription operation into the query it runs (once per distinct
/// document and variables, as queries are).
pub fn compile(
    state: &ServerState,
    query_text: &str,
    doc: &Valid<ExecutableDocument>,
    operation: &Operation,
    operation_name: Option<&str>,
    variables: Option<&serde_json::Value>,
) -> Result<Arc<CompiledQuery>, Rejection> {
    if crate::server::incremental::has_active_defer_or_stream(doc, operation, variables) {
        return Err(Rejection::new(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "@defer and @stream are not supported in subscriptions",
        ));
    }
    let compiled = match state.cache.operation(query_text, operation_name, variables) {
        Some(compiled) => compiled,
        None => {
            let catalog = state.current_catalog();
            let planner = QueryPlanner::with_context(
                &catalog,
                &state.default_schema,
                Some(&**doc),
                variables,
            )
            .with_limits(state.settings().limits.plan_limits());
            let plan = planner.plan_operation(operation).map_err(|err| Rejection {
                status: StatusCode::OK,
                errors: planner_errors(&err),
            })?;
            let mut compiled = SqlCompiler::compile_operation(&plan);
            if state.sql_comments {
                let name = operation
                    .name
                    .as_ref()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "anonymous".to_string());
                compiled.tag(&[
                    ("application", "graphpg"),
                    ("graphql_operation", &name),
                    ("graphql_type", "subscription"),
                ]);
            }
            let compiled = Arc::new(compiled);
            state
                .cache
                .store_operation(query_text, operation_name, variables, compiled.clone());
            compiled
        }
    };
    match &*compiled {
        CompiledOperation::Query(query) => Ok(Arc::new(query.clone())),
        CompiledOperation::Mutation(_) => Err(Rejection::new(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "a subscription reads: its field must be one of the Query type's",
        )),
    }
}

/// The client a subscription counts against for `max_per_subject`: the token's subject, else
/// the address it connects from.
pub fn client_key(auth: Option<&AuthSessionContext>, peer: Option<std::net::IpAddr>) -> String {
    match (auth.and_then(|a| a.subject.as_deref()), peer) {
        (Some(subject), _) => format!("sub:{subject}"),
        (None, Some(ip)) => format!("ip:{ip}"),
        (None, None) => "anonymous".to_string(),
    }
}
