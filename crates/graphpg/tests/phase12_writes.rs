//! Rows addressed by unique keys (`teamBySlug`, `updateTeamBySlug`, `deleteTeamBySlug`),
//! upserts (`onConflict` on creates), bulk inserts (`createTeams`), filter-based bulk
//! updates and deletes (`updateTeams`, `deleteTeams`), and nested creates (`create` and
//! `connect` through to-one, to-many and many-to-many relations).
//!
//! Uses a `write_fixture` schema created on demand (recreated when `FIXTURE_VERSION` changes);
//! writes run in rolled-back transactions, so the seed rows stay as they are.
mod common;
use axum::body::Body;
use axum::http::{header, Request};
use graphpg::catalog::CatalogInspector;
use graphpg::config::EngineConfig;
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

const SCHEMA: &str = "write_fixture";
const FIXTURE_VERSION: &str = "write_fixture v3";

const FIXTURE_SQL: &str = r#"
CREATE SCHEMA write_fixture;
SET search_path = write_fixture, public;

-- Serial primary key and a unique constraint
CREATE TABLE teams (
    id serial PRIMARY KEY,
    slug text NOT NULL UNIQUE,
    name text NOT NULL,
    score int NOT NULL DEFAULT 0,
    note text
);

-- Composite primary key
CREATE TABLE members (
    team_id int NOT NULL REFERENCES teams(id),
    email text NOT NULL,
    role text NOT NULL DEFAULT 'member',
    PRIMARY KEY (team_id, email)
);

-- Composite unique key beside a generated primary key; a partial unique index (not a key)
CREATE TABLE codes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org int NOT NULL,
    code text NOT NULL,
    title text,
    email text,
    UNIQUE (org, code)
);
CREATE UNIQUE INDEX codes_email_key ON codes (email) WHERE email IS NOT NULL;

-- Case-insensitive unique key
CREATE TABLE handles (id int PRIMARY KEY, handle citext NOT NULL UNIQUE, bio text);

-- No key at all
CREATE TABLE log_lines (line text NOT NULL, level int NOT NULL DEFAULT 1, team_id int REFERENCES teams(id));

-- Nested creates: two levels of to-many children, and a many-to-many relation
CREATE TABLE players (id serial PRIMARY KEY, team_id int NOT NULL REFERENCES teams(id), name text NOT NULL);
CREATE TABLE scores (id serial PRIMARY KEY, player_id int NOT NULL REFERENCES players(id), points int NOT NULL);
CREATE TABLE tags (id serial PRIMARY KEY, label text NOT NULL UNIQUE);
CREATE TABLE team_tags (team_id int NOT NULL REFERENCES teams(id), tag_id int NOT NULL REFERENCES tags(id), PRIMARY KEY (team_id, tag_id));

-- Nested writes in updates: a nullable foreign key (children can be detached)
CREATE TABLE fans (id serial PRIMARY KEY, team_id int REFERENCES teams(id), name text NOT NULL);

INSERT INTO teams (slug, name, score, note) VALUES ('red', 'Red', 10, 'first'), ('blue', 'Blue', 20, NULL), ('green', 'Green', 30, NULL);
INSERT INTO members VALUES (1, 'a@red', 'owner'), (1, 'b@red', 'member'), (2, 'c@blue', 'member');
INSERT INTO codes (org, code, title) VALUES (1, 'A', 'Alpha'), (1, 'B', 'Beta'), (2, 'A', 'Other alpha');
INSERT INTO handles VALUES (1, 'Ada', 'first');
INSERT INTO log_lines VALUES ('boot', 1), ('warn', 2);
INSERT INTO players (team_id, name) VALUES (1, 'Rita'), (2, 'Bob');
INSERT INTO tags (label) VALUES ('old');
INSERT INTO fans (team_id, name) VALUES (1, 'Fay'), (1, 'Gus'), (NULL, 'Hal');
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

/// Creates the fixture schema once (serialised by an advisory lock, since tests run
/// concurrently), recreating it when the fixture changed.
async fn ensure_fixture(client: &tokio_postgres::Client) {
    client
        .execute("SELECT pg_advisory_lock(424244)", &[])
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
        .execute("SELECT pg_advisory_unlock(424244)", &[])
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
    let config = EngineConfig::from_yaml(config).expect("config parses");
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

    /// Runs `queries` in order in one transaction that is rolled back afterwards.
    async fn run_rolled_back(&self, queries: &[&str]) -> Vec<serde_json::Value> {
        self.client.batch_execute("BEGIN").await.unwrap();
        let mut out = Vec::new();
        for query in queries {
            match self.try_run(query).await {
                Ok(data) => out.push(data),
                Err(e) => {
                    self.client.batch_execute("ROLLBACK").await.unwrap();
                    panic!("{e}\nquery: {query}");
                }
            }
        }
        self.client.batch_execute("ROLLBACK").await.unwrap();
        out
    }

    /// Like `run_rolled_back` for one query that is expected to fail.
    async fn error_rolled_back(&self, query: &str) -> String {
        self.client.batch_execute("BEGIN").await.unwrap();
        let result = self.try_run(query).await;
        self.client.batch_execute("ROLLBACK").await.unwrap();
        result.expect_err("expected an error")
    }

    /// SDL block of type/input/enum `name`.
    fn type_sdl(&self, keyword: &str, name: &str) -> String {
        let start = self
            .sdl
            .find(&format!("{keyword} {name} "))
            .unwrap_or_else(|| panic!("no {keyword} {name} in SDL:\n{}", self.sdl));
        let end = self.sdl[start..]
            .find("\n}")
            .map(|e| start + e)
            .unwrap_or(self.sdl.len());
        self.sdl[start..end].to_string()
    }
}

