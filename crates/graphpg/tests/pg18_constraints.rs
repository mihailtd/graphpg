//! PostgreSQL 18 and validity: `NOT ENFORCED` constraints and virtual generated columns, and
//! `NOT VALID` checks. graphpg derives enums, input rules, descriptions and error codes from
//! constraints; only what the database checks for every row may feed them.
mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use tokio_postgres::NoTls;

const FIXTURE: &str = r#"
DROP SCHEMA IF EXISTS pg18_constraints CASCADE;
CREATE SCHEMA pg18_constraints;
SET search_path = pg18_constraints;
CREATE TABLE teams (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE players (
    id int PRIMARY KEY,
    -- checked: an enum, an input rule and a code
    position text NOT NULL CHECK (position IN ('keeper', 'field')),
    -- not enforced: none of these
    tier text NOT NULL CONSTRAINT players_tier_check CHECK (tier IN ('gold', 'silver')) NOT ENFORCED,
    score int NOT NULL CONSTRAINT players_score_check CHECK (score >= 0) NOT ENFORCED,
    team_id int CONSTRAINT players_team_fkey REFERENCES teams (id) NOT ENFORCED,
    -- virtual (the default kind in PostgreSQL 18): read, never written
    label text GENERATED ALWAYS AS (upper(position)) VIRTUAL
);
-- Older rows may break a NOT VALID check: no enum
ALTER TABLE players ADD COLUMN mood text;
INSERT INTO teams VALUES (1, 'Reds');
INSERT INTO players (id, position, tier, score, team_id, mood) VALUES (1, 'keeper', 'bronze', -5, 99, 'grumpy');
ALTER TABLE players ADD CONSTRAINT players_mood_check CHECK (mood IN ('happy', 'sad')) NOT VALID;
"#;

async fn sdl() -> String {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute("SELECT pg_advisory_lock(7461018)")
        .await
        .unwrap();
    client.batch_execute(FIXTURE).await.unwrap();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("pg18_constraints")
        .await
        .unwrap();
    client
        .batch_execute("SELECT pg_advisory_unlock(7461018)")
        .await
        .unwrap();
    SchemaBuilder::build(&catalog).unwrap();
    SchemaBuilder::generate_sdl(&catalog)
}

fn block<'s>(sdl: &'s str, header: &str) -> &'s str {
    let start = sdl
        .find(header)
        .unwrap_or_else(|| panic!("no `{header}` in:\n{sdl}"));
    let end = sdl[start..].find("\n}").map_or(sdl.len(), |e| start + e);
    &sdl[start..end]
}

#[tokio::test]
async fn only_enforced_valid_constraints_shape_the_schema() {
    let sdl = sdl().await;
    let player = block(&sdl, "type Player ");
    // An enforced, validated value list is an enum; the others stay text
    assert!(player.contains("position: PlayerPosition!"), "{player}");
    assert!(player.contains("tier: String!"), "{player}");
    assert!(player.contains("mood: String"), "{player}");
    assert!(!sdl.contains("enum PlayerTier"), "{sdl}");
    assert!(!sdl.contains("enum PlayerMood"), "{sdl}");

    // No input rule from an unenforced check
    let input = block(&sdl, "input CreatePlayerInput");
    let score = input.lines().find(|l| l.contains("score:")).unwrap();
    assert!(!score.contains("@constraint"), "{score}");
    // A virtual generated column is read, never written
    assert!(player.contains("label: String"), "{player}");
    assert!(!input.contains("label"), "{input}");

    // An unenforced foreign key is still a relation, but no write fails on it
    assert!(player.contains("team: Team"), "{player}");
    let codes = block(&sdl, "enum PlayerErrorCode");
    assert!(codes.contains("POSITION_INVALID"), "{codes}");
    for absent in ["TIER_INVALID", "SCORE_INVALID", "TEAM_NOT_FOUND"] {
        assert!(!codes.contains(absent), "{absent} in {codes}");
    }
}
