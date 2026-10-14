//! `graphpg.yaml`: the configuration of one graphpg server. Shared sections (`server`,
//! `database`, `auth`, `rbac`, `debug`) apply to every API; `graphql` and `openapi` hold what
//! belongs to one API, each switchable with `enabled`. Everything else describes the
//! generated APIs where the database cannot say what is wanted: rename, describe or hide
//! types and fields, choose which operations exist, declare the keys and relations of views,
//! and mount functions as fields. Those entries are keyed by database object name (stable
//! across renames) and shared by both APIs. Schema design itself (tables, constraints, types,
//! functions) belongs in SQL.
//!
//! One file serves every environment: `dev:` and `prod:` blocks hold the settings that differ,
//! merged over the rest for the mode the server starts in (see [`super::loader`]).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The HTTP listener.
    #[serde(default)]
    pub server: ServerConfig,
    /// The PostgreSQL connection, the schemas to expose and the read replicas.
    #[serde(default)]
    pub database: DatabaseConfig,
    /// Authentication (OAuth 2.0 bearer tokens) and the Row-Level Security session context.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Role-based access control (RBAC) schema pruning configuration.
    #[serde(default)]
    pub rbac: RbacConfig,
    /// Diagnostics added to responses. For development only.
    #[serde(default)]
    pub debug: DebugConfig,
    /// The GraphQL API.
    #[serde(default)]
    pub graphql: GraphqlConfig,
    /// The REST API and its OpenAPI description.
    #[serde(default, alias = "rest")]
    pub openapi: OpenapiConfig,
    /// How much of a failure the client sees.
    #[serde(default)]
    pub errors: ErrorsConfig,
    /// Security headers on every response.
    #[serde(default)]
    pub headers: HeadersConfig,
    /// Schema watching on DDL changes and auto-export.
    #[serde(default)]
    pub watch: WatchConfig,
    /// Schema reload on request (`NOTIFY`, `SIGHUP`), in any mode.
    #[serde(default)]
    pub reload: ReloadConfig,
    /// File paths for exported schemas.
    #[serde(default)]
    pub export: ExportConfig,
    /// Tables, views, materialized views and foreign tables, by relation name.
    #[serde(default)]
    pub entities: BTreeMap<String, EntityConfig>,
    /// Table-valued functions and computed-column functions, by function name.
    #[serde(default)]
    pub functions: BTreeMap<String, FunctionConfig>,
    /// Enum and composite types: PostgreSQL types by type name, `CHECK`-closed text columns
    /// (exposed as enums) by `<table>.<column>`.
    #[serde(default)]
    pub types: BTreeMap<String, TypeConfig>,
    /// Objects created by PostgreSQL extensions.
    #[serde(default)]
    pub extensions: ExtensionsConfig,
    /// Search root fields (`usersFuzzySearch`, ...).
    #[serde(default)]
    pub search: SearchConfig,
    /// Safety limits shared by the GraphQL and REST APIs.
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Logs, Prometheus metrics, traces, and OpenTelemetry (OTLP) export.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Event triggers: webhooks on row changes and on schedules.
    #[serde(default)]
    pub events: EventTriggersConfig,
    /// Live queries: GraphQL subscriptions whose result is sent again when it changes.
    #[serde(default)]
    pub subscriptions: SubscriptionsConfig,
}

/// A number given as text, which is what `${NAME}` produces: `port: '${PORT}'`.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Native<T> {
        Value(T),
        Text(String),
    }
    match Native::<T>::deserialize(deserializer)? {
        Native::Value(value) => Ok(value),
        Native::Text(text) => text
            .trim()
            .to_ascii_lowercase()
            .parse()
            .map_err(|e| serde::de::Error::custom(format!("invalid value {text:?}: {e}"))),
    }
}

/// A switch given as text, which is what `${NAME}` produces: `true`, `false`, `1`, `0`,
/// `yes`, `no`, `on` or `off`.
fn switch<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Native {
        Value(bool),
        Text(String),
    }
    match Native::deserialize(deserializer)? {
        Native::Value(value) => Ok(value),
        Native::Text(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" => Ok(false),
            _ => Err(serde::de::Error::custom(format!(
                "invalid value {text:?} (true or false)"
            ))),
        },
    }
}

/// A limit given as a number, as text (which is what `${NAME}` produces), or switched off with
/// `off`, `none`, `unlimited` or `null`.
fn limit<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Native<T> {
        Value(T),
        Text(String),
    }
    match Option::<Native<T>>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Native::Value(value)) => Ok(Some(value)),
        Some(Native::Text(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "unlimited" | "null" => Ok(None),
            other => other.parse().map(Some).map_err(|e| {
                serde::de::Error::custom(format!(
                    "invalid value {text:?}: {e} (a number, or `off` for no limit)"
                ))
            }),
        },
    }
}

/// A duration with a unit: `500ms`, `30s`, `2m` or `1h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout(pub std::time::Duration);

impl Timeout {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim().to_ascii_lowercase();
        let split = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (digits, unit) = text.split_at(split);
        let amount: u64 = digits.parse().map_err(|_| {
            format!("invalid duration {text:?}: write a number and a unit, like `30s`")
        })?;
        let millis = match unit.trim() {
            "ms" => amount,
            "s" => amount.saturating_mul(1000),
            "m" => amount.saturating_mul(60_000),
            "h" => amount.saturating_mul(3_600_000),
            "d" => amount.saturating_mul(86_400_000),
            "" => {
                return Err(format!(
                "invalid duration {text:?}: a unit is needed (`ms`, `s`, `m` or `h`), like `30s`"
            ))
            }
            other => {
                return Err(format!(
                    "invalid duration {text:?}: unknown unit `{other}` (`ms`, `s`, `m`, `h` or `d`)"
                ))
            }
        };
        Ok(Self(std::time::Duration::from_millis(millis)))
    }
}

impl std::fmt::Display for Timeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}ms", self.0.as_millis())
    }
}

impl Serialize for Timeout {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

fn timeout<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Timeout>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Native {
        Number(u64),
        Text(String),
    }
    match Option::<Native>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Native::Number(0)) => Ok(None),
        // A bare number has no unit: say so
        Some(Native::Number(n)) => Timeout::parse(&n.to_string())
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(Native::Text(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "unlimited" | "null" | "0" => Ok(None),
            _ => Timeout::parse(&text)
                .map(Some)
                .map_err(serde::de::Error::custom),
        },
    }
}

/// How many rows a request may ask for, nest, read or write, and how long it may run. The same
/// limits apply to GraphQL and REST: they are enforced on the plan both APIs produce.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Rows per page: `first` / `last`.
    #[serde(default)]
    pub page_size: PageSizeConfig,
    /// Deepest nesting of rows (a list is 1, the rows of a relation under it 2, ...).
    #[serde(default = "default_depth", deserialize_with = "limit")]
    pub depth: Option<usize>,
    /// Largest estimated cost: the rows an operation reads, each step's page size times the
    /// rows it hangs from, summed.
    #[serde(default = "default_cost", deserialize_with = "limit")]
    pub cost: Option<u64>,
    /// Most rows one write inserts, nested writes included (GraphQL `create<Types>` and REST
    /// bulk creates).
    #[serde(default = "default_bulk_rows", deserialize_with = "limit")]
    pub bulk_rows: Option<usize>,
    /// GraphQL `update<Types>(where:)` and `delete<Types>(where:)` with no condition (every row)
    /// need `unfiltered: true`. REST always asks for it (`unfiltered=true`). Off by default.
    #[serde(default)]
    pub require_filter: bool,
    /// Most groups an aggregate request may return.
    #[serde(default = "default_groups", deserialize_with = "limit")]
    pub groups: Option<usize>,
    /// Largest request body, in bytes.
    #[serde(default = "default_body", deserialize_with = "limit")]
    pub request_body_bytes: Option<usize>,
    /// `statement_timeout` of every connection: a statement running longer is cancelled and the
    /// request fails with `STATEMENT_TIMEOUT`. A role's own `statement_timeout` still applies
    /// first when it is lower.
    #[serde(default = "default_timeout", deserialize_with = "timeout")]
    pub statement_timeout: Option<Timeout>,
    /// Threshold for `Prefer: count=estimated`. When the estimated row count is at or below this
    /// threshold, an exact count is computed; above this threshold, the planner estimate is used.
    #[serde(default = "default_count_threshold", deserialize_with = "limit")]
    pub count_threshold: Option<u64>,
    /// Requests per client and minute, over GraphQL and REST. Off by default.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
}