#[tokio::test]
async fn test_unique_keys_become_lookups_and_conflict_keys() {
    let env = setup().await;
    let query = env.type_sdl("type", "Query");
    assert!(
        query.contains("  teamBySlug(slug: String!): Team"),
        "{query}"
    );
    assert!(
        query.contains("  codeByOrgAndCode(org: Int!, code: String!): Code"),
        "{query}"
    );
    assert!(
        !query.contains("codeByEmail"),
        "a partial unique index is no key: {query}"
    );
    assert!(
        !query.contains("logLine(") && !query.contains("logLineBy"),
        "no key, no lookups: {query}"
    );

    let mutation = env.type_sdl("type", "Mutation");
    for field in [
        "  updateTeamBySlug(slug: String!, input: UpdateTeamInput!, \"\"\"Write only if the row's `rowVersion` is still this one (else `STALE_VERSION`).\"\"\" expectedVersion: String): UpdateTeamPayload!",
        "  deleteTeamBySlug(slug: String!, \"\"\"Write only if the row's `rowVersion` is still this one (else `STALE_VERSION`).\"\"\" expectedVersion: String): DeleteTeamPayload!",
        "  createTeams(inputs: [CreateTeamInput!]!, onConflict: TeamOnConflict): CreateTeamsPayload!",
        "  updateTeams(where: TeamFilter!, set: UpdateTeamInput!): UpdateTeamsPayload!",
        "  deleteTeams(where: TeamFilter!): DeleteTeamsPayload!",
        "  createLogLines(inputs: [CreateLogLineInput!]!): CreateLogLinesPayload!",
        "  updateLogLines(where: LogLineFilter!, set: UpdateLogLineInput!): UpdateLogLinesPayload!",
    ] {
        assert!(mutation.contains(field), "missing `{field}`:\n{mutation}");
    }
    assert!(
        !mutation.contains("updateLogLine("),
        "single-row update needs a key: {mutation}"
    );

    let keys = env.type_sdl("enum", "TeamConflictKey");
    assert!(
        keys.contains("  PRIMARY_KEY") && keys.contains("  SLUG"),
        "{keys}"
    );
    assert!(env
        .type_sdl("enum", "CodeConflictKey")
        .contains("  ORG_AND_CODE"));
    assert!(env
        .type_sdl("input", "TeamOnConflict")
        .contains("key: TeamConflictKey! = PRIMARY_KEY"));
    assert!(env
        .type_sdl("type", "CreateTeamsPayload")
        .contains("affectedCount: Int!"));
}

#[tokio::test]
async fn test_lookup_update_and_delete_by_unique_key() {
    let env = setup().await;
    let data = env
        .run_rolled_back(&[
            r#"{ teamBySlug(slug: "blue") { databaseId name } codeByOrgAndCode(org: 1, code: "B") { title } missing: teamBySlug(slug: "nope") { name } }"#,
            r#"mutation { updateTeamBySlug(slug: "blue", input: { name: "Navy" }) { team { databaseId slug name score } } }"#,
            r#"mutation { deleteTeamBySlug(slug: "green") { team { slug } } }"#,
            r#"mutation { updateTeamBySlug(slug: "nope", input: { name: "x" }) { team { slug } } }"#,
            r#"{ teams { slug } }"#,
        ])
        .await;
    assert_eq!(
        data[0],
        json!({ "teamBySlug": { "databaseId": 2, "name": "Blue" }, "codeByOrgAndCode": { "title": "Beta" }, "missing": null })
    );
    assert_eq!(
        data[1]["updateTeamBySlug"]["team"],
        json!({ "databaseId": 2, "slug": "blue", "name": "Navy", "score": 20 })
    );
    assert_eq!(
        data[2]["deleteTeamBySlug"]["team"],
        json!({ "slug": "green" })
    );
    assert_eq!(data[3]["updateTeamBySlug"]["team"], json!(null));
    assert_eq!(
        data[4]["teams"],
        json!([{ "slug": "red" }, { "slug": "blue" }])
    );
}

