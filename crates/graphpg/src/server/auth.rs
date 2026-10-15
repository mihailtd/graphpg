//! Request authentication as an OAuth 2.0 resource server.
//!
//! - Access tokens arrive as `Authorization: Bearer <token>` (RFC 6750), and only there.
//! - Tokens are JWTs (RFC 7519, profiled for access tokens by RFC 9068), verified per the JWT
//!   best current practices (RFC 8725): the signature with an accepted algorithm and the
//!   matching key (never `none`), `iss`, `aud`, `exp` and `nbf`.
//! - Keys come from the authorization server's JSON Web Key Set (RFC 7517), found through its
//!   metadata (OpenID Connect discovery, RFC 8414), cached, and re-fetched when a token names
//!   a key the cache lacks (key rotation); or from a shared secret for HMAC-signed tokens.
//! - Failures answer `401` with a `WWW-Authenticate: Bearer` challenge (`invalid_token`).
//!
//! A verified token's claims become session settings of the request's transaction, which runs
//! as the configured PostgreSQL role (never one the token names), so Row-Level Security
//! policies decide what the request reads and writes by comparing claims with the data.
use crate::config::models::AuthConfig;
use crate::execution::engine::Connection;
use crate::execution::ExecutionError;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::jwk::{Jwk, PublicKeyUse};
use jsonwebtoken::{Algorithm, AlgorithmFamily, DecodingKey, Header, Validation};
use serde_json::{Map, Value};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Algorithms accepted with a key set when the configuration names none.
const ASYMMETRIC: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];
/// Cached keys are re-fetched once this old.
const KEYS_MAX_AGE: Duration = Duration::from_secs(600);
/// Fetches for keys the cache lacks are at most this frequent (tokens naming unknown keys
/// must not make the engine hammer the authorization server).
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// Timeout of metadata and key set requests.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Sets a request's role and settings for its transaction, in one statement: `role` first,
/// then the claims.
const APPLY_SQL: &str = "SELECT count(set_config(s.name, s.value, true)) FROM unnest($1::text[], $2::text[]) AS s(name, value)";

/// The PostgreSQL role and settings of an authenticated request, applied to its transaction
/// (`set_config(..., true)`): PostgreSQL discards them when the transaction ends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthSessionContext {
    /// `(setting, value)` pairs, `role` (when configured) first.
    pub settings: Vec<(String, String)>,
    /// When the token expires (its `exp`, seconds since the Unix epoch): what is open on it,
    /// a subscription, ends then. `None` without a token.
    pub expires_at: Option<u64>,
    /// The token's `sub`, the client counted by per-client limits.
    pub subject: Option<String>,
}

impl AuthSessionContext {
    /// The role the request runs as.
    pub fn role(&self) -> Option<&str> {
        self.settings
            .iter()
            .find(|(name, _)| name == "role")
            .map(|(_, value)| value.as_str())
    }

    fn arrays(&self) -> (Vec<&str>, Vec<&str>) {
        self.settings
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .unzip()
    }

    /// Applies the role and settings to the transaction open on `conn`. Inside a transaction
    /// the caller opened, they stay in effect until that transaction ends.
    pub async fn apply<C: Connection>(&self, conn: &C) -> Result<(), ExecutionError> {
        let statement = conn.prepare(APPLY_SQL).await?;
        let (names, values) = self.arrays();
        conn.client()
            .query_one(&statement, &[&names, &values])
            .await?;
        Ok(())
    }

    /// [`apply`](Self::apply) on any client, e.g. a `tokio_postgres` transaction.
    pub async fn apply_on(
        &self,
        client: &impl tokio_postgres::GenericClient,
    ) -> Result<(), tokio_postgres::Error> {
        let (names, values) = self.arrays();
        client
            .query_one(APPLY_SQL, &[&names, &values])
            .await
            .map(|_| ())
    }
}

/// Why a request was not authenticated.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthError {
    /// No access token, and requests without one are not allowed (401, a bare challenge).
    Missing,
    /// A malformed `Authorization` header (400, `invalid_request`).
    InvalidRequest(String),
    /// The token is malformed, expired, not for this API or not signed by the authorization
    /// server (401, `invalid_token`).
    InvalidToken(String),
    /// The authorization server's keys cannot be loaded (503).
    Unavailable(String),
}

