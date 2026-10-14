//! Many-to-many relations through a junction table that can link the same two rows more than
//! once (a person with two roles in one team): the related rows are a set, so each appears
//! once in lists, counts, pages and relation filters.
mod common;
use axum::body::Body;
use axum::http::{header, Request};
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const FIXTURE: &str = r#"
DROP SCHEMA IF EXISTS m2m_fixture CASCADE;
CREATE SCHEMA m2m_fixture;
CREATE TABLE m2m_fixture.teams (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE m2m_fixture.people (id int PRIMARY KEY, name text NOT NULL);
-- Its own key: the same person can hold several roles in one team
CREATE TABLE m2m_fixture.memberships (
    id serial PRIMARY KEY,
    team_id int NOT NULL REFERENCES m2m_fixture.teams,
    person_id int NOT NULL REFERENCES m2m_fixture.people,
    role text NOT NULL
);
CREATE INDEX ON m2m_fixture.memberships (team_id);
CREATE INDEX ON m2m_fixture.memberships (person_id);
INSERT INTO m2m_fixture.teams VALUES (1, 'core'), (2, 'empty');
INSERT INTO m2m_fixture.people VALUES (1, 'ada'), (2, 'bob'), (3, 'cy'), (4, 'dee');
INSERT INTO m2m_fixture.memberships (team_id, person_id, role) VALUES
    (1, 1, 'lead'), (1, 1, 'reviewer'), (1, 2, 'dev'), (1, 3, 'dev'), (1, 3, 'ops');
"#;

static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn app() -> axum::Router {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("connect to the test database");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    INIT.get_or_init(|| async {
        client
            .batch_execute(&format!(
                "SELECT pg_advisory_lock(7461001); {FIXTURE} SELECT pg_advisory_unlock(7461001);"
            ))
            .await
            .expect("m2m fixture");
    })
    .await;
    let config = Config {
        database: graphpg::config::DatabaseConfig {
            schemas: vec!["m2m_fixture".to_string()],
            ..Default::default()
        },
        ..Config::default()
    };
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config("m2m_fixture", &config)
        .await
        .expect("inspect m2m_fixture");
    let schema = SchemaBuilder::build(&catalog).expect("schema");
    let state = ServerState::new(
        create_pool(common::db_url(), 4).expect("pool"),
        schema,
        catalog,
        "m2m_fixture".to_string(),
        Default::default(),
    )
    .with_config(config);
    create_router(Arc::new(state))
}

async fn graphql(app: &axum::Router, query: &str) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "query": query }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body.get("errors").is_none(), "{body}");
    body["data"].clone()
}

fn ids(nodes: &Value) -> Vec<i64> {
    nodes
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["databaseId"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn related_rows_appear_once_however_many_junction_rows_link_them() {
    let app = app().await;
    let data = graphql(
        &app,
        "{ team(id: 1) {
            peopleViaMemberships { databaseId }
            peopleViaMembershipsConnection(orderBy: [ID_ASC]) { totalCount nodes { databaseId } }
        } }",
    )
    .await;
    let mut list = ids(&data["team"]["peopleViaMemberships"]);
    list.sort();
    assert_eq!(list, vec![1, 2, 3]);
    let connection = &data["team"]["peopleViaMembershipsConnection"];
    assert_eq!(connection["totalCount"], 3);
    assert_eq!(ids(&connection["nodes"]), vec![1, 2, 3]);

    // The other way round
    let data = graphql(
        &app,
        "{ person(id: 1) { teamsViaMembershipsConnection { totalCount nodes { databaseId } } } }",
    )
    .await;
    let teams = &data["person"]["teamsViaMembershipsConnection"];
    assert_eq!(teams["totalCount"], 1);
    assert_eq!(ids(&teams["nodes"]), vec![1]);
}

#[tokio::test]
async fn pages_of_a_many_to_many_connection_add_up_to_its_total() {
    let app = app().await;
    let mut seen = Vec::new();
    let mut after = String::new();
    loop {
        let data = graphql(
            &app,
            &format!(
                "{{ team(id: 1) {{ peopleViaMembershipsConnection(first: 1, orderBy: [ID_ASC]{after}) {{
                    totalCount pageInfo {{ hasNextPage endCursor }} nodes {{ databaseId }}
                }} }} }}"
            ),
        )
        .await;
        let page = &data["team"]["peopleViaMembershipsConnection"];
        assert_eq!(page["totalCount"], 3);
        seen.extend(ids(&page["nodes"]));
        if page["pageInfo"]["hasNextPage"] != true {
            break;
        }
        after = format!(", after: {}", page["pageInfo"]["endCursor"]);
        assert!(seen.len() <= 3, "paging does not end: {seen:?}");
    }
    assert_eq!(seen, vec![1, 2, 3]);
}

#[tokio::test]
async fn filters_through_a_many_to_many_relation_count_each_row_once() {
    let app = app().await;
    let data = graphql(
        &app,
        "{ teamsConnection(filter: { peopleViaMemberships: { count: { equalTo: 3 } } }) { nodes { databaseId } } }",
    )
    .await;
    assert_eq!(ids(&data["teamsConnection"]["nodes"]), vec![1]);
    let data = graphql(
        &app,
        "{ teamsConnection(filter: { peopleViaMemberships: { some: { name: { equalTo: \"ada\" } } } }) { nodes { databaseId } } }",
    )
    .await;
    assert_eq!(ids(&data["teamsConnection"]["nodes"]), vec![1]);
    let data = graphql(
        &app,
        "{ teamsConnection(filter: { peopleViaMemberships: { none: {} } }, orderBy: [ID_ASC]) { nodes { databaseId } } }",
    )
    .await;
    assert_eq!(ids(&data["teamsConnection"]["nodes"]), vec![2]);
}