#[tokio::test]
async fn test_create_many_keeps_input_order_across_column_sets() {
    let env = setup().await;
    // Rows setting different fields insert in separate statements; the payload still lists
    // them in input order. Omitted fields take their default, explicit nulls stay null.
    let data = env
        .run_rolled_back(&[r#"mutation {
            createTeams(inputs: [
                { slug: "a", name: "A" },
                { slug: "b", name: "B", score: 5, note: "n" },
                { slug: "c", name: "C" },
                { slug: "d", name: "D", score: 7, note: null }
            ]) {
                __typename
                count: affectedCount
                rows: teams { slug score note }
            }
        }"#])
        .await;
    assert_eq!(
        data[0]["createTeams"],
        json!({
            "__typename": "CreateTeamsPayload",
            "count": 4,
            "rows": [
                { "slug": "a", "score": 0, "note": null },
                { "slug": "b", "score": 5, "note": "n" },
                { "slug": "c", "score": 0, "note": null },
                { "slug": "d", "score": 7, "note": null }
            ]
        })
    );

    let empty = env
        .run_rolled_back(&["mutation { createTeams(inputs: []) { affectedCount teams { slug } } }"])
        .await;
    assert_eq!(
        empty[0]["createTeams"],
        json!({ "affectedCount": 0, "teams": [] })
    );

    // Composite primary key, and relations of the new rows reflect the write
    let members = env
        .run_rolled_back(&[r#"mutation {
            createMembers(inputs: [{ teamId: 2, email: "z@blue" }, { teamId: 3, email: "y@green", role: "owner" }]) {
                members { email role team { slug } }
            }
        }"#])
        .await;
    assert_eq!(
        members[0]["createMembers"]["members"],
        json!([
            { "email": "z@blue", "role": "member", "team": { "slug": "blue" } },
            { "email": "y@green", "role": "owner", "team": { "slug": "green" } }
        ])
    );
}

#[tokio::test]
async fn test_upsert_updates_or_keeps_the_conflicting_row() {
    let env = setup().await;
    let data = env
        .run_rolled_back(&[
            // UPDATE (default): the fields sent, apart from the key; score is kept
            r#"mutation { createTeam(input: { slug: "red", name: "Crimson" }, onConflict: { key: SLUG }) { team { databaseId name score note } } }"#,
            // NOTHING: the existing row, unchanged
            r#"mutation { createTeam(input: { slug: "blue", name: "Ignored" }, onConflict: { key: SLUG, action: NOTHING }) { team { databaseId name } } }"#,
            // An explicit update list: only score is taken from the proposed row
            r#"mutation { createTeam(input: { slug: "green", name: "Ignored", score: 99 }, onConflict: { key: SLUG, update: [SCORE] }) { team { name score } } }"#,
            // No conflict: inserted
            r#"mutation { createTeam(input: { slug: "gold", name: "Gold" }, onConflict: { key: SLUG }) { team { slug name score } } }"#,
            // On the primary key
            r#"mutation { createTeam(input: { id: 1, slug: "red", name: "By id" }, onConflict: {}) { team { databaseId name } } }"#,
        ])
        .await;
    assert_eq!(
        data[0]["createTeam"]["team"],
        json!({ "databaseId": 1, "name": "Crimson", "score": 10, "note": "first" })
    );
    assert_eq!(
        data[1]["createTeam"]["team"],
        json!({ "databaseId": 2, "name": "Blue" })
    );
    assert_eq!(
        data[2]["createTeam"]["team"],
        json!({ "name": "Green", "score": 99 })
    );
    assert_eq!(
        data[3]["createTeam"]["team"],
        json!({ "slug": "gold", "name": "Gold", "score": 0 })
    );
    assert_eq!(
        data[4]["createTeam"]["team"],
        json!({ "databaseId": 1, "name": "By id" })
    );
}

#[tokio::test]
async fn test_bulk_upsert_lists_every_row_in_input_order() {
    let env = setup().await;
    let data = env
        .run_rolled_back(&[
            r#"mutation {
                createTeams(
                    inputs: [{ slug: "new1", name: "N1" }, { slug: "blue", name: "Kept" }, { slug: "new2", name: "N2" }]
                    onConflict: { key: SLUG, action: NOTHING }
                ) { affectedCount teams { slug name } }
            }"#,
            r#"mutation {
                createCodes(inputs: [{ org: 1, code: "A", title: "Alpha 2" }, { org: 3, code: "A", title: "New" }], onConflict: { key: ORG_AND_CODE }) {
                    affectedCount codes { org code title }
                }
            }"#,
        ])
        .await;
    // Rows left alone are listed but not counted
    assert_eq!(
        data[0]["createTeams"],
        json!({ "affectedCount": 2, "teams": [{ "slug": "new1", "name": "N1" }, { "slug": "blue", "name": "Blue" }, { "slug": "new2", "name": "N2" }] })
    );
    assert_eq!(
        data[1]["createCodes"],
        json!({ "affectedCount": 2, "codes": [{ "org": 1, "code": "A", "title": "Alpha 2" }, { "org": 3, "code": "A", "title": "New" }] })
    );
}

