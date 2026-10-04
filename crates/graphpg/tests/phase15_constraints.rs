//! Constraint-aware errors and type-modifier limits: every unique, foreign key, check and
//! exclusion constraint (and unique index) gets an error code in its table's
//! `<Type>ErrorCode` enum, violations are reported with that code and the input fields
//! holding the offending values, and `varchar(n)` / `numeric(p, s)` limits are checked (and
//! documented) before any SQL runs.
//!
//! Uses a `constraint_fixture` schema created on demand; writes run in rolled-back
//! transactions (planning errors) or fail and roll back (violations).
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use graphpg::{DatabaseCatalog, PlannerError};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "constraint_fixture";
const FIXTURE_VERSION: &str = "constraint_fixture v1";

const FIXTURE_SQL: &str = r#"
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE SCHEMA constraint_fixture;
SET search_path = constraint_fixture, public;

CREATE TABLE accounts (
    id serial PRIMARY KEY,
    email varchar(40) NOT NULL UNIQUE,
    handle char(4),
    balance numeric(8, 2) NOT NULL DEFAULT 0 CONSTRAINT balance_non_negative CHECK (balance >= 0),
    score int CHECK (score BETWEEN 0 AND 100),
    tags varchar(5)[],
    lo int,
    hi int,
    CHECK (lo < hi)
);
COMMENT ON COLUMN accounts.email IS 'Where statements go.';
CREATE UNIQUE INDEX accounts_lower_handle ON accounts (lower(handle));

CREATE TABLE rooms (id serial PRIMARY KEY, name text NOT NULL);
CREATE TABLE bookings (
    id serial PRIMARY KEY,
    room_id int NOT NULL REFERENCES rooms(id),
    during tstzrange NOT NULL,
    CONSTRAINT overlapping_booking EXCLUDE USING gist (room_id WITH =, during WITH &&)
);
CREATE TABLE transfers (
    id serial PRIMARY KEY,
    account_id int NOT NULL REFERENCES accounts(id),
    amount numeric(10, 2) NOT NULL
);

INSERT INTO accounts (email, handle, balance, score) VALUES ('ada@example.com', 'ada', 10, 50), ('bob@example.com', 'bob', 0, NULL);
INSERT INTO rooms (name) VALUES ('Blue');
INSERT INTO bookings (room_id, during) VALUES (1, '[2024-01-01 10:00+00, 2024-01-01 11:00+00)');
INSERT INTO transfers (account_id, amount) VALUES (1, 5);
"#;

async fn connect() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls).await.expect("Failed to connect to PostgreSQL");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    client
}

/// Creates the fixture schema once (serialised by an advisory lock), recreating it when the
/// fixture changed.
async fn ensure_fixture(client: &tokio_postgres::Client) {
    client.execute("SELECT pg_advisory_lock(424246)", &[]).await.unwrap();
    let version: Option<String> = client
        .query_opt("SELECT obj_description(oid, 'pg_namespace') FROM pg_namespace WHERE nspname = $1", &[&SCHEMA])
        .await
        .unwrap()
        .and_then(|row| row.get(0));
    if version.as_deref() != Some(FIXTURE_VERSION) {
        client
            .batch_execute(&format!(
                "BEGIN; DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; {FIXTURE_SQL} COMMENT ON SCHEMA {SCHEMA} IS '{FIXTURE_VERSION}'; COMMIT;"
            ))
            .await
            .expect("fixture setup failed");
    }
    client.execute("SELECT pg_advisory_unlock(424246)", &[]).await.unwrap();
}

struct Env {
    catalog: DatabaseCatalog,
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    sdl: String,
}

async fn setup() -> Env {
    let client = connect().await;
    ensure_fixture(&client).await;
    let catalog = CatalogInspector::new(&client).inspect_schema(SCHEMA).await.expect("inspection failed");
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    let schema = SchemaBuilder::build(&catalog).unwrap_or_else(|e| panic!("invalid schema: {e}\n{sdl}"));
    Env { catalog, schema, sdl }
}

