//! Requests authenticated with OAuth 2.0 bearer tokens run under PostgreSQL Row-Level
//! Security as the token's role, with its claims as session settings.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::models::{AuthConfig, RolesConfig, SessionVariablesConfig};
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, Authenticator, ServerState};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const JWT_SECRET: &str = "super-secret-key-for-rls-testing-1234567890";
const ISSUER: &str = "https://issuer.test/";
const AUDIENCE: &str = "graphpg-test";

async fn setup_rls_db(client: &tokio_postgres::Client) {
    let sql = r#"
        DO $$
        BEGIN
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rls_app_user') THEN
                CREATE ROLE rls_app_user;
            END IF;
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rls_anon_user') THEN
                CREATE ROLE rls_anon_user;
            END IF;
        END
        $$;

        DROP SCHEMA IF EXISTS rls_test CASCADE;
        CREATE SCHEMA rls_test;

        GRANT USAGE ON SCHEMA rls_test TO rls_app_user, rls_anon_user;

        CREATE TABLE rls_test.documents (
            id serial PRIMARY KEY,
            name text NOT NULL,
            owner_id text NOT NULL,
            department text NOT NULL,
            tenant_id text NOT NULL DEFAULT 'tenant-1'
        );

        ALTER TABLE rls_test.documents ENABLE ROW LEVEL SECURITY;

        GRANT ALL ON rls_test.documents TO rls_app_user;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA rls_test TO rls_app_user;
        GRANT SELECT ON rls_test.documents TO rls_anon_user;

        -- 1. Owner policy for authenticated app user
        CREATE POLICY document_owner_policy ON rls_test.documents
            AS PERMISSIVE
            FOR ALL
            TO rls_app_user
            USING (owner_id = current_setting('request.jwt.claim.sub', true))
            WITH CHECK (owner_id = current_setting('request.jwt.claim.sub', true));

        -- 2. Doctor department policy for authenticated app user
        CREATE POLICY editor_dept_policy ON rls_test.documents
            AS PERMISSIVE
            FOR SELECT
            TO rls_app_user
            USING (
                current_setting('request.jwt.claim.role', true) = 'editor'
                AND department = current_setting('request.jwt.claim.department', true)
            );

        -- 3. Custom session mapping policy (tenant check if claim is present)
        CREATE POLICY tenant_isolation_policy ON rls_test.documents
            AS RESTRICTIVE
            FOR ALL
            TO rls_app_user
            USING (
                current_setting('app.tenant_id', true) IS NULL
                OR current_setting('app.tenant_id', true) = ''
                OR tenant_id = current_setting('app.tenant_id', true)
            );

        -- 4. Anonymous policy (only public_info records)
        CREATE POLICY anon_policy ON rls_test.documents
            AS PERMISSIVE
            FOR SELECT
            TO rls_anon_user
            USING (department = 'public_info');

        -- Seed initial records
        INSERT INTO rls_test.documents (name, owner_id, department, tenant_id) VALUES
            ('Alice Smith', 'alice', 'cardiology', 'tenant-1'),
            ('Bob Jones', 'bob', 'neurology', 'tenant-1'),
            ('Charlie Brown', 'charlie', 'cardiology', 'tenant-1'),
            ('Diana Prince', 'diana', 'public_info', 'tenant-1'),
            ('Evan Wright', 'evan', 'cardiology', 'acme-2');
    "#;
    client
        .batch_execute(sql)
        .await
        .expect("Failed to setup rls_test database schema and policies");
}

static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn ensure_rls_db(client: &tokio_postgres::Client) {
    INIT.get_or_init(|| async {
        setup_rls_db(client).await;
    })
    .await;
}