#[tokio::test]
async fn test_upsert_matches_rows_with_the_column_type_equality() {
    let env = setup().await;
    // `citext`: the conflicting row differs in case, and is still the one returned. Its
    // primary key is not rewritten from the proposed row unless asked for.
    let data = env
        .run_rolled_back(&[
            r#"mutation { createHandle(input: { id: 2, handle: "ADA", bio: "second" }, onConflict: { key: HANDLE }) { handle { databaseId handle bio } } }"#,
            r#"mutation { createHandle(input: { id: 3, handle: "ada", bio: "third" }, onConflict: { key: HANDLE, update: [ID, BIO] }) { handle { databaseId bio } } }"#,
        ])
        .await;
    assert_eq!(
        data[0]["createHandle"]["handle"],
        json!({ "databaseId": 1, "handle": "Ada", "bio": "second" })
    );
    assert_eq!(
        data[1]["createHandle"]["handle"],
        json!({ "databaseId": 3, "bio": "third" })
    );
}

#[tokio::test]
async fn test_upsert_requires_the_conflict_key_in_every_row() {
    let env = setup().await;
    // `id` has a default, so the input type does not require it
    let err = env
        .error_rolled_back(
            r#"mutation { createTeams(inputs: [{ id: 1, slug: "x", name: "X" }, { slug: "y", name: "Y" }], onConflict: {}) { affectedCount } }"#,
        )
        .await;
    assert!(err.contains("`inputs[1]` must set `id`"), "{err}");
    let err = env
        .error_rolled_back(r#"mutation { createTeam(input: { name: "Y", slug: "y" }, onConflict: {}) { team { slug } } }"#)
        .await;
    assert!(err.contains("`input` must set `id`"), "{err}");
}

#[tokio::test]
async fn test_update_and_delete_many_by_filter() {
    let env = setup().await;
    let data = env
        .run_rolled_back(&[
            r#"mutation { updateTeams(where: { score: { greaterThanOrEqualTo: 20 } }, set: { note: "top" }) { affectedCount teams { slug note } } }"#,
            // Filters on relations work too
            r#"mutation { deleteMembers(where: { team: { slug: { equalTo: "red" } } }) { affectedCount members { email } } }"#,
            r#"mutation { updateTeams(where: { slug: { equalTo: "nope" } }, set: { note: "x" }) { affectedCount teams { slug } } }"#,
            r#"{ members { email } teamsConnection(orderBy: [PRIMARY_KEY_ASC]) { nodes { slug note } } }"#,
        ])
        .await;
    assert_eq!(
        data[3]["teamsConnection"]["nodes"],
        json!([{ "slug": "red", "note": "first" }, { "slug": "blue", "note": "top" }, { "slug": "green", "note": "top" }])
    );
    let mut updated = data[0]["updateTeams"]["teams"].as_array().unwrap().clone();
    updated.sort_by_key(|t| t["slug"].as_str().unwrap().to_string());
    assert_eq!(data[0]["updateTeams"]["affectedCount"], json!(2));
    assert_eq!(
        updated,
        vec![
            json!({ "slug": "blue", "note": "top" }),
            json!({ "slug": "green", "note": "top" })
        ]
    );
    assert_eq!(data[1]["deleteMembers"]["affectedCount"], json!(2));
    assert_eq!(
        data[2]["updateTeams"],
        json!({ "affectedCount": 0, "teams": [] })
    );
    assert_eq!(data[3]["members"], json!([{ "email": "c@blue" }]));
}

#[tokio::test]
async fn test_many_mutations_on_a_table_without_key() {
    let env = setup().await;
    let data = env
        .run_rolled_back(&[
            // Two column sets: two inserts in one statement
            r#"mutation { createLogLines(inputs: [{ line: "a" }, { line: "b", level: 3 }]) { affectedCount logLines { line level } } }"#,
            r#"mutation { updateLogLines(where: { level: { equalTo: 2 } }, set: { level: 5 }) { affectedCount logLines { line level } } }"#,
            r#"mutation { deleteLogLines(where: { line: { in: ["boot", "a"] } }) { affectedCount } }"#,
            r#"{ logLines { line level } }"#,
        ])
        .await;
    assert_eq!(
        data[0]["createLogLines"],
        json!({ "affectedCount": 2, "logLines": [{ "line": "a", "level": 1 }, { "line": "b", "level": 3 }] })
    );
    assert_eq!(
        data[1]["updateLogLines"],
        json!({ "affectedCount": 1, "logLines": [{ "line": "warn", "level": 5 }] })
    );
    assert_eq!(data[2]["deleteLogLines"], json!({ "affectedCount": 2 }));
    let mut rest: Vec<String> = data[3]["logLines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["line"].as_str().unwrap().to_string())
        .collect();
    rest.sort();
    assert_eq!(rest, vec!["b", "warn"]);
}

#[tokio::test]
async fn test_config_omits_bulk_and_upsert_operations() {
    let env = setup_with(
        r#"
entities:
  teams:
    omit_operations: [delete, update_many]
  codes:
    omit_operations: [upsert, create_many]
  members:
    omit_operations: [update]
"#,
    )
    .await;
    let mutation = env.type_sdl("type", "Mutation");
    for field in ["deleteTeam(", "deleteTeamBySlug(", "deleteTeams("] {
        assert!(
            !mutation.contains(field),
            "omitting delete omits every delete: {mutation}"
        );
    }
    assert!(!mutation.contains("updateTeams("), "{mutation}");
    assert!(
        mutation.contains("updateTeamBySlug(") && mutation.contains("createTeams("),
        "{mutation}"
    );
    assert!(
        mutation.contains("  createCode(input: CreateCodeInput!): CreateCodePayload!"),
        "{mutation}"
    );
    assert!(!mutation.contains("createCodes("), "{mutation}");
    assert!(
        !mutation.contains("updateMembers(") && !mutation.contains("MemberOnConflict"),
        "update covers upserts: {mutation}"
    );
    assert!(
        !env.sdl.contains("CodeOnConflict"),
        "no onConflict, no conflict types"
    );
}

#[tokio::test]
async fn test_bulk_mutation_over_http_with_variables_and_debug_sql() {
    let client = connect().await;
    ensure_fixture(&client).await;
    let catalog = CatalogInspector::new(&client)
        .inspect_schema(SCHEMA)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    let state = Arc::new(ServerState::new(
        create_pool(common::db_url(), 2).unwrap(),
        schema,
        catalog,
        SCHEMA.to_string(),
        DebugOptions {
            sql: true,
            explain: true,
        },
    ));
    let app = create_router(state);
    // Inserts, then removes the same rows, so nothing is left behind
    let body = json!({
        "query": r#"mutation($rows: [CreateTeamInput!]!) {
            created: createTeams(inputs: $rows, onConflict: { key: SLUG }) { teams { slug name } }
            deleted: deleteTeams(where: { slug: { in: ["h1", "h2"] } }) { affectedCount }
        }"#,
        "variables": { "rows": [{ "slug": "h1", "name": "H1" }, { "slug": "h2", "name": "H2" }] }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        res["data"]["created"]["teams"],
        json!([{ "slug": "h1", "name": "H1" }, { "slug": "h2", "name": "H2" }]),
        "{res}"
    );
    assert_eq!(res["data"]["deleted"]["affectedCount"], json!(2), "{res}");
    let mutations = &res["extensions"]["sql"]["mutations"];
    assert!(
        mutations[0]["writes"][0]["query"]
            .as_str()
            .unwrap()
            .contains("ON CONFLICT"),
        "{res}"
    );
    assert_eq!(
        mutations[1]["writes"],
        json!([]),
        "a delete is one statement: {res}"
    );
    assert!(
        res["extensions"]["explain"]
            .as_array()
            .is_some_and(|l| !l.is_empty()),
        "{res}"
    );
}

#[tokio::test]
async fn test_nested_relation_inputs() {
    let env = setup().await;
    let player = env.type_sdl("input", "CreatePlayerInput");
    assert!(
        player.contains("  teamId: Int\n"),
        "filled by `team`, so optional: {player}"
    );
    assert!(player.contains("  team: TeamToOneInput"), "{player}");
    assert!(player.contains("  scores: ScoreToManyInput"), "{player}");
    let team = env.type_sdl("input", "CreateTeamInput");
    assert!(team.contains("  players: PlayerToManyInput"), "{team}");
    assert!(
        team.contains("  tags: TagToManyInput"),
        "many-to-many: {team}"
    );
    let key = env.type_sdl("input", "TeamKeyInput");
    assert!(
        key.contains("  id: ID") && key.contains("  slug: String"),
        "{key}"
    );
    let to_one = env.type_sdl("input", "TeamToOneInput");
    assert!(
        to_one.contains("connect: TeamKeyInput") && to_one.contains("create: CreateTeamInput"),
        "{to_one}"
    );
    // Single-row updates nest too, with `disconnect` and `set`
    assert!(env
        .type_sdl("input", "UpdatePlayerInput")
        .contains("  team: TeamToOneUpdateInput"));
    assert!(env
        .type_sdl("input", "UpdateTeamInput")
        .contains("  tags: TagToManyUpdateInput"));
    let to_many = env.type_sdl("input", "PlayerToManyUpdateInput");
    for field in [
        "create: [CreatePlayerInput!]",
        "connect: [PlayerKeyInput!]",
        "disconnect: [PlayerKeyInput!]",
        "set: [PlayerKeyInput!]",
    ] {
        assert!(to_many.contains(field), "{field}: {to_many}");
    }
    assert!(env
        .type_sdl("input", "TeamToOneUpdateInput")
        .contains("disconnect: Boolean"));
}

#[tokio::test]
async fn test_nested_to_one_create_and_connect() {
    let env = setup().await;
    let data = env
        .run_rolled_back(&[
            // A new parent, created first
            r#"mutation { createPlayer(input: { name: "P1", team: { create: { slug: "new", name: "New" } } }) { player { name team { slug name } } } }"#,
            // An existing parent by primary key (no lookup: the foreign key is set directly)
            r#"mutation { createPlayer(input: { name: "P2", team: { connect: { id: "2" } } }) { player { name team { slug } } } }"#,
            // By a unique key (looked up)
            r#"mutation { createPlayer(input: { name: "P3", team: { connect: { slug: "green" } } }) { player { name team { slug } } } }"#,
            // Parent of a parent
            r#"mutation {
                createScore(input: { points: 7, player: { create: { name: "Deep", team: { create: { slug: "deep", name: "Deep" } } } } }) {
                    score { points player { name team { slug } } }
                }
            }"#,
            // A keyless table reads its payload from its own insert
            r#"mutation { createLogLine(input: { line: "hi", team: { connect: { slug: "red" } } }) { logLine { line team { slug } } } }"#,
        ])
        .await;
    assert_eq!(
        data[0]["createPlayer"]["player"],
        json!({ "name": "P1", "team": { "slug": "new", "name": "New" } })
    );
    assert_eq!(
        data[1]["createPlayer"]["player"],
        json!({ "name": "P2", "team": { "slug": "blue" } })
    );
    assert_eq!(
        data[2]["createPlayer"]["player"],
        json!({ "name": "P3", "team": { "slug": "green" } })
    );
    assert_eq!(
        data[3]["createScore"]["score"],
        json!({ "points": 7, "player": { "name": "Deep", "team": { "slug": "deep" } } })
    );
    assert_eq!(
        data[4]["createLogLine"]["logLine"],
        json!({ "line": "hi", "team": { "slug": "red" } })
    );
}

#[tokio::test]
async fn test_nested_to_many_and_many_to_many() {
    let env = setup().await;
    let data = env
        .run_rolled_back(&[
            r#"mutation {
                createTeam(input: {
                    slug: "x", name: "X"
                    players: {
                        create: [{ name: "a", scores: { create: [{ points: 1 }, { points: 2 }] } }, { name: "b" }]
                        connect: [{ id: 2 }]
                    }
                    tags: { create: [{ label: "fast" }], connect: [{ label: "old" }] }
                }) {
                    team { slug players { name scores { points } } tags { label } }
                }
            }"#,
            r#"{ bob: player(id: 2) { team { slug } } }"#,
        ])
        .await;
    let team = &data[0]["createTeam"]["team"];
    let mut players = team["players"].as_array().unwrap().clone();
    players.sort_by_key(|p| p["name"].as_str().unwrap().to_string());
    assert_eq!(
        players,
        vec![
            json!({ "name": "Bob", "scores": [] }),
            json!({ "name": "a", "scores": [{ "points": 1 }, { "points": 2 }] }),
            json!({ "name": "b", "scores": [] }),
        ]
    );
    let mut tags: Vec<&str> = team["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["label"].as_str().unwrap())
        .collect();
    tags.sort();
    assert_eq!(tags, vec!["fast", "old"]);
    assert_eq!(
        data[1]["bob"],
        json!({ "team": { "slug": "x" } }),
        "connecting moves the row"
    );
}

#[tokio::test]
async fn test_nested_bulk_create_batches_statements() {
    let env = setup().await;
    let query = r#"mutation {
        createTeams(inputs: [
            { slug: "a", name: "A", players: { create: [{ name: "a1" }, { name: "a2" }] } }
            { slug: "b", name: "B", players: { create: [{ name: "b1", scores: { create: { points: 3 } } }] } }
            { slug: "c", name: "C" }
        ]) {
            affectedCount
            teams { slug players { name scores { points } } }
        }
    }"#;
    // Teams, then all players, then all scores: one statement each
    let doc =
        apollo_compiler::ExecutableDocument::parse_and_validate(&env.schema, query, "q.graphql")
            .unwrap();
    let plan = QueryPlanner::new(&env.catalog, SCHEMA).plan(&doc).unwrap();
    let graphpg::compiler::CompiledOperation::Mutation(mutations) =
        graphpg::SqlCompiler::compile_operation(&plan)
    else {
        panic!("a mutation")
    };
    let writes: Vec<&str> = mutations[0]
        .writes
        .iter()
        .map(|w| w.statement.query.sql.as_str())
        .collect();
    assert_eq!(writes.len(), 3, "{writes:#?}");
    assert!(
        writes[1].contains("\"players\"") && writes[1].contains("unnest("),
        "{writes:#?}"
    );

    let data = env.run_rolled_back(&[query]).await;
    assert_eq!(
        data[0]["createTeams"],
        json!({
            "affectedCount": 3,
            "teams": [
                { "slug": "a", "players": [{ "name": "a1", "scores": [] }, { "name": "a2", "scores": [] }] },
                { "slug": "b", "players": [{ "name": "b1", "scores": [{ "points": 3 }] }] },
                { "slug": "c", "players": [] }
            ]
        })
    );
}

