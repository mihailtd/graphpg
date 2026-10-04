//! Relational expressiveness: `some` / `every` / `none` and aggregate filters on to-many
//! relations, ordering by related rows' columns, related row counts and computed columns
//! (with keyset pagination through them), and hierarchies — `ancestors` / `descendants`
//! from foreign keys referencing their own table (recursive, cycle-safe) and from `ltree`
//! paths.
//!
//! Uses a `relational_fixture` schema created on demand; the cycle test runs in a
//! rolled-back transaction.
mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::DatabaseCatalog;
use serde_json::json;
use tokio_postgres::NoTls;

const SCHEMA: &str = "relational_fixture";
const FIXTURE_VERSION: &str = "relational_fixture v1";

const FIXTURE_SQL: &str = r#"
CREATE EXTENSION IF NOT EXISTS ltree;
CREATE SCHEMA relational_fixture;
SET search_path = relational_fixture, public;

CREATE TABLE authors (id serial PRIMARY KEY, name text NOT NULL);
CREATE TABLE forums (id serial PRIMARY KEY, name text NOT NULL, owner_id int REFERENCES authors(id));
CREATE TABLE posts (
    id serial PRIMARY KEY,
    forum_id int NOT NULL REFERENCES forums(id),
    author_id int REFERENCES authors(id),
    score int NOT NULL,
    flagged boolean
);
CREATE TABLE tags (id serial PRIMARY KEY, label text NOT NULL);
CREATE TABLE forum_tags (forum_id int REFERENCES forums(id), tag_id int REFERENCES tags(id), PRIMARY KEY (forum_id, tag_id));
CREATE FUNCTION forums_post_total(f forums) RETURNS bigint LANGUAGE sql STABLE
    AS $$ SELECT coalesce(sum(score), 0) FROM relational_fixture.posts WHERE forum_id = f.id $$;

-- Two hierarchies over one table
CREATE TABLE staff (
    id int PRIMARY KEY,
    manager_id int REFERENCES staff(id),
    mentor_id int REFERENCES staff(id),
    name text NOT NULL
);
CREATE TABLE categories (id serial PRIMARY KEY, path ltree NOT NULL UNIQUE, name text NOT NULL);

INSERT INTO authors (name) VALUES ('Ada'), ('Bob'), ('Cy');
INSERT INTO forums (name, owner_id) VALUES ('Rust', 2), ('Go', 1), ('Empty', NULL);
INSERT INTO posts (forum_id, author_id, score, flagged) VALUES
    (1, 1, 10, false), (1, 2, 3, NULL), (1, 1, 7, false), (2, 2, 1, true);
INSERT INTO tags (label) VALUES ('lang'), ('fast');
INSERT INTO forum_tags VALUES (1, 1), (1, 2), (2, 1);
INSERT INTO staff VALUES (1, NULL, NULL, 'CEO'), (2, 1, NULL, 'CTO'), (3, 2, 1, 'Dev'), (4, 3, 3, 'Intern');
INSERT INTO categories (path, name) VALUES
    ('top', 'Top'), ('top.science', 'Science'), ('top.science.astronomy', 'Astronomy'), ('top.arts', 'Arts');
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
        .execute("SELECT pg_advisory_lock(424248)", &[])
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
        .execute("SELECT pg_advisory_unlock(424248)", &[])
        .await
        .unwrap();
}

struct Env {
    client: tokio_postgres::Client,
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: DatabaseCatalog,
    sdl: String,
}

