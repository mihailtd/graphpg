//! Throughput, operations and the remaining write and analytics features: the request
//! cache, sqlcommenter tags, estimated counts, startup advice, `@name` / `@deprecated`
//! comment hints, richer aggregates (distinct counts, statistics, percentiles, filters,
//! time zones, bins, gap filling) and window values, nested `connectOrCreate` / `update` /
//! `delete` / `sync` (MERGE), streaming exports and COPY imports, and read-replica routing.
//!
//! Uses an `ops_fixture` schema created on demand; writes run in rolled-back transactions,
//! except imports, which commit into their own table and clean up.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::{AdviceLevel, CatalogInspector};
use graphpg::config::Config;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{
    create_pool, create_router, DebugOptions, ServerState, MIN_WAL_POSITION_HEADER,
    WAL_POSITION_HEADER,
};
use graphpg::DatabaseCatalog;
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "ops_fixture";
const FIXTURE_VERSION: &str = "ops_fixture v1";

const FIXTURE_SQL: &str = r#"
CREATE SCHEMA ops_fixture;
SET search_path = ops_fixture, public;

CREATE TABLE accounts (id serial PRIMARY KEY, owner text NOT NULL);
-- No index on `account_id`: the advice reports it
CREATE TABLE entries (
    id serial PRIMARY KEY,
    account_id int NOT NULL REFERENCES accounts(id),
    booked_at timestamptz NOT NULL,
    amount numeric(12, 2) NOT NULL,
    kind text NOT NULL
);

CREATE TABLE legacy_items (id serial PRIMARY KEY, code text, old_name text);
COMMENT ON TABLE legacy_items IS '@deprecated Use accounts instead.';
COMMENT ON COLUMN legacy_items.old_name IS E'The old name.\n@name formerName\n@deprecated Use `code`.';

CREATE TABLE teams (id serial PRIMARY KEY, name text NOT NULL UNIQUE);
CREATE TABLE members (id serial PRIMARY KEY, team_id int REFERENCES teams(id), email text NOT NULL UNIQUE, role text NOT NULL DEFAULT 'member');
CREATE INDEX ON members (team_id);
CREATE TABLE tags (id serial PRIMARY KEY, label text NOT NULL UNIQUE);
CREATE TABLE team_tags (team_id int NOT NULL REFERENCES teams(id), tag_id int NOT NULL REFERENCES tags(id), PRIMARY KEY (team_id, tag_id));

CREATE TABLE numbers (id int PRIMARY KEY, n int NOT NULL);
CREATE TABLE imports (id serial PRIMARY KEY, label text NOT NULL UNIQUE, qty int, tags text[]);

INSERT INTO accounts (owner) VALUES ('Doe, Jane'), ('Roe "Rick"');
INSERT INTO entries (account_id, booked_at, amount, kind) VALUES
    (1, '2024-01-01 10:00+00', 100, 'deposit'),
    (1, '2024-01-01 10:20+00', -30, 'withdrawal'),
    (2, '2024-01-01 10:05+00', 500, 'deposit'),
    (1, '2024-01-03 09:00+00', 50, 'deposit'),
    (2, '2024-01-03 23:30+00', -200, 'withdrawal');
INSERT INTO teams (name) VALUES ('Core'), ('Other');
INSERT INTO members (team_id, email, role) VALUES (1, 'a@x', 'member'), (1, 'b@x', 'member'), (2, 'c@x', 'member'), (NULL, 'loner@x', 'member');
INSERT INTO tags (label) VALUES ('old');
INSERT INTO numbers SELECT g, g % 100 FROM generate_series(1, 2000) g;
ANALYZE numbers;
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
        .execute("SELECT pg_advisory_lock(424250)", &[])
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
        .execute("SELECT pg_advisory_unlock(424250)", &[])
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

    /// Runs `queries` in one transaction that is rolled back; each result or error.
    async fn rolled_back(&self, queries: &[&str]) -> Vec<Result<serde_json::Value, String>> {
        self.client.batch_execute("BEGIN").await.unwrap();
        let mut out = Vec::new();
        for q in queries {
            out.push(self.try_run(q).await);
        }
        self.client.batch_execute("ROLLBACK").await.unwrap();
        out
    }

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

    fn state(&self, debug: bool) -> Arc<ServerState> {
        let pool = create_pool(common::db_url(), 2).unwrap();
        let debug = DebugOptions {
            sql: debug,
            explain: false,
        };
        Arc::new(ServerState::new(
            pool,
            self.schema.clone(),
            self.catalog.clone(),
            SCHEMA.to_string(),
            debug,
        ))
    }
}

