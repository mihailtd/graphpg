//! Time, history and correctness: PostgreSQL 18 temporal keys (`WITHOUT OVERLAPS`) and
//! temporal foreign keys (`PERIOD`), `asOf` arguments, `history` connections, the rows
//! before an update (`RETURNING OLD`), optimistic concurrency (`rowVersion` /
//! `expectedVersion`), and retries of serialization failures under the database's default
//! isolation level.
//!
//! Uses a `temporal_fixture` schema created on demand; writes run in rolled-back
//! transactions, except the retry test, which commits increments of its own counter.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use graphpg::DatabaseCatalog;
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "temporal_fixture";
const FIXTURE_VERSION: &str = "temporal_fixture v1";

const FIXTURE_SQL: &str = r#"
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE SCHEMA temporal_fixture;
SET search_path = temporal_fixture, public;

-- Versions of an account over non-overlapping periods
CREATE TABLE accounts (
    id int NOT NULL,
    valid_at tstzrange NOT NULL,
    owner text NOT NULL,
    balance numeric(12, 2) NOT NULL DEFAULT 0,
    PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)
);
-- Each entry refers to the account versions covering its period
CREATE TABLE entries (
    id serial PRIMARY KEY,
    account_id int NOT NULL,
    valid_at tstzrange NOT NULL,
    amount numeric(12, 2) NOT NULL,
    FOREIGN KEY (account_id, PERIOD valid_at) REFERENCES accounts (id, PERIOD valid_at)
);

CREATE TABLE counters (id int PRIMARY KEY, n int NOT NULL DEFAULT 0);
CREATE FUNCTION bump(counter_id int) RETURNS counters LANGUAGE sql VOLATILE
    AS $$ UPDATE temporal_fixture.counters SET n = n + 1 WHERE id = counter_id RETURNING * $$;

-- A version column a trigger maintains (named in the config)
CREATE TABLE documents (id int PRIMARY KEY, body text NOT NULL, version int NOT NULL DEFAULT 1);
CREATE FUNCTION next_version() RETURNS trigger LANGUAGE plpgsql
    AS $$ BEGIN NEW.version := OLD.version + 1; RETURN NEW; END $$;
CREATE TRIGGER documents_version BEFORE UPDATE ON documents FOR EACH ROW EXECUTE FUNCTION next_version();

INSERT INTO accounts VALUES
    (1, '[2024-01-01 00:00+00, 2024-06-01 00:00+00)', 'Ada', 100),
    (1, '[2024-06-01 00:00+00,)', 'Ada Lovelace', 250),
    (2, '[2024-01-01 00:00+00,)', 'Bob', 5);
INSERT INTO entries (account_id, valid_at, amount) VALUES
    (1, '[2024-02-01 00:00+00, 2024-02-02 00:00+00)', 10),
    (1, '[2024-07-01 00:00+00, 2024-07-02 00:00+00)', 20),
    (2, '[2024-03-01 00:00+00, 2024-03-02 00:00+00)', 1);
INSERT INTO counters VALUES (1, 0), (50, 0);
INSERT INTO documents VALUES (1, 'draft');
"#;

async fn connect() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to PostgreSQL");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    client
}

async fn ensure_fixture(client: &tokio_postgres::Client) {
    client
        .execute("SELECT pg_advisory_lock(424247)", &[])
        .await
        .unwrap();
    let version: Option<String> = client
        .query_opt(
            "SELECT obj_description(oid, 'pg_namespace') FROM pg_namespace WHERE nspname = $1",
            &[&SCHEMA],
        )
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
    client
        .execute("SELECT pg_advisory_unlock(424247)", &[])
        .await
        .unwrap();
}

struct Env {
    client: tokio_postgres::Client,
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: DatabaseCatalog,
    sdl: String,
}

async fn setup_with(config: &str) -> Env {
    let client = connect().await;
    ensure_fixture(&client).await;
    let config = Config::from_yaml(config).expect("config parses");
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .expect("inspection failed");
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    let schema =
        SchemaBuilder::build(&catalog).unwrap_or_else(|e| panic!("invalid schema: {e}\n{sdl}"));
    Env {
        client,
        schema,
        catalog,
        sdl,
    }
}

async fn setup() -> Env {
    setup_with("").await
}