/// Tokens signed with the shared secret. Requests with one run as `rls_app_user`, without one
/// as `rls_anon_user`; claims (`role` and `department` included) are what the policies read.
fn secret_config() -> AuthConfig {
    let mut mappings = BTreeMap::new();
    mappings.insert("tenant".to_string(), "app.tenant_id".to_string());
    AuthConfig {
        enabled: true,
        issuer: Some(ISSUER.to_string()),
        audience: vec![AUDIENCE.to_string()],
        secret: Some(JWT_SECRET.to_string()),
        roles: RolesConfig {
            authenticated: Some("rls_app_user".to_string()),
            anonymous: Some("rls_anon_user".to_string()),
        },
        session_variables: SessionVariablesConfig {
            mappings,
            ..SessionVariablesConfig::default()
        },
        ..AuthConfig::default()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `claims` with an issuer, audience and expiry unless they set their own.
fn with_defaults(claims: serde_json::Value) -> serde_json::Value {
    let mut full = json!({ "iss": ISSUER, "aud": AUDIENCE, "exp": now() + 300 });
    for (k, v) in claims.as_object().unwrap() {
        full[k] = v.clone();
    }
    full
}

/// An HS256 access token for `claims`.
fn token(claims: serde_json::Value) -> String {
    token_with(claims, JWT_SECRET.as_bytes())
}

fn token_with(claims: serde_json::Value, secret: &[u8]) -> String {
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &with_defaults(claims),
        &EncodingKey::from_secret(secret),
    )
    .unwrap()
}

fn bearer(token: &str) -> Vec<(&'static str, String)> {
    vec![("authorization", format!("Bearer {token}"))]
}

async fn setup_app(auth_cfg: AuthConfig) -> axum::Router {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    ensure_rls_db(&client).await;

    let config = Config {
        database: graphpg::config::DatabaseConfig {
            schemas: vec!["rls_test".to_string()],
            ..Default::default()
        },
        ..Config::default()
    };
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config("rls_test", &config)
        .await
        .expect("inspect rls_test");
    let schema = SchemaBuilder::build(&catalog).expect("Failed to build GraphQL schema");
    let auth = Authenticator::new(&auth_cfg)
        .await
        .expect("valid auth configuration");
    let state = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 5).expect("Failed to create pool"),
            schema,
            catalog,
            "rls_test".to_string(),
            Default::default(),
        )
        .with_auth(auth),
    );
    create_router(state)
}

/// POSTs `body` to `uri`: status, `WWW-Authenticate` and the JSON (or raw text) body.
async fn post(
    app: &axum::Router,
    uri: &str,
    body: serde_json::Value,
    headers: Vec<(&'static str, String)>,
) -> (StatusCode, String, serde_json::Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let www = response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&bytes) }));
    (status, www, json)
}