#[tokio::test]
async fn test_nested_create_errors() {
    let env = setup().await;
    let cases = [
        (
            r#"mutation { createPlayer(input: { name: "P", team: { connect: { slug: "nope" } } }) { player { name } } }"#,
            "`input.team.connect` matches no row to connect",
        ),
        (
            r#"mutation { createPlayer(input: { name: "P", team: { connect: { id: 1 }, create: { slug: "s", name: "S" } } }) { player { name } } }"#,
            "`input.team` must set exactly one of `connect`, `create` and `connectOrCreate`",
        ),
        (
            r#"mutation { createPlayer(input: { name: "P", teamId: 1, team: { connect: { id: 1 } } }) { player { name } } }"#,
            "`input` sets `teamId` both directly and through `team`",
        ),
        (
            r#"mutation { createPlayer(input: { name: "P" }) { player { name } } }"#,
            "`input` must set `teamId` or `team`",
        ),
        (
            r#"mutation { createPlayer(input: { name: "P", team: { connect: { name: "Red" } } }) { player { name } } }"#,
            "validation",
        ),
        (
            r#"mutation { createPlayer(input: { name: "P", team: { connect: {} } }) { player { name } } }"#,
            "`input.team.connect` must identify a `Team` by `id`, or by `slug`",
        ),
        (
            r#"mutation { updatePlayer(id: 1, input: { team: { disconnect: true } }) { player { name } } }"#,
            "`input.team.disconnect`: `teamId` is required",
        ),
        (
            r#"mutation { updateTeam(id: 1, input: { players: { disconnect: [{ id: 1 }] } }) { team { slug } } }"#,
            "`Player.teamId` is required, so rows cannot be disconnected",
        ),
        (
            r#"mutation { updateTeams(where: {}, set: { players: { connect: [{ id: 1 }] } }) { affectedCount } }"#,
            "nested relations are written by single-row updates only",
        ),
        (
            r#"mutation { updateTeam(id: 999, input: { players: { create: [{ name: "x" }] } }) { team { slug } } }"#,
            "`input` matches no `teams` row",
        ),
        (
            r#"mutation { createTeam(input: { slug: "z", name: "Z", players: { connect: [{ id: 999 }] } }) { team { slug } } }"#,
            "`input.players.connect[0]` matches no row to connect",
        ),
    ];
    for (query, expected) in cases {
        let err = env.error_rolled_back(query).await;
        assert!(err.contains(expected), "expected `{expected}` in: {err}");
    }
}

