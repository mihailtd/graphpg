//! Request rate limits (`limits.rate_limit`): a bucket per client over GraphQL and REST,
//! `RateLimit-*` headers, `429` with `RATE_LIMITED`, and nothing at all when off.
mod common;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::models::{AuthConfig, LimitsConfig, RateConfig, RateLimitConfig};
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, Authenticator, DebugOptions, ServerState};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SECRET: &str = "rate-limit-test-secret-0123456789abcdef";
const ISSUER: &str = "https://issuer.test/";
const AUDIENCE: &str = "graphpg-test";

async fn forum_state() -> ServerState {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("app_public")
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    )
}

/// Limits allowing `burst` requests at once, refilled slowly (one a minute).
fn limited(burst: u32, rate_limit: RateLimitConfig) -> LimitsConfig {
    let rate = RateConfig {
        requests_per_minute: Some(1),
        burst: Some(burst),
    };
    LimitsConfig {
        rate_limit: RateLimitConfig {
            enabled: true,
            anonymous: rate.clone(),
            authenticated: rate,
            ..rate_limit
        },
        ..LimitsConfig::default()
    }
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    peer: &str,
    headers: &[(&str, String)],
    body: Option<Value>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    let body = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let mut request = builder.body(body).unwrap();
    let peer: SocketAddr = format!("{peer}:40000").parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    let res = app.clone().oneshot(request).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn graphql(app: &axum::Router, peer: &str, headers: &[(&str, String)]) -> Reply {
    send(
        app,
        "POST",
        "/graphql",
        peer,
        headers,
        Some(json!({ "query": "{ forums { name } }" })),
    )
    .await
}

fn header<'r>(reply: &'r Reply, name: &str) -> &'r str {
    reply
        .headers
        .get(name)
        .map(|v| v.to_str().unwrap())
        .unwrap_or_else(|| panic!("no {name} header"))
}

#[tokio::test]
async fn graphql_and_rest_share_a_clients_bucket() {
    let state = forum_state()
        .await
        .with_limits(limited(3, RateLimitConfig::default()));
    let app = create_router(Arc::new(state));
    let peer = "192.0.2.10";

    let first = graphql(&app, peer, &[]).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(header(&first, "ratelimit-limit"), "3");
    assert_eq!(header(&first, "ratelimit-remaining"), "2");
    assert_eq!(header(&first, "ratelimit-reset"), "60");

    let rest = send(&app, "GET", "/rest/forums", peer, &[], None).await;
    assert_eq!(rest.status, StatusCode::OK, "{}", rest.body);
    assert_eq!(header(&rest, "ratelimit-remaining"), "1");
    assert_eq!(graphql(&app, peer, &[]).await.status, StatusCode::OK);

    // GraphQL: a GraphQL error, before the operation is parsed
    let refused = graphql(&app, peer, &[]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        refused.body["errors"][0]["extensions"]["code"],
        "RATE_LIMITED"
    );
    assert_eq!(header(&refused, "retry-after"), "60");
    assert_eq!(header(&refused, "ratelimit-remaining"), "0");
    assert!(refused.body["data"].is_null());

    // REST: a problem document
    let refused = send(&app, "GET", "/rest/forums", peer, &[], None).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&refused, "content-type"), "application/problem+json");
    assert_eq!(refused.body["code"], "RATE_LIMITED");
    assert_eq!(refused.body["status"], 429);
    assert_eq!(header(&refused, "retry-after"), "60");

    // Another address has a bucket of its own
    assert_eq!(
        graphql(&app, "192.0.2.11", &[]).await.status,
        StatusCode::OK
    );

    // Probes and metrics are never limited
    let health = send(&app, "GET", "/health", peer, &[], None).await;
    assert_eq!(health.status, StatusCode::OK);
    assert!(health.headers.get("ratelimit-limit").is_none());
}

#[tokio::test]
async fn off_counts_nothing_and_sends_no_headers() {
    let app = create_router(Arc::new(forum_state().await));
    for _ in 0..20 {
        let reply = graphql(&app, "192.0.2.20", &[]).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.headers.get("ratelimit-limit").is_none());
    }
}

#[tokio::test]
async fn an_unlimited_tier_is_not_counted() {
    let mut limits = limited(1, RateLimitConfig::default());
    limits.rate_limit.anonymous.requests_per_minute = None;
    let app = create_router(Arc::new(forum_state().await.with_limits(limits)));
    for _ in 0..5 {
        let reply = graphql(&app, "192.0.2.30", &[]).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.headers.get("ratelimit-limit").is_none());
    }
}

#[tokio::test]
async fn behind_a_trusted_proxy_the_forwarded_client_counts() {
    let state = forum_state().await.with_limits(limited(
        1,
        RateLimitConfig {
            trusted_proxies: vec!["10.0.0.0/8".into()],
            ..RateLimitConfig::default()
        },
    ));
    let app = create_router(Arc::new(state));
    let proxy = "10.0.0.5";
    let from = |client: &str| vec![("x-forwarded-for", client.to_string())];

    assert_eq!(
        graphql(&app, proxy, &from("203.0.113.1")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        graphql(&app, proxy, &from("203.0.113.2")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        graphql(&app, proxy, &from("203.0.113.1")).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    // An untrusted peer cannot pose as someone else
    assert_eq!(
        graphql(&app, "198.51.100.1", &from("203.0.113.9"))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        graphql(&app, "198.51.100.1", &from("203.0.113.10"))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

fn token(sub: &str) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 300;
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &json!({ "iss": ISSUER, "aud": AUDIENCE, "exp": exp, "sub": sub }),
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn a_verified_token_is_its_own_client() {
    let auth = AuthConfig {
        enabled: true,
        issuer: Some(ISSUER.to_string()),
        audience: vec![AUDIENCE.to_string()],
        ..AuthConfig::default()
    };
    let auth = Authenticator::new(&auth, Some(SECRET)).await.unwrap();
    let state = forum_state()
        .await
        .with_auth(auth)
        .with_limits(limited(1, RateLimitConfig::default()));
    let app = create_router(Arc::new(state));
    let peer = "192.0.2.40";
    let bearer = |token: String| vec![("authorization", format!("Bearer {token}"))];

    // Two users behind one address are counted apart
    assert_eq!(
        graphql(&app, peer, &bearer(token("ada"))).await.status,
        StatusCode::OK
    );
    assert_eq!(
        graphql(&app, peer, &bearer(token("bob"))).await.status,
        StatusCode::OK
    );
    // A token per request does not reset a user's bucket
    assert_eq!(
        graphql(&app, peer, &bearer(token("ada"))).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A token that does not verify counts against the address (and is then refused)
    let forged = bearer(format!("{}x", token("carol")));
    assert_eq!(
        graphql(&app, peer, &forged).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        graphql(&app, peer, &forged).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
}
