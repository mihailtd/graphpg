//! Functions as fields, detected without config: rows of a table (a connection for a set),
//! values (scalars, enums, composites, arrays), columns of their own, computed columns on a
//! table's type, and `VOLATILE` functions as mutations.
//!
//! Uses a `function_fixture` schema created on demand (recreated when `FIXTURE_VERSION` changes).
mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::DatabaseCatalog;
use serde_json::json;
use tokio_postgres::NoTls;

const SCHEMA: &str = "function_fixture";
const FIXTURE_VERSION: &str = "function_fixture v3";

const FIXTURE_SQL: &str = r#"
CREATE SCHEMA function_fixture;
SET LOCAL search_path = function_fixture;

CREATE TYPE mood AS ENUM ('happy', 'grumpy');
CREATE TYPE point2 AS (x int, y int);
CREATE TABLE teams (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE members (
    id int PRIMARY KEY,
    team_id int REFERENCES teams(id),
    name text NOT NULL,
    mood mood NOT NULL DEFAULT 'happy',
    score int NOT NULL DEFAULT 0
);
INSERT INTO teams VALUES (1, 'Red'), (2, 'Blue');
INSERT INTO members VALUES (1, 1, 'Ada', 'happy', 5), (2, 1, 'Bob', 'grumpy', 3), (3, 2, 'Cyd', 'happy', 8), (4, 2, 'Dana', 'grumpy', 1);

-- Rows of a table: a set (in the function's own order), one row, none, an array
CREATE FUNCTION search_members(q text) RETURNS SETOF members LANGUAGE sql STABLE
    AS $$ SELECT * FROM function_fixture.members WHERE name ILIKE '%' || q || '%' ORDER BY score DESC $$;
COMMENT ON FUNCTION search_members(text) IS 'Members by name, best first';
CREATE FUNCTION best_member() RETURNS members LANGUAGE sql STABLE AS $$ SELECT * FROM function_fixture.members ORDER BY score DESC LIMIT 1 $$;
CREATE FUNCTION nobody() RETURNS members LANGUAGE sql STABLE AS $$ SELECT * FROM function_fixture.members WHERE false $$;
CREATE FUNCTION members_by_ids(ids int[]) RETURNS members[] LANGUAGE sql STABLE
    AS $$ SELECT array_agg(m ORDER BY array_position(ids, m.id)) FROM function_fixture.members m WHERE m.id = ANY(ids) $$;

-- Values
CREATE FUNCTION add(a int, b int DEFAULT 10) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT a + b $$;
CREATE FUNCTION big() RETURNS bigint LANGUAGE sql IMMUTABLE AS $$ SELECT 9007199254740993 $$;
CREATE FUNCTION moods() RETURNS SETOF mood LANGUAGE sql STABLE AS $$ SELECT unnest(enum_range(NULL::function_fixture.mood)) $$;
CREATE FUNCTION mood_of(p_mood mood) RETURNS mood LANGUAGE sql IMMUTABLE AS $$ SELECT p_mood $$;
CREATE FUNCTION origin() RETURNS point2 LANGUAGE sql IMMUTABLE AS $$ SELECT ROW(0, 1)::function_fixture.point2 $$;
CREATE FUNCTION tags() RETURNS text[] LANGUAGE sql IMMUTABLE AS $$ SELECT ARRAY['a', 'b'] $$;
CREATE FUNCTION nothing() RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT NULL::int $$;
CREATE FUNCTION manhattan(p point2) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT abs(p.x) + abs(p.y) $$;
-- Columns of its own, one row
CREATE FUNCTION stats(OUT total int, OUT top text) LANGUAGE sql STABLE AS $$ SELECT count(*)::int, max(name) FROM function_fixture.members $$;

-- Computed columns: functions of a row
CREATE FUNCTION teams_ranked(t teams) RETURNS SETOF members LANGUAGE sql STABLE
    AS $$ SELECT * FROM function_fixture.members WHERE team_id = t.id ORDER BY score DESC $$;
CREATE FUNCTION teams_captain(t teams) RETURNS members LANGUAGE sql STABLE
    AS $$ SELECT * FROM function_fixture.members WHERE team_id = t.id ORDER BY score DESC LIMIT 1 $$;
CREATE FUNCTION members_greeting(m members, greeting text DEFAULT 'Hi') RETURNS text LANGUAGE sql STABLE
    AS $$ SELECT greeting || ' ' || m.name $$;

-- VOLATILE functions: mutations
CREATE FUNCTION bump_score(member_id int, by int DEFAULT 1) RETURNS members LANGUAGE sql
    AS $$ UPDATE function_fixture.members SET score = score + by WHERE id = member_id RETURNING * $$;
CREATE FUNCTION team_with_member(team text, member text) RETURNS teams LANGUAGE sql AS $$
    WITH t AS (INSERT INTO function_fixture.teams SELECT max(id) + 1, team FROM function_fixture.teams RETURNING *),
         m AS (INSERT INTO function_fixture.members (id, team_id, name) SELECT 100, t.id, member FROM t)
    SELECT * FROM t
$$;
CREATE FUNCTION bump_team(team int) RETURNS SETOF members LANGUAGE sql
    AS $$ UPDATE function_fixture.members SET score = score + 10 WHERE team_id = team RETURNING * $$;
CREATE FUNCTION reset_scores() RETURNS void LANGUAGE sql AS $$ UPDATE function_fixture.members SET score = 0 $$;
CREATE FUNCTION total_score() RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT sum(score)::int FROM function_fixture.members $$;

-- Not exposed: triggers, polymorphic functions, procedures; of overloads only the first
CREATE FUNCTION tg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE FUNCTION anything(x anyelement) RETURNS anyelement LANGUAGE sql IMMUTABLE AS $$ SELECT x $$;
CREATE PROCEDURE proc() LANGUAGE sql AS $$ SELECT 1 $$;
CREATE FUNCTION twice(a int) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT 2 * a $$;
CREATE FUNCTION twice(a text) RETURNS text LANGUAGE sql IMMUTABLE AS $$ SELECT a || a $$;
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

    /// Runs `query` in a transaction that is rolled back afterwards.
    async fn run_rolled_back(&self, query: &str) -> serde_json::Value {
        self.client.batch_execute("BEGIN").await.unwrap();
        let result = self.try_run(query).await;
        self.client.batch_execute("ROLLBACK").await.unwrap();
        result.unwrap_or_else(|e| panic!("{e}\nquery: {query}"))
    }

    /// SDL block of type `name`.
    fn type_sdl(&self, name: &str) -> String {
        let start = self
            .sdl
            .find(&format!("type {name} "))
            .unwrap_or_else(|| panic!("no type {name} in SDL:\n{}", self.sdl));
        let end = self.sdl[start..]
            .find("\n}")
            .map(|e| start + e + 1)
            .unwrap_or(self.sdl.len());
        self.sdl[start..end].to_string()
    }
}

#[tokio::test]
async fn test_function_fields_are_typed_by_what_they_return() {
    let env = setup().await;
    let query = env.type_sdl("Query");
    let conn = "first: Int, after: String, last: Int, before: String, offset: Int, condition: MemberCondition, filter: MemberFilter, orderBy: [MemberOrderBy!]";
    for field in [
        format!("  searchMembers(q: String!, {conn}): MemberConnection!"),
        format!("  membersByIds(ids: [Int]!, {conn}): MemberConnection!"),
        "  bestMember: Member".to_string(),
        "  add(a: Int!, b: Int): Int".to_string(),
        "  big: BigInt".to_string(),
        "  moods: [Mood]!".to_string(),
        "  moodOf(pMood: Mood!): Mood".to_string(),
        "  origin: Point2".to_string(),
        "  tags: [String]".to_string(),
        "  stats: StatsRow".to_string(),
        "  manhattan(p: Point2Input!): Int".to_string(),
        "  twice(a: Int!): Int".to_string(),
    ] {
        assert!(query.contains(&format!("{field}\n")), "{field}\n{query}");
    }
    assert!(
        query.contains("\"\"\"Members by name, best first\"\"\""),
        "{query}"
    );
    for absent in [
        "tg",
        "anything",
        "proc",
        "bumpScore",
        "resetScores",
        "ranked",
        "captain",
        "greeting",
    ] {
        assert!(!query.contains(&format!("  {absent}")), "{absent}: {query}");
    }

    let team = env.type_sdl("Team");
    assert!(
        team.contains(&format!("  ranked({conn}): MemberConnection!")),
        "{team}"
    );
    assert!(team.contains("  captain: Member\n"), "{team}");
    assert!(env
        .type_sdl("Member")
        .contains("  greeting(greeting: String): String"));

    let mutation = env.type_sdl("Mutation");
    for field in [
        "  bumpScore(memberId: ID!, by: Int): Member",
        "  teamWithMember(team: String!, member: String!): Team",
        "  bumpTeam(team: Int!): [Member!]!",
        "  resetScores: Boolean",
        "  totalScore: Int",
    ] {
        assert!(
            mutation.contains(&format!("{field}\n")),
            "{field}\n{mutation}"
        );
    }
}

#[tokio::test]
async fn test_functions_returning_rows() {
    let env = setup().await;
    let names = |conn: &serde_json::Value| -> Vec<String> {
        conn["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().to_string())
            .collect()
    };

    // A set of rows is a connection, in the function's order unless `orderBy` says otherwise
    let data = env
        .run(
            r#"{
                all: searchMembers(q: "") { totalCount nodes { name team { name } } }
                page: searchMembers(q: "", first: 2) { nodes { name } pageInfo { hasNextPage endCursor } }
                last: searchMembers(q: "", last: 1) { nodes { name } }
                byName: searchMembers(q: "", orderBy: [NAME_DESC]) { nodes { name } }
                filtered: searchMembers(q: "a", filter: { score: { greaterThan: 2 } }) { nodes { name } }
                condition: searchMembers(q: "", condition: { mood: GRUMPY }) { nodes { name mood } }
                byIds: membersByIds(ids: [3, 1]) { nodes { name } }
            }"#,
        )
        .await;
    assert_eq!(data["all"]["totalCount"], 4);
    assert_eq!(names(&data["all"]), ["Cyd", "Ada", "Bob", "Dana"]);
    assert_eq!(data["all"]["nodes"][0]["team"], json!({ "name": "Blue" }));
    assert_eq!(names(&data["page"]), ["Cyd", "Ada"]);
    assert_eq!(data["page"]["pageInfo"]["hasNextPage"], true);
    assert_eq!(names(&data["last"]), ["Dana"]);
    assert_eq!(names(&data["byName"]), ["Dana", "Cyd", "Bob", "Ada"]);
    assert_eq!(names(&data["filtered"]), ["Ada"]);
    assert_eq!(
        data["condition"]["nodes"],
        json!([{ "name": "Bob", "mood": "GRUMPY" }, { "name": "Dana", "mood": "GRUMPY" }])
    );
    assert_eq!(names(&data["byIds"]), ["Cyd", "Ada"]);

    // Cursors page through the function's order
    let cursor = data["page"]["pageInfo"]["endCursor"].as_str().unwrap();
    let next = env
        .run(&format!(
            r#"{{ searchMembers(q: "", first: 2, after: "{cursor}") {{ nodes {{ name }} }} }}"#
        ))
        .await;
    assert_eq!(names(&next["searchMembers"]), ["Bob", "Dana"]);

    // One row, or null when the function returns none
    let data = env
        .run("{ bestMember { name team { name } } nobody { name } }")
        .await;
    assert_eq!(
        data["bestMember"],
        json!({ "name": "Cyd", "team": { "name": "Blue" } })
    );
    assert_eq!(data["nobody"], json!(null));
}

#[tokio::test]
async fn test_functions_returning_values() {
    let env = setup().await;
    let data = env
        .run(
            r#"{
                add(a: 1) three: add(a: 1, b: 2) big moods moodOf(pMood: GRUMPY)
                origin { x y } tags nothing stats { total top } twice(a: 4)
                manhattan(p: { x: -2, y: 3 })
            }"#,
        )
        .await;
    assert_eq!(
        data,
        json!({
            "add": 11, "three": 3, "big": "9007199254740993", "moods": ["HAPPY", "GRUMPY"], "moodOf": "GRUMPY",
            "origin": { "x": 0, "y": 1 }, "tags": ["a", "b"], "nothing": null, "stats": { "total": 4, "top": "Dana" },
            "twice": 8, "manhattan": 5
        })
    );
}

