//! PostgreSQL 19 auto-adapting feature tests.
//!
//! Runs against BOTH PostgreSQL 18 and PostgreSQL 19:
//! - On PostgreSQL < 19: verifies strict gating (PG 19 syntax is never emitted, GraphQL SDL /
//!   OpenAPI stay clean, and PG 19-only endpoints/actions reject cleanly).
//! - On PostgreSQL >= 19: exercises live `ON CONFLICT DO SELECT` (`connectOrCreate`,
//!   `ConflictAction.SELECT`, REST `resolution=select`), `WAIT FOR LSN` replica routing,
//!   `COPY ... TO STDOUT (FORMAT json[, FORCE_ARRAY])` streaming exports, `IGNORE NULLS`
//!   value windows, `pg_stat_statements` generic/custom plan counts, `EXPLAIN (ANALYZE, IO)`,
//!   JSONPath string methods, and `postgres_fdw` `import_stats` advice.
use crate::common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::statements::{top_statements, StatementsError};
use graphpg::catalog::{CatalogInspector, DatabaseCatalog};
use graphpg::compiler::{CompiledOperation, SqlCompiler};
use graphpg::config::{models::Timeout, Config};
use graphpg::openapi::OpenApiGenerator;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{
    create_pool, create_router, DebugOptions, ServerState, MIN_WAL_POSITION_HEADER,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "pg19_fixture";
const FIXTURE_VERSION: &str = "pg19_fixture v1";

const FIXTURE_SQL: &str = r#"
CREATE SCHEMA pg19_fixture;
SET LOCAL search_path = pg19_fixture, public;

CREATE TABLE authors (
    id serial PRIMARY KEY,
    name text NOT NULL
);

CREATE TABLE tags (
    id serial PRIMARY KEY,
    slug text NOT NULL UNIQUE,
    label text NOT NULL DEFAULT 'default'
);

CREATE TABLE notes (
    id serial PRIMARY KEY,
    author_id int NOT NULL REFERENCES authors(id),
    tag_id int NOT NULL REFERENCES tags(id),
    body text NOT NULL
);

CREATE TABLE readings (
    id serial PRIMARY KEY,
    grp text NOT NULL,
    seq int NOT NULL,
    val int,
    label text
);

CREATE TABLE docs (
    id serial PRIMARY KEY,
    doc jsonb NOT NULL
);

CREATE TABLE export_items (
    id serial PRIMARY KEY,
    tag_id int REFERENCES tags(id),
    code text NOT NULL,
    payload jsonb NOT NULL
);

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pg19_coc_reader') THEN
        CREATE ROLE pg19_coc_reader NOLOGIN;
    END IF;
END $$;
GRANT USAGE ON SCHEMA pg19_fixture TO pg19_coc_reader;
REVOKE ALL ON TABLE authors, tags, notes FROM pg19_coc_reader;
GRANT SELECT, INSERT ON TABLE authors, tags, notes TO pg19_coc_reader;
GRANT USAGE, SELECT ON SEQUENCE authors_id_seq, tags_id_seq, notes_id_seq TO pg19_coc_reader;

INSERT INTO authors (id, name) VALUES (1, 'Ada'), (2, 'Grace');
SELECT setval('authors_id_seq', 100);
INSERT INTO tags (id, slug, label) VALUES (1, 'existing', 'Original'), (2, 'second', 'Two');
SELECT setval('tags_id_seq', 100);

INSERT INTO readings (grp, seq, val, label) VALUES
    ('a', 1, 10, 'first-a'),
    ('a', 2, NULL, NULL),
    ('a', 3, NULL, 'third-a'),
    ('a', 4, 25, NULL),
    ('b', 1, NULL, NULL),
    ('b', 2, 7, 'second-b');

INSERT INTO docs (doc) VALUES
    ('{"name": "  Alice Smith  ", "code": "x:pg19:z"}'),
    ('{"name": "Bob", "code": "y:other:w"}');

INSERT INTO export_items (tag_id, code, payload)
SELECT (g % 2) + 1, 'code-' || g, jsonb_build_object('idx', g, 'ok', g % 2 = 0)
FROM generate_series(1, 1100) g;
INSERT INTO export_items (tag_id, code, payload)
VALUES (1, E'tricky''$$1$gp0$\n"q"\\s', '{"special": true}');
"#;

async fn connect() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to PostgreSQL");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {e}");
        }
    });
    client
}

