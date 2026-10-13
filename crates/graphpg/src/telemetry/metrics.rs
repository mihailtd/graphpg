//! The server's metrics, named after the OpenTelemetry semantic conventions.
//!
//! Every measurement is recorded once here and reaches two places: the Prometheus text
//! exposition served at `telemetry.metrics.path`, and, with `telemetry.otlp`, the
//! OpenTelemetry instruments pushed over OTLP. The Prometheus names are the ones an OTLP
//! backend (Mimir, Prometheus' OTLP receiver) gives the pushed metrics —
//! `http.server.request.duration` in seconds becomes `http_server_request_duration_seconds`
//! with an `http_route` label — so one dashboard reads either.
//!
//! Labels are bounded: routes are templates (`/rest/{resource}`), never raw paths; methods
//! outside the standard ones are `_OTHER`; GraphQL operation names past
//! [`MAX_OPERATION_NAMES`] distinct ones are `_OTHER`.
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Upper bounds (seconds) of the duration histograms: those the HTTP semantic conventions
/// recommend.
pub const DURATION_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Distinct GraphQL operation names given a label of their own; later ones are `_OTHER`.
pub const MAX_OPERATION_NAMES: usize = 200;

/// The value of a label whose real value would be unbounded.
const OTHER: &str = "_OTHER";

/// The process's metrics.
pub fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::new)
}

/// A metric's name and labels, as OpenTelemetry names them; the Prometheus names follow.
pub(super) struct Descriptor {
    /// `http.server.request.duration`
    pub name: &'static str,
    /// UCUM unit: `s`, or `{request}` for a count of things (no Prometheus suffix).
    pub unit: &'static str,
    pub help: &'static str,
    pub labels: &'static [&'static str],
}

impl Descriptor {
    /// `http_server_request_duration_seconds`, `graphpg_cache_requests_total`
    fn prometheus_name(&self, counter: bool) -> String {
        let mut name = self.name.replace('.', "_");
        if self.unit == "s" {
            name.push_str("_seconds");
        }
        if counter {
            name.push_str("_total");
        }
        name
    }
}

/// `http.route` -> `http_route`
fn prometheus_label(name: &str) -> String {
    name.replace('.', "_")
}

/// A histogram of one label set: cumulative bucket counts, as Prometheus exposes them.
struct HistogramCell {
    buckets: [AtomicU64; DURATION_BUCKETS.len()],
    count: AtomicU64,
    /// An `f64`, as bits.
    sum: AtomicU64,
}

impl HistogramCell {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0f64.to_bits()),
        }
    }

    fn observe(&self, value: f64) {
        for (bound, bucket) in DURATION_BUCKETS.iter().zip(&self.buckets) {
            if value <= *bound {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .sum
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + value).to_bits())
            });
    }
}

/// One metric's cells, by label values (in the order of the descriptor's labels).
struct Family<C> {
    descriptor: Descriptor,
    cells: RwLock<HashMap<Vec<String>, Arc<C>>>,
}

impl<C> Family<C> {
    fn new(descriptor: Descriptor) -> Self {
        Self {
            descriptor,
            cells: RwLock::new(HashMap::new()),
        }
    }

    fn cell(&self, values: &[&str], new: impl FnOnce() -> C) -> Arc<C> {
        debug_assert_eq!(values.len(), self.descriptor.labels.len());
        let key: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        if let Some(cell) = self.cells.read().expect("metrics lock").get(&key) {
            return cell.clone();
        }
        self.cells
            .write()
            .expect("metrics lock")
            .entry(key)
            .or_insert_with(|| Arc::new(new()))
            .clone()
    }

    /// The cells, sorted by label values so the exposition is stable.
    fn sorted(&self) -> Vec<(Vec<String>, Arc<C>)> {
        let mut cells: Vec<_> = self
            .cells
            .read()
            .expect("metrics lock")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        cells.sort_by(|a, b| a.0.cmp(&b.0));
        cells
    }

