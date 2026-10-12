//! Request rate limits (`limits.rate_limit`), for the GraphQL and REST APIs.
//!
//! - A client is the verified access token's `key_claim` (`sub`), else its IP address: the
//!   connection's peer, or the `X-Forwarded-For` entry a trusted proxy added.
//! - Each client has a token bucket, kept as the Generic Cell Rate Algorithm's one timestamp
//!   (the theoretical arrival time): it refills at `requests_per_minute` and holds `burst`.
//!   A bucket that has refilled is the same as none, so it is dropped first when the table
//!   is full.
//! - Every answer carries `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset`;
//!   a limited request is answered `429` with `Retry-After` and the code `RATE_LIMITED`
//!   (a GraphQL error, or a REST problem document), before it is parsed or planned.
use super::http::ServerState;
use super::rest::ProblemDetails;
use crate::config::models::{IpRange, RateConfig, RateLimitConfig};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Independently locked parts of the client table.
const SHARDS: usize = 16;
const NANOS_PER_MINUTE: u64 = 60_000_000_000;
const NANOS_PER_SECOND: u64 = 1_000_000_000;

pub const LIMIT_HEADER: HeaderName = HeaderName::from_static("ratelimit-limit");
pub const REMAINING_HEADER: HeaderName = HeaderName::from_static("ratelimit-remaining");
pub const RESET_HEADER: HeaderName = HeaderName::from_static("ratelimit-reset");

/// A rate: one request every `interval` nanoseconds, `burst` at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rate {
    interval: u64,
    burst: u64,
}

impl Rate {
    /// `None` for a rate that is `off`.
    fn of(config: &RateConfig) -> Option<Self> {
        let per_minute = u64::from(config.requests_per_minute?.max(1));
        Some(Self {
            interval: (NANOS_PER_MINUTE / per_minute).max(1),
            burst: u64::from(config.burst.unwrap_or(per_minute as u32).max(1)),
        })
    }
}

/// What a client's bucket says of one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub allowed: bool,
    /// Requests the bucket holds (`RateLimit-Limit`).
    pub limit: u64,
    /// Requests left now (`RateLimit-Remaining`).
    pub remaining: u64,
    /// Seconds until the bucket is full again (`RateLimit-Reset`).
    pub reset: u64,
    /// Seconds until a limited request may be retried (`Retry-After`).
    pub retry_after: u64,
}

impl Outcome {
    fn write_headers(&self, headers: &mut HeaderMap) {
        headers.insert(LIMIT_HEADER, HeaderValue::from(self.limit));
        headers.insert(REMAINING_HEADER, HeaderValue::from(self.remaining));
        headers.insert(RESET_HEADER, HeaderValue::from(self.reset));
        if !self.allowed {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(self.retry_after));
        }
    }
}

/// The buckets of every client this instance has seen recently.
pub struct RateLimiter {
    anonymous: Option<Rate>,
    authenticated: Option<Rate>,
    key_claim: String,
    trusted_proxies: Vec<IpRange>,
    max_per_shard: usize,
    start: Instant,
    /// Client -> theoretical arrival time, in nanoseconds since `start`.
    shards: Vec<Mutex<HashMap<String, u64>>>,
}