impl AuthError {
    pub fn message(&self) -> String {
        match self {
            Self::Missing => {
                "authentication required: send an access token as `Authorization: Bearer <token>`"
                    .to_string()
            }
            Self::InvalidRequest(m) | Self::InvalidToken(m) | Self::Unavailable(m) => m.clone(),
        }
    }
}

/// An RFC 6750 `WWW-Authenticate` challenge (quotes and backslashes cannot appear in its
/// quoted strings).
fn challenge(error: Option<&str>, description: &str) -> HeaderValue {
    let clean = |s: &str| {
        s.chars()
            .filter(|c| *c != '"' && *c != '\\' && (' '..='~').contains(c))
            .collect::<String>()
    };
    let value = match error {
        None => "Bearer".to_string(),
        Some(error) => format!(
            "Bearer error=\"{error}\", error_description=\"{}\"",
            clean(description)
        ),
    };
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("Bearer"))
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let message = self.message();
        let (status, code, www) = match &self {
            Self::Missing => (
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
                Some(challenge(None, "")),
            ),
            Self::InvalidRequest(m) => (
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                Some(challenge(Some("invalid_request"), m)),
            ),
            Self::InvalidToken(m) => (
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
                Some(challenge(Some("invalid_token"), m)),
            ),
            Self::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "AUTH_UNAVAILABLE", None),
        };
        let body = serde_json::json!({ "errors": [{ "message": message, "extensions": { "code": code } }] });
        let mut response = (status, axum::Json(body)).into_response();
        if let Some(www) = www {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, www);
        }
        response
    }
}

/// Authenticates requests per an enabled [`AuthConfig`].
pub struct Authenticator {
    config: AuthConfig,
    algorithms: Vec<Algorithm>,
    keys: Keys,
}

enum Keys {
    Secret(DecodingKey),
    Jwks(Jwks),
}

impl Authenticator {
    /// The authenticator of `config`, `None` when authentication is off. The configuration is
    /// checked (every problem reported at once); with a key set, its keys are loaded now, a
    /// failure logged and retried on the first requests.
    pub async fn new(
        config: &AuthConfig,
        secret: Option<&str>,
    ) -> Result<Option<Arc<Self>>, String> {
        if !config.enabled {
            return Ok(None);
        }
        let checked = check(config, secret)?;
        let keys = match checked.secret {
            Some(secret) => Keys::Secret(DecodingKey::from_secret(secret.as_bytes())),
            None => {
                let jwks = Jwks::new(checked.issuer, config.jwks_uri.clone())?;
                if let Err(err) = jwks.refresh(true).await {
                    tracing::error!(error = %err, "auth: could not load the authorization server's signing keys; retrying on requests");
                }
                Keys::Jwks(jwks)
            }
        };
        Ok(Some(Arc::new(Self {
            config: config.clone(),
            algorithms: checked.algorithms,
            keys,
        })))
    }