async fn send(
    state: Arc<ServerState>,
    uri: &str,
    content_type: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, content_type);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = create_router(state)
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

async fn graphql(
    state: Arc<ServerState>,
    body: serde_json::Value,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let (status, headers, bytes) = send(
        state,
        "/graphql",
        "application/json",
        serde_json::to_vec(&body).unwrap(),
        headers,
    )
    .await;
    (status, headers, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn test_request_cache_and_sql_comments() {
    let env = setup().await;
    let state = env.state(true);
    let body = json!({ "query": "query Owners($id: Int!) { account(id: $id) { owner } }", "variables": { "id": 1 } });
    let (_, _, first) = graphql(state.clone(), body.clone(), &[]).await;
    let (_, _, second) = graphql(state.clone(), body, &[]).await;
    let (_, _, other) = graphql(state.clone(), json!({ "query": "query Owners($id: Int!) { account(id: $id) { owner } }", "variables": { "id": 2 } }), &[]).await;
    assert_eq!(first["extensions"]["cache"], "miss");
    assert_eq!(second["extensions"]["cache"], "hit", "{second}");
    assert_eq!(second["data"], first["data"]);
    assert_eq!(
        other["extensions"]["cache"], "miss",
        "other variables compile anew"
    );
    assert_eq!(other["data"]["account"]["owner"], "Roe \"Rick\"");
    let sql = first["extensions"]["sql"]["query"].as_str().unwrap();
    assert!(
        sql.contains("/*application='graphpg',graphql_operation='Owners',graphql_type='query'*/"),
        "{sql}"
    );

    // A schema reload empties the cache
    state.reload(env.schema.clone(), env.catalog.clone());
    let (_, _, after) = graphql(state.clone(), json!({ "query": "query Owners($id: Int!) { account(id: $id) { owner } }", "variables": { "id": 1 } }), &[]).await;
    assert_eq!(after["extensions"]["cache"], "miss");

    let quiet = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 1).unwrap(),
            env.schema.clone(),
            env.catalog.clone(),
            SCHEMA.to_string(),
            DebugOptions {
                sql: true,
                explain: false,
            },
        )
        .with_sql_comments(false),
    );
    let (_, _, plain) = graphql(quiet, json!({ "query": "{ accounts { owner } }" }), &[]).await;
    assert!(!plain["extensions"]["sql"]["query"]
        .as_str()
        .unwrap()
        .contains("/*"));
}

#[tokio::test]
async fn test_estimated_counts() {
    let env = setup().await;
    let data = env
        .run(
            r#"{
                all: numbersConnection { exact: totalCount estimate: totalCount(estimate: true) }
                few: numbersConnection(filter: { n: { lessThan: 10 } }) { exact: totalCount estimate: totalCount(estimate: true) }
                nested: accountsConnection(orderBy: [PRIMARY_KEY_ASC]) { nodes { entriesConnection { totalCount(estimate: true) } } }
            }"#,
        )
        .await;
    assert_eq!(
        data["all"],
        json!({ "exact": 2000, "estimate": 2000 }),
        "table statistics"
    );
    assert_eq!(data["few"]["exact"], 200);
    let estimate = data["few"]["estimate"].as_i64().unwrap();
    assert!(
        (100..=400).contains(&estimate),
        "EXPLAIN's estimate: {estimate}"
    );
    assert_eq!(
        data["nested"]["nodes"][0]["entriesConnection"]["totalCount"], 3,
        "a parent's rows count exactly"
    );
}