/// How many requests a client may make: a token bucket per client (GCRA), refilled at
/// `requests_per_minute` and holding `burst`. A client is the verified token's `key_claim`
/// (`sub`), else its IP address. State lives in each instance's memory: with N instances
/// behind a load balancer a client gets up to N times the rate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Limit requests at all. Off, nothing is counted and no `RateLimit-*` header is sent.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// Requests without a valid access token, by IP address.
    #[serde(default = "default_anonymous_rate")]
    pub anonymous: RateConfig,
    /// Requests with a valid access token, by `key_claim`.
    #[serde(default = "default_authenticated_rate")]
    pub authenticated: RateConfig,
    /// The claim that names a client (a claim name or a dot-separated path, like the session
    /// variable mappings). A token without it counts as anonymous.
    #[serde(default = "default_key_claim")]
    pub key_claim: String,
    /// Proxies trusted to name the client in `X-Forwarded-For`: addresses or CIDR ranges.
    /// Without them the client is the connection's peer address.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Clients tracked at most per instance. Clients whose bucket is full again are dropped
    /// first; past the bound every client starts over.
    #[serde(default = "default_max_clients", deserialize_with = "lenient")]
    pub max_clients: usize,
}

/// The rate of one kind of client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RateConfig {
    /// Requests per minute, or `off` for no limit.
    #[serde(deserialize_with = "limit")]
    pub requests_per_minute: Option<u32>,
    /// Requests allowed at once, after a quiet spell (default: `requests_per_minute`).
    #[serde(default, deserialize_with = "limit")]
    pub burst: Option<u32>,
}

fn default_anonymous_rate() -> RateConfig {
    RateConfig {
        requests_per_minute: Some(60),
        burst: None,
    }
}
fn default_authenticated_rate() -> RateConfig {
    RateConfig {
        requests_per_minute: Some(600),
        burst: None,
    }
}
fn default_key_claim() -> String {
    "sub".to_string()
}
fn default_max_clients() -> usize {
    100_000
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            anonymous: default_anonymous_rate(),
            authenticated: default_authenticated_rate(),
            key_claim: default_key_claim(),
            trusted_proxies: Vec::new(),
            max_clients: default_max_clients(),
        }
    }
}

impl RateLimitConfig {
    /// Problems with the values.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (tier, rate) in [
            ("anonymous", &self.anonymous),
            ("authenticated", &self.authenticated),
        ] {
            if rate.requests_per_minute == Some(0) {
                problems.push(format!(
                    "limits.rate_limit.{tier}.requests_per_minute: must be at least 1 (use `off` for no limit)"
                ));
            }
            if rate.burst == Some(0) {
                problems.push(format!(
                    "limits.rate_limit.{tier}.burst: must be at least 1"
                ));
            }
        }
        if self.key_claim.trim().is_empty() {
            problems.push("limits.rate_limit.key_claim: must name a claim".to_string());
        }
        if self.max_clients == 0 {
            problems.push("limits.rate_limit.max_clients: must be at least 1".to_string());
        }
        for proxy in &self.trusted_proxies {
            if let Err(err) = IpRange::parse(proxy) {
                problems.push(format!("limits.rate_limit.trusted_proxies: {err}"));
            }
        }
        problems
    }
}

/// An address, or a range of them in CIDR notation (`10.0.0.0/8`, `fd00::/8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpRange {
    pub network: std::net::IpAddr,
    pub prefix: u8,
}

impl IpRange {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let network: std::net::IpAddr = address
            .parse()
            .map_err(|_| format!("`{text}` is not an IP address or CIDR range"))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("`{text}`: the prefix length must be 0 to {max}"))?,
        };
        Ok(Self { network, prefix })
    }

    pub fn contains(&self, address: std::net::IpAddr) -> bool {
        use std::net::IpAddr;
        // An IPv4 peer of a dual-stack socket arrives as `::ffff:a.b.c.d`
        let address = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(address),
            v4 => v4,
        };
        let bits = |a: IpAddr| -> u128 {
            match a {
                IpAddr::V4(v4) => u32::from(v4) as u128,
                IpAddr::V6(v6) => u128::from(v6),
            }
        };
        if self.network.is_ipv4() != address.is_ipv4() {
            return false;
        }
        let width = if address.is_ipv4() { 32 } else { 128 };
        if self.prefix == 0 {
            return true;
        }
        let shift = width - self.prefix as u32;
        (bits(self.network) >> shift) == (bits(address) >> shift)
    }
}

/// Page sizes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PageSizeConfig {
    /// Rows of a connection asked for neither `first` nor `last`.
    #[serde(default = "default_page_size", deserialize_with = "lenient")]
    pub default: usize,
    /// Largest `first` / `last`, and the most rows of a list that has no page size (plain list
    /// fields, to-many relations).
    #[serde(default = "default_page_max", deserialize_with = "limit")]
    pub max: Option<usize>,
}

fn default_page_size() -> usize {
    crate::plan::Limits::DEFAULT_PAGE_SIZE
}
fn default_page_max() -> Option<usize> {
    Some(1000)
}
fn default_depth() -> Option<usize> {
    Some(10)
}
fn default_cost() -> Option<u64> {
    Some(10_000_000)
}
fn default_bulk_rows() -> Option<usize> {
    Some(1000)
}
fn default_groups() -> Option<usize> {
    Some(10_000)
}
fn default_body() -> Option<usize> {
    Some(1024 * 1024)
}
fn default_timeout() -> Option<Timeout> {
    Some(Timeout(std::time::Duration::from_secs(30)))
}
fn default_count_threshold() -> Option<u64> {
    Some(10_000)
}

impl Default for PageSizeConfig {
    fn default() -> Self {
        Self {
            default: default_page_size(),
            max: default_page_max(),
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            page_size: PageSizeConfig::default(),
            depth: default_depth(),
            cost: default_cost(),
            bulk_rows: default_bulk_rows(),
            require_filter: false,
            groups: default_groups(),
            request_body_bytes: default_body(),
            statement_timeout: default_timeout(),
            count_threshold: default_count_threshold(),
            rate_limit: RateLimitConfig::default(),
        }
    }
}

impl LimitsConfig {
    /// The limits the planner enforces on every plan.
    pub fn plan_limits(&self) -> crate::plan::Limits {
        crate::plan::Limits {
            page_default: Some(self.page_size.default),
            page_max: self.page_size.max,
            depth: self.depth,
            cost: self.cost,
            bulk_rows: self.bulk_rows,
            require_filter: self.require_filter,
        }
    }

    /// Problems with the values, all reported together.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let positive = |name: &str, value: Option<u64>, problems: &mut Vec<String>| {
            if value == Some(0) {
                problems.push(format!(
                    "limits.{name}: must be at least 1 (use `off` for no limit)"
                ));
            }
        };
        if self.page_size.default == 0 {
            problems.push("limits.page_size.default: must be at least 1".to_string());
        }
        positive(
            "page_size.max",
            self.page_size.max.map(|n| n as u64),
            &mut problems,
        );
        positive("depth", self.depth.map(|n| n as u64), &mut problems);
        positive("cost", self.cost, &mut problems);
        positive("bulk_rows", self.bulk_rows.map(|n| n as u64), &mut problems);
        positive("groups", self.groups.map(|n| n as u64), &mut problems);
        positive(
            "request_body_bytes",
            self.request_body_bytes.map(|n| n as u64),
            &mut problems,
        );
        if let Some(max) = self.page_size.max {
            if self.page_size.default > max {
                problems.push(format!(
                    "limits.page_size.default ({}) is larger than limits.page_size.max ({max})",
                    self.page_size.default
                ));
            }
        }
        problems.extend(self.rate_limit.problems());
        problems
    }
}

/// How much of an unexpected failure (a database error nobody can act on) reaches the client.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ErrorDetail {
    /// A fixed message, a stable code and a request ID; the real error is logged under that
    /// ID. The default in `prod`.
    #[default]
    Masked,
    /// The database's own message too. The default in `dev`.
    Full,
}