#[tokio::test]
async fn test_nested_update_to_one() {
    let env = setup().await;
    let data = env
        .run_rolled_back(&[
            r#"mutation { updateFan(id: 1, input: { team: { connect: { slug: "blue" } } }) { fan { name team { slug } } } }"#,
            r#"mutation { updateFan(id: 1, input: { team: { create: { slug: "new", name: "New" } } }) { fan { team { slug name } } } }"#,
            r#"mutation { updateFan(id: 1, input: { name: "Fay 2", team: { disconnect: true } }) { fan { name team { slug } } } }"#,
            r#"mutation { updatePlayer(id: 1, input: { team: { connect: { slug: "green" } } }) { player { name team { slug } } } }"#,
        ])
        .await;
    assert_eq!(
        data[0]["updateFan"]["fan"],
        json!({ "name": "Fay", "team": { "slug": "blue" } })
    );
    assert_eq!(
        data[1]["updateFan"]["fan"],
        json!({ "team": { "slug": "new", "name": "New" } })
    );
    assert_eq!(
        data[2]["updateFan"]["fan"],
        json!({ "name": "Fay 2", "team": null })
    );
    assert_eq!(
        data[3]["updatePlayer"]["player"],
        json!({ "name": "Rita", "team": { "slug": "green" } })
    );
}