impl RateLimiter {
    /// The limiter of `config`, `None` when rate limiting is off.
    pub fn new(config: &RateLimitConfig) -> Option<Arc<Self>> {
        if !config.enabled {
            return None;
        }
        Some(Arc::new(Self {
            anonymous: Rate::of(&config.anonymous),
            authenticated: Rate::of(&config.authenticated),
            key_claim: config.key_claim.clone(),
            trusted_proxies: config
                .trusted_proxies
                .iter()
                .filter_map(|p| IpRange::parse(p).ok())
                .collect(),
            max_per_shard: config.max_clients.div_ceil(SHARDS).max(1),
            start: Instant::now(),
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
        }))
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// Takes one request from `client`'s bucket at `now` (nanoseconds since `start`).
    fn take(&self, client: &str, rate: Rate, now: u64) -> Outcome {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        client.hash(&mut hasher);
        let mut shard = self.shards[hasher.finish() as usize % SHARDS]
            .lock()
            .unwrap();
        let capacity = rate.interval * rate.burst;
        let tat = shard.get(client).copied().unwrap_or(now).max(now);
        let next = tat + rate.interval;
        let seconds = |nanos: u64| nanos.div_ceil(NANOS_PER_SECOND);
        if next - now > capacity {
            let wait = next - now - capacity;
            return Outcome {
                allowed: false,
                limit: rate.burst,
                remaining: 0,
                reset: seconds(tat - now),
                retry_after: seconds(wait).max(1),
            };
        }
        if !shard.contains_key(client) && shard.len() >= self.max_per_shard {
            // Full buckets need no entry; past the bound every client starts over
            shard.retain(|_, tat| *tat > now);
            if shard.len() >= self.max_per_shard {
                shard.clear();
            }
        }
        shard.insert(client.to_string(), next);
        Outcome {
            allowed: true,
            limit: rate.burst,
            remaining: (capacity - (next - now)) / rate.interval,
            reset: seconds(next - now),
            retry_after: 0,
        }
    }

    /// The client a request counts against and its rate (`None`: not limited).
    async fn client(
        &self,
        state: &ServerState,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
    ) -> (String, Option<Rate>) {
        if let Some(auth) = &state.auth {
            if headers.contains_key(header::AUTHORIZATION) {
                if let Some(id) = auth.verified_claim(headers, &self.key_claim).await {
                    return (format!("token:{id}"), self.authenticated);
                }
            }
        }
        let ip = self.client_ip(peer, headers);
        let ip = ip.map_or_else(|| "unknown".to_string(), |ip| ip.to_string());
        (format!("ip:{ip}"), self.anonymous)
    }

    /// The client's address: the peer, or when the peer is a trusted proxy, the last
    /// `X-Forwarded-For` entry no trusted proxy added.
    fn client_ip(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> Option<IpAddr> {
        let trusted = |ip: IpAddr| self.trusted_proxies.iter().any(|r| r.contains(ip));
        let peer = peer?;
        if !trusted(peer) {
            return Some(peer);
        }
        let forwarded: Vec<IpAddr> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|entry| entry.trim().parse().ok())
            .collect();
        forwarded
            .iter()
            .rev()
            .find(|ip| !trusted(**ip))
            .or(forwarded.first())
            .copied()
            .or(Some(peer))
    }
}

/// Which API answers a limited request, and so in which shape.
#[derive(Clone, Copy)]
enum Api {
    Graphql,
    Rest,
}

/// Limits a GraphQL route (`/graphql`, `/export`, `/import/...`).
pub(crate) async fn limit_graphql(
    State(state): State<Arc<ServerState>>,
    request: Request,
    next: Next,
) -> Response {
    limit(state, request, next, Api::Graphql).await
}

/// Limits a REST route.
pub(crate) async fn limit_rest(
    State(state): State<Arc<ServerState>>,
    request: Request,
    next: Next,
) -> Response {
    limit(state, request, next, Api::Rest).await
}

async fn limit(state: Arc<ServerState>, request: Request, next: Next, api: Api) -> Response {
    let Some(limiter) = state.rate_limiter.clone() else {
        return next.run(request).await;
    };
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let (client, rate) = limiter.client(&state, request.headers(), peer).await;
    let Some(rate) = rate else {
        return next.run(request).await;
    };
    let outcome = limiter.take(&client, rate, limiter.now());
    if !outcome.allowed {
        tracing::debug!(%client, retry_after = outcome.retry_after, "rate limited");
        let mut response = limited(api, outcome.retry_after);
        outcome.write_headers(response.headers_mut());
        return response;
    }
    let mut response = next.run(request).await;
    outcome.write_headers(response.headers_mut());
    response
}

/// The answer to a limited request.
fn limited(api: Api, retry_after: u64) -> Response {
    let message = format!("too many requests: retry in {retry_after} s");
    match api {
        Api::Graphql => {
            let body = serde_json::json!({
                "errors": [{
                    "message": message,
                    "extensions": { "code": "RATE_LIMITED", "retryAfter": retry_after }
                }]
            });
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(
                    header::CONTENT_TYPE,
                    "application/graphql-response+json; charset=utf-8",
                )],
                axum::Json(body),
            )
                .into_response()
        }
        Api::Rest => ProblemDetails::new(StatusCode::TOO_MANY_REQUESTS, message)
            .with_code("RATE_LIMITED")
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(config: RateLimitConfig) -> Arc<RateLimiter> {
        RateLimiter::new(&RateLimitConfig {
            enabled: true,
            ..config
        })
        .unwrap()
    }