    /// The role and settings a request runs with.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthSessionContext, AuthError> {
        let Some(token) = bearer_token(headers)? else {
            return match &self.config.roles.anonymous {
                Some(role) => Ok(AuthSessionContext {
                    settings: vec![("role".to_string(), role.clone())],
                    ..Default::default()
                }),
                None => Err(AuthError::Missing),
            };
        };
        let claims = self.verify(token).await?;
        Ok(self.context(claims))
    }

    /// The claim at `path` of the request's token, when the token is valid and has it (the
    /// client a rate limit counts it against).
    pub async fn verified_claim(&self, headers: &HeaderMap, path: &str) -> Option<String> {
        let token = bearer_token(headers).ok()??;
        let claims = self.verify(token).await.ok()?;
        claim(&claims, path)
            .filter(|v| !v.is_null())
            .map(setting_value)
    }

    /// The claims of a valid token.
    async fn verify(&self, token: &str) -> Result<Map<String, Value>, AuthError> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|err| AuthError::InvalidToken(format!("malformed token: {err}")))?;
        if !self.algorithms.contains(&header.alg) {
            return Err(AuthError::InvalidToken(format!(
                "signature algorithm {:?} is not accepted",
                header.alg
            )));
        }
        if let Some(expected) = &self.config.token_type {
            let typ = header
                .typ
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let expected = expected.to_ascii_lowercase();
            if typ != expected && typ != format!("application/{expected}") {
                return Err(AuthError::InvalidToken(format!(
                    "token type must be `{expected}`"
                )));
            }
        }
        let keys = match &self.keys {
            Keys::Secret(key) => vec![key.clone()],
            Keys::Jwks(jwks) => jwks.keys_for(&header).await?,
        };

        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[self.config.issuer.as_deref().unwrap_or_default()]);
        validation.set_audience(&self.config.audience);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = self.config.leeway_seconds;

        // Without a `kid`, the key set's keys of the token's algorithm are tried in turn
        for key in &keys {
            match jsonwebtoken::decode::<Map<String, Value>>(token, key, &validation) {
                // PostgreSQL text holds no NUL character: a claim with one cannot be a setting
                Ok(data) if has_nul(&Value::Object(data.claims.clone())) => {
                    return Err(AuthError::InvalidToken(
                        "the token's claims contain a NUL character".to_string(),
                    ))
                }
                Ok(data) => return Ok(data.claims),
                Err(err) if *err.kind() == ErrorKind::InvalidSignature => continue,
                Err(err) => return Err(AuthError::InvalidToken(describe(err.kind()))),
            }
        }
        Err(AuthError::InvalidToken(
            "the token's signature does not verify".to_string(),
        ))
    }

    /// The role and settings of verified `claims`: the configured role (claims never choose
    /// it), and the claims as settings.
    fn context(&self, claims: Map<String, Value>) -> AuthSessionContext {
        let session = &self.config.session_variables;
        let mut settings: Vec<(String, String)> = self
            .config
            .roles
            .authenticated
            .iter()
            .map(|role| ("role".to_string(), role.clone()))
            .collect();
        settings.push((
            session.claims_setting.clone(),
            Value::Object(claims.clone()).to_string(),
        ));
        if !session.prefix.is_empty() {
            for (name, value) in &claims {
                // Setting names are identifiers: `https://example.com/roles` gets none. A claim
                // never replaces the claims setting or a mapped one (the token's issuer may let
                // users choose some claims)
                let setting = format!("{}{name}", session.prefix);
                if is_identifier(name)
                    && setting != session.claims_setting
                    && !session.mappings.values().any(|m| *m == setting)
                {
                    settings.push((setting, setting_value(value)));
                }
            }
        }
        for (path, setting) in &session.mappings {
            if let Some(value) = claim(&claims, path).filter(|v| !v.is_null()) {
                settings.push((setting.clone(), setting_value(value)));
            }
        }
        AuthSessionContext {
            settings,
            expires_at: claims.get("exp").and_then(Value::as_u64),
            subject: claims
                .get("sub")
                .filter(|v| !v.is_null())
                .map(setting_value),
        }
    }
}

/// The access token of a request: `Authorization: Bearer <token>`, `None` without one.
fn bearer_token(headers: &HeaderMap) -> Result<Option<&str>, AuthError> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(AuthError::InvalidRequest(
            "more than one Authorization header".to_string(),
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| AuthError::InvalidRequest("malformed Authorization header".to_string()))?;
    let (scheme, token) = value.trim().split_once(' ').unwrap_or((value.trim(), ""));
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthError::InvalidRequest(format!(
            "unsupported authorization scheme `{scheme}`: send `Bearer <token>`"
        )));
    }
    let token = token.trim();
    // RFC 6750 b64token characters
    let b64token = |c: char| c.is_ascii_alphanumeric() || "-._~+/=".contains(c);
    if token.is_empty() || !token.chars().all(b64token) {
        return Err(AuthError::InvalidRequest(
            "malformed bearer token".to_string(),
        ));
    }
    Ok(Some(token))
}

/// The claim at `path`: a claim name (which may contain dots, as namespaced claims do), else
/// a dot-separated path into nested claims.
fn claim<'c>(claims: &'c Map<String, Value>, path: &str) -> Option<&'c Value> {
    if let Some(value) = claims.get(path) {
        return Some(value);
    }
    let mut parts = path.split('.');
    let mut current = claims.get(parts.next()?)?;
    for part in parts {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

/// Whether a NUL character is in `value`, a key or a string of it.
fn has_nul(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains('\0'),
        Value::Array(items) => items.iter().any(has_nul),
        Value::Object(map) => map.iter().any(|(k, v)| k.contains('\0') || has_nul(v)),
        _ => false,
    }
}

/// A claim as a setting value: strings as they are, other values as JSON.
fn setting_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A setting name a custom setting can have: identifiers joined by dots, at least two.
fn is_setting_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|p| is_identifier(p))
}