#[tokio::test]
async fn test_nested_update_to_many_and_many_to_many() {
    let env = setup().await;
    let names = |team: &serde_json::Value, field: &str, key: &str| -> Vec<String> {
        let mut v: Vec<String> = team[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[key].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    // Rows of its own, created in the transaction: tests run concurrently, and updates of
    // the same rows in open transactions would wait on each other
    env.client.batch_execute("BEGIN").await.unwrap();
    let run = |query: String| {
        let env = &env;
        async move {
            env.try_run(&query)
                .await
                .unwrap_or_else(|e| panic!("{e}\nquery: {query}"))
        }
    };
    let created = run(r#"mutation {
        createTeam(input: { slug: "nu", name: "Nu", fans: { create: [{ name: "Fay" }, { name: "Gus" }] } }) { team { databaseId fans { databaseId name } } }
        createFan(input: { name: "Hal" }) { fan { databaseId } }
        createTag(input: { label: "nu-old" }) { tag { label } }
    }"#
    .to_string())
    .await;
    let team = created["createTeam"]["team"]["databaseId"]
        .as_i64()
        .unwrap();
    let fan_id = |name: &str| {
        created["createTeam"]["team"]["fans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap()["databaseId"]
            .as_i64()
            .unwrap()
    };
    let (fay, gus, hal) = (
        fan_id("Fay"),
        fan_id("Gus"),
        created["createFan"]["fan"]["databaseId"].as_i64().unwrap(),
    );
    let update = |input: String, selection: &str| {
        format!(
            "mutation {{ updateTeam(id: {team}, input: {input}) {{ team {{ {selection} }} }} }}"
        )
    };

    // Children: connect and disconnect move foreign keys
    let data = run(update(
        format!("{{ fans: {{ connect: [{{ id: {hal} }}], disconnect: [{{ id: {gus} }}] }} }}"),
        "fans { name }",
    ))
    .await;
    assert_eq!(
        names(&data["updateTeam"]["team"], "fans", "name"),
        ["Fay", "Hal"]
    );
    // `set` keeps exactly the listed rows, plus the ones it creates
    let data = run(update(
        format!(
            r#"{{ name: "Nu 2", fans: {{ set: [{{ id: {hal} }}], create: [{{ name: "Ivy" }}] }} }}"#
        ),
        "name fans { name }",
    ))
    .await;
    assert_eq!(data["updateTeam"]["team"]["name"], "Nu 2");
    assert_eq!(
        names(&data["updateTeam"]["team"], "fans", "name"),
        ["Hal", "Ivy"]
    );
    let data = run(format!(
        "{{ fay: fan(id: {fay}) {{ team {{ slug }} }} gus: fan(id: {gus}) {{ team {{ slug }} }} }}"
    ))
    .await;
    assert_eq!(
        data,
        json!({ "fay": { "team": null }, "gus": { "team": null } }),
        "detached, not deleted"
    );

    // Many-to-many: junction rows; connecting a related row again is a no-op
    let tags = |input: &str| update(format!("{{ tags: {input} }}"), "tags { label }");
    let data = run(tags(
        r#"{ connect: [{ label: "nu-old" }], create: [{ label: "nu-new" }] }"#,
    ))
    .await;
    assert_eq!(
        names(&data["updateTeam"]["team"], "tags", "label"),
        ["nu-new", "nu-old"]
    );
    let data = run(tags(r#"{ connect: [{ label: "nu-old" }] }"#)).await;
    assert_eq!(
        names(&data["updateTeam"]["team"], "tags", "label"),
        ["nu-new", "nu-old"]
    );
    let data = run(tags(r#"{ disconnect: [{ label: "nu-old" }] }"#)).await;
    assert_eq!(
        names(&data["updateTeam"]["team"], "tags", "label"),
        ["nu-new"]
    );
    let data = run(tags(r#"{ set: [{ label: "nu-old" }] }"#)).await;
    assert_eq!(
        names(&data["updateTeam"]["team"], "tags", "label"),
        ["nu-old"]
    );
    let data = run(r#"{ tagByLabel(label: "nu-new") { label } }"#.to_string()).await;
    assert_eq!(
        data["tagByLabel"],
        json!({ "label": "nu-new" }),
        "unlinked tags stay"
    );
    env.client.batch_execute("ROLLBACK").await.unwrap();
}

#[tokio::test]
async fn test_upsert_with_nested_writes() {
    let env = setup().await;
    // Rows of its own: tests run concurrently, and upserts of the same keys in open
    // transactions would wait on each other
    let data = env
        .run_rolled_back(&[
            r#"mutation { a: createTeam(input: { slug: "up-a", name: "A", players: { create: [{ name: "Rita" }] } }) { team { slug } }
                          b: createTeam(input: { slug: "up-b", name: "B" }) { team { slug } } }"#,
            // The conflicting row is left alone, and still gets the nested rows
            r#"mutation { createTeam(input: { slug: "up-a", name: "Ignored", players: { create: [{ name: "Xena" }] }, tags: { create: [{ label: "t1" }] } },
                          onConflict: { key: SLUG, action: NOTHING }) { team { name players { name } tags { label } } } }"#,
            // ... or updated
            r#"mutation { createTeam(input: { slug: "up-b", name: "Navy", fans: { create: [{ name: "Yan" }] } }, onConflict: { key: SLUG }) { team { name fans { name } } } }"#,
            // Several rows, conflicting or new
            r#"mutation { createTeams(inputs: [{ slug: "up-b", name: "G", players: { create: [{ name: "Zed" }] } }, { slug: "up-c", name: "C", players: { create: [{ name: "Ann" }] } }],
                          onConflict: { key: SLUG, action: NOTHING }) { teams { slug name players { name } } } }"#,
        ])
        .await;
    let mut players: Vec<&str> = data[1]["createTeam"]["team"]["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    players.sort();
    assert_eq!(data[1]["createTeam"]["team"]["name"], "A");
    assert_eq!(players, ["Rita", "Xena"]);
    assert_eq!(
        data[1]["createTeam"]["team"]["tags"],
        json!([{ "label": "t1" }])
    );
    assert_eq!(
        data[2]["createTeam"]["team"],
        json!({ "name": "Navy", "fans": [{ "name": "Yan" }] })
    );
    assert_eq!(
        data[3]["createTeams"]["teams"],
        json!([
            { "slug": "up-b", "name": "Navy", "players": [{ "name": "Zed" }] },
            { "slug": "up-c", "name": "C", "players": [{ "name": "Ann" }] }
        ])
    );
}