    fn labels(&self, values: &[String], extra: Option<(&str, &str)>) -> String {
        let mut pairs: Vec<String> = self
            .descriptor
            .labels
            .iter()
            .zip(values)
            .map(|(name, value)| format!("{}=\"{}\"", prometheus_label(name), escape(value)))
            .collect();
        if let Some((name, value)) = extra {
            pairs.push(format!("{name}=\"{value}\""));
        }
        if pairs.is_empty() {
            String::new()
        } else {
            format!("{{{}}}", pairs.join(","))
        }
    }
}

/// Escapes a label value: `\`, `"` and newlines.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// A connection pool whose connections are reported.
struct WatchedPool {
    name: String,
    pool: deadpool_postgres::Pool,
}

/// The connections of a pool, as [`Metrics::pools`] reports them.
pub struct PoolUsage {
    pub name: String,
    pub idle: i64,
    pub used: i64,
    pub max: i64,
    pub pending: i64,
}

pub struct Metrics {
    /// Record for the Prometheus endpoint (`telemetry.metrics.enabled`).
    prometheus: AtomicBool,
    http_duration: Family<HistogramCell>,
    http_active: AtomicI64,
    graphql_duration: Family<HistogramCell>,
    db_duration: Family<HistogramCell>,
    planning_duration: Family<HistogramCell>,
    cache_requests: Family<AtomicU64>,
    event_deliveries: Family<AtomicU64>,
    operation_names: Mutex<HashSet<String>>,
    pools: RwLock<Vec<WatchedPool>>,
    started: SystemTime,
    #[cfg(feature = "otel")]
    otel: OnceLock<super::otel::Instruments>,
}

pub(super) const HTTP_DURATION: Descriptor = Descriptor {
    name: "http.server.request.duration",
    unit: "s",
    help: "Duration of HTTP server requests.",
    labels: &[
        "http.request.method",
        "http.route",
        "http.response.status_code",
    ],
};
pub(super) const HTTP_ACTIVE: Descriptor = Descriptor {
    name: "http.server.active_requests",
    unit: "{request}",
    help: "Number of active HTTP server requests.",
    labels: &[],
};
pub(super) const GRAPHQL_DURATION: Descriptor = Descriptor {
    name: "graphpg.graphql.operation.duration",
    unit: "s",
    help: "Duration of GraphQL operations, by operation; error.type is the first error's code.",
    labels: &[
        "graphql.operation.type",
        "graphql.operation.name",
        "error.type",
    ],
};
pub(super) const DB_DURATION: Descriptor = Descriptor {
    name: "db.client.operation.duration",
    unit: "s",
    help: "Duration of database operations (one query, or the transaction of a mutation, retries included); error.type is the SQLSTATE.",
    labels: &["db.system.name", "graphpg.operation.type", "error.type"],
};
pub(super) const PLANNING_DURATION: Descriptor = Descriptor {
    name: "graphpg.planning.duration",
    unit: "s",
    help: "Duration of planning and compiling an operation into SQL (cache misses only).",
    labels: &["graphpg.api"],
};
pub(super) const CACHE_REQUESTS: Descriptor = Descriptor {
    name: "graphpg.cache.requests",
    unit: "{request}",
    help: "Lookups in the GraphQL document and operation caches.",
    labels: &["graphpg.cache.name", "graphpg.cache.result"],
};
pub(super) const EVENT_DELIVERIES: Descriptor = Descriptor {
    name: "graphpg.event.deliveries",
    unit: "{delivery}",
    help:
        "Event webhook delivery attempts, by trigger and outcome (delivered, retry, dead, unknown).",
    labels: &["graphpg.event.trigger", "graphpg.event.outcome"],
};
pub(super) const POOL_COUNT: Descriptor = Descriptor {
    name: "db.client.connection.count",
    unit: "{connection}",
    help: "Connections of the pool, by state (idle or used).",
    labels: &[
        "db.client.connection.pool.name",
        "db.client.connection.state",
    ],
};
pub(super) const POOL_MAX: Descriptor = Descriptor {
    name: "db.client.connection.max",
    unit: "{connection}",
    help: "Most connections the pool opens.",
    labels: &["db.client.connection.pool.name"],
};
pub(super) const POOL_PENDING: Descriptor = Descriptor {
    name: "db.client.connection.pending_requests",
    unit: "{request}",
    help: "Requests waiting for a connection of the pool.",
    labels: &["db.client.connection.pool.name"],
};