async fn setup() -> Env {
    let client = connect().await;
    ensure_fixture(&client).await;
    let catalog = CatalogInspector::new(&client)
        .inspect_schema(SCHEMA)
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

impl Env {
    async fn run(&self, query: &str) -> serde_json::Value {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &self.schema,
            query,
            "q.graphql",
        )
        .unwrap_or_else(|e| panic!("validation: {e}\nquery: {query}"));
        let plan = QueryPlanner::new(&self.catalog, SCHEMA)
            .plan(&doc)
            .unwrap_or_else(|e| panic!("planning: {e}"));
        let result = ExecutionEngine::execute(&self.client, &plan)
            .await
            .unwrap_or_else(|e| panic!("execution: {e}\nquery: {query}"));
        result["data"].clone()
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
}

/// `name`s of a connection's nodes.
fn names(data: &serde_json::Value, key: &str) -> Vec<String> {
    data[key]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("no {key} in {data}"))
        .iter()
        .map(|n| n["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn test_to_many_filters() {
    let env = setup().await;
    let filter = env.block("input", "PostToManyFilter");
    for field in [
        "some: PostFilter",
        "every: PostFilter",
        "none: PostFilter",
        "count: IntFilter",
        "sum: PostHavingNumbersInput",
    ] {
        assert!(filter.contains(field), "{field} missing:\n{filter}");
    }
    assert!(env
        .block("input", "ForumFilter")
        .contains("  posts: PostToManyFilter\n"));
    assert!(
        env.block("input", "ForumFilter")
            .contains("  tags: TagToManyFilter\n"),
        "many-to-many too"
    );

    let data = env
        .run(
            r#"{
                some: forumsConnection(filter: { posts: { some: { score: { greaterThan: 5 } } } }) { nodes { name } }
                every: forumsConnection(filter: { posts: { every: { flagged: { equalTo: false } } } }) { nodes { name } }
                none: forumsConnection(filter: { posts: { none: { score: { greaterThan: 5 } } } }) { nodes { name } }
                count: forumsConnection(filter: { posts: { count: { greaterThanOrEqualTo: 2 } } }) { nodes { name } }
                zero: forumsConnection(filter: { posts: { count: { equalTo: 0 } } }) { nodes { name } }
                sum: forumsConnection(filter: { posts: { sum: { score: { greaterThan: 15 } } } }) { nodes { name } }
                both: forumsConnection(filter: { posts: { some: { authorId: { equalTo: 2 } }, max: { score: { lessThan: 5 } } } }) { nodes { name } }
                tagged: forumsConnection(filter: { tags: { some: { label: { equalTo: "fast" } } } }) { nodes { name } }
                untagged: forumsConnection(filter: { tags: { none: {} } }) { nodes { name } }
                nested: authorsConnection(filter: { posts: { some: { forum: { name: { equalTo: "Go" } } } } }) { nodes { name } }
            }"#,
        )
        .await;
    assert_eq!(names(&data, "some"), ["Rust"]);
    // A NULL `flagged` is not `false`: Rust's posts are not all unflagged; Empty has no posts
    assert_eq!(names(&data, "every"), ["Empty"]);
    assert_eq!(names(&data, "none"), ["Go", "Empty"]);
    assert_eq!(names(&data, "count"), ["Rust"]);
    assert_eq!(names(&data, "zero"), ["Empty"]);
    assert_eq!(names(&data, "sum"), ["Rust"]);
    assert_eq!(names(&data, "both"), ["Go"]);
    assert_eq!(names(&data, "tagged"), ["Rust"]);
    assert_eq!(names(&data, "untagged"), ["Empty"]);
    assert_eq!(names(&data, "nested"), ["Bob"]);
}

#[tokio::test]
async fn test_order_by_relations_counts_and_computed_columns() {
    let env = setup().await;
    let order = env.block("enum", "ForumOrderBy");
    for value in [
        "OWNER_NAME_ASC",
        "POSTS_COUNT_DESC",
        "POST_TOTAL_DESC",
        "TAGS_COUNT_ASC",
    ] {
        assert!(
            order.contains(&format!("  {value}\n")),
            "{value} missing:\n{order}"
        );
    }
    let data = env
        .run(
            r#"{
                byCount: forumsConnection(orderBy: [POSTS_COUNT_DESC]) { nodes { name } }
                byOwner: forumsConnection(orderBy: [OWNER_NAME_ASC]) { nodes { name } }
                byOwnerDesc: forumsConnection(orderBy: [OWNER_NAME_DESC]) { nodes { name } }
                byTotal: forumsConnection(orderBy: [POST_TOTAL_DESC]) { nodes { name postTotal } }
                byTags: forumsConnection(orderBy: [TAGS_COUNT_DESC, NAME_ASC]) { nodes { name } }
                posts: postsConnection(orderBy: [FORUM_NAME_ASC, SCORE_DESC]) { nodes { score } }
            }"#,
        )
        .await;
    assert_eq!(names(&data, "byCount"), ["Rust", "Go", "Empty"]);
    // Rows without a related row sort last ascending, first descending
    assert_eq!(names(&data, "byOwner"), ["Go", "Rust", "Empty"]);
    assert_eq!(names(&data, "byOwnerDesc"), ["Empty", "Rust", "Go"]);
    assert_eq!(
        data["byTotal"]["nodes"],
        json!([{ "name": "Rust", "postTotal": "20" }, { "name": "Go", "postTotal": "1" }, { "name": "Empty", "postTotal": "0" }])
    );
    assert_eq!(names(&data, "byTags"), ["Rust", "Go", "Empty"]);
    assert_eq!(
        data["posts"]["nodes"],
        json!([{ "score": 1 }, { "score": 10 }, { "score": 7 }, { "score": 3 }])
    );

    // Cursors carry the sorted values: pages through an expression order, both ways
    for (order, expected) in [
        ("POSTS_COUNT_DESC", ["Rust", "Go", "Empty"]),
        ("OWNER_NAME_ASC", ["Go", "Rust", "Empty"]),
        ("OWNER_NAME_DESC", ["Empty", "Rust", "Go"]),
        ("POST_TOTAL_ASC", ["Empty", "Go", "Rust"]),
    ] {
        let mut seen = Vec::new();
        let mut after = String::new();
        loop {
            let page = env.run(&format!("{{ forumsConnection(first: 1, orderBy: [{order}]{after}) {{ nodes {{ name }} pageInfo {{ hasNextPage endCursor }} }} }}")).await;
            let page = &page["forumsConnection"];
            seen.extend(
                page["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n["name"].as_str().unwrap().to_string()),
            );
            if !page["pageInfo"]["hasNextPage"].as_bool().unwrap() {
                break;
            }
            after = format!(
                r#", after: "{}""#,
                page["pageInfo"]["endCursor"].as_str().unwrap()
            );
        }
        assert_eq!(seen, expected, "forward through {order}");
        let back = env
            .run(&format!(
                "{{ forumsConnection(last: 2, orderBy: [{order}]) {{ nodes {{ name }} }} }}"
            ))
            .await;
        assert_eq!(
            names(&back, "forumsConnection"),
            expected[1..],
            "last page of {order}"
        );
    }
}

#[tokio::test]
async fn test_hierarchies_from_self_references() {
    let env = setup().await;
    let staff = env.block("type", "Staff");
    for field in [
        "  ancestors(",
        "  descendants(",
        "  mentorAncestors(",
        "  mentorDescendants(",
    ] {
        assert!(staff.contains(field), "{field} missing:\n{staff}");
    }
    let data = env
        .run(
            r#"{
                intern: staffById(id: 4) { ancestors { name } mentorAncestors { name } }
                ceo: staffById(id: 1) {
                    descendants { name }
                    near: descendants(maxDepth: 1) { name }
                    interns: descendants(filter: { name: { startsWith: "I" } }) { name }
                    mentored: mentorDescendants { name ancestors { name } }
                }
            }"#,
        )
        .await;
    assert_eq!(
        data["intern"]["ancestors"],
        json!([{ "name": "Dev" }, { "name": "CTO" }, { "name": "CEO" }])
    );
    assert_eq!(
        data["intern"]["mentorAncestors"],
        json!([{ "name": "Dev" }, { "name": "CEO" }])
    );
    assert_eq!(
        data["ceo"]["descendants"],
        json!([{ "name": "CTO" }, { "name": "Dev" }, { "name": "Intern" }])
    );
    assert_eq!(data["ceo"]["near"], json!([{ "name": "CTO" }]));
    assert_eq!(data["ceo"]["interns"], json!([{ "name": "Intern" }]));
    assert_eq!(
        data["ceo"]["mentored"],
        json!([
            { "name": "Dev", "ancestors": [{ "name": "CTO" }, { "name": "CEO" }] },
            { "name": "Intern", "ancestors": [{ "name": "Dev" }, { "name": "CTO" }, { "name": "CEO" }] },
        ])
    );

    // A cycle ends the walk instead of looping
    env.client
        .batch_execute("BEGIN; UPDATE relational_fixture.staff SET manager_id = 4 WHERE id = 1")
        .await
        .unwrap();
    let cyclic = env
        .run("{ staff: staffById(id: 1) { descendants { name } ancestors { name } } }")
        .await;
    env.client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(
        cyclic["staff"]["descendants"],
        json!([{ "name": "CTO" }, { "name": "Dev" }, { "name": "Intern" }])
    );
    assert_eq!(
        cyclic["staff"]["ancestors"],
        json!([{ "name": "Intern" }, { "name": "Dev" }, { "name": "CTO" }])
    );
}