impl Env {
    /// SDL block of type/input/enum `name`.
    fn block(&self, keyword: &str, name: &str) -> String {
        let start = self.sdl.find(&format!("{keyword} {name} ")).unwrap_or_else(|| panic!("no {keyword} {name}:\n{}", self.sdl));
        // Include the description above the block
        let doc_start = self.sdl[..start].rfind("\n\n").map(|i| i + 2).unwrap_or(0);
        let end = self.sdl[start..].find("\n}").map(|e| start + e + 1).unwrap_or(self.sdl.len());
        self.sdl[doc_start..end].to_string()
    }

    /// Plans `query`, expecting it to be rejected with input errors.
    fn input_errors(&self, query: &str) -> Vec<graphpg::InputError> {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(&self.schema, query, "q.graphql").expect("valid query");
        match QueryPlanner::new(&self.catalog, SCHEMA).plan(&doc) {
            Err(PlannerError::Input(errors)) => errors,
            other => panic!("expected input errors, got {other:?}"),
        }
    }

    /// Runs `query` over HTTP (writes are committed unless they fail).
    async fn post(&self, query: &str) -> (StatusCode, serde_json::Value) {
        let pool = create_pool(common::db_url(), 1).unwrap();
        let state = Arc::new(ServerState::new(pool, self.schema.clone(), self.catalog.clone(), SCHEMA.to_string(), DebugOptions::default()));
        let request = Request::builder()
            .method("POST")
            .uri("/graphql")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&json!({ "query": query })).unwrap()))
            .unwrap();
        let response = create_router(state).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Runs a mutation expected to violate a constraint; returns the error's extensions.
    async fn violation(&self, query: &str) -> serde_json::Value {
        let (status, body) = self.post(query).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["data"].is_null(), "{body}");
        let error = &body["errors"][0];
        assert!(error["message"].as_str().unwrap().contains("rolled back"), "{error}");
        error["extensions"].clone()
    }
}

#[tokio::test]
async fn test_error_codes_come_from_constraints() {
    let env = setup().await;
    let account = env.block("enum", "AccountErrorCode");
    for code in [
        "EMAIL_REQUIRED",
        "EMAIL_TOO_LONG",
        "HANDLE_TOO_LONG",
        "BALANCE_OUT_OF_RANGE",
        "TAGS_TOO_LONG",
        "ID_TAKEN",
        "EMAIL_TAKEN",
        // A unique index on an expression covers the columns it reads
        "HANDLE_TAKEN",
        // A named check keeps its name; a generated name gives way to the columns
        "BALANCE_NON_NEGATIVE",
        "SCORE_INVALID",
        "LO_AND_HI_INVALID",
        // Foreign keys referencing the table
        "REFERENCED_BY_TRANSFERS",
    ] {
        assert!(account.contains(&format!("  {code}\n")), "missing {code}:\n{account}");
    }
    assert!(account.contains("Another row already has this `email` (unique `accounts_email_key`)."), "{account}");
    assert!(account.contains("Check `balance_non_negative` failed: `balance >= (0)::numeric`."), "{account}");

    let booking = env.block("enum", "BookingErrorCode");
    for code in ["OVERLAPPING_BOOKING", "ROOM_NOT_FOUND", "ROOM_ID_REQUIRED", "DURING_REQUIRED"] {
        assert!(booking.contains(&format!("  {code}\n")), "missing {code}:\n{booking}");
    }
    assert!(env.block("enum", "RoomErrorCode").contains("  REFERENCED_BY_BOOKINGS\n"));
}