impl Env {
    async fn try_run(&self, query: &str) -> Result<serde_json::Value, String> {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &self.schema,
            query,
            "q.graphql",
        )
        .map_err(|e| format!("validation: {e}"))?;
        let plan = QueryPlanner::new(&self.catalog, SCHEMA)
            .plan(&doc)
            .map_err(|e| format!("planning: {e}"))?;
        let result = ExecutionEngine::execute(&self.client, &plan)
            .await
            .map_err(|e| format!("execution: {e}"))?;
        Ok(result["data"].clone())
    }

    async fn run(&self, query: &str) -> serde_json::Value {
        self.try_run(query)
            .await
            .unwrap_or_else(|e| panic!("{e}\nquery: {query}"))
    }

    /// SDL block of type/input/enum `name`.
    fn block(&self, keyword: &str, name: &str) -> String {
        let start = self
            .sdl
            .find(&format!("{keyword} {name} "))
            .unwrap_or_else(|| panic!("no {keyword} {name}:\n{}", self.sdl));
        let end = self.sdl[start..]
            .find("\n}")
            .map(|e| start + e + 1)
            .unwrap_or(self.sdl.len());
        self.sdl[start..end].to_string()
    }

    async fn begin(&self) {
        self.client.batch_execute("BEGIN").await.unwrap();
    }

    async fn rollback(&self) {
        self.client.batch_execute("ROLLBACK").await.unwrap();
    }
}

const AS_OF_DOC: &str = "\"\"\"Only the versions valid at this point: whose `validAt` contains it.\"\"\" asOf: Datetime";

#[tokio::test]
async fn test_temporal_keys_shape_the_schema() {
    let env = setup().await;
    let query = env.block("type", "Query");
    // A version is looked up by its entity and a point in time, not by its period
    assert!(
        query.contains(&format!("  account(id: Int!, {AS_OF_DOC}): Account")),
        "{query}"
    );
    assert!(
        query.contains(&format!("  accounts({AS_OF_DOC}): [Account!]!")),
        "{query}"
    );
    assert!(
        query.contains(&format!(
            "offset: Int, {AS_OF_DOC}, condition: AccountCondition"
        )),
        "{query}"
    );
    assert!(
        query.contains("  entries: [Entry!]!"),
        "tables without a temporal key take no asOf: {query}"
    );

    let account = env.block("type", "Account");
    assert!(
        account.contains(&format!("  entries({AS_OF_DOC}): [Entry!]!"))
            || account.contains("  entries: [Entry!]!"),
        "{account}"
    );
    assert!(
        account.contains(&format!("  history({AS_OF_DOC}): [Account!]!")),
        "{account}"
    );
    assert!(account.contains("  historyConnection("), "{account}");
    assert!(account.contains("  rowVersion: String"), "{account}");
    // The to-one relation of a temporal foreign key reads the version valid at a point
    assert!(
        env.block("type", "Entry")
            .contains(&format!("  account({AS_OF_DOC}): Account")),
        "{}",
        env.block("type", "Entry")
    );
}

#[tokio::test]
async fn test_as_of_reads_the_versions_valid_then() {
    let env = setup().await;
    let data = env
        .run(
            r#"{
                march: account(id: 1, asOf: "2024-03-01T00:00:00Z") { owner balance }
                latest: account(id: 1) { owner }
                never: account(id: 1, asOf: "2023-01-01T00:00:00Z") { owner }
                july: accounts(asOf: "2024-07-01T00:00:00Z") { id owner }
                all: accounts { id owner }
                count: accountsConnection(asOf: "2024-02-01T00:00:00Z") { totalCount nodes { owner } }
            }"#,
        )
        .await;
    assert_eq!(
        data["march"],
        json!({ "owner": "Ada", "balance": "100.00" })
    );
    assert_eq!(data["latest"], json!({ "owner": "Ada Lovelace" }));
    assert_eq!(data["never"], json!(null));
    assert_eq!(
        data["july"],
        json!([{ "id": 1, "owner": "Ada Lovelace" }, { "id": 2, "owner": "Bob" }])
    );
    // Lists go by entity, then period
    assert_eq!(
        data["all"],
        json!([{ "id": 1, "owner": "Ada" }, { "id": 1, "owner": "Ada Lovelace" }, { "id": 2, "owner": "Bob" }])
    );
    assert_eq!(
        data["count"],
        json!({ "totalCount": 2, "nodes": [{ "owner": "Ada" }, { "owner": "Bob" }] })
    );
}