/// Custom error mapping for a SQLSTATE code or constraint name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CustomErrorConfig {
    /// HTTP status code (e.g. 400, 404, 409, 422).
    #[serde(default)]
    pub status: Option<u16>,
    /// Problem title (defaults to canonical reason phrase for the status).
    #[serde(default)]
    pub title: Option<String>,
    /// Machine-readable code (e.g. `PAYMENT_REQUIRED`).
    #[serde(default)]
    pub code: Option<String>,
    /// RFC 9457 type URI (defaults to `about:blank`).
    #[serde(default, rename = "type")]
    pub problem_type: Option<String>,
}

/// Error detail, for GraphQL and REST alike. Typed constraint errors (`EMAIL_TAKEN`) and
/// invalid input are always shown: the client can act on them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ErrorsConfig {
    #[serde(default)]
    pub detail: ErrorDetail,
    /// Maps SQLSTATE error codes or constraint names to custom HTTP status and problem responses.
    #[serde(default)]
    pub map: BTreeMap<String, CustomErrorConfig>,
}

/// Which security headers are sent.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HeaderPreset {
    /// `nosniff`, `Referrer-Policy`, HSTS, `X-Frame-Options`, a CSP that allows nothing, and
    /// a same-origin opener policy. The default in `prod`.
    #[default]
    Strict,
    /// `nosniff` and `Referrer-Policy` only. The default in `dev`.
    Relaxed,
    /// None.
    Off,
}

/// Security headers: a preset, and header overrides on top of it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    #[serde(default)]
    pub preset: HeaderPreset,
    /// Headers to set (or remove, with an empty value) after the preset.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
}

/// The HTTP listener.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address to listen on.
    #[serde(default = "default_host")]
    pub host: String,
    /// Port to listen on.
    #[serde(deserialize_with = "lenient", default = "default_port")]
    pub port: u16,
    /// Cross-origin requests from browsers. Off in `prod` until origins are listed; `dev`
    /// allows every origin.
    #[serde(default)]
    pub cors: CorsConfig,
}

/// Cross-origin resource sharing. With no `origins`, no CORS headers are sent and browsers
/// block cross-origin calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Allowed origins, like `https://app.example.com`, or `*` for any.
    #[serde(default)]
    pub origins: Vec<String>,
    /// Allowed methods (default: GET, POST, PUT, PATCH, DELETE, OPTIONS, HEAD).
    #[serde(default)]
    pub methods: Vec<String>,
    /// Allowed request headers (default: authorization, content-type, accept, prefer,
    /// x-request-id, if-none-match, if-match, range, x-min-wal-position).
    #[serde(default)]
    pub headers: Vec<String>,
    /// Response headers a page's scripts may read beyond the CORS-safelisted ones (default:
    /// every header graphpg sends, see `DEFAULT_EXPOSE_HEADERS` in the router).
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Allow cookies and credentials. Needs explicit `origins`, not `*`.
    #[serde(default, deserialize_with = "switch")]
    pub credentials: bool,
    /// Seconds a browser may cache the preflight answer.
    #[serde(default)]
    pub max_age: Option<u64>,
}

impl CorsConfig {
    pub fn enabled(&self) -> bool {
        !self.origins.is_empty()
    }

    fn any_origin(&self) -> bool {
        self.origins.iter().any(|o| o == "*")
    }

    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.credentials && self.any_origin() {
            problems.push(
                "server.cors.credentials cannot be combined with origin `*`: list the origins"
                    .to_string(),
            );
        }
        if self.any_origin() && self.origins.len() > 1 {
            problems.push("server.cors.origins: `*` cannot be listed with other origins".into());
        }
        problems
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    4000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            cors: CorsConfig::default(),
        }
    }
}

/// Schema inspected when the config names none.
pub const DEFAULT_SCHEMA: &str = "app_public";

/// The schemas to expose and connection pool settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Connections in the pool.
    #[serde(deserialize_with = "lenient", default = "default_pool_size")]
    pub pool_size: usize,
    /// Prepared statements each connection caches (0: none). A cached statement skips
    /// PostgreSQL's planning when an operation runs again, and holds its plan in the
    /// connection's backend meanwhile.
    #[serde(deserialize_with = "lenient", default = "default_statement_cache")]
    pub statement_cache: usize,
    /// Schemas to expose, the first being the default schema (`app_public` when empty).
    #[serde(default)]
    pub schemas: Vec<String>,
    /// Append a sqlcommenter comment (application, operation or route) to every statement.
    #[serde(deserialize_with = "switch", default = "default_true")]
    pub sql_comments: bool,
}

fn default_pool_size() -> usize {
    10
}
fn default_statement_cache() -> usize {
    crate::execution::engine::MAX_CACHED_STATEMENTS
}
fn default_true() -> bool {
    true
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            pool_size: default_pool_size(),
            statement_cache: default_statement_cache(),
            schemas: Vec::new(),
            sql_comments: true,
        }
    }
}

impl DatabaseConfig {
    /// The schemas to inspect, the first being the default: [`schemas`](Self::schemas), else
    /// [`DEFAULT_SCHEMA`].
    pub fn resolved_schemas(&self) -> Vec<String> {
        if self.schemas.is_empty() {
            vec![DEFAULT_SCHEMA.to_string()]
        } else {
            self.schemas.clone()
        }
    }
}

/// Diagnostics added to responses. Never enable them in production.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DebugConfig {
    /// Include the generated SQL in the response `extensions`.
    #[serde(default, deserialize_with = "switch")]
    pub sql: bool,
    /// Include PostgreSQL's `EXPLAIN (ANALYZE)` output of every statement.
    #[serde(default, deserialize_with = "switch")]
    pub explain: bool,
    /// Serve `GET /debug/statements`: the most expensive operations so far, from
    /// `pg_stat_statements`, with the indexes their plans lack (`graphpg advise --statements`).
    /// On in `dev` mode.
    #[serde(default, deserialize_with = "switch")]
    pub statements: bool,
}

/// The GraphQL API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GraphqlConfig {
    /// Serve the GraphQL API at `/graphql`.
    #[serde(deserialize_with = "switch", default = "default_true")]
    pub enabled: bool,
    /// Serve the GraphiQL IDE at `/graphiql`. On in `dev` mode, off in `prod`.
    #[serde(default, deserialize_with = "switch")]
    pub ui: bool,
    /// Answer introspection queries (`__schema`, `__type`). On in `dev` mode, off in `prod`.
    #[serde(default, deserialize_with = "switch")]
    pub introspection: bool,
    /// Serve the schema as SDL at `sdl_path` (and the Federation subgraph SDL at
    /// `subgraph_path` when federation is on). On in `dev` mode, off in `prod`.
    #[serde(default, deserialize_with = "switch")]
    pub sdl: bool,
    /// Path to serve the GraphQL SDL at (defaults to `/schema.graphql`).
    #[serde(default = "default_sdl_path")]
    pub sdl_path: String,
    /// Path to serve the Federation subgraph SDL at (defaults to `/subgraph.graphql`).
    #[serde(default = "default_subgraph_path")]
    pub subgraph_path: String,
    /// Apollo Federation configuration.
    #[serde(default)]
    pub federation: FederationConfig,
    /// Operations clients send by hash instead of text, and the allow-list of operations.
    #[serde(default)]
    pub persisted_queries: PersistedQueriesConfig,
}

/// Persisted queries: an operation sent as the SHA-256 hash of its text
/// (`extensions.persistedQuery.sha256Hash`, Apollo's protocol) or as a manifest id
/// (`documentId`). Off by default.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersistedQueriesConfig {
    /// Accept persisted queries at all. Off, a request naming one is answered
    /// `PERSISTED_QUERY_NOT_SUPPORTED` (clients then send the text) and nothing is stored.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// Automatic persisted queries (APQ): a client sending a hash with its text registers it
    /// in this instance's memory, and later sends the hash alone. Ignored with `allow_list`.
    #[serde(default = "default_true", deserialize_with = "switch")]
    pub automatic: bool,
    /// A JSON file of known operations, read at startup: an Apollo persisted query manifest
    /// (`{"operations": [{"id", "body"}]}`) or a map of ids to texts (`{"<id>": "<text>"}`).
    #[serde(default)]
    pub manifest: Option<String>,
    /// Run only the manifest's operations: by id, by hash, or as text whose hash is listed.
    #[serde(default, deserialize_with = "switch")]
    pub allow_list: bool,
}

impl Default for PersistedQueriesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            automatic: true,
            manifest: None,
            allow_list: false,
        }
    }
}