async fn ensure_fixture(client: &tokio_postgres::Client) {
    client
        .execute("SELECT pg_advisory_lock(424299)", &[])
        .await
        .unwrap();
    let version: Option<String> = client
        .query_opt(
            "SELECT obj_description(oid, 'pg_namespace') FROM pg_namespace WHERE nspname = $1",
            &[&SCHEMA],
        )
        .await
        .unwrap()
        .and_then(|r| r.get(0));
    if version.as_deref() != Some(FIXTURE_VERSION) {
        client
            .batch_execute(&format!(
                "BEGIN; DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; {FIXTURE_SQL} COMMENT ON SCHEMA {SCHEMA} IS '{FIXTURE_VERSION}'; COMMIT;"
            ))
            .await
            .expect("pg19 fixture setup failed");
    }
    client
        .execute("SELECT pg_advisory_unlock(424299)", &[])
        .await
        .unwrap();
}

struct Env {
    client: tokio_postgres::Client,
    config: Config,
    catalog: DatabaseCatalog,
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    sdl: String,
    openapi: Value,
}

async fn setup() -> Env {
    let client = connect().await;
    ensure_fixture(&client).await;
    let config = Config::default();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .expect("inspect pg19_fixture");
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    let schema =
        SchemaBuilder::build(&catalog).unwrap_or_else(|e| panic!("invalid schema: {e}\n{sdl}"));
    let openapi = OpenApiGenerator::generate(&catalog, &config);
    Env {
        client,
        config,
        catalog,
        schema,
        sdl,
        openapi,
    }
}

impl Env {
    fn state(&self, debug: DebugOptions) -> Arc<ServerState> {
        Arc::new(
            ServerState::new(
                create_pool(common::db_url(), 8).unwrap(),
                self.schema.clone(),
                self.catalog.clone(),
                SCHEMA.to_string(),
                debug,
            )
            .with_config(self.config.clone()),
        )
    }

    fn compile_gql(&self, query: &str) -> CompiledOperation {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &self.schema,
            query,
            "q.graphql",
        )
        .unwrap();
        let plan = QueryPlanner::new(&self.catalog, SCHEMA).plan(&doc).unwrap();
        SqlCompiler::compile_operation(&plan)
    }
}

async fn http_call(
    state: Arc<ServerState>,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let bytes = match body {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            serde_json::to_vec(&v).unwrap()
        }
        None => Vec::new(),
    };
    let response = create_router(state)
        .oneshot(builder.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let hdrs = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, hdrs, body)
}

#[tokio::test]
async fn test_pg19_connect_or_create_do_select_and_concurrency() {
    let env = setup().await;
    let coc_matching = r#"mutation {
        createNote(input: {
            authorId: 1,
            body: "note",
            tag: { connectOrCreate: { where: { slug: "existing" }, create: { slug: "existing", label: "X" } } }
        }) { note { databaseId tag { slug } } }
    }"#;
    let coc_mismatched = r#"mutation {
        createNote(input: {
            authorId: 1,
            body: "mismatch",
            tag: { connectOrCreate: { where: { slug: "existing" }, create: { slug: "other-slug", label: "Y" } } }
        }) { note { databaseId tag { slug } } }
    }"#;

    let CompiledOperation::Mutation(muts_match) = env.compile_gql(coc_matching) else {
        panic!("expected mutation");
    };
    let sql_match = &muts_match[0].writes[0].statement.query.sql;

    let CompiledOperation::Mutation(muts_mismatch) = env.compile_gql(coc_mismatched) else {
        panic!("expected mutation");
    };
    let sql_mismatch = &muts_mismatch[0].writes[0].statement.query.sql;
    assert!(
        sql_mismatch.contains("WITH _found AS"),
        "mismatched key/create values must always fall back to WITH _found: {sql_mismatch}"
    );

    assert!(
        !sql_mismatch.contains("ON CONFLICT"),
        "without a key that `where` and `create` agree on there is no arbiter: {sql_mismatch}"
    );
    assert!(!muts_mismatch[0].writes[0].retry_if_empty);

    if env.catalog.server_version < 190000 {
        // PG 18: the 2-CTE form gives way to a concurrent insert of the key instead of
        // failing with 23505, and the executor reads the committed row on a second run.
        assert!(
            sql_match.contains("WITH _found AS")
                && sql_match.contains("ON CONFLICT (\"slug\") DO NOTHING RETURNING"),
            "PG < 19 must use the 2-CTE form with ON CONFLICT DO NOTHING: {sql_match}"
        );
        assert!(!sql_match.contains("DO SELECT"), "{sql_match}");
        assert!(muts_match[0].writes[0].retry_if_empty);
    } else {
        assert!(!muts_match[0].writes[0].retry_if_empty);
        check_do_select(&env, sql_match, coc_matching).await;
    }

    // 20 concurrent tasks racing on the exact same new unique slug, on every version:
    // 0 errors, a single tag row
    race_same_new_slug(&env).await;
}