#[tokio::test]
async fn test_period_foreign_keys_join_by_overlap() {
    let env = setup().await;
    let data = env
        .run(
            r#"{
                entries { amount account { owner } later: account(asOf: "2024-12-01T00:00:00Z") { owner } }
                early: account(id: 1, asOf: "2024-03-01T00:00:00Z") { entries { amount } }
                late: account(id: 1) { entries { amount } }
                ada: entriesConnection(filter: { account: { owner: { equalTo: "Ada" } } }) { nodes { amount } }
            }"#,
        )
        .await;
    // Each entry refers to the account version its period overlaps (never an equal period)
    assert_eq!(
        data["entries"],
        json!([
            { "amount": "10.00", "account": { "owner": "Ada" }, "later": { "owner": "Ada Lovelace" } },
            { "amount": "20.00", "account": { "owner": "Ada Lovelace" }, "later": { "owner": "Ada Lovelace" } },
            { "amount": "1.00", "account": { "owner": "Bob" }, "later": { "owner": "Bob" } },
        ])
    );
    assert_eq!(data["early"]["entries"], json!([{ "amount": "10.00" }]));
    assert_eq!(data["late"]["entries"], json!([{ "amount": "20.00" }]));
    assert_eq!(
        data["ada"]["nodes"],
        json!([{ "amount": "10.00" }]),
        "relation filters overlap too"
    );
}

#[tokio::test]
async fn test_history_lists_every_version() {
    let env = setup().await;
    let data = env
        .run(
            r#"{ account(id: 1) {
                history { owner validAt { start { value } } }
                historyConnection { totalCount }
                then: history(asOf: "2024-01-15T00:00:00Z") { owner }
            } }"#,
        )
        .await;
    assert_eq!(
        data["account"]["history"],
        json!([
            { "owner": "Ada", "validAt": { "start": { "value": "2024-01-01T00:00:00+00:00" } } },
            { "owner": "Ada Lovelace", "validAt": { "start": { "value": "2024-06-01T00:00:00+00:00" } } },
        ])
    );
    assert_eq!(data["account"]["historyConnection"]["totalCount"], 2);
    assert_eq!(data["account"]["then"], json!([{ "owner": "Ada" }]));
}