impl PersistedQueriesConfig {
    /// Problems with the values.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.allow_list && !self.enabled {
            problems.push(
                "graphql.persisted_queries.allow_list: needs graphql.persisted_queries.enabled"
                    .to_string(),
            );
        }
        if self.allow_list && self.manifest.is_none() {
            problems.push(
                "graphql.persisted_queries.allow_list: needs a manifest of the allowed operations"
                    .to_string(),
            );
        }
        problems
    }
}

fn default_sdl_path() -> String {
    "/schema.graphql".to_string()
}
fn default_subgraph_path() -> String {
    "/subgraph.graphql".to_string()
}

impl Default for GraphqlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ui: false,
            introspection: false,
            sdl: false,
            sdl_path: default_sdl_path(),
            subgraph_path: default_subgraph_path(),
            federation: FederationConfig::default(),
            persisted_queries: PersistedQueriesConfig::default(),
        }
    }
}

fn default_openapi_path() -> String {
    "/openapi.json".to_string()
}

/// The REST API and its OpenAPI description.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OpenapiConfig {
    /// Serve the REST API and OpenAPI document.
    #[serde(deserialize_with = "switch", default = "default_true")]
    pub enabled: bool,
    /// Serve the Scalar API reference at `/scalar`, showing the OpenAPI document at
    /// `/openapi.json`. On in `dev` mode, off in `prod`.
    #[serde(default, deserialize_with = "switch")]
    pub ui: bool,
    /// Serve the OpenAPI 3.1 document at `path` (default `/openapi.json`). On in `dev` mode, off in `prod`.
    #[serde(default, alias = "schema", deserialize_with = "switch")]
    pub spec: bool,
    /// Path to serve the OpenAPI document at (defaults to `/openapi.json`).
    #[serde(default = "default_openapi_path", alias = "spec_path")]
    pub path: String,
}

impl Default for OpenapiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ui: false,
            spec: false,
            path: default_openapi_path(),
        }
    }
}

pub type RestConfig = OpenapiConfig;

/// Apollo Federation configuration for exposing this engine as a federated subgraph.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FederationConfig {
    /// Enable Apollo Federation subgraph mode.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// Federation spec version (defaults to "v2.3").
    #[serde(default)]
    pub version: Option<String>,
}

/// Role-based access control (RBAC) schema pruning configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RbacConfig {
    /// Whether to prune the GraphQL schema based on PostgreSQL role privileges.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// Optional PostgreSQL role to switch to (`SET ROLE`) during schema inspection.
    /// If None, privileges are checked against the currently connected database user/role.
    #[serde(default)]
    pub role: Option<String>,
}

/// Authentication: the engine is an OAuth 2.0 resource server. Requests carry access tokens
/// as `Authorization: Bearer <token>` (RFC 6750); tokens are JWTs (RFC 9068) signed by the
/// authorization server, whose claims become the PostgreSQL role and the session settings
/// Row-Level Security policies read (`current_setting('request.jwt.claims', true)`).
///
/// Off by default: every request then runs as the engine's database user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Whether requests are authenticated.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// The authorization server's issuer identifier: a token's `iss` must equal it, and the
    /// signing keys are discovered from its metadata unless `jwks_uri` names them.
    #[serde(default)]
    pub issuer: Option<String>,
    /// This API's identifiers: a token's `aud` must contain one of them.
    #[serde(default, deserialize_with = "one_or_many")]
    pub audience: Vec<String>,
    /// URL of the authorization server's JSON Web Key Set, when it is not discovered from
    /// `issuer` (OpenID Connect discovery, then RFC 8414 metadata).
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// Signature algorithms (`alg`) accepted. By default the asymmetric ones with a key set
    /// (RS256, RS384, RS512, PS256, PS384, PS512, ES256, ES384, EdDSA), HS256 with a secret.
    #[serde(default)]
    pub algorithms: Vec<String>,
    /// The `typ` header tokens must have, e.g. `at+jwt` (RFC 9068); not checked when unset,
    /// since many authorization servers issue `JWT`.
    #[serde(default)]
    pub token_type: Option<String>,
    /// Clock skew tolerated when checking `exp` and `nbf`, in seconds.
    #[serde(default = "default_leeway", deserialize_with = "lenient")]
    pub leeway_seconds: u64,
    /// The PostgreSQL roles requests run as.
    #[serde(default)]
    pub roles: RolesConfig,
    /// The session settings a request's claims are written to.
    #[serde(default)]
    pub session_variables: SessionVariablesConfig,
}

fn default_leeway() -> u64 {
    30
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            issuer: None,
            audience: Vec::new(),
            jwks_uri: None,
            algorithms: Vec::new(),
            token_type: None,
            leeway_seconds: default_leeway(),
            roles: RolesConfig::default(),
            session_variables: SessionVariablesConfig::default(),
        }
    }
}

/// A string or a list of strings.
fn one_or_many<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

/// The PostgreSQL roles requests run as (`SET LOCAL ROLE`), fixed by deployment: tokens never
/// choose one. Their claims (an application `role` claim included) are data that Row-Level
/// Security policies compare with whatever the schema holds; what they mean is the
/// application's to decide. The engine's database user must be a member of each role, and
/// should itself hold no privileges (`NOINHERIT`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RolesConfig {
    /// Role of requests with a valid token. Unset, they run as the engine's database user,
    /// which must then neither own the tables nor bypass RLS for policies to apply.
    #[serde(default)]
    pub authenticated: Option<String>,
    /// Role of requests without a token. Unset, they are rejected (401).
    #[serde(default)]
    pub anonymous: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionVariablesConfig {
    /// Prefix of a setting per top-level claim (`request.jwt.claim.sub`); claims whose names
    /// are not identifiers get none. Empty: no per-claim settings.
    #[serde(default = "default_claim_prefix")]
    pub prefix: String,
    /// Setting holding every claim as JSON.
    #[serde(default = "default_claims_setting")]
    pub claims_setting: String,
    /// Further settings: claim (name or dot-separated path) -> setting name, e.g.
    /// `org_id: app.current_org_id`.
    #[serde(default)]
    pub mappings: BTreeMap<String, String>,
}

fn default_claim_prefix() -> String {
    "request.jwt.claim.".to_string()
}
fn default_claims_setting() -> String {
    "request.jwt.claims".to_string()
}

impl Default for SessionVariablesConfig {
    fn default() -> Self {
        Self {
            prefix: default_claim_prefix(),
            claims_setting: default_claims_setting(),
            mappings: BTreeMap::new(),
        }
    }
}

/// Search fields return the best matches, without pagination: `first` hits, `default_limit`
/// when it is left out, never more than `max_limit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    /// Hits returned without `first` (10).
    pub default_limit: Option<usize>,
    /// Largest accepted `first` (50).
    pub max_limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeConfig {
    /// GraphQL type name (a composite's input type is `<name>Input`).
    pub type_name: Option<String>,
    /// Type description (defaults to the type's SQL comment).
    pub description: Option<String>,
    /// Enums: GraphQL value per database label, e.g. `in-progress: ACTIVE`.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// Composites: per-attribute changes, by attribute name.
    #[serde(default)]
    pub attributes: BTreeMap<String, AttributeConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributeConfig {
    pub field_name: Option<String>,
    pub description: Option<String>,
}

/// Tables, views and functions owned by extensions (`pgcrypto`, `citext`, `postgis`, ...) are
/// hidden unless their extension is listed here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionsConfig {
    /// Extensions whose objects are exposed; `"*"` exposes every extension's objects.
    #[serde(default)]
    pub expose: Vec<String>,
    /// What graphpg infers from the extensions the database uses (see `docs/extensions.md`),
    /// by integration: `auto` (the default) when the schema uses the extension, or `off`.
    #[serde(default)]
    pub integrations: BTreeMap<String, crate::catalog::integrations::IntegrationMode>,
}

impl ExtensionsConfig {
    /// Whether integration `i` may be used.
    pub fn integration_on(&self, i: crate::catalog::integrations::Integration) -> bool {
        self.integrations.get(i.name()).copied().unwrap_or_default()
            != crate::catalog::integrations::IntegrationMode::Off
    }

    /// Problems with the values.
    pub fn problems(&self) -> Vec<String> {
        self.integrations
            .keys()
            .filter(|k| crate::catalog::integrations::Integration::from_name(k).is_none())
            .map(|k| {
                let known: Vec<&str> = crate::catalog::integrations::Integration::ALL
                    .iter()
                    .map(|i| i.name())
                    .collect();
                format!(
                    "extensions.integrations.{k}: unknown integration ({})",
                    known.join(", ")
                )
            })
            .collect()
    }