    fn rate(per_minute: u32, burst: Option<u32>) -> RateConfig {
        RateConfig {
            requests_per_minute: Some(per_minute),
            burst,
        }
    }

    #[test]
    fn off_builds_nothing() {
        assert!(RateLimiter::new(&RateLimitConfig::default()).is_none());
    }

    #[test]
    fn burst_then_refill_at_the_rate() {
        let limiter = limiter(RateLimitConfig::default());
        // 60 a minute, 3 at once: one per second
        let rate = Rate::of(&rate(60, Some(3))).unwrap();
        let s = NANOS_PER_SECOND;
        let remaining: Vec<u64> = (0..3)
            .map(|_| limiter.take("a", rate, 10 * s))
            .inspect(|o| assert!(o.allowed))
            .map(|o| o.remaining)
            .collect();
        assert_eq!(remaining, [2, 1, 0]);
        let refused = limiter.take("a", rate, 10 * s);
        assert!(!refused.allowed);
        assert_eq!((refused.retry_after, refused.reset), (1, 3));
        // Another client has a bucket of its own
        assert!(limiter.take("b", rate, 10 * s).allowed);
        // One request back each second
        let later = limiter.take("a", rate, 11 * s);
        assert!(later.allowed);
        assert_eq!(later.remaining, 0);
        assert!(!limiter.take("a", rate, 11 * s).allowed);
        // Full again after the burst's worth of time
        assert_eq!(limiter.take("a", rate, 20 * s).remaining, 2);
    }

    #[test]
    fn a_refused_request_does_not_count() {
        let limiter = limiter(RateLimitConfig::default());
        let rate = Rate::of(&rate(60, Some(1))).unwrap();
        let s = NANOS_PER_SECOND;
        assert!(limiter.take("a", rate, s).allowed);
        for _ in 0..10 {
            assert!(!limiter.take("a", rate, s).allowed);
        }
        assert!(limiter.take("a", rate, 2 * s).allowed);
    }

    #[test]
    fn burst_defaults_to_the_rate_and_off_is_unlimited() {
        assert_eq!(Rate::of(&rate(120, None)).unwrap().burst, 120);
        let off = RateConfig {
            requests_per_minute: None,
            burst: None,
        };
        assert_eq!(Rate::of(&off), None);
    }

    #[test]
    fn the_table_stays_bounded() {
        let limiter = limiter(RateLimitConfig {
            max_clients: SHARDS,
            ..Default::default()
        });
        let rate = Rate::of(&rate(1, Some(1))).unwrap();
        for i in 0..1000 {
            limiter.take(&format!("client-{i}"), rate, 0);
        }
        let tracked: usize = limiter.shards.iter().map(|s| s.lock().unwrap().len()).sum();
        assert!(tracked <= SHARDS, "{tracked} clients tracked");
    }

    #[test]
    fn forwarded_for_counts_only_behind_a_trusted_proxy() {
        let limiter = limiter(RateLimitConfig {
            trusted_proxies: vec!["10.0.0.0/8".into()],
            ..Default::default()
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.1.1.1, 203.0.113.7, 10.1.2.3"),
        );
        let ip = |s: &str| s.parse::<IpAddr>().ok();
        // The proxy's own hop is skipped, an address the client wrote is not trusted
        assert_eq!(
            limiter.client_ip(ip("10.0.0.1"), &headers),
            ip("203.0.113.7")
        );
        // A client connecting directly cannot choose its address
        assert_eq!(
            limiter.client_ip(ip("198.51.100.1"), &headers),
            ip("198.51.100.1")
        );
        assert_eq!(
            limiter.client_ip(ip("10.0.0.1"), &HeaderMap::new()),
            ip("10.0.0.1")
        );
    }

    #[test]
    fn ip_ranges() {
        let range = IpRange::parse("10.0.0.0/8").unwrap();
        assert!(range.contains("10.200.0.1".parse().unwrap()));
        assert!(range.contains("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!range.contains("11.0.0.1".parse().unwrap()));
        assert!(IpRange::parse("fd00::/8")
            .unwrap()
            .contains("fd12::1".parse().unwrap()));
        assert!(IpRange::parse("0.0.0.0/0")
            .unwrap()
            .contains("8.8.8.8".parse().unwrap()));
        assert!(IpRange::parse("10.0.0.0/33").is_err());
        assert!(IpRange::parse("proxy").is_err());
    }
}