/// Decrements `http.server.active_requests` when the request ends, finished or cancelled.
pub struct ActiveRequest(());

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        metrics().add_active(-1);
    }
}

impl Metrics {
    fn new() -> Self {
        Self {
            prometheus: AtomicBool::new(false),
            http_duration: Family::new(HTTP_DURATION),
            http_active: AtomicI64::new(0),
            graphql_duration: Family::new(GRAPHQL_DURATION),
            db_duration: Family::new(DB_DURATION),
            planning_duration: Family::new(PLANNING_DURATION),
            cache_requests: Family::new(CACHE_REQUESTS),
            event_deliveries: Family::new(EVENT_DELIVERIES),
            operation_names: Mutex::new(HashSet::new()),
            pools: RwLock::new(Vec::new()),
            started: SystemTime::now(),
            #[cfg(feature = "otel")]
            otel: OnceLock::new(),
        }
    }

    /// Records for the Prometheus endpoint from now on.
    pub fn enable_prometheus(&self) {
        self.prometheus.store(true, Ordering::Relaxed);
    }

    fn prometheus(&self) -> bool {
        self.prometheus.load(Ordering::Relaxed)
    }

    #[cfg(feature = "otel")]
    pub(super) fn attach_otel(&self, instruments: super::otel::Instruments) {
        let _ = self.otel.set(instruments);
    }

    #[cfg(feature = "otel")]
    fn otel(&self) -> Option<&super::otel::Instruments> {
        self.otel.get()
    }

    /// Reports the connections of `pool` as `name` (`primary`, `replica_1`, ...).
    pub fn watch_pool(&self, name: impl Into<String>, pool: deadpool_postgres::Pool) {
        self.pools.write().expect("metrics lock").push(WatchedPool {
            name: name.into(),
            pool,
        });
    }

    /// The connections of every watched pool now.
    pub fn pools(&self) -> Vec<PoolUsage> {
        self.pools
            .read()
            .expect("metrics lock")
            .iter()
            .map(|watched| {
                let status = watched.pool.status();
                PoolUsage {
                    name: watched.name.clone(),
                    idle: status.available as i64,
                    used: status.size.saturating_sub(status.available) as i64,
                    max: status.max_size as i64,
                    pending: status.waiting as i64,
                }
            })
            .collect()
    }

    /// A request started; it ends when the returned guard is dropped.
    pub fn active_request(&self) -> ActiveRequest {
        self.add_active(1);
        ActiveRequest(())
    }

    fn add_active(&self, delta: i64) {
        self.http_active.fetch_add(delta, Ordering::Relaxed);
        #[cfg(feature = "otel")]
        if let Some(otel) = self.otel() {
            otel.http_active(delta);
        }
    }

    /// An HTTP request was answered. `route` is the matched route template, empty when no
    /// route matched.
    pub fn http_request(&self, method: &str, route: &str, status: u16, seconds: f64) {
        let status = status.to_string();
        let values = [method, route, status.as_str()];
        if self.prometheus() {
            self.http_duration
                .cell(&values, HistogramCell::new)
                .observe(seconds);
        }
        #[cfg(feature = "otel")]
        if let Some(otel) = self.otel() {
            otel.http_duration(&values, seconds);
        }
    }

    /// A GraphQL operation was answered. `error` is the code of its first error.
    pub fn graphql_operation(&self, kind: &str, name: &str, error: Option<&str>, seconds: f64) {
        let name = self.bounded_operation_name(name);
        let values = [kind, name.as_str(), error.unwrap_or("")];
        if self.prometheus() {
            self.graphql_duration
                .cell(&values, HistogramCell::new)
                .observe(seconds);
        }
        #[cfg(feature = "otel")]
        if let Some(otel) = self.otel() {
            otel.graphql_duration(&values, seconds);
        }
    }