#[tokio::test]
async fn test_startup_advice() {
    let env = setup().await;
    let advice = CatalogInspector::new(&env.client)
        .advise(SCHEMA, &env.catalog)
        .await
        .unwrap();
    let warnings: Vec<&str> = advice
        .iter()
        .filter(|a| a.level == AdviceLevel::Warning)
        .map(|a| a.message.as_str())
        .collect();
    assert!(
        warnings
            .iter()
            .any(|m| m.contains("`entries_account_id_fkey`")
                && m.contains("CREATE INDEX ON \"entries\" (\"account_id\")")),
        "{warnings:?}"
    );
    assert!(
        !warnings.iter().any(|m| m.contains("members_team_id_fkey")),
        "an index leads with it: {warnings:?}"
    );
    assert!(advice
        .iter()
        .any(|a| a.level == AdviceLevel::Info
            && a.message.starts_with("`entries`: no index leads with")));
}

#[tokio::test]
async fn test_comment_hints_and_deprecations() {
    let env = setup_with(
        "entities:\n  members:\n    columns:\n      role: { deprecated: Roles moved to teams. }\n",
    )
    .await;
    let item = env.block("type", "LegacyItem");
    assert!(
        item.contains("  \"\"\"The old name.\"\"\"\n  formerName: String @deprecated(reason: \"Use `code`.\")"),
        "{item}"
    );
    let query = env.block("type", "Query");
    assert!(
        query.contains(
            "  legacyItems: [LegacyItem!]! @deprecated(reason: \"Use accounts instead.\")"
        ),
        "{query}"
    );
    assert!(env
        .block("type", "Mutation")
        .contains("CreateLegacyItemPayload! @deprecated(reason: \"Use accounts instead.\")"));
    assert!(env
        .block("type", "Member")
        .contains("  role: String! @deprecated(reason: \"Roles moved to teams.\")"));
    // Deprecated fields still work
    assert_eq!(
        env.run("{ legacyItems { formerName } }").await,
        json!({ "legacyItems": [] })
    );
}

#[tokio::test]
async fn test_richer_aggregates() {
    let env = setup().await;
    let data = env
        .run(
            r#"{ entriesConnection {
                aggregates {
                    distinctCount { kind accountId }
                    total: sum { amount }
                    median { amount }
                    p90: percentile(fraction: 0.9) { amount }
                    stddevSample { amount }
                }
                deposits: aggregates(filter: { kind: { equalTo: "deposit" } }) { sum { amount } max { bookedAt } }
                days: groupedAggregates(groupBy: [BOOKED_AT_TRUNCATED_TO_DAY], fillGaps: true) { keys count sum { amount } }
                paris: groupedAggregates(groupBy: [BOOKED_AT_TRUNCATED_TO_DAY], timeZone: "Europe/Paris", fillGaps: true) { count }
                bins: groupedAggregates(groupBy: [BOOKED_AT_BINNED], binWidth: "PT15M", having: { count: { greaterThan: 0 } }) { keys count }
            } }"#,
        )
        .await;
    let c = &data["entriesConnection"];
    assert_eq!(
        c["aggregates"]["distinctCount"],
        json!({ "kind": 2, "accountId": 2 })
    );
    assert_eq!(c["aggregates"]["total"]["amount"].as_f64(), Some(420.0));
    assert_eq!(c["aggregates"]["median"]["amount"].as_f64(), Some(50.0));
    assert!(
        (c["aggregates"]["p90"]["amount"].as_f64().unwrap() - 340.0).abs() < 1e-9,
        "{c}"
    );
    assert!(c["aggregates"]["stddevSample"]["amount"].as_f64().unwrap() > 200.0);
    assert_eq!(
        c["deposits"]["sum"]["amount"].as_f64(),
        Some(650.0),
        "aggregate FILTER"
    );
    assert_eq!(
        c["deposits"]["max"]["bookedAt"],
        "2024-01-03T09:00:00+00:00"
    );
    // The empty day between is filled in
    assert_eq!(
        c["days"],
        json!([
            { "keys": ["2024-01-01T00:00:00+00:00"], "count": 3, "sum": { "amount": 570.0 } },
            { "keys": ["2024-01-02T00:00:00+00:00"], "count": 0, "sum": { "amount": null } },
            { "keys": ["2024-01-03T00:00:00+00:00"], "count": 2, "sum": { "amount": -150.0 } },
        ])
    );
    // 23:30 UTC on the 3rd is the 4th in Paris
    assert_eq!(
        c["paris"],
        json!([{ "count": 3 }, { "count": 0 }, { "count": 1 }, { "count": 1 }])
    );
    let bins: Vec<(String, i64)> = c["bins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["keys"][0].as_str().unwrap().to_string(),
                b["count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        &bins[..2],
        [
            ("2024-01-01T10:00:00+00:00".to_string(), 2),
            ("2024-01-01T10:15:00+00:00".to_string(), 1)
        ]
    );

    let err = env
        .try_run(
            "{ entriesConnection { groupedAggregates(groupBy: [BOOKED_AT_BINNED]) { count } } }",
        )
        .await
        .unwrap_err();
    assert!(err.contains("`_BINNED` keys need `binWidth`"), "{err}");
}