#[tokio::test]
async fn test_computed_columns_take_the_row() {
    let env = setup().await;
    let data = env
        .run(
            r#"{
                team(id: 1) {
                    captain { name }
                    ranked { totalCount nodes { name greeting } }
                    top: ranked(first: 1, orderBy: [NAME_DESC]) { nodes { name } }
                }
                searchMembers(q: "Cyd") { nodes { greeting yo: greeting(greeting: "Yo") } }
            }"#,
        )
        .await;
    assert_eq!(
        data["team"],
        json!({
            "captain": { "name": "Ada" },
            "ranked": { "totalCount": 2, "nodes": [{ "name": "Ada", "greeting": "Hi Ada" }, { "name": "Bob", "greeting": "Hi Bob" }] },
            "top": { "nodes": [{ "name": "Bob" }] }
        })
    );
    // The row passed in is the member's, even where rows carry extra columns (`__ord`)
    assert_eq!(
        data["searchMembers"]["nodes"],
        json!([{ "greeting": "Hi Cyd", "yo": "Yo Cyd" }])
    );
}

#[tokio::test]
async fn test_volatile_functions_are_mutations() {
    let env = setup().await;
    // Relay IDs work for key-like arguments
    let ada = base64_encode("Member:1");
    let data = env
        .run_rolled_back(&format!(
            r#"mutation {{
                bumpScore(memberId: "{ada}", by: 2) {{ name score team {{ name }} }}
                teamWithMember(team: "Green", member: "Eve") {{ name members {{ name }} }}
                bumpTeam(team: 2) {{ name score team {{ name }} }}
                before: totalScore
                resetScores
                after: totalScore
            }}"#
        ))
        .await;
    assert_eq!(
        data["bumpScore"],
        json!({ "name": "Ada", "score": 7, "team": { "name": "Red" } })
    );
    // Rows are read back after the function ran, so their relations see its writes
    assert_eq!(
        data["teamWithMember"],
        json!({ "name": "Green", "members": [{ "name": "Eve" }] })
    );
    assert_eq!(
        data["bumpTeam"],
        json!([{ "name": "Cyd", "score": 18, "team": { "name": "Blue" } }, { "name": "Dana", "score": 11, "team": { "name": "Blue" } }])
    );
    assert_eq!(data["before"], 39);
    assert_eq!(data["resetScores"], true);
    assert_eq!(data["after"], 0);
    let data = env.run("{ member(id: 1) { score } }").await;
    assert_eq!(data["member"]["score"], 5, "rolled back");
}

#[tokio::test]
async fn test_config_renames_and_omits_functions() {
    let env = setup_with(
        r#"
functions:
  search_members: { field_name: findMembers, description: Configured }
  best_member: { omit: true }
  teams_captain: { field_name: leader }
  members_greeting: { omit: true }
  bump_score: { field_name: bump }
"#,
    )
    .await;
    let query = env.type_sdl("Query");
    assert!(
        query.contains("\"\"\"Configured\"\"\"\n  findMembers(q: String!"),
        "{query}"
    );
    assert!(
        !query.contains("searchMembers") && !query.contains("bestMember"),
        "{query}"
    );
    assert!(env.type_sdl("Team").contains("  leader: Member"));
    assert!(!env.type_sdl("Member").contains("greeting"));
    assert!(env.type_sdl("Mutation").contains("  bump(memberId: ID!"));
    let data = env.run("{ team(id: 2) { leader { name } } }").await;
    assert_eq!(data["team"]["leader"]["name"], "Cyd");
}

fn base64_encode(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text)
}