    /// A query or mutation ran on the database. `error` is its SQLSTATE, or the kind of
    /// failure without one.
    pub fn db_operation(&self, kind: &str, error: Option<&str>, seconds: f64) {
        let values = ["postgresql", kind, error.unwrap_or("")];
        if self.prometheus() {
            self.db_duration
                .cell(&values, HistogramCell::new)
                .observe(seconds);
        }
        #[cfg(feature = "otel")]
        if let Some(otel) = self.otel() {
            otel.db_duration(&values, seconds);
        }
    }

    /// An operation of `api` (`graphql`, `rest`) was planned and compiled.
    pub fn planning(&self, api: &str, seconds: f64) {
        let values = [api];
        if self.prometheus() {
            self.planning_duration
                .cell(&values, HistogramCell::new)
                .observe(seconds);
        }
        #[cfg(feature = "otel")]
        if let Some(otel) = self.otel() {
            otel.planning_duration(&values, seconds);
        }
    }

    /// A lookup in `cache` (`document`, `operation`).
    pub fn cache(&self, cache: &str, hit: bool) {
        let values = [cache, if hit { "hit" } else { "miss" }];
        if self.prometheus() {
            self.cache_requests
                .cell(&values, || AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(feature = "otel")]
        if let Some(otel) = self.otel() {
            otel.cache_requests(&values);
        }
    }

    /// One event webhook delivery attempt of `trigger` (a configured name: bounded).
    pub fn event_delivery(&self, trigger: &str, outcome: &str) {
        let values = [trigger, outcome];
        if self.prometheus() {
            self.event_deliveries
                .cell(&values, || AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(feature = "otel")]
        if let Some(otel) = self.otel() {
            otel.event_deliveries(&values);
        }
    }

    /// `name` while fewer than [`MAX_OPERATION_NAMES`] names were seen, else `_OTHER`.
    fn bounded_operation_name(&self, name: &str) -> String {
        let mut names = self.operation_names.lock().expect("metrics lock");
        if names.contains(name) {
            return name.to_string();
        }
        if names.len() < MAX_OPERATION_NAMES {
            names.insert(name.to_string());
            return name.to_string();
        }
        OTHER.to_string()
    }

    /// The Prometheus text exposition (format 0.0.4) of every metric.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let header = |out: &mut String, d: &Descriptor, name: &str, kind: &str| {
            let _ = writeln!(out, "# HELP {name} {}", d.help);
            let _ = writeln!(out, "# TYPE {name} {kind}");
        };

        let _ = writeln!(
            out,
            "# HELP graphpg_build_info The running graphpg version.\n# TYPE graphpg_build_info gauge\ngraphpg_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );
        let started = self
            .started
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "# HELP process_start_time_seconds Start time of the process since the Unix epoch, in seconds.\n# TYPE process_start_time_seconds gauge\nprocess_start_time_seconds {started}"
        );

        for family in [
            &self.http_duration,
            &self.graphql_duration,
            &self.db_duration,
            &self.planning_duration,
        ] {
            let name = family.descriptor.prometheus_name(false);
            header(&mut out, &family.descriptor, &name, "histogram");
            for (values, cell) in family.sorted() {
                for (bound, bucket) in DURATION_BUCKETS.iter().zip(&cell.buckets) {
                    let le = bound.to_string();
                    let _ = writeln!(
                        out,
                        "{name}_bucket{} {}",
                        family.labels(&values, Some(("le", &le))),
                        bucket.load(Ordering::Relaxed)
                    );
                }
                let count = cell.count.load(Ordering::Relaxed);
                let _ = writeln!(
                    out,
                    "{name}_bucket{} {count}",
                    family.labels(&values, Some(("le", "+Inf")))
                );
                let labels = family.labels(&values, None);
                let sum = f64::from_bits(cell.sum.load(Ordering::Relaxed));
                let _ = writeln!(out, "{name}_sum{labels} {sum}");
                let _ = writeln!(out, "{name}_count{labels} {count}");
            }
        }

        let name = HTTP_ACTIVE.prometheus_name(false);
        header(&mut out, &HTTP_ACTIVE, &name, "gauge");
        let _ = writeln!(out, "{name} {}", self.http_active.load(Ordering::Relaxed));

        for family in [&self.cache_requests, &self.event_deliveries] {
            let name = family.descriptor.prometheus_name(true);
            header(&mut out, &family.descriptor, &name, "counter");
            for (values, cell) in family.sorted() {
                let _ = writeln!(
                    out,
                    "{name}{} {}",
                    family.labels(&values, None),
                    cell.load(Ordering::Relaxed)
                );
            }
        }

        let pools = self.pools();
        if !pools.is_empty() {
            let pool_label = prometheus_label(POOL_COUNT.labels[0]);
            let state_label = prometheus_label(POOL_COUNT.labels[1]);
            let name = POOL_COUNT.prometheus_name(false);
            header(&mut out, &POOL_COUNT, &name, "gauge");
            for pool in &pools {
                let pool_name = escape(&pool.name);
                for (state, value) in [("idle", pool.idle), ("used", pool.used)] {
                    let _ = writeln!(
                        out,
                        "{name}{{{pool_label}=\"{pool_name}\",{state_label}=\"{state}\"}} {value}"
                    );
                }
            }
            for (descriptor, value) in [
                (&POOL_MAX, (|p: &PoolUsage| p.max) as fn(&PoolUsage) -> i64),
                (&POOL_PENDING, |p: &PoolUsage| p.pending),
            ] {
                let name = descriptor.prometheus_name(false);
                header(&mut out, descriptor, &name, "gauge");
                for pool in &pools {
                    let _ = writeln!(
                        out,
                        "{name}{{{pool_label}=\"{}\"}} {}",
                        escape(&pool.name),
                        value(pool)
                    );
                }
            }
        }
        out
    }
}