#[tokio::test]
async fn test_window_values_on_edges() {
    let env = setup().await;
    let first = env
        .run("{ entriesConnection(orderBy: [BOOKED_AT_ASC], first: 2) { pageInfo { endCursor } } }")
        .await;
    let cursor = first["entriesConnection"]["pageInfo"]["endCursor"]
        .as_str()
        .unwrap()
        .to_string();
    let data = env
        .run(&format!(
            r#"{{ entriesConnection(orderBy: [BOOKED_AT_ASC], first: 2, after: "{cursor}") {{
                edges {{ rowNumber runningSum {{ amount }} balance: runningSum(partitionBy: [ACCOUNT_ID]) {{ amount }} inAccount: rowNumber(partitionBy: [ACCOUNT_ID]) node {{ amount }} }}
            }} }}"#
        ))
        .await;
    // Window values count the rows before the page too (edges also carry their cursor)
    let edges: Vec<serde_json::Value> = data["entriesConnection"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let mut e = e.clone();
            e.as_object_mut().unwrap().remove("cursor");
            e
        })
        .collect();
    assert_eq!(
        json!(edges),
        json!([
            { "rowNumber": 3, "runningSum": { "amount": 570.0 }, "balance": { "amount": 70.0 }, "inAccount": 2, "node": { "amount": "-30.00" } },
            { "rowNumber": 4, "runningSum": { "amount": 620.0 }, "balance": { "amount": 120.0 }, "inAccount": 3, "node": { "amount": "50.00" } },
        ])
    );
    let ranked = env.run("{ entriesConnection(orderBy: [KIND_ASC], last: 1) { edges { rank denseRank percentRank node { kind } } } }").await;
    let edge = &ranked["entriesConnection"]["edges"][0];
    assert_eq!(
        (
            &edge["rank"],
            &edge["denseRank"],
            &edge["percentRank"],
            &edge["node"]
        ),
        (
            &json!(4),
            &json!(2),
            &json!(0.75),
            &json!({ "kind": "withdrawal" })
        )
    );
}