#[tokio::test]
async fn test_limits_and_checks_are_documented() {
    let env = setup().await;
    let account = env.block("type", "Account");
    assert!(account.contains("\"\"\"Where statements go.\n\nAt most 40 characters.\"\"\"\n  email: String!"), "{account}");
    assert!(
        account.contains("At most 8 digits, 2 of them after the decimal point (rounded to 2 decimal places).\n\nMust satisfy `balance >= (0)::numeric`.\"\"\"\n  balance: Decimal!"),
        "{account}"
    );
    assert!(account.contains("Must satisfy `(score >= 0) AND (score <= 100)`."), "{account}");
    assert!(account.contains("\"\"\"Rows must satisfy `lo < hi`.\"\"\"\ntype Account"), "{account}");
    // Inputs carry the same descriptions
    let create = env.block("input", "CreateAccountInput");
    assert!(create.contains("At most 40 characters.\"\"\"\n  email: String!"), "{create}");
    assert!(env.block("input", "UpdateAccountInput").contains("At most 5 characters.\"\"\"\n  tags: [String]"));
}

#[tokio::test]
async fn test_limits_are_checked_before_any_sql() {
    let env = setup().await;
    let errors = env.input_errors(
        r#"mutation { createAccount(input: {
            email: "a-very-long-address-that-does-not-fit@example.com",
            handle: "ab   ",
            balance: "1234567.891",
            tags: ["ok", "much-too-long"]
        }) { account { id } } }"#,
    );
    let summary: Vec<(Option<&str>, &str)> = errors.iter().map(|e| (e.path.as_deref(), e.code.as_str())).collect();
    assert_eq!(
        summary,
        [
            (Some("input.email"), "EMAIL_TOO_LONG"),
            (Some("input.balance"), "BALANCE_OUT_OF_RANGE"),
            (Some("input.tags"), "TAGS_TOO_LONG"),
        ],
        "trailing blanks beyond the length are dropped, as PostgreSQL does: {errors:?}"
    );
    assert!(errors[1].message.contains("does not fit `numeric(8, 2)`"), "{errors:?}");

    // Rounding decides: 999999.994 rounds to 999999.99, 999999.995 to 1000000.00
    let ok = env.input_errors(r#"mutation { a: createAccount(input: { email: "x", balance: "999999.995" }) { account { id } } }"#);
    assert_eq!(ok.len(), 1);
    let doc = r#"mutation { a: createAccount(input: { email: "x", balance: "999999.994" }) { account { id } } }"#;
    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(&env.schema, doc, "q.graphql").unwrap();
    assert!(QueryPlanner::new(&env.catalog, SCHEMA).plan(&doc).is_ok());

    // Nested and bulk inputs report their own paths; filters are not limited
    let errors = env.input_errors(
        r#"mutation { updateAccounts(where: { email: { equalTo: "a-very-long-address-that-does-not-fit@example.com" } }, set: { handle: "toolong" }) { affectedCount } }"#,
    );
    assert_eq!(errors.len(), 1);
    assert_eq!((errors[0].path.as_deref(), errors[0].code.as_str()), (Some("set.handle"), "HANDLE_TOO_LONG"));
}