    pub fn exposes_all(&self) -> bool {
        self.expose.iter().any(|e| e == "*")
    }
}

/// Logs, metrics and traces: what a running server says about itself, for Prometheus, an
/// OpenTelemetry collector, or the Grafana LGTM stack (Loki, Grafana, Tempo, Mimir).
/// Read at startup: a change needs a restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// `service.name` of exported telemetry (default: `OTEL_SERVICE_NAME`, else `graphpg`).
    #[serde(default)]
    pub service_name: Option<String>,
    /// Log lines on standard output.
    #[serde(default)]
    pub logs: LogsConfig,
    /// Prometheus metrics endpoint.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// Request traces.
    #[serde(default)]
    pub traces: TracesConfig,
    /// Push traces, metrics and logs to an OpenTelemetry (OTLP/HTTP) endpoint.
    #[serde(default)]
    pub otlp: OtlpConfig,
}

impl TelemetryConfig {
    /// `service_name`, else `OTEL_SERVICE_NAME`, else `graphpg`.
    pub fn resolved_service_name(&self) -> String {
        self.service_name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("OTEL_SERVICE_NAME")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "graphpg".to_string())
    }

    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.metrics.path.starts_with('/') {
            problems.push(format!(
                "telemetry.metrics.path must start with `/`: {:?}",
                self.metrics.path
            ));
        }
        if let Some(listen) = &self.metrics.listen {
            if listen.parse::<std::net::SocketAddr>().is_err() {
                problems.push(format!(
                    "telemetry.metrics.listen must be an address and a port, like `0.0.0.0:9464`: {listen:?}"
                ));
            }
        }
        if !(0.0..=1.0).contains(&self.traces.sample_ratio) {
            problems.push(format!(
                "telemetry.traces.sample_ratio must be between 0 and 1: {}",
                self.traces.sample_ratio
            ));
        }
        if let Some(endpoint) = self.otlp.endpoint.as_deref().filter(|e| !e.is_empty()) {
            if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
                problems.push(format!(
                    "telemetry.otlp.endpoint must be an http:// or https:// URL: {endpoint:?}"
                ));
            }
        }
        problems
    }
}

/// How log lines are written.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Text,
    /// One JSON object per line, for Loki, Elasticsearch, CloudWatch ...
    Json,
}

/// Log lines on standard output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogsConfig {
    /// Lowest level logged, or a filter like `info,graphpg=debug` (`RUST_LOG` wins when set).
    #[serde(default = "default_log_level")]
    pub level: String,
    /// `text` (default) or `json`.
    #[serde(default)]
    pub format: LogFormat,
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::Text,
        }
    }
}

/// The Prometheus metrics endpoint (OpenMetrics text format).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Serve metrics. On in `dev` mode, off in `prod`.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// Path of the endpoint.
    #[serde(default = "default_metrics_path")]
    pub path: String,
    /// Serve the endpoint on a listener of its own (`0.0.0.0:9464`) instead of the API's port,
    /// to keep it off the public network.
    #[serde(default)]
    pub listen: Option<String>,
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_metrics_path(),
            listen: None,
        }
    }
}

/// Request traces: one span per request, with planning, compilation and execution inside it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TracesConfig {
    /// Share of new traces recorded, from 0 to 1. A request carrying a W3C `traceparent`
    /// follows the caller's decision.
    #[serde(default = "default_sample_ratio", deserialize_with = "lenient")]
    pub sample_ratio: f64,
}

fn default_sample_ratio() -> f64 {
    1.0
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            sample_ratio: default_sample_ratio(),
        }
    }
}

/// OpenTelemetry export over OTLP/HTTP (protobuf): to a collector, Grafana Alloy, the
/// `grafana/otel-lgtm` image, Grafana Cloud, or any OTLP backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OtlpConfig {
    /// Base URL, like `http://localhost:4318`; signals go to `/v1/traces`, `/v1/metrics` and
    /// `/v1/logs` below it. Unset: `OTEL_EXPORTER_OTLP_ENDPOINT`, else no export.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Headers of every export request, like `Authorization` (use `${NAME}` for credentials).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Export traces.
    #[serde(default = "default_true", deserialize_with = "switch")]
    pub traces: bool,
    /// Export metrics.
    #[serde(default = "default_true", deserialize_with = "switch")]
    pub metrics: bool,
    /// Export logs.
    #[serde(default = "default_true", deserialize_with = "switch")]
    pub logs: bool,
    /// How often metrics are pushed.
    #[serde(default = "default_export_interval")]
    pub interval: Timeout,
}

fn default_export_interval() -> Timeout {
    Timeout(std::time::Duration::from_secs(60))
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            headers: BTreeMap::new(),
            traces: true,
            metrics: true,
            logs: true,
            interval: default_export_interval(),
        }
    }
}

impl OtlpConfig {
    /// The base URL to export to: `endpoint`, else `OTEL_EXPORTER_OTLP_ENDPOINT`.
    pub fn resolved_endpoint(&self) -> Option<String> {
        self.endpoint
            .clone()
            .filter(|e| !e.is_empty())
            .or_else(|| {
                std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .ok()
                    .filter(|e| !e.is_empty())
            })
            .map(|e| e.trim_end_matches('/').to_string())
    }
}

/// Schema watching on DDL changes and auto-export.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    /// Re-introspect on DDL changes. On in `dev` mode, off in `prod`.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// On each schema change, write the export files.
    #[serde(default = "default_true", deserialize_with = "switch")]
    pub export: bool,
}

/// Reloads the schema without a restart when asked: `NOTIFY <channel>` (from a migration) or
/// `SIGHUP`. Only the database is read again; the configuration is the one the server started
/// with. A reload that fails the startup checks (pinned schemas with `export.pin: fail`, the
/// persisted query manifest in prod) keeps the schema being served.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReloadConfig {
    /// Listen for reload requests (a connection of its own) and handle `SIGHUP`.
    #[serde(default = "default_true", deserialize_with = "switch")]
    pub enabled: bool,
    /// The channel `NOTIFY` reloads on: `NOTIFY graphpg_reload` (any payload).
    #[serde(default = "default_reload_channel")]
    pub channel: String,
}

fn default_reload_channel() -> String {
    "graphpg_reload".to_string()
}

impl Default for ReloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channel: default_reload_channel(),
        }
    }
}

impl ReloadConfig {
    pub fn problems(&self) -> Vec<String> {
        let valid = self
            .channel
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && self
                .channel
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && self.channel.len() <= 63;
        if valid {
            Vec::new()
        } else {
            vec![format!(
                "reload.channel: `{}` is not a channel name (lowercase letters, digits and `_`, at most 63)",
                self.channel
            )]
        }
    }
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            export: true,
        }
    }
}

/// File paths for exported schemas.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    /// Path to export GraphQL SDL schema.
    #[serde(default = "default_export_graphql")]
    pub graphql: String,
    /// Path to export Apollo Federation Subgraph SDL.
    #[serde(default = "default_export_subgraph")]
    pub subgraph: String,
    /// Path to export YAML config JSON schema.
    #[serde(default = "default_export_config_schema")]
    pub config_schema: String,
    /// Path to export the OpenAPI 3.1 document: YAML for `.yaml` / `.yml`, else JSON.
    #[serde(default = "default_export_openapi")]
    pub openapi: String,
    /// What to do at startup when an exported file differs from the schema the database
    /// generates now (files that do not exist pin nothing): `fail` (prod default), `warn` (dev
    /// default) or `off`.
    #[serde(default)]
    pub pin: PinMode,
}

/// What a differing pinned schema does at startup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PinMode {
    /// Refuse to start.
    #[default]
    Fail,
    /// Log what changed and start.
    Warn,
    /// Do not check.
    Off,
}

impl<'de> Deserialize<'de> for PinMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Native {
            Switch(bool),
            Text(String),
        }
        match Native::deserialize(deserializer)? {
            Native::Switch(false) => Ok(Self::Off),
            Native::Switch(true) => Ok(Self::Fail),
            Native::Text(text) => match text.trim().to_ascii_lowercase().as_str() {
                "fail" | "on" | "true" | "yes" => Ok(Self::Fail),
                "warn" => Ok(Self::Warn),
                "off" | "false" | "no" | "none" => Ok(Self::Off),
                _ => Err(serde::de::Error::custom(format!(
                    "invalid value {text:?} (`fail`, `warn` or `off`)"
                ))),
            },
        }
    }
}