#[tokio::test]
async fn test_connect_or_create() {
    let env = setup().await;
    assert!(env
        .block("input", "TeamToOneInput")
        .contains("  connectOrCreate: TeamConnectOrCreateInput\n"));
    let results = env
        .rolled_back(&[
            r#"mutation { createMember(input: { email: "n1@x", team: { connectOrCreate: { where: { name: "Red" }, create: { name: "Red" } } } }) { member { team { name } } } }"#,
            r#"mutation { createMember(input: { email: "n2@x", team: { connectOrCreate: { where: { name: "Red" }, create: { name: "Red" } } } }) { member { team { name } } } }"#,
            r#"{ teamsConnection(filter: { name: { equalTo: "Red" } }) { totalCount nodes { members { email } } } }"#,
            r#"mutation { updateTeam(id: "VGVhbTox", input: { members: { connectOrCreate: [
                { where: { email: "loner@x" }, create: { email: "loner@x" } },
                { where: { email: "new@x" }, create: { email: "new@x", role: "lead" } }
            ] } }) { team { members { email role } } } }"#,
            r#"mutation { createTeam(input: { name: "Blue", tags: { connectOrCreate: [{ where: { label: "old" }, create: { label: "old" } }, { where: { label: "fast" }, create: { label: "fast" } }] } }) { team { tags { label } } } }"#,
            r#"{ tagsConnection { totalCount } }"#,
        ])
        .await;
    let ok = |i: usize| results[i].clone().unwrap_or_else(|e| panic!("{i}: {e}"));
    assert_eq!(ok(0)["createMember"]["member"]["team"]["name"], "Red");
    assert_eq!(ok(1)["createMember"]["member"]["team"]["name"], "Red");
    assert_eq!(
        ok(2)["teamsConnection"]["totalCount"],
        1,
        "created once, then found"
    );
    assert_eq!(
        ok(2)["teamsConnection"]["nodes"][0]["members"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        ok(3)["updateTeam"]["team"]["members"],
        json!([{ "email": "a@x", "role": "member" }, { "email": "b@x", "role": "member" }, { "email": "loner@x", "role": "member" }, { "email": "new@x", "role": "lead" }])
    );
    assert_eq!(
        ok(4)["createTeam"]["team"]["tags"],
        json!([{ "label": "old" }, { "label": "fast" }])
    );
    assert_eq!(ok(5)["tagsConnection"]["totalCount"], 2);
}