#[tokio::test]
async fn test_violations_are_typed_and_field_mapped() {
    let env = setup().await;

    let ext = env.violation(r#"mutation { createAccount(input: { email: "ada@example.com" }) { account { id } } }"#).await;
    assert_eq!(
        ext,
        json!({ "code": "EMAIL_TAKEN", "type": "AccountErrorCode", "constraint": "accounts_email_key", "fields": ["input.email"] })
    );

    // Of several rows written by one statement, the one the database names
    let ext = env
        .violation(r#"mutation { createAccounts(inputs: [{ email: "new@example.com" }, { email: "bob@example.com" }]) { affectedCount } }"#)
        .await;
    assert_eq!((ext["code"].as_str(), &ext["fields"]), (Some("EMAIL_TAKEN"), &json!(["inputs[1].email"])));

    let ext = env.violation(r#"mutation { createAccount(input: { email: "c@example.com", handle: "ADA" }) { account { id } } }"#).await;
    assert_eq!((ext["code"].as_str(), ext["constraint"].as_str()), (Some("HANDLE_TAKEN"), Some("accounts_lower_handle")));

    let ext = env.violation(r#"mutation { createAccount(input: { email: "c@example.com", balance: "-1" }) { account { id } } }"#).await;
    assert_eq!((ext["code"].as_str(), &ext["fields"]), (Some("BALANCE_NON_NEGATIVE"), &json!(["input.balance"])));

    let ext = env.violation(r#"mutation { updateAccount(id: 1, input: { score: 101 }) { account { id } } }"#).await;
    assert_eq!((ext["code"].as_str(), &ext["fields"]), (Some("SCORE_INVALID"), &json!(["input.score"])));

    let ext = env.violation(r#"mutation { updateAccounts(where: {}, set: { lo: 5, hi: 1 }) { affectedCount } }"#).await;
    assert_eq!((ext["code"].as_str(), &ext["fields"]), (Some("LO_AND_HI_INVALID"), &json!(["set.lo", "set.hi"])));

    let ext = env.violation(r#"mutation { updateAccount(id: 1, input: { email: null }) { account { id } } }"#).await;
    assert_eq!(ext, json!({ "code": "EMAIL_REQUIRED", "type": "AccountErrorCode", "constraint": null, "fields": ["input.email"] }));

    let ext = env
        .violation(
            r#"mutation { createBooking(input: { roomId: 1, during: { start: { value: "2024-01-01T10:30:00Z" }, end: { value: "2024-01-01T12:00:00Z" } } }) { booking { id } } }"#,
        )
        .await;
    assert_eq!(
        ext,
        json!({ "code": "OVERLAPPING_BOOKING", "type": "BookingErrorCode", "constraint": "overlapping_booking", "fields": ["input.roomId", "input.during"] })
    );

    let ext = env
        .violation(r#"mutation { createBooking(input: { roomId: 99, during: { start: { value: "2025-01-01T00:00:00Z" } } }) { booking { id } } }"#)
        .await;
    assert_eq!((ext["code"].as_str(), &ext["fields"]), (Some("ROOM_NOT_FOUND"), &json!(["input.roomId"])));

    // Deleting a row others refer to reports the referenced table's code
    let ext = env.violation(r#"mutation { deleteAccount(id: 1) { account { id } } }"#).await;
    assert_eq!(
        ext,
        json!({ "code": "REFERENCED_BY_TRANSFERS", "type": "AccountErrorCode", "constraint": "transfers_account_id_fkey", "fields": [] })
    );

    // Rows created through a nested relation report the child table's code
    let ext = env
        .violation(
            r#"mutation { createRoom(input: { name: "Red", bookings: { create: [
                { during: { start: { value: "2024-02-01T10:00:00Z" }, end: { value: "2024-02-01T12:00:00Z" } } },
                { during: { start: { value: "2024-02-01T11:00:00Z" }, end: { value: "2024-02-01T13:00:00Z" } } }
            ] } }) { room { id } } }"#,
        )
        .await;
    assert_eq!((ext["code"].as_str(), ext["type"].as_str()), (Some("OVERLAPPING_BOOKING"), Some("BookingErrorCode")));

    // Nothing was written
    let (_, body) = env.post("{ accounts { email } rooms { name } }").await;
    assert_eq!(body["data"]["accounts"].as_array().unwrap().len(), 2, "{body}");
    assert_eq!(body["data"]["rooms"], json!([{ "name": "Blue" }]));
}

#[tokio::test]
async fn test_input_errors_over_http() {
    let env = setup().await;
    // A value of the wrong GraphQL type fails validation (400); a valid one exceeding its
    // column's limits is rejected by planning, per field
    let (status, _) = env.post(r#"mutation { createAccount(input: { email: "a", score: "x" }) { account { id } } }"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = env
        .post(r#"mutation { createAccount(input: { email: "a-very-long-address-that-does-not-fit@example.com" }) { account { id } } }"#)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["errors"][0]["extensions"], json!({ "code": "EMAIL_TOO_LONG", "field": "input.email" }), "{body}");
}