fn default_export_graphql() -> String {
    "schema.graphql".to_string()
}
fn default_export_subgraph() -> String {
    "subgraph.graphql".to_string()
}
fn default_export_openapi() -> String {
    "openapi.json".to_string()
}
fn default_export_config_schema() -> String {
    "graphpg.schema.json".to_string()
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            graphql: default_export_graphql(),
            subgraph: default_export_subgraph(),
            config_schema: default_export_config_schema(),
            openapi: default_export_openapi(),
            pin: PinMode::default(),
        }
    }
}

/// Secrets loaded from environment variables only (`DATABASE_URL`, `DATABASE_REPLICA_URLS`, `AUTH_JWT_SECRET`).
/// Never held in `Config`, serialized, or logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Secrets {
    pub database_url: String,
    pub replica_urls: Vec<String>,
    pub auth_jwt_secret: Option<String>,
}

impl Secrets {
    pub fn from_env() -> Result<Self, String> {
        Self::from_env_with(&|k| std::env::var(k).ok())
    }

    pub fn from_env_with(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let database_url = lookup("DATABASE_URL")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "DATABASE_URL is required: set the environment variable".to_string())?;

        let replica_urls = lookup("DATABASE_REPLICA_URLS")
            .unwrap_or_default()
            .split(',')
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty())
            .collect();

        let auth_jwt_secret = lookup("AUTH_JWT_SECRET").filter(|s| !s.trim().is_empty());

        Ok(Self {
            database_url,
            replica_urls,
            auth_jwt_secret,
        })
    }
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("database_url", &"[REDACTED]")
            .field(
                "replica_urls",
                &format!("[REDACTED; {}]", self.replica_urls.len()),
            )
            .field(
                "auth_jwt_secret",
                &self.auth_jwt_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Root fields an entity can generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// Single row by primary key (`user(id: ...)`).
    Read,
    /// Plain list (`users`).
    List,
    /// Relay connection (`usersConnection`).
    Connection,
    /// Insert rows: also omits `create_many`.
    Create,
    /// Update rows (`updateUser`, `updateUserByEmail`): also omits `update_many` and `upsert`.
    Update,
    /// Delete rows (`deleteUser`, `deleteUserByEmail`): also omits `delete_many`.
    Delete,
    /// Insert several rows (`createUsers`).
    CreateMany,
    /// Update the rows matching a filter (`updateUsers`).
    UpdateMany,
    /// Delete the rows matching a filter (`deleteUsers`).
    DeleteMany,
    /// The `onConflict` argument of creates.
    Upsert,
}

impl Operation {
    pub const ALL: [Operation; 10] = [
        Self::Read,
        Self::List,
        Self::Connection,
        Self::Create,
        Self::Update,
        Self::Delete,
        Self::CreateMany,
        Self::UpdateMany,
        Self::DeleteMany,
        Self::Upsert,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::List => "list",
            Self::Connection => "connection",
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::CreateMany => "create_many",
            Self::UpdateMany => "update_many",
            Self::DeleteMany => "delete_many",
            Self::Upsert => "upsert",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityConfig {
    /// GraphQL type name. Derived names follow it (`<Type>Filter`, `create<Type>`, ...).
    pub type_name: Option<String>,
    /// Type description (defaults to the relation's SQL comment).
    pub description: Option<String>,
    /// Deprecate the type's root fields and the relation fields leading to it, with this
    /// reason (also `@deprecated <reason>` in the SQL comment).
    pub deprecated: Option<String>,
    /// Remove the type, its root fields and every relation field pointing at it.
    #[serde(default)]
    pub omit: bool,
    /// Root fields not to generate.
    #[serde(default)]
    pub omit_operations: Vec<Operation>,
    /// Name of the list root field.
    pub list_field: Option<String>,
    /// Name of the single-row-by-key root field.
    pub single_field: Option<String>,
    /// Name of the connection root field.
    pub connection_field: Option<String>,
    /// Key columns of a relation without a primary key (views, materialized views). Gives
    /// the type row lookups, stable cursors and, for a single column, a Relay `id`.
    #[serde(default)]
    pub primary_key: Vec<String>,
    /// Column a trigger maintains as the row's version: `rowVersion` reads it and
    /// `expectedVersion` is compared with it. Without one, rows are versioned by `xmin`,
    /// which every table has (views need a version column to be versioned at all).
    pub version_column: Option<String>,
    /// Per-column changes, by column name.
    #[serde(default)]
    pub columns: BTreeMap<String, ColumnConfig>,
    /// Relation fields of foreign keys, by constraint name. A name that is not a constraint
    /// of the relation declares a new (virtual) foreign key and needs `columns` and `references`.
    #[serde(default)]
    pub foreign_keys: BTreeMap<String, ForeignKeyConfig>,
    /// Apollo Federation key fields override (e.g. `"id"` or `"tenantId userId"`).
    #[serde(default)]
    pub key_fields: Option<String>,
    /// Mark entity as @shareable in Apollo Federation.
    #[serde(default)]
    pub shareable: Option<bool>,
    /// Change events of the table (event triggers): the OpenAPI document describes them as
    /// `webhooks`, with the payload typed from the table's row.
    #[serde(default)]
    pub events: Option<EventsConfig>,
}

/// The change events of a table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EventsConfig {
    /// Row changes that raise an event (default: all of them).
    #[serde(default = "EventOperation::all")]
    pub operations: Vec<EventOperation>,
    /// What the events of the table mean, for the documentation of the webhook.
    pub description: Option<String>,
}

/// A change of a row.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EventOperation {
    Insert,
    Update,
    Delete,
}

/// Event triggers (see `docs/events.md`): webhooks called on row changes, captured in the same
/// transaction as the change, and on schedules. Nothing is installed or run without a trigger
/// or a schedule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventTriggersConfig {
    /// Capture and deliver events at all. Off, nothing is installed, checked or delivered.
    #[serde(default = "default_true", deserialize_with = "switch")]
    pub enabled: bool,
    /// The schema graphpg keeps its event table and functions in.
    #[serde(default = "default_events_schema")]
    pub schema: String,
    /// `auto`: create and update the event table, functions and triggers at startup.
    /// `manual`: only check them; `graphpg events sql` prints the DDL.
    #[serde(default)]
    pub install: EventInstall,
    /// Webhooks on row changes, by name.
    #[serde(default)]
    pub triggers: BTreeMap<String, EventTriggerConfig>,
    /// Webhooks on a schedule (`cron`), or for one-off events scheduled from SQL, by name.
    #[serde(default)]
    pub schedules: BTreeMap<String, ScheduleConfig>,
    /// How events are delivered.
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

fn default_events_schema() -> String {
    "graphpg_events".to_string()
}

impl Default for EventTriggersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            schema: default_events_schema(),
            install: EventInstall::default(),
            triggers: BTreeMap::new(),
            schedules: BTreeMap::new(),
            delivery: DeliveryConfig::default(),
        }
    }
}

impl EventTriggersConfig {
    /// Whether anything is to be captured or delivered.
    pub fn active(&self) -> bool {
        self.enabled && !(self.triggers.is_empty() && self.schedules.is_empty())
    }

    /// The webhook of trigger or schedule `name`.
    pub fn webhook(&self, name: &str) -> Option<WebhookConfig> {
        self.triggers
            .get(name)
            .map(EventTriggerConfig::webhook)
            .or_else(|| self.schedules.get(name).map(ScheduleConfig::webhook))
    }