fn describe(kind: &ErrorKind) -> String {
    match kind {
        ErrorKind::ExpiredSignature => "the token has expired".to_string(),
        ErrorKind::ImmatureSignature => "the token is not valid yet".to_string(),
        ErrorKind::InvalidIssuer => "the token is from another issuer".to_string(),
        ErrorKind::InvalidAudience => "the token is not for this API".to_string(),
        ErrorKind::MissingRequiredClaim(claim) => format!("the token has no `{claim}` claim"),
        ErrorKind::InvalidAlgorithm => "the token's algorithm does not match its key".to_string(),
        other => format!("invalid token: {other:?}"),
    }
}

/// What [`check`] derives from a valid configuration.
struct Checked {
    issuer: String,
    secret: Option<String>,
    algorithms: Vec<Algorithm>,
}

/// Checks an enabled configuration, reporting every problem.
fn check(config: &AuthConfig, secret: Option<&str>) -> Result<Checked, String> {
    let mut errors = Vec::new();
    let issuer = config.issuer.clone().unwrap_or_default();
    if issuer.is_empty() {
        errors.push("auth.issuer: required (a token's `iss` must equal it)".to_string());
    }
    if config.audience.iter().all(|a| a.trim().is_empty()) {
        errors.push("auth.audience: required (a token's `aud` must contain it)".to_string());
    }

    let secret = secret.map(str::to_string);
    if let Some(ref secret) = secret {
        // RFC 7518 §3.2: an HMAC key at least as long as the hash
        if secret.len() < 32 {
            errors.push("AUTH_JWT_SECRET: must be at least 32 bytes".to_string());
        }
        if config.jwks_uri.is_some() {
            errors.push("auth: set AUTH_JWT_SECRET or `jwks_uri`, not both".to_string());
        }
    } else {
        for (key, url) in [
            ("issuer", Some(&issuer)),
            ("jwks_uri", config.jwks_uri.as_ref()),
        ] {
            if let Some(url) = url.filter(|u| !u.is_empty()) {
                if let Err(err) = check_url(url) {
                    errors.push(format!("auth.{key}: {err}"));
                }
            }
        }
    }

    let algorithms: Vec<Algorithm> = if config.algorithms.is_empty() {
        if secret.is_some() {
            vec![Algorithm::HS256]
        } else {
            ASYMMETRIC.to_vec()
        }
    } else {
        config
            .algorithms
            .iter()
            .filter_map(|name| match name.parse::<Algorithm>() {
                Ok(alg) => Some(alg),
                Err(_) => {
                    errors.push(format!("auth.algorithms: unknown algorithm `{name}`"));
                    None
                }
            })
            .collect()
    };
    for alg in &algorithms {
        let hmac = alg.family() == AlgorithmFamily::Hmac;
        if hmac && secret.is_none() {
            errors.push(format!("auth.algorithms: {alg:?} needs AUTH_JWT_SECRET"));
        }
        if !hmac && secret.is_some() {
            errors.push(format!(
                "auth.algorithms: {alg:?} needs a key set, not AUTH_JWT_SECRET"
            ));
        }
    }

    let roles = &config.roles;
    for role in roles.authenticated.iter().chain(&roles.anonymous) {
        if role.is_empty() {
            errors.push("auth.roles: role names cannot be empty".to_string());
        }
    }
    let session = &config.session_variables;
    if !is_setting_name(&session.claims_setting) {
        errors.push(format!("auth.session_variables.claims_setting: `{}` is not a setting name (like `request.jwt.claims`)", session.claims_setting));
    }
    if !session.prefix.is_empty() && !is_setting_name(&format!("{}x", session.prefix)) {
        errors.push(format!(
            "auth.session_variables.prefix: `{}` must be dotted identifiers ending in `.`",
            session.prefix
        ));
    }
    for (path, setting) in &session.mappings {
        if !is_setting_name(setting) {
            errors.push(format!("auth.session_variables.mappings.{path}: `{setting}` is not a setting name (like `app.tenant_id`)"));
        }
    }

    if errors.is_empty() {
        Ok(Checked {
            issuer,
            secret,
            algorithms,
        })
    } else {
        Err(errors.join("\n"))
    }
}

/// Keys and metadata are fetched over HTTPS, except from this machine (development servers).
fn check_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|err| format!("`{url}` is not a URL: {err}"))?;
    let local = matches!(
        parsed.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if local => Ok(parsed),
        _ => Err(format!("`{url}` must use https")),
    }
}