async fn check_do_select(env: &Env, sql_match: &str, coc_matching: &str) {
    assert!(
        sql_match.contains("ON CONFLICT (\"slug\") DO SELECT RETURNING"),
        "PG 19+ should compile matching connectOrCreate to ON CONFLICT DO SELECT: {sql_match}"
    );
    assert!(
        !sql_match.contains("WITH _found"),
        "should not emit 2-CTE SQL: {sql_match}"
    );
    assert!(
        !sql_match.contains("FOR KEY SHARE"),
        "must not emit FOR KEY SHARE (requires UPDATE privilege): {sql_match}"
    );

    // Role with only SELECT + INSERT privileges (no UPDATE) must succeed under DO SELECT
    env.client
        .batch_execute("BEGIN; SET LOCAL ROLE pg19_coc_reader;")
        .await
        .unwrap();
    let res = graphpg::execution::ExecutionEngine::execute(&env.client, &{
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &env.schema,
            coc_matching,
            "q.graphql",
        )
        .unwrap();
        QueryPlanner::new(&env.catalog, SCHEMA).plan(&doc).unwrap()
    })
    .await
    .expect("DO SELECT must work without UPDATE privilege");
    env.client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(res["data"]["createNote"]["note"]["tag"]["slug"], "existing");
}

async fn race_same_new_slug(env: &Env) {
    let state = env.state(DebugOptions::default());
    let unique_slug = format!("race-{}", std::process::id());
    let mutation = r#"mutation($slug: String!) {
        createNote(input: {
            authorId: 1,
            body: "race",
            tag: { connectOrCreate: { where: { slug: $slug }, create: { slug: $slug, label: "Race" } } }
        }) { note { databaseId tag { databaseId slug } } }
    }"#;
    let mut handles = Vec::new();
    for _ in 0..20 {
        let st = state.clone();
        let sl = unique_slug.clone();
        handles.push(tokio::spawn(async move {
            let (status, _, bytes) = http_call(
                st,
                "POST",
                "/graphql",
                Some(json!({ "query": mutation, "variables": { "slug": sl } })),
                &[],
            )
            .await;
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            (status, body)
        }));
    }
    let mut tag_ids = Vec::new();
    for h in handles {
        let (status, body) = h.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.get("errors").is_none(),
            "unexpected race error: {body}"
        );
        tag_ids.push(
            body["data"]["createNote"]["note"]["tag"]["databaseId"]
                .as_i64()
                .unwrap(),
        );
    }
    assert!(
        tag_ids.windows(2).all(|w| w[0] == w[1]),
        "all 20 concurrent connectOrCreate calls must resolve to one tag row: {tag_ids:?}"
    );
    // Clean up committed race rows
    env.client
        .execute("DELETE FROM pg19_fixture.notes WHERE body = 'race'", &[])
        .await
        .unwrap();
    env.client
        .execute(
            "DELETE FROM pg19_fixture.tags WHERE slug = $1",
            &[&unique_slug],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_pg19_on_conflict_select_action() {
    let env = setup().await;
    let state = env.state(DebugOptions {
        sql: true,
        explain: false,
    });
    let put_enum = env.openapi["paths"]["/rest/tags"]["put"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "resolution")
        .unwrap()["schema"]["enum"]
        .as_array()
        .unwrap()
        .clone();

    if env.catalog.server_version < 190000 {
        assert!(!env.sdl.contains("ON CONFLICT DO SELECT"));
        assert!(!put_enum.iter().any(|v| v == "select"), "{put_enum:?}");
        let (status, _, _) = http_call(
            state,
            "PUT",
            "/rest/tags?conflict=slug&resolution=select",
            Some(json!([{ "slug": "existing", "label": "Ignored" }])),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        return;
    }

    assert!(
        env.sdl.contains("SELECT\n"),
        "ConflictAction must include SELECT on PG 19+"
    );
    assert!(put_enum.iter().any(|v| v == "select"), "{put_enum:?}");

    // Mixed new + existing row via GraphQL with previousTags and DO SELECT
    let gql = r#"mutation {
        createTags(
            inputs: [
                { slug: "existing", label: "OverwriteAttempt" },
                { slug: "sel-new-gql", label: "NewGql" }
            ],
            onConflict: { key: SLUG, action: SELECT }
        ) {
            affectedCount
            tags { slug label }
            previousTags { slug label }
        }
    }"#;
    let ctid_before: String = env
        .client
        .query_one(
            "SELECT ctid::text FROM pg19_fixture.tags WHERE slug = 'existing'",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    let (status, _, bytes) = http_call(
        state.clone(),
        "POST",
        "/graphql",
        Some(json!({ "query": gql })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(body["data"]["createTags"]["affectedCount"], 2);
    assert_eq!(
        body["data"]["createTags"]["tags"],
        json!([
            { "slug": "existing", "label": "Original" },
            { "slug": "sel-new-gql", "label": "NewGql" }
        ])
    );
    assert_eq!(
        body["data"]["createTags"]["previousTags"],
        json!([
            { "slug": "existing", "label": "Original" },
            null
        ])
    );
    let ctid_after: String = env
        .client
        .query_one(
            "SELECT ctid::text FROM pg19_fixture.tags WHERE slug = 'existing'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        ctid_before, ctid_after,
        "DO SELECT must create zero dead tuples"
    );

    // Payload with ONLY affectedCount (tests RETURNING 1 fallback when returning is empty)
    let gql_count_only = r#"mutation {
        createTags(
            inputs: [{ slug: "existing", label: "Ignored" }],
            onConflict: { key: SLUG, action: SELECT }
        ) { affectedCount }
    }"#;
    let (_, _, bytes) = http_call(
        state.clone(),
        "POST",
        "/graphql",
        Some(json!({ "query": gql_count_only })),
        &[],
    )
    .await;
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(body["data"]["createTags"]["affectedCount"], 1);

    // REST resolution=select returns existing + inserted rows without updating existing row
    let (status, _, bytes) = http_call(
        state.clone(),
        "PUT",
        "/rest/tags?conflict=slug&resolution=select",
        Some(json!([
            { "slug": "existing", "label": "RestAttempt" },
            { "slug": "sel-new-rest", "label": "NewRest" }
        ])),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rest_rows: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(rest_rows[0]["label"], "Original");
    assert_eq!(rest_rows[1]["label"], "NewRest");

    // resolution=select combined with update= fails with 400
    let (status, _, _) = http_call(
        state,
        "PUT",
        "/rest/tags?conflict=slug&resolution=select&update=label",
        Some(json!([{ "slug": "existing", "label": "X" }])),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    env.client
        .execute(
            "DELETE FROM pg19_fixture.tags WHERE slug IN ('sel-new-gql', 'sel-new-rest')",
            &[],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_pg19_wait_for_lsn_and_replica_routing() {
    let env = setup().await;
    let parsed = Config::from_yaml("database:\n  replica_wait_timeout: 75ms\n").unwrap();
    assert_eq!(
        parsed.database.replica_wait_timeout,
        Some(Timeout(Duration::from_millis(75)))
    );

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
        .with_replicas(vec![create_pool(common::db_url(), 2).unwrap()])
        .with_replica_wait_timeout(Some(Timeout(Duration::from_millis(50)))),
    );
    state.refresh_replicas().await;
    let ver = state.replicas[0].server_version.load(Ordering::Relaxed);
    assert!(ver >= 180000, "unexpected replica version: {ver}");
    assert!(!state.replicas[0].standby.load(Ordering::Relaxed));

    // Simulate standby mode against primary DB: WAIT FOR LSN on PG 19 detects 'not in recovery'
    // and safely clears standby flag and falls back to primary; malformed LSNs also fall back cleanly.
    state.replicas[0].standby.store(true, Ordering::Relaxed);
    let (_, _, bytes) = http_call(
        state.clone(),
        "POST",
        "/graphql",
        Some(json!({ "query": "{ authors { name } }" })),
        &[(MIN_WAL_POSITION_HEADER, "0/1")],
    )
    .await;
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["extensions"]["database"], "primary", "{body}");
    if env.catalog.server_version >= 190000 {
        assert!(
            !state.replicas[0].standby.load(Ordering::Relaxed),
            "WAIT FOR LSN 'not in recovery' must flip standby=false"
        );
    }

    state.replicas[0].standby.store(true, Ordering::Relaxed);
    let (_, _, bytes) = http_call(
        state,
        "POST",
        "/graphql",
        Some(json!({ "query": "{ authors { name } }" })),
        &[(
            MIN_WAL_POSITION_HEADER,
            "0/1'; DROP TABLE pg19_fixture.authors; --",
        )],
    )
    .await;
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(body["extensions"]["database"], "primary");
}

#[tokio::test]
async fn test_pg19_streaming_exports() {
    let env = setup().await;
    let state = env.state(DebugOptions::default());

    // 1. Parameterized + >1100 rows NDJSON export
    let ndjson_req = json!({
        "query": "query($min: Int!) { exportItemsConnection(filter: { id: { greaterThanOrEqualTo: $min } }, orderBy: [PRIMARY_KEY_ASC]) { nodes { databaseId code payload tag { slug } } } }",
        "variables": { "min": 1 }
    });
    let (status, hdrs, bytes) = http_call(
        state.clone(),
        "POST",
        "/export",
        Some(ndjson_req.clone()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hdrs[header::CONTENT_TYPE], "application/x-ndjson");
    let ndjson_rows: Vec<Value> = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(ndjson_rows.len(), 1101);
    assert_eq!(ndjson_rows[0]["tag"]["slug"], "second");
    assert_eq!(
        ndjson_rows.last().unwrap()["code"],
        "tricky'$$1$gp0$\n\"q\"\\s"
    );

    // 2. format="json" (FORCE_ARRAY on PG 19, JSON array on PG 18) with special parameter chars
    let special_req = json!({
        "query": "query($code: String!) { exportItemsConnection(filter: { code: { equalTo: $code } }, orderBy: [PRIMARY_KEY_ASC]) { nodes { databaseId code payload } } }",
        "variables": { "code": "tricky'$$1$gp0$\n\"q\"\\s" },
        "format": "json"
    });
    let (status, hdrs, bytes) =
        http_call(state.clone(), "POST", "/export", Some(special_req), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hdrs[header::CONTENT_TYPE], "application/json");
    let arr: Value = serde_json::from_slice(&bytes).unwrap();
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["payload"], json!({"special": true}));

    // 3. Empty result with format="json" produces valid []
    let empty_req = json!({
        "query": "{ exportItemsConnection(filter: { id: { lessThan: 0 } }) { nodes { databaseId } } }",
        "format": "json"
    });
    let (status, _, bytes) =
        http_call(state.clone(), "POST", "/export", Some(empty_req), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let empty_arr: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(empty_arr, json!([]));

    // 4. Invalid format -> 400
    let (status, _, _) = http_call(
        state,
        "POST",
        "/export",
        Some(json!({ "query": "{ exportItemsConnection { nodes { databaseId } } }", "format": "xml" })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_pg19_window_ignore_nulls() {
    let env = setup().await;
    let state = env.state(DebugOptions::default());
    let list_params = &env.openapi["paths"]["/rest/readings"]["get"]["parameters"]
        .as_array()
        .unwrap();

    if env.catalog.server_version < 190000 {
        assert!(!env.sdl.contains("WindowValues"));
        assert!(!list_params.iter().any(|p| p["name"] == "ignoreNulls"));
        let (status, _, _) =
            http_call(state, "GET", "/rest/readings?lastValue=val", None, &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        return;
    }

    assert!(env.sdl.contains("type ReadingWindowValues"));
    assert!(list_params.iter().any(|p| p["name"] == "ignoreNulls"));

    let gql = r#"{
        readingsConnection(orderBy: [SEQ_ASC, PRIMARY_KEY_ASC]) {
            edges {
                node { grp seq }
                lag(partitionBy: [GRP], offset: 1, ignoreNulls: true) { val }
                lead(partitionBy: [GRP], offset: 1, ignoreNulls: true) { val }
                firstValue(partitionBy: [GRP], ignoreNulls: true) { val }
                lastValue(partitionBy: [GRP], ignoreNulls: true) { val label }
                nthValue(n: 2, partitionBy: [GRP], ignoreNulls: true) { val }
            }
        }
    }"#;
    let (status, _, bytes) = http_call(
        state.clone(),
        "POST",
        "/graphql",
        Some(json!({ "query": gql })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    let edges = body["data"]["readingsConnection"]["edges"]
        .as_array()
        .unwrap();
    let group_a: Vec<&Value> = edges.iter().filter(|e| e["node"]["grp"] == "a").collect();
    assert_eq!(group_a.len(), 4);
    let locf: Vec<Option<i64>> = group_a
        .iter()
        .map(|e| e["lastValue"]["val"].as_i64())
        .collect();
    assert_eq!(locf, vec![Some(10), Some(10), Some(10), Some(25)]);
    assert_eq!(group_a[2]["lag"]["val"], 10);
    assert_eq!(group_a[0]["lead"]["val"], 25);
    assert_eq!(group_a[3]["firstValue"]["val"], 10);
    assert_eq!(group_a[0]["nthValue"]["val"], 25);
    assert_eq!(group_a[3]["lastValue"]["label"], "third-a");

    // REST value windows + ignoreNulls + windowNth
    let (status, _, bytes) = http_call(
        state,
        "GET",
        "/rest/readings?filter[grp][eq]=a&order=seq&partition=grp&lastValue=val,label&nthValue=val&windowNth=2&ignoreNulls=true",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(rows[2]["_window"]["lastValue"]["val"], 10);
    assert_eq!(rows[3]["_window"]["lastValue"]["label"], "third-a");
    assert_eq!(rows[0]["_window"]["nthValue"]["val"], 25);
}

#[tokio::test]
async fn test_pg19_statements_plan_calls_and_explain_io() {
    let env = setup().await;
    let state = env.state(DebugOptions {
        sql: true,
        explain: true,
    });

    // Execute parameterized query 8 times with debug.explain=true (tests EXPLAIN IO on PG 19, no IO on PG 18)
    let mut saw_explain = false;
    for i in 1..=8 {
        let query = format!(
            "query Pg19PlanTest {{ readingsConnection(filter: {{ grp: {{ equalTo: \"{}\" }} }}, first: 5) {{ nodes {{ seq val }} }} }}",
            if i % 2 == 0 { "a" } else { "b" }
        );
        let (status, _, bytes) = http_call(
            state.clone(),
            "POST",
            "/graphql",
            Some(json!({ "query": query })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.get("errors").is_none(), "{body}");
        assert!(body["extensions"]["explain"].is_array(), "{body}");
        saw_explain = true;
    }
    assert!(saw_explain);

    if !common::with_extensions(&["pg_stat_statements"]).await {
        return;
    }
    let catalog = CatalogInspector::new(&env.client)
        .inspect_schema_with_config(SCHEMA, &env.config)
        .await
        .unwrap();
    let report = match top_statements(&env.client, &catalog, 100).await {
        Ok(r) => r,
        Err(StatementsError::Unreadable(_)) => return,
        Err(e) => panic!("{e}"),
    };
    if let Some(op) = report
        .operations
        .iter()
        .find(|o| o.operation == "query Pg19PlanTest")
    {
        if env.catalog.server_version >= 190000 {
            let g = op.generic_plan_calls.expect("PG 19 generic_plan_calls");
            let c = op.custom_plan_calls.expect("PG 19 custom_plan_calls");
            assert!(g + c >= 1, "{op:?}");
        } else {
            assert!(op.generic_plan_calls.is_none());
            assert!(op.custom_plan_calls.is_none());
        }
    }
}

#[tokio::test]
async fn test_pg19_jsonpath_string_methods() {
    let env = setup().await;
    let state = env.state(DebugOptions::default());

    // graphpg binds the expression as a `jsonpath` parameter of PostgreSQL's own `@@` /
    // `jsonb_path_query_first`, so the server's JSONPath language is the API's. PostgreSQL 18
    // has no string methods: its parser rejects them and the client gets a typed input
    // error, never a 500.
    if env.catalog.server_version < 190000 {
        let gql = r#"{ docsConnection(filter: { doc: { matchesPath: "$.name.lower() == \"alice\"" } }) { nodes { id } } }"#;
        let (status, _, bytes) = http_call(
            state.clone(),
            "POST",
            "/graphql",
            Some(json!({ "query": gql })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["errors"][0]["extensions"]["code"], "BAD_USER_INPUT",
            "{body}"
        );

        let (status, _, bytes) = http_call(
            state,
            "GET",
            "/rest/docs?filter[id][eq]=1&path[doc]=$.name.btrim().upper()",
            None,
            &[],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let problem: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(problem["code"], "BAD_USER_INPUT", "{problem}");
        return;
    }

    let gql = r#"{
        docsConnection(filter: { doc: { matchesPath: "$.code.split_part(\":\", 2) == \"pg19\"" } }) {
            nodes {
                trimmedLower: doc(path: "$.name.btrim().lower()")
                replaced: doc(path: "$.code.replace(\"pg19\", \"ok\")")
            }
        }
    }"#;
    let (status, _, bytes) = http_call(
        state.clone(),
        "POST",
        "/graphql",
        Some(json!({ "query": gql })),
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(
        body["data"]["docsConnection"]["nodes"],
        json!([{ "trimmedLower": "alice smith", "replaced": "x:ok:z" }])
    );

    let (status, _, bytes) = http_call(
        state,
        "GET",
        "/rest/docs?filter[id][eq]=1&path[doc]=$.name.btrim().upper()",
        None,
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let rows: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(rows[0]["doc"], "ALICE SMITH");
}

#[tokio::test]
async fn test_pg19_postgres_fdw_import_stats_advice() {
    if !common::with_extensions(&["postgres_fdw"]).await {
        return;
    }
    let client = connect().await;
    let ver: i32 = client
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .await
        .unwrap()
        .get(0);
    if ver < 190000 {
        return;
    }
    client
        .batch_execute(
            "DROP SCHEMA IF EXISTS pg19_fdw_adv CASCADE;
             DROP SERVER IF EXISTS pg19_fdw_adv_srv CASCADE;
             CREATE SCHEMA pg19_fdw_adv;
             CREATE TABLE pg19_fdw_adv.remote_t (id int PRIMARY KEY);
             CREATE SERVER pg19_fdw_adv_srv FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname 'postgres');
             CREATE FOREIGN TABLE pg19_fdw_adv.ft (id int NOT NULL) SERVER pg19_fdw_adv_srv OPTIONS (schema_name 'pg19_fdw_adv', table_name 'remote_t');",
        )
        .await
        .unwrap();

    let mut config = Config::default();
    config.entities.insert(
        "ft".to_string(),
        serde_json::from_value(json!({ "primary_key": ["id"] })).unwrap(),
    );
    let inspector = CatalogInspector::new(&client);
    let catalog = inspector
        .inspect_schema_with_config("pg19_fdw_adv", &config)
        .await
        .unwrap();
    let advice = inspector
        .advise_foreign_tables(&["pg19_fdw_adv"], &catalog, &config.auth)
        .await
        .unwrap();
    client
        .batch_execute("DROP SCHEMA pg19_fdw_adv CASCADE; DROP SERVER pg19_fdw_adv_srv CASCADE;")
        .await
        .unwrap();
    assert!(
        advice.iter().any(|a| a.message.contains("import_stats")),
        "expected PG 19 import_stats advice: {advice:#?}"
    );
}