    /// Problems with the values, all reported together.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let name_ok = |n: &str| {
            n.chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
                && n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && n.len() <= 63
        };
        if !name_ok(&self.schema) {
            problems.push(format!(
                "events.schema: `{}` is not a lowercase identifier",
                self.schema
            ));
        }
        for (name, t) in &self.triggers {
            let path = format!("events.triggers.{name}");
            if !name_ok(name) || name.len() > 40 {
                problems.push(format!(
                    "{path}: the name must be a lowercase identifier of at most 40 characters"
                ));
            }
            if self.schedules.contains_key(name) {
                problems.push(format!("{path}: a schedule has the same name"));
            }
            if t.operations.is_empty() {
                problems.push(format!("{path}.operations: name at least one"));
            }
            if !t.columns.is_empty() && !t.operations.contains(&EventOperation::Update) {
                problems.push(format!(
                    "{path}.columns: only updates are filtered by columns, and `update` is not among its operations"
                ));
            }
            problems.extend(t.webhook().problems(&path));
        }
        for (name, s) in &self.schedules {
            let path = format!("events.schedules.{name}");
            if !name_ok(name) {
                problems.push(format!("{path}: the name must be a lowercase identifier"));
            }
            if let Some(cron) = &s.cron {
                if let Err(err) = crate::events::cron::Schedule::parse(cron) {
                    problems.push(format!("{path}.cron: {err}"));
                }
            }
            problems.extend(s.webhook().problems(&path));
        }
        let d = &self.delivery;
        if d.concurrency == 0 || d.batch == 0 {
            problems.push("events.delivery: concurrency and batch must be at least 1".to_string());
        }
        problems
    }
}

/// Who installs the event table, functions and triggers.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventInstall {
    /// graphpg, at startup.
    #[default]
    Auto,
    /// A migration (`graphpg events sql`); graphpg checks.
    Manual,
}

/// Where and how an event is sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// The endpoint: `POST`ed the event as JSON.
    pub url: String,
    /// Headers added to every request (values may read `${NAME}` from the environment).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Signing secret (Standard Webhooks): `whsec_<base64>`, or any text.
    #[serde(default)]
    pub secret: Option<String>,
    /// How long a request may take.
    #[serde(default = "default_webhook_timeout")]
    pub timeout: Timeout,
    /// Retries of failed deliveries.
    #[serde(default)]
    pub retry: RetryConfig,
}

fn default_webhook_timeout() -> Timeout {
    Timeout(std::time::Duration::from_secs(10))
}

impl WebhookConfig {
    fn problems(&self, path: &str) -> Vec<String> {
        let mut problems = Vec::new();
        if !(self.url.starts_with("https://") || self.url.starts_with("http://")) {
            problems.push(format!("{path}.url: `{}` is not an http(s) URL", self.url));
        }
        if self.retry.max_attempts == 0 {
            problems.push(format!("{path}.retry.max_attempts: must be at least 1"));
        }
        problems
    }
}

/// Retries: exponential backoff with jitter, then the event is dead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    /// Deliveries attempted at most, the first included.
    #[serde(default = "default_max_attempts", deserialize_with = "lenient")]
    pub max_attempts: u32,
    /// Wait before the second attempt, doubled for each further one.
    #[serde(default = "default_backoff")]
    pub backoff: Timeout,
    /// Longest wait between attempts.
    #[serde(default = "default_max_backoff")]
    pub max_backoff: Timeout,
}

fn default_max_attempts() -> u32 {
    10
}
fn default_backoff() -> Timeout {
    Timeout(std::time::Duration::from_secs(10))
}
fn default_max_backoff() -> Timeout {
    Timeout(std::time::Duration::from_secs(3600))
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff: default_backoff(),
            max_backoff: default_max_backoff(),
        }
    }
}

/// A webhook on row changes of a table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventTriggerConfig {
    /// The table, `table` or `schema.table`.
    pub table: String,
    /// The changes that raise an event (default: all of them).
    #[serde(default = "EventOperation::all")]
    pub operations: Vec<EventOperation>,
    /// Updates raise an event only when one of these columns changes (default: any change).
    #[serde(default)]
    pub columns: Vec<String>,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default = "default_webhook_timeout")]
    pub timeout: Timeout,
    #[serde(default)]
    pub retry: RetryConfig,
}

/// A webhook on a schedule, or for one-off events scheduled from SQL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    /// When: 5-field cron in UTC (`0 3 * * *`), or `@hourly`, `@daily`, `@weekly`,
    /// `@monthly`. Without it, only events scheduled from SQL are sent.
    #[serde(default)]
    pub cron: Option<String>,
    /// The `payload` member of the event.
    #[serde(default)]
    pub payload: serde_json::Value,
    /// Slots missed while no instance ran: `latest` sends the most recent one, `none` none.
    #[serde(default)]
    pub catch_up: CatchUp,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default = "default_webhook_timeout")]
    pub timeout: Timeout,
    #[serde(default)]
    pub retry: RetryConfig,
}

impl EventTriggerConfig {
    pub fn webhook(&self) -> WebhookConfig {
        WebhookConfig {
            url: self.url.clone(),
            headers: self.headers.clone(),
            secret: self.secret.clone(),
            timeout: self.timeout,
            retry: self.retry.clone(),
        }
    }
}

impl ScheduleConfig {
    pub fn webhook(&self) -> WebhookConfig {
        WebhookConfig {
            url: self.url.clone(),
            headers: self.headers.clone(),
            secret: self.secret.clone(),
            timeout: self.timeout,
            retry: self.retry.clone(),
        }
    }
}

/// Missed schedule slots.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CatchUp {
    #[default]
    Latest,
    None,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            headers: BTreeMap::new(),
            secret: None,
            timeout: default_webhook_timeout(),
            retry: RetryConfig::default(),
        }
    }
}

/// How events are delivered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeliveryConfig {
    /// Requests in flight at once, per instance.
    #[serde(default = "default_concurrency", deserialize_with = "lenient")]
    pub concurrency: usize,
    /// Events claimed at a time.
    #[serde(default = "default_batch", deserialize_with = "lenient")]
    pub batch: usize,
    /// Longest wait for new events when no notification arrives.
    #[serde(default = "default_poll")]
    pub poll: Timeout,
    /// How long a claimed event is held: past it, another instance may deliver it again.
    #[serde(default = "default_lease")]
    pub lease: Timeout,
    /// Delivered events are deleted after this long.
    #[serde(default = "default_retention")]
    pub retention: Timeout,
    /// Dead events (out of attempts) are deleted after this long.
    #[serde(default = "default_dead_retention")]
    pub dead_retention: Timeout,
}

fn default_concurrency() -> usize {
    4
}
fn default_batch() -> usize {
    50
}
fn default_poll() -> Timeout {
    Timeout(std::time::Duration::from_secs(5))
}
fn default_lease() -> Timeout {
    Timeout(std::time::Duration::from_secs(60))
}
fn default_retention() -> Timeout {
    Timeout(std::time::Duration::from_secs(7 * 86_400))
}
fn default_dead_retention() -> Timeout {
    Timeout(std::time::Duration::from_secs(30 * 86_400))
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            batch: default_batch(),
            poll: default_poll(),
            lease: default_lease(),
            retention: default_retention(),
            dead_retention: default_dead_retention(),
        }
    }
}