/// A verification key of the key set.
struct VerifyKey {
    kid: Option<String>,
    /// The key's `alg`, when it names one.
    alg: Option<Algorithm>,
    family: AlgorithmFamily,
    key: DecodingKey,
}

#[derive(Default)]
struct KeySet {
    keys: Vec<VerifyKey>,
    fetched: Option<Instant>,
}

impl KeySet {
    /// Keys that may have signed a token with `header`: of its algorithm, and its `kid` when
    /// it names one.
    fn candidates(&self, header: &Header) -> Vec<DecodingKey> {
        self.keys
            .iter()
            .filter(|k| k.family == header.alg.family() && k.alg.is_none_or(|a| a == header.alg))
            .filter(|k| header.kid.is_none() || k.kid == header.kid)
            .map(|k| k.key.clone())
            .collect()
    }

    fn stale(&self) -> bool {
        self.fetched.is_none_or(|at| at.elapsed() > KEYS_MAX_AGE)
    }
}

/// The authorization server's key set, cached.
struct Jwks {
    http: reqwest::Client,
    issuer: String,
    configured_uri: Option<String>,
    current: RwLock<Arc<KeySet>>,
    /// The discovered key set URI and the last fetch attempt; held while fetching, so
    /// concurrent requests wait for one fetch.
    refreshing: tokio::sync::Mutex<(Option<String>, Option<Instant>)>,
}

impl Jwks {
    fn new(issuer: String, configured_uri: Option<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|err| format!("auth: HTTP client: {err}"))?;
        Ok(Self {
            http,
            issuer,
            configured_uri,
            current: RwLock::new(Arc::new(KeySet::default())),
            refreshing: tokio::sync::Mutex::new((None, None)),
        })
    }

    fn snapshot(&self) -> Arc<KeySet> {
        self.current.read().unwrap().clone()
    }

    /// Keys that may have signed a token with `header`, re-fetching the set when it is stale
    /// or lacks them.
    async fn keys_for(&self, header: &Header) -> Result<Vec<DecodingKey>, AuthError> {
        let set = self.snapshot();
        let found = set.candidates(header);
        if !found.is_empty() && !set.stale() {
            return Ok(found);
        }
        if let Err(err) = self.refresh(false).await {
            tracing::warn!(error = %err, "auth: could not refresh the authorization server's signing keys");
        }
        let set = self.snapshot();
        if set.fetched.is_none() {
            return Err(AuthError::Unavailable(
                "the authorization server's signing keys are unavailable".to_string(),
            ));
        }
        let found = set.candidates(header);
        if found.is_empty() {
            return Err(AuthError::InvalidToken(
                "the token is signed with an unknown key".to_string(),
            ));
        }
        Ok(found)
    }

    /// Fetches the key set, unless another fetch ran less than [`MIN_REFRESH_INTERVAL`] ago
    /// (`force` ignores that).
    async fn refresh(&self, force: bool) -> Result<(), String> {
        let mut state = self.refreshing.lock().await;
        if !force
            && state
                .1
                .is_some_and(|at| at.elapsed() < MIN_REFRESH_INTERVAL)
        {
            return Ok(());
        }
        state.1 = Some(Instant::now());
        let uri = match (&self.configured_uri, &state.0) {
            (Some(uri), _) | (None, Some(uri)) => uri.clone(),
            (None, None) => {
                let uri = self.discover().await?;
                state.0 = Some(uri.clone());
                uri
            }
        };
        let body = self.fetch(&uri).await?;
        let keys = parse_jwks(&body)?;
        if keys.is_empty() {
            return Err(format!("`{uri}` has no usable signing keys"));
        }
        tracing::info!(keys = keys.len(), %uri, "auth: loaded signing keys");
        *self.current.write().unwrap() = Arc::new(KeySet {
            keys,
            fetched: Some(Instant::now()),
        });
        Ok(())
    }

    /// The key set URI from the issuer's metadata: OpenID Connect discovery
    /// (`<issuer>/.well-known/openid-configuration`), else RFC 8414
    /// (`<origin>/.well-known/oauth-authorization-server<path>`). The metadata must name the
    /// same issuer.
    async fn discover(&self) -> Result<String, String> {
        let issuer = check_url(&self.issuer)?;
        let path = issuer.path().trim_end_matches('/');
        let origin = issuer.origin().ascii_serialization();
        let candidates = [
            format!(
                "{}/.well-known/openid-configuration",
                self.issuer.trim_end_matches('/')
            ),
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
        ];
        let mut failures = Vec::new();
        for url in candidates {
            let metadata = match self.fetch(&url).await {
                Ok(metadata) => metadata,
                Err(err) => {
                    failures.push(err);
                    continue;
                }
            };
            if metadata.get("issuer").and_then(Value::as_str) != Some(self.issuer.as_str()) {
                return Err(format!(
                    "`{url}` names another issuer than `{}`",
                    self.issuer
                ));
            }
            let uri = metadata
                .get("jwks_uri")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("`{url}` has no `jwks_uri`"))?;
            check_url(uri)?;
            return Ok(uri.to_string());
        }
        Err(format!(
            "no authorization server metadata for `{}`: {}",
            self.issuer,
            failures.join("; ")
        ))
    }

    async fn fetch(&self, url: &str) -> Result<Value, String> {
        let response = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| format!("`{url}`: {err}"))?;
        if !response.status().is_success() {
            return Err(format!("`{url}`: HTTP {}", response.status()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| format!("`{url}`: {err}"))?;
        serde_json::from_slice(&bytes).map_err(|err| format!("`{url}`: not JSON: {err}"))
    }
}