/// The method as the semantic conventions name it: the standard methods, else `_OTHER`.
pub fn method_label(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::TRACE => "TRACE",
        Method::CONNECT => "CONNECT",
        _ => OTHER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_histograms_with_cumulative_buckets() {
        let m = Metrics::new();
        m.enable_prometheus();
        m.http_request("GET", "/rest/{resource}", 200, 0.02);
        m.http_request("GET", "/rest/{resource}", 200, 0.3);
        let text = m.render();
        assert!(text.contains("# TYPE http_server_request_duration_seconds histogram"));
        let labels = r#"http_request_method="GET",http_route="/rest/{resource}",http_response_status_code="200""#;
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_bucket{{{labels},le=\"0.01\"}} 0"
        )));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_bucket{{{labels},le=\"0.025\"}} 1"
        )));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} 2"
        )));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_count{{{labels}}} 2"
        )));
        assert!(text.contains("graphpg_build_info{version="));
    }

    #[test]
    fn counters_have_the_total_suffix() {
        let m = Metrics::new();
        m.enable_prometheus();
        m.cache("operation", true);
        m.cache("operation", true);
        m.cache("operation", false);
        let text = m.render();
        assert!(text.contains(
            r#"graphpg_cache_requests_total{graphpg_cache_name="operation",graphpg_cache_result="hit"} 2"#
        ));
        assert!(text.contains(
            r#"graphpg_cache_requests_total{graphpg_cache_name="operation",graphpg_cache_result="miss"} 1"#
        ));
    }

    #[test]
    fn nothing_is_recorded_while_disabled() {
        let m = Metrics::new();
        m.http_request("GET", "/graphql", 200, 0.1);
        assert!(!m
            .render()
            .contains("http_server_request_duration_seconds_count{"));
    }

    #[test]
    fn operation_names_are_bounded() {
        let m = Metrics::new();
        m.enable_prometheus();
        for i in 0..MAX_OPERATION_NAMES + 5 {
            m.graphql_operation("query", &format!("Op{i}"), None, 0.01);
        }
        m.graphql_operation("query", "Op0", Some("BAD_USER_INPUT"), 0.01);
        let text = m.render();
        assert!(text.contains(r#"graphql_operation_name="_OTHER""#));
        assert!(text.contains(
            r#"graphpg_graphql_operation_duration_seconds_count{graphql_operation_type="query",graphql_operation_name="Op0",error_type="BAD_USER_INPUT"} 1"#
        ));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