impl EventOperation {
    pub fn all() -> Vec<Self> {
        vec![Self::Insert, Self::Update, Self::Delete]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnConfig {
    /// GraphQL field name (also used in inputs, filters and `orderBy` values).
    pub field_name: Option<String>,
    /// Field description (defaults to the column's SQL comment).
    pub description: Option<String>,
    /// Deprecate the field, with this reason (also `@deprecated <reason>` in the SQL comment).
    pub deprecated: Option<String>,
    /// Hide the column from every type, input, filter and ordering.
    #[serde(default)]
    pub omit: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyConfig {
    /// Referencing columns (declares a virtual foreign key).
    #[serde(default)]
    pub columns: Vec<String>,
    /// Referenced relation and columns (declares a virtual foreign key).
    pub references: Option<ReferenceConfig>,
    /// Name of the to-one relation field on this type.
    pub field_name: Option<String>,
    /// Name of the to-many relation field on the referenced type; its connection field is
    /// `<name>Connection`.
    pub reverse_field_name: Option<String>,
    /// Hide both relation fields.
    #[serde(default)]
    pub omit: bool,
    /// Hide the to-many relation fields on the referenced type.
    #[serde(default)]
    pub omit_reverse: bool,
    /// Foreign tables: read the to-many relation of a page of parents in one query
    /// (`= ANY` their keys) instead of one per parent. Every child of those parents is
    /// fetched and trimmed to the page size locally, so use it when parents have few
    /// children each (see `docs/foreign-data.md`).
    #[serde(default)]
    pub batch: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceConfig {
    /// Referenced relation, in the same schema.
    pub table: String,
    /// Referenced columns (default: its primary key).
    #[serde(default)]
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionConfig {
    /// Root Query field name (table-valued functions) or field name on the table's type
    /// (computed columns).
    pub field_name: Option<String>,
    /// Field description (defaults to the function's SQL comment).
    pub description: Option<String>,
    /// Deprecate the function's fields, with this reason.
    pub deprecated: Option<String>,
    /// Remove the root Query field, or the computed column field.
    #[serde(default)]
    pub omit: bool,
    /// Expose a table-valued function as a field of a table/view type too.
    pub mount: Option<MountConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    /// Relation whose type gets the field (default: the one detected from the function's
    /// first argument).
    pub on: Option<String>,
    /// Field name on that type.
    pub field_name: Option<String>,
    /// Function arguments filled from the parent row: `argument: column`. Default: the first
    /// argument gets the parent's single-column primary key. Other arguments become field
    /// arguments.
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    /// Do not mount (also disables a detected mount).
    #[serde(default)]
    pub omit: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sample_yaml_config() {
        let yaml_str = r#"
entities:
  users:
    type_name: Person
    omit_operations: [delete]
    columns:
      password_hash: { omit: true }
      birth_date: { field_name: dateOfBirth }

  user_contact_view:
    primary_key: [user_id, value]
    foreign_keys:
      user_contact_view_user:
        columns: [user_id]
        references: { table: users }
        field_name: user
        reverse_field_name: contacts

functions:
  flatten_menu_items:
    mount:
      on: menus
      field_name: flattenedItems
      arguments: { m_id: id }
"#;
        let config = Config::from_yaml(yaml_str).expect("failed to parse YAML");
        assert_eq!(config.entities.len(), 2);

        let users = &config.entities["users"];
        assert_eq!(users.type_name.as_deref(), Some("Person"));
        assert_eq!(users.omit_operations, vec![Operation::Delete]);
        assert!(users.columns["password_hash"].omit);
        assert_eq!(
            users.columns["birth_date"].field_name.as_deref(),
            Some("dateOfBirth")
        );

        let view = &config.entities["user_contact_view"];
        assert_eq!(view.primary_key, vec!["user_id", "value"]);
        let fk = &view.foreign_keys["user_contact_view_user"];
        assert_eq!(fk.columns, vec!["user_id"]);
        assert_eq!(fk.references.as_ref().unwrap().table, "users");
        assert!(fk.references.as_ref().unwrap().columns.is_empty());
        assert_eq!(fk.reverse_field_name.as_deref(), Some("contacts"));

        let mount = config.functions["flatten_menu_items"]
            .mount
            .as_ref()
            .unwrap();
        assert_eq!(mount.on.as_deref(), Some("menus"));
        assert_eq!(mount.arguments["m_id"], "id");
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = Config::from_yaml("entities:\n  users:\n    rename_type: X\n").unwrap_err();
        assert!(err.to_string().contains("rename_type"), "{err}");
    }

    #[test]
    fn empty_config() {
        let config = Config::from_yaml("  \n").unwrap();
        assert!(config.entities.is_empty() && config.functions.is_empty());
    }

    #[test]
    fn secrets_debug_is_redacted_and_database_url_is_required() {
        // Redacting Debug: raw values must never appear in Debug format
        let secrets = Secrets {
            database_url: "postgres://user:secretpass@db.example.com/mydb".to_string(),
            replica_urls: vec![
                "postgres://user:secretpass@replica1/mydb".to_string(),
                "postgres://user:secretpass@replica2/mydb".to_string(),
            ],
            auth_jwt_secret: Some("my-super-secret-jwt-signing-key".to_string()),
        };
        let debug_str = format!("{secrets:?}");
        assert!(!debug_str.contains("secretpass"), "{debug_str}");
        assert!(!debug_str.contains("secretpass"), "{debug_str}");
        assert!(
            !debug_str.contains("my-super-secret-jwt-signing-key"),
            "{debug_str}"
        );
        assert!(debug_str.contains("[REDACTED]"), "{debug_str}");

        // DATABASE_URL is required
        let err = Secrets::from_env_with(&|_| None).unwrap_err();
        assert!(err.contains("DATABASE_URL"), "{err}");

        // DATABASE_URL is parsed
        let parsed = Secrets::from_env_with(&|key| match key {
            "DATABASE_URL" => Some("postgres://localhost/mydb".to_string()),
            "DATABASE_REPLICA_URLS" => Some("postgres://r1, postgres://r2".to_string()),
            "AUTH_JWT_SECRET" => Some("jwtsecret".to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(parsed.database_url, "postgres://localhost/mydb");
        assert_eq!(parsed.replica_urls, vec!["postgres://r1", "postgres://r2"]);
        assert_eq!(parsed.auth_jwt_secret.as_deref(), Some("jwtsecret"));
    }
}

/// Live queries (see `docs/subscriptions.md`): a `Subscription` type with the `Query` type's
/// root fields, whose result is sent again whenever it changes. Every `interval`, each
/// instance runs its subscriptions' queries again, batched, and sends the results that
/// changed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionsConfig {
    /// Serve subscriptions at all (off by default: the schema has no `Subscription` type).
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// How often subscriptions' queries are run again.
    #[serde(default = "default_subscription_interval")]
    pub interval: Timeout,
    /// Where they run: `replica` (one of `database.replicas` when there are any, else the
    /// primary) or `primary`.
    #[serde(default)]
    pub source: SubscriptionSource,
    /// Subscriptions open at once on an instance; more are refused.
    #[serde(default = "default_max_subscriptions", deserialize_with = "lenient")]
    pub max_per_instance: usize,
    /// Subscriptions open at once per client (the token's `sub`, or the address without a
    /// token), per instance; more are refused.
    #[serde(default = "default_max_per_subject", deserialize_with = "lenient")]
    pub max_per_subject: usize,
    /// Subscriptions run in one statement at most (those of one operation, run with the
    /// same claims).
    #[serde(default = "default_subscription_batch", deserialize_with = "lenient")]
    pub batch: usize,
    /// Connections a round of re-runs uses at once.
    #[serde(
        default = "default_subscription_concurrency",
        deserialize_with = "lenient"
    )]
    pub concurrency: usize,
    /// Keep-alive messages on idle streams (WebSocket pings, SSE comments, empty multipart
    /// parts), so proxies keep them open.
    #[serde(default = "default_keep_alive")]
    pub keep_alive: Timeout,
    /// How long a WebSocket client has to send `connection_init`.
    #[serde(default = "default_init_timeout")]
    pub init_timeout: Timeout,
}

fn default_subscription_interval() -> Timeout {
    Timeout(std::time::Duration::from_secs(1))
}
fn default_max_subscriptions() -> usize {
    10_000
}
fn default_max_per_subject() -> usize {
    100
}
fn default_subscription_batch() -> usize {
    100
}
fn default_subscription_concurrency() -> usize {
    4
}
fn default_keep_alive() -> Timeout {
    Timeout(std::time::Duration::from_secs(15))
}
fn default_init_timeout() -> Timeout {
    Timeout(std::time::Duration::from_secs(10))
}

impl Default for SubscriptionsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: default_subscription_interval(),
            source: SubscriptionSource::default(),
            max_per_instance: default_max_subscriptions(),
            max_per_subject: default_max_per_subject(),
            batch: default_subscription_batch(),
            concurrency: default_subscription_concurrency(),
            keep_alive: default_keep_alive(),
            init_timeout: default_init_timeout(),
        }
    }
}

impl SubscriptionsConfig {
    /// Problems with the values, all reported together.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.interval.0 < std::time::Duration::from_millis(50) {
            problems.push("subscriptions.interval: must be at least 50ms".to_string());
        }
        for (name, value) in [
            ("max_per_instance", self.max_per_instance),
            ("max_per_subject", self.max_per_subject),
            ("batch", self.batch),
            ("concurrency", self.concurrency),
        ] {
            if value == 0 {
                problems.push(format!("subscriptions.{name}: must be at least 1"));
            }
        }
        if self.keep_alive.0 < std::time::Duration::from_secs(1) {
            problems.push("subscriptions.keep_alive: must be at least 1s".to_string());
        }
        if self.init_timeout.0.is_zero() {
            problems.push("subscriptions.init_timeout: must be more than 0".to_string());
        }
        problems
    }
}

/// Where subscriptions' queries run.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionSource {
    /// A read replica when there is one (`database.replicas`), else the primary.
    #[default]
    Replica,
    /// Always the primary.
    Primary,
}