#[tokio::test]
async fn test_payloads_show_the_rows_before_the_write() {
    let env = setup().await;
    assert!(
        env.catalog.server_version >= 180000,
        "the fixture server is PostgreSQL 18"
    );
    env.begin().await;
    let update = env.try_run(r#"mutation { updateCounter(id: 1, input: { n: 5 }) { counter { n } previousCounter { n rowVersion } } }"#).await;
    let many = env
        .try_run(r#"mutation { updateCounters(where: { id: { in: [1, 50] } }, set: { n: 7 }) { counters { databaseId n } previousCounters { databaseId n } } }"#)
        .await;
    let upsert = env
        .try_run(
            r#"mutation { createCounters(inputs: [{ id: 1, n: 9 }, { id: 2, n: 1 }], onConflict: { key: PRIMARY_KEY }) { counters { databaseId n } previousCounters { n } } }"#,
        )
        .await;
    env.rollback().await;
    assert_eq!(
        update.unwrap()["updateCounter"],
        json!({ "counter": { "n": 5 }, "previousCounter": { "n": 0, "rowVersion": null } })
    );
    // Previous rows follow the order of the updated rows (the order the statement returned
    // them; counter 50 is incremented by the retry test, so only its identity is checked)
    let many = many.unwrap()["updateCounters"].clone();
    let (rows, previous) = (
        many["counters"].as_array().unwrap(),
        many["previousCounters"].as_array().unwrap(),
    );
    assert_eq!(rows.len(), 2);
    for (row, prev) in rows.iter().zip(previous) {
        assert_eq!(row["n"], 7);
        assert_eq!(row["databaseId"], prev["databaseId"], "{many}");
        if row["databaseId"] == 1 {
            assert_eq!(prev["n"], 5);
        }
    }
    // An upsert's inserted rows had no previous version
    assert_eq!(
        upsert.unwrap()["createCounters"],
        json!({ "counters": [{ "databaseId": 1, "n": 9 }, { "databaseId": 2, "n": 1 }], "previousCounters": [{ "n": 7 }, null] })
    );
}

#[tokio::test]
async fn test_expected_version_guards_writes() {
    let env = setup().await;
    env.begin().await;
    let read = env.run("{ counter(id: 1) { rowVersion } }").await;
    let v1 = read["counter"]["rowVersion"].as_str().unwrap().to_string();
    let first = env.try_run(&format!(r#"mutation {{ updateCounter(id: 1, input: {{ n: 1 }}, expectedVersion: "{v1}") {{ counter {{ n rowVersion }} }} }}"#)).await;
    let stale = env.try_run(&format!(r#"mutation {{ updateCounter(id: 1, input: {{ n: 2 }}, expectedVersion: "{v1}") {{ counter {{ n }} }} }}"#)).await;
    let stale_delete = env
        .try_run(&format!(
            r#"mutation {{ deleteCounter(id: 1, expectedVersion: "{v1}") {{ counter {{ n }} }} }}"#
        ))
        .await;
    let after = env.run("{ counter(id: 1) { n } }").await;
    // A missing row is no conflict: nothing is written
    let missing = env.try_run(r#"mutation { updateCounter(id: 999, input: { n: 2 }, expectedVersion: "1") { counter { n } } }"#).await;
    env.rollback().await;

    let first = first.unwrap();
    assert_eq!(first["updateCounter"]["counter"]["n"], 1);
    assert_ne!(
        first["updateCounter"]["counter"]["rowVersion"],
        json!(v1),
        "every write changes the version"
    );
    for err in [stale.unwrap_err(), stale_delete.unwrap_err()] {
        assert!(
            err.contains("changed since `expectedVersion` was read"),
            "{err}"
        );
    }
    assert_eq!(after["counter"]["n"], 1, "stale writes changed nothing");
    assert_eq!(missing.unwrap()["updateCounter"]["counter"], json!(null));
}

#[tokio::test]
async fn test_configured_version_column() {
    let env = setup_with("entities:\n  documents: { version_column: version }\n").await;
    env.begin().await;
    let read = env.run("{ document(id: 1) { rowVersion } }").await;
    let ok = env.try_run(r#"mutation { updateDocument(id: 1, input: { body: "final" }, expectedVersion: "1") { document { body rowVersion } } }"#).await;
    let stale = env.try_run(r#"mutation { updateDocument(id: 1, input: { body: "again" }, expectedVersion: "1") { document { body } } }"#).await;
    env.rollback().await;
    assert_eq!(read["document"]["rowVersion"], "1");
    assert_eq!(
        ok.unwrap()["updateDocument"]["document"],
        json!({ "body": "final", "rowVersion": "2" })
    );
    assert!(stale.is_err());

    let config = Config::from_yaml("entities:\n  documents: { version_column: nope }\n").unwrap();
    let err = CatalogInspector::new(&env.client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("entities.documents.version_column: no column `nope`"),
        "{err}"
    );
}

async fn post(state: Arc<ServerState>, query: &str) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({ "query": query })).unwrap(),
        ))
        .unwrap();
    let response = create_router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn test_stale_version_over_http() {
    let env = setup().await;
    let pool = create_pool(common::db_url(), 1).unwrap();
    let state = Arc::new(ServerState::new(
        pool,
        env.schema.clone(),
        env.catalog.clone(),
        SCHEMA.to_string(),
        DebugOptions::default(),
    ));
    let (_, body) = post(
        state,
        r#"mutation { deleteCounter(id: 1, expectedVersion: "0") { counter { n } } }"#,
    )
    .await;
    assert_eq!(
        body["errors"][0]["path"],
        json!(["deleteCounter"]),
        "{body}"
    );
    assert_eq!(
        body["errors"][0]["extensions"],
        json!({ "code": "STALE_VERSION", "type": "CounterErrorCode", "constraint": null, "fields": ["expectedVersion"] })
    );
}

#[tokio::test]
async fn test_serialization_failures_are_retried() {
    let env = setup().await;
    // Every transaction of this pool runs SERIALIZABLE, as a database or role setting would
    let url = format!(
        "{}?options=-c%20default_transaction_isolation%3Dserializable",
        common::db_url()
    );
    let pool = create_pool(&url, 6).unwrap();
    let state = Arc::new(ServerState::new(
        pool,
        env.schema.clone(),
        env.catalog.clone(),
        SCHEMA.to_string(),
        DebugOptions::default(),
    ));
    let before: i32 = env
        .client
        .query_one("SELECT n FROM temporal_fixture.counters WHERE id = 50", &[])
        .await
        .unwrap()
        .get(0);

    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let state = state.clone();
            tokio::spawn(
                async move { post(state, r#"mutation { bump(counterId: 50) { n } }"#).await },
            )
        })
        .collect();
    for task in tasks {
        let (status, body) = task.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["errors"].is_null(),
            "concurrent increments conflict, and are retried: {body}"
        );
    }
    let after: i32 = env
        .client
        .query_one("SELECT n FROM temporal_fixture.counters WHERE id = 50", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(after - before, 6, "every increment applied exactly once");
}