async fn documents(
    app: &axum::Router,
    headers: Vec<(&'static str, String)>,
) -> (StatusCode, String, serde_json::Value) {
    post(
        app,
        "/graphql",
        json!({ "query": "{ documents { id name ownerId department tenantId } }" }),
        headers,
    )
    .await
}

fn rows(res: &serde_json::Value) -> Vec<serde_json::Value> {
    res["data"]["documents"]
        .as_array()
        .unwrap_or_else(|| panic!("no documents in {res}"))
        .clone()
}

#[tokio::test]
async fn test_rls_document_query_scoped_to_owner() {
    let app = setup_app(secret_config()).await;
    for (owner, name) in [("alice", "Alice Smith"), ("bob", "Bob Jones")] {
        let (status, _, res) = documents(&app, bearer(&token(json!({ "sub": owner })))).await;
        assert_eq!(status, StatusCode::OK, "{res}");
        let rows = rows(&res);
        assert_eq!(rows.len(), 1, "{owner} sees only their own record");
        assert_eq!(rows[0]["ownerId"], owner);
        assert_eq!(rows[0]["name"], name);
    }
}

#[tokio::test]
async fn test_rls_editor_department_policy() {
    let app = setup_app(secret_config()).await;
    let (status, _, res) = documents(
        &app,
        bearer(&token(
            json!({ "sub": "dr_cardio", "role": "editor", "department": "cardiology" }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows(&res);
    // Alice, Charlie and Evan
    assert_eq!(
        rows.len(),
        3,
        "a cardiology editor sees every cardiology document"
    );
    assert!(rows.iter().all(|p| p["department"] == "cardiology"));
}

#[tokio::test]
async fn test_rls_custom_session_variable_mapping_tenant_isolation() {
    let app = setup_app(secret_config()).await;
    let claims = json!({ "sub": "dr_cardio", "role": "editor", "department": "cardiology", "tenant": "tenant-1" });
    let (status, _, res) = documents(&app, bearer(&token(claims))).await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows(&res);
    // Alice and Charlie: Evan is in acme-2
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|p| p["department"] == "cardiology" && p["tenantId"] == "tenant-1"));
}

#[tokio::test]
async fn test_rls_anonymous_unauthenticated_request() {
    let app = setup_app(secret_config()).await;
    let (status, _, res) = documents(&app, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows(&res);
    assert_eq!(
        rows.len(),
        1,
        "anonymous requests see public_info records only"
    );
    assert_eq!(rows[0]["name"], "Diana Prince");
}

#[tokio::test]
async fn test_claims_header_is_not_trusted() {
    let app = setup_app(secret_config()).await;
    // Not part of OAuth: an unauthenticated request, whatever it claims
    let headers = vec![("x-jwt-claims", json!({ "sub": "charlie" }).to_string())];
    let (status, _, res) = documents(&app, headers).await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows(&res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["department"], "public_info");
}

#[tokio::test]
async fn test_invalid_tokens_are_rejected_with_a_bearer_challenge() {
    let app = setup_app(secret_config()).await;
    let cases = [
        (
            "forged",
            token_with(
                json!({ "sub": "alice" }),
                b"another-secret-of-at-least-32-bytes!!",
            ),
            "signature",
        ),
        ("malformed", "not-a-valid-jwt".to_string(), "malformed"),
        (
            "expired",
            token(json!({ "sub": "alice", "exp": now() - 120 })),
            "expired",
        ),
        (
            "not yet valid",
            token(json!({ "sub": "alice", "nbf": now() + 120 })),
            "not valid yet",
        ),
        (
            "other audience",
            token(json!({ "sub": "alice", "aud": "another-api" })),
            "not for this API",
        ),
        (
            "other issuer",
            token(json!({ "sub": "alice", "iss": "https://elsewhere.test/" })),
            "another issuer",
        ),
        // `alg: none` (RFC 8725): never accepted
        (
            "unsigned",
            format!(
                "{}.{}.",
                b64(r#"{"alg":"none","typ":"JWT"}"#),
                b64(&with_defaults(json!({ "sub": "alice" })).to_string())
            ),
            "malformed",
        ),
    ];
    for (case, token, expected) in cases {
        let (status, www, res) = documents(&app, bearer(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{case}: {res}");
        assert!(
            www.starts_with("Bearer error=\"invalid_token\""),
            "{case}: {www}"
        );
        let message = res["errors"][0]["message"].as_str().unwrap();
        assert!(message.contains(expected), "{case}: {message}");
        assert!(res.get("data").is_none(), "{case}");
    }
}

fn b64(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
}

#[tokio::test]
async fn test_requests_without_a_token_need_an_anonymous_role() {
    let mut cfg = secret_config();
    cfg.roles.anonymous = None;
    let app = setup_app(cfg).await;
    let (status, www, _) = documents(&app, vec![]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        www, "Bearer",
        "RFC 6750 §3.1: no error code when the request had no token"
    );

    let (status, www, _) = documents(
        &app,
        vec![("authorization", "Basic dXNlcjpwYXNz".to_string())],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(www.contains("invalid_request"), "{www}");
}

#[tokio::test]
async fn test_role_claims_are_data_not_database_roles() {
    let app = setup_app(secret_config()).await;
    // A token naming a privileged database role still runs as `rls_app_user`
    let (status, _, res) = documents(
        &app,
        bearer(&token(json!({ "sub": "alice", "role": "postgres" }))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    let rows = rows(&res);
    assert_eq!(rows.len(), 1, "still restricted by the policies");
    assert_eq!(rows[0]["ownerId"], "alice");
}

#[tokio::test]
async fn test_rls_mutation_with_check_enforcement() {
    let app = setup_app(secret_config()).await;
    // Rows of an owner and department no other test counts
    let frank = bearer(&token(json!({ "sub": "frank" })));
    let create = |owner: &str| {
        json!({ "query": format!(
            r#"mutation {{ createDocument(input: {{ name: "Frank Followup", ownerId: "{owner}", department: "mutation_ward", tenantId: "acme-9" }}) {{ document {{ id ownerId }} }} }}"#
        ) })
    };

    let (status, _, res) = post(&app, "/graphql", create("frank"), frank.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        res["data"]["createDocument"]["document"]["ownerId"], "frank",
        "{res}"
    );

    // WITH CHECK: frank cannot create bob's record
    let (_, _, res) = post(&app, "/graphql", create("bob"), frank).await;
    let message = res["errors"][0]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error: {res}"));
    assert!(
        message.contains("row-level security") || message.contains("violates"),
        "{message}"
    );
}

#[tokio::test]
async fn test_rls_zero_session_leakage_across_pooled_connections() {
    let app = setup_app(secret_config()).await;
    let alice = bearer(&token(json!({ "sub": "alice" })));
    for _ in 0..3 {
        let (_, _, res) = documents(&app, alice.clone()).await;
        assert_eq!(rows(&res)[0]["ownerId"], "alice");
        // The next request on the same pool runs as the anonymous role, without Alice's claims
        let (_, _, res) = documents(&app, vec![]).await;
        let rows = rows(&res);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["department"], "public_info");
    }
}

#[tokio::test]
async fn test_bulk_export_runs_as_the_token_role() {
    let mut cfg = secret_config();
    cfg.roles.anonymous = None;
    let app = setup_app(cfg).await;
    let export = json!({ "query": "{ documentsConnection { nodes { name ownerId } } }" });

    let (status, _, _) = post(&app, "/export", export.clone(), vec![]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "exports need a token");

    let (status, _, res) = post(
        &app,
        "/export",
        export,
        bearer(&token(json!({ "sub": "alice" }))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // NDJSON: one line parses as JSON, several do not
    let lines: Vec<serde_json::Value> = match res["raw"].as_str() {
        Some(raw) => raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        None => vec![res],
    };
    assert_eq!(
        lines,
        vec![json!({ "name": "Alice Smith", "ownerId": "alice" })],
        "Row-Level Security applies to exports"
    );
}

/// An authorization server on this machine: OpenID Connect discovery and a key set holding
/// one Ed25519 key (`kid` `k1`). Returns its issuer and a token signer.
async fn authorization_server() -> (
    String,
    impl Fn(&str, &[u8; 32], serde_json::Value) -> String,
) {
    fn ed25519(seed: &[u8; 32]) -> EncodingKey {
        // PKCS#8 wrapping of an Ed25519 private key
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend_from_slice(seed);
        EncodingKey::from_ed_der(&der)
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let mut jwk =
        jsonwebtoken::jwk::Jwk::from_encoding_key(&ed25519(&[7; 32]), Algorithm::EdDSA).unwrap();
    jwk.common.key_id = Some("k1".to_string());
    let metadata = json!({ "issuer": issuer, "jwks_uri": format!("{issuer}/keys") });
    let keys = json!({ "keys": [jwk] });
    let router = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            axum::routing::get(move || async move { axum::Json(metadata) }),
        )
        .route(
            "/keys",
            axum::routing::get(move || async move { axum::Json(keys) }),
        );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let iss = issuer.clone();
    let sign = move |kid: &str, seed: &[u8; 32], claims: serde_json::Value| {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(kid.to_string());
        let mut claims = with_defaults(claims);
        claims["iss"] = json!(iss);
        jsonwebtoken::encode(&header, &claims, &ed25519(seed)).unwrap()
    };
    (issuer, sign)
}

#[tokio::test]
async fn test_signing_keys_are_discovered_from_the_issuer() {
    let (issuer, sign) = authorization_server().await;
    let mut cfg = secret_config();
    cfg.secret = None;
    cfg.issuer = Some(issuer);
    let app = setup_app(cfg).await;

    let (status, _, res) = documents(
        &app,
        bearer(&sign("k1", &[7; 32], json!({ "sub": "alice" }))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(rows(&res)[0]["ownerId"], "alice");

    // Signed by a key the authorization server does not publish
    let (status, _, res) = documents(
        &app,
        bearer(&sign("k2", &[9; 32], json!({ "sub": "alice" }))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{res}");
    // Its `kid` claiming the published key does not help
    let (status, _, _) = documents(
        &app,
        bearer(&sign("k1", &[9; 32], json!({ "sub": "alice" }))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