#[tokio::test]
async fn test_nested_update_delete_and_referenced_update() {
    let env = setup().await;
    let team = r#"updateTeam(id: "VGVhbTox""#;
    let results = env
        .rolled_back(&[
            &format!(
                r#"mutation {{ {team}, input: {{ members: {{ update: [{{ where: {{ email: "a@x" }}, data: {{ role: "owner" }} }}], delete: [{{ email: "b@x" }}] }} }}) {{ team {{ members {{ email role }} }} }} }}"#
            ),
            r#"mutation { updateMember(id: "TWVtYmVyOjM=", input: { team: { update: { name: "Renamed" } } }) { member { email team { name } } } }"#,
        ])
        .await;
    let updated = results[0].clone().unwrap();
    assert_eq!(
        updated["updateTeam"]["team"]["members"],
        json!([{ "email": "a@x", "role": "owner" }])
    );
    assert_eq!(
        results[1].clone().unwrap()["updateMember"]["member"],
        json!({ "email": "c@x", "team": { "name": "Renamed" } })
    );

    // Only the row's own children
    let err = env
        .rolled_back(&[&format!(r#"mutation {{ {team}, input: {{ members: {{ delete: [{{ email: "c@x" }}] }} }}) {{ team {{ name }} }} }}"#)])
        .await
        .remove(0)
        .unwrap_err();
    assert!(
        err.contains("`input.members.delete[0]` matches no `members` row related to it"),
        "{err}"
    );
    let err = env
        .rolled_back(&[r#"mutation { updateTeam(id: "VGVhbTox", input: { tags: { delete: [{ label: "old" }] } }) { team { name } } }"#])
        .await
        .remove(0)
        .unwrap_err();
    assert!(err.contains("linked, not owned"), "{err}");
}

#[tokio::test]
async fn test_sync_with_merge() {
    let env = setup().await;
    assert!(env
        .block("input", "MemberToManyUpdateInput")
        .contains("  sync: [CreateMemberInput!]\n"));
    let results = env
        .rolled_back(&[
            r#"mutation { updateTeam(id: "VGVhbTox", input: { members: { sync: [
                { id: 1, email: "a@x", role: "lead" },
                { email: "fresh@x", role: "member" }
            ] } }) { team { members { databaseId email role } } } }"#,
            r#"{ membersConnection(orderBy: [EMAIL_ASC]) { nodes { email team { name } } } }"#,
        ])
        .await;
    let synced = results[0].clone().unwrap_or_else(|e| panic!("{e}"));
    let members = synced["updateTeam"]["team"]["members"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(members.len(), 2, "{members:?}");
    assert_eq!(
        members[0],
        json!({ "databaseId": 1, "email": "a@x", "role": "lead" }),
        "matched by key: updated"
    );
    assert_eq!(members[1]["email"], "fresh@x", "no key: inserted");
    let emails: Vec<&str> = results[1].as_ref().unwrap()["membersConnection"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["email"].as_str().unwrap())
        .collect();
    assert!(
        !emails.contains(&"b@x"),
        "unlisted children are deleted: {emails:?}"
    );
    assert!(
        emails.contains(&"c@x") && emails.contains(&"loner@x"),
        "other rows are left alone: {emails:?}"
    );

    let err = env
        .rolled_back(&[r#"mutation { updateTeam(id: "VGVhbTox", input: { members: { sync: [{ email: "a@x" }, { email: "z@x", role: "x" }] } }) { team { name } } }"#])
        .await
        .remove(0)
        .unwrap_err();
    assert!(err.contains("must set the same fields"), "{err}");
}

#[tokio::test]
async fn test_streaming_exports() {
    let env = setup().await;
    let state = env.state(false);
    let request = json!({ "query": "{ numbersConnection(filter: { n: { lessThan: 60 } }, orderBy: [PRIMARY_KEY_ASC]) { nodes { databaseId n } } }" });
    let (status, headers, body) = send(
        state.clone(),
        "/export",
        "application/json",
        serde_json::to_vec(&request).unwrap(),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/x-ndjson");
    let text = String::from_utf8(body).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1200, "more rows than one portal fetch");
    assert_eq!(lines[0], json!({ "databaseId": 1, "n": 1 }));
    assert_eq!(lines[1199], json!({ "databaseId": 2000, "n": 0 }));

    let request = json!({ "query": "{ accountsConnection(orderBy: [PRIMARY_KEY_ASC]) { nodes { databaseId owner entries { amount } } } }", "format": "csv" });
    let (_, headers, body) = send(
        state.clone(),
        "/export",
        "application/json",
        serde_json::to_vec(&request).unwrap(),
        &[],
    )
    .await;
    assert_eq!(headers[header::CONTENT_TYPE], "text/csv; charset=utf-8");
    let csv = String::from_utf8(body).unwrap();
    let rows: Vec<&str> = csv.split("\r\n").collect();
    assert_eq!(rows[0], "databaseId,owner,entries");
    assert!(
        rows[1].starts_with("1,\"Doe, Jane\",\"[{\"\"amount\"\":\"\"100.00\"\"}"),
        "{csv}"
    );
    assert!(rows[2].starts_with("2,\"Roe \"\"Rick\"\"\","), "{csv}");

    let (status, _, _) = send(
        state,
        "/export",
        "application/json",
        serde_json::to_vec(&json!({ "query": "{ accounts { owner } }" })).unwrap(),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "only connections export");
}

#[tokio::test]
async fn test_copy_imports() {
    let env = setup().await;
    let state = env.state(false);
    let lines = "{\"label\": \"imp-a\", \"qty\": 1, \"tags\": [\"x\", \"y\"]}\n{\"label\": \"imp-b\", \"qty\": null, \"tags\": []}\n\n{\"label\": \"imp-c\", \"qty\": 3, \"tags\": null}";
    let (status, _, body) = send(
        state.clone(),
        "/import/Import",
        "application/x-ndjson",
        lines.as_bytes().to_vec(),
        &[],
    )
    .await;
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let rows = env.client.query("SELECT label, qty, tags FROM ops_fixture.imports WHERE label LIKE 'imp-%' ORDER BY label", &[]).await.unwrap();
    env.client
        .execute(
            "DELETE FROM ops_fixture.imports WHERE label LIKE 'imp-%'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        (status, &body),
        (StatusCode::OK, &json!({ "data": { "inserted": 3 } }))
    );
    let got: Vec<(String, Option<i32>, Option<Vec<String>>)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        got,
        [
            (
                "imp-a".to_string(),
                Some(1),
                Some(vec!["x".to_string(), "y".to_string()])
            ),
            ("imp-b".to_string(), None, Some(vec![])),
            ("imp-c".to_string(), Some(3), None)
        ]
    );

    // An invalid row, or a violated constraint: nothing is imported
    let (status, _, body) = send(
        state.clone(),
        "/import/Import",
        "application/x-ndjson",
        b"{\"label\": \"imp-d\"}\n{\"label\": \"imp-e\", \"qty\": \"many\"}".to_vec(),
        &[],
    )
    .await;
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0]["extensions"],
        json!({ "code": "BAD_USER_INPUT", "field": "line 2.qty", "line": 2 }),
        "{body}"
    );
    let (_, _, body) = send(
        state,
        "/import/Import",
        "application/x-ndjson",
        b"{\"label\": \"imp-f\"}\n{\"label\": \"imp-f\"}".to_vec(),
        &[],
    )
    .await;
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["errors"][0]["extensions"]["code"], "LABEL_TAKEN",
        "{body}"
    );
    let left: i64 = env
        .client
        .query_one(
            "SELECT count(*) FROM ops_fixture.imports WHERE label LIKE 'imp-%'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 0);
}