/// The signature verification keys of a JWK Set; keys for encryption, of unsupported types
/// or malformed are skipped.
fn parse_jwks(body: &Value) -> Result<Vec<VerifyKey>, String> {
    let keys = body
        .get("keys")
        .and_then(Value::as_array)
        .ok_or("the key set has no `keys`")?;
    Ok(keys
        .iter()
        .filter_map(|raw| {
            let jwk: Jwk = serde_json::from_value(raw.clone()).ok()?;
            if matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)) {
                return None;
            }
            let alg = jwk
                .common
                .key_algorithm
                .and_then(|a| Algorithm::try_from(a).ok());
            let key = DecodingKey::from_jwk(&jwk).ok()?;
            let family = key.family();
            if family == AlgorithmFamily::Hmac {
                return None;
            }
            Some(VerifyKey {
                kid: jwk.common.key_id.clone(),
                alg,
                family,
                key,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AuthConfig {
        AuthConfig {
            enabled: true,
            issuer: Some("https://auth.example.com/".to_string()),
            audience: vec!["api".to_string()],
            ..AuthConfig::default()
        }
    }

    #[test]
    fn bearer_tokens_come_only_from_the_authorization_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), Ok(None));
        headers.insert(header::AUTHORIZATION, "bearer abc.def.ghi".parse().unwrap());
        assert_eq!(bearer_token(&headers), Ok(Some("abc.def.ghi")));
        headers.insert(header::AUTHORIZATION, "Basic dXNlcjpwYXNz".parse().unwrap());
        assert!(matches!(
            bearer_token(&headers),
            Err(AuthError::InvalidRequest(_))
        ));
        headers.insert(header::AUTHORIZATION, "Bearer a b".parse().unwrap());
        assert!(matches!(
            bearer_token(&headers),
            Err(AuthError::InvalidRequest(_))
        ));
    }

    #[test]
    fn configuration_problems_are_reported_together() {
        let mut cfg = config();
        cfg.issuer = None;
        cfg.audience.clear();
        let secret = Some("short");
        cfg.algorithms = vec!["RS256".to_string(), "none".to_string()];
        cfg.session_variables.claims_setting = "claims".to_string();
        let Err(err) = check(&cfg, secret) else {
            panic!("accepted")
        };
        for expected in [
            "auth.issuer",
            "auth.audience",
            "32 bytes",
            "RS256 needs a key set",
            "unknown algorithm `none`",
            "claims_setting",
        ] {
            assert!(err.contains(expected), "missing `{expected}` in:\n{err}");
        }
        let mut cfg = config();
        cfg.jwks_uri = Some("http://keys.example.com/jwks".to_string());
        let Err(err) = check(&cfg, None) else {
            panic!("accepted")
        };
        assert!(err.contains("must use https"), "{err}");
    }

    #[test]
    fn claims_by_name_or_path() {
        let claims: Map<String, Value> = serde_json::from_value(
            serde_json::json!({ "https://example.com/role": "a", "app": { "role": "b" } }),
        )
        .unwrap();
        assert_eq!(
            claim(&claims, "https://example.com/role"),
            Some(&Value::from("a"))
        );
        assert_eq!(claim(&claims, "app.role"), Some(&Value::from("b")));
        assert_eq!(claim(&claims, "app.missing"), None);
    }
}