#[tokio::test]
async fn test_hierarchies_from_ltree_paths() {
    let env = setup().await;
    assert!(env
        .block("input", "CategoryFilter")
        .contains("  path: LTreeFilter\n"));
    let data = env
        .run(
            r#"{
                science: categoryByPath(path: "top.science") { pathAncestors { name } pathDescendants { name } }
                top: categoryByPath(path: "top") { pathDescendants { name } near: pathDescendants(maxDepth: 1) { name } }
                below: categoriesConnection(filter: { path: { descendantOf: "top.science" } }, orderBy: [PATH_ASC]) { nodes { name } }
                above: categoriesConnection(filter: { path: { ancestorOf: "top.science.astronomy" } }, orderBy: [PATH_ASC]) { nodes { name } }
                pattern: categoriesConnection(filter: { path: { matches: "*.science.*" } }, orderBy: [PATH_ASC]) { nodes { name } }
                text: categoriesConnection(filter: { path: { matchesText: "astronomy | arts" } }, orderBy: [PATH_ASC]) { nodes { name } }
            }"#,
        )
        .await;
    assert_eq!(
        data["science"],
        json!({ "pathAncestors": [{ "name": "Top" }], "pathDescendants": [{ "name": "Astronomy" }] })
    );
    assert_eq!(
        data["top"]["pathDescendants"],
        json!([{ "name": "Science" }, { "name": "Arts" }, { "name": "Astronomy" }])
    );
    assert_eq!(
        data["top"]["near"],
        json!([{ "name": "Science" }, { "name": "Arts" }])
    );
    assert_eq!(names(&data, "below"), ["Science", "Astronomy"]);
    assert_eq!(names(&data, "above"), ["Top", "Science", "Astronomy"]);
    assert_eq!(names(&data, "pattern"), ["Science", "Astronomy"]);
    assert_eq!(names(&data, "text"), ["Arts", "Astronomy"]);
}