#[tokio::test]
async fn test_read_replica_routing() {
    let env = setup().await;
    let state = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            env.schema.clone(),
            env.catalog.clone(),
            SCHEMA.to_string(),
            DebugOptions {
                sql: true,
                explain: false,
            },
        )
        .with_replicas(vec![create_pool(common::db_url(), 2).unwrap()]),
    );
    let query = json!({ "query": "{ accounts { owner } }" });

    // The "replica" is this primary: not a standby, so not used
    state.refresh_replicas().await;
    assert!(!state.replicas[0].standby.load(Ordering::Relaxed));
    let (_, _, body) = graphql(state.clone(), query.clone(), &[]).await;
    assert_eq!(body["extensions"]["database"], "primary");

    // Taken for a standby, it serves queries...
    state.replicas[0].standby.store(true, Ordering::Relaxed);
    let (_, _, body) = graphql(state.clone(), query.clone(), &[]).await;
    assert_eq!(body["extensions"]["database"], "replica");
    assert_eq!(body["data"]["accounts"].as_array().unwrap().len(), 2);
    // ...unless it has not replayed the client's own writes
    let (_, _, body) = graphql(state.clone(), query, &[(MIN_WAL_POSITION_HEADER, "0/1")]).await;
    assert_eq!(
        body["extensions"]["database"], "primary",
        "a primary replays no WAL"
    );

    // Mutations run on the primary and report its WAL position
    let (_, headers, body) = graphql(state, json!({ "query": r#"mutation { updateAccount(id: "QWNjb3VudDox", input: { owner: "Doe, Jane" }) { account { owner } } }"# }), &[]).await;
    assert_eq!(body["extensions"]["database"], "primary");
    let lsn = headers
        .get(WAL_POSITION_HEADER)
        .expect("WAL position")
        .to_str()
        .unwrap();
    assert!(lsn.contains('/'), "{lsn}");
}
