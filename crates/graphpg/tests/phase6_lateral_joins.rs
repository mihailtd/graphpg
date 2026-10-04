mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::compiler::SqlCompiler;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::{JoinCondition, OperationPlan, SelectedField, Step};
use serde_json::json;
use tokio_postgres::types::ToSql;
use tokio_postgres::NoTls;


async fn setup_test() -> (
    tokio_postgres::Client,
    apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    graphpg::DatabaseCatalog,
) {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    let inspector = CatalogInspector::new(&client);
    let catalog = inspector
        .inspect_schema("app_public")
        .await
        .expect("Failed to inspect app_public schema");

    let schema = SchemaBuilder::build(&catalog).expect("Failed to build valid GraphQL schema");

    (client, schema, catalog)
}

fn plan_query(
    schema: &apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: &graphpg::DatabaseCatalog,
    query_str: &str,
) -> OperationPlan {
    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(schema, query_str, "q.graphql")
        .expect("Query validation failed");
    QueryPlanner::new(catalog, "app_public")
        .plan(&doc)
        .expect("Query planning failed")
}

/// Collects `(Node Type, Relation Name, Index Name)` for every node of an EXPLAIN (FORMAT JSON) plan.
fn collect_plan_nodes(node: &serde_json::Value, out: &mut Vec<(String, String, String)>) {
    let s = |k: &str| node.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    out.push((s("Node Type"), s("Relation Name"), s("Index Name")));
    if let Some(children) = node.get("Plans").and_then(|p| p.as_array()) {
        for child in children {
            collect_plan_nodes(child, out);
        }
    }
}

const MESSAGE_WITH_FORUM: &str = r#"
    query {
        message(id: "ca70ca70-0000-0000-0000-cec111a0ca70") {
            id
            forum { id name }
        }
    }
"#;

#[tokio::test]
async fn test_nested_relations_compile_to_left_join_lateral() {
    let (_client, schema, catalog) = setup_test().await;

    let plan = plan_query(
        &schema,
        &catalog,
        r#"
        query {
            forums {
                name
                messages { body author { username } }
            }
        }
    "#,
    );
    let sql = SqlCompiler::compile(&plan).sql;
    println!("Compiled SQL:\n{}", sql);

    // To-many relations are laterals (messages); to-one relations onto a primary key are plain
    // LEFT JOINs inside the enclosing scope (author inside messages)
    assert_eq!(sql.matches("LEFT JOIN LATERAL").count(), 1, "{sql}");
    assert!(sql.contains("'messages': _lat0.data"), "{sql}");
    assert!(sql.contains("LEFT JOIN \"app_public\".\"users\" t2 ON t2.\"id\" = t1.\"author_id\""), "{sql}");
    assert!(sql.contains("'author': CASE WHEN t2.\"id\" IS NULL THEN NULL ELSE JSON_OBJECT("), "{sql}");
    // SQL/JSON standard constructors (PG 16+), returning `json` to keep key order
    assert!(sql.contains("JSON_OBJECT(") && sql.contains("JSON_ARRAYAGG("), "{sql}");
    assert!(!sql.contains("json_build_object") && !sql.contains("json_agg("), "{sql}");
    // Explicit `RETURNING json`: otherwise any jsonb input makes the result jsonb (sorted keys)
    assert_eq!(
        sql.matches("JSON_OBJECT(").count(),
        sql.matches(" RETURNING json)").count() - sql.matches("JSON_ARRAYAGG(").count(),
        "{sql}"
    );
    assert!(!sql.contains("RETURNING jsonb"), "{sql}");
    // FK joins compare native types so indexes stay usable
    assert!(!sql.contains("::text = t"), "join condition must not cast to text: {sql}");
}

#[tokio::test]
async fn test_where_and_join_use_native_types() {
    let (_client, schema, catalog) = setup_test().await;

    let plan = plan_query(&schema, &catalog, MESSAGE_WITH_FORUM);
    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL:\n{}", compiled.sql);

    assert!(compiled.sql.contains("t0.\"id\" = $1::text::uuid"), "{}", compiled.sql);
    assert!(compiled.sql.contains("t1.\"id\" = t0.\"forum_id\""), "{}", compiled.sql);
}

#[tokio::test]
async fn test_explain_uses_primary_key_indexes() {
    let (client, schema, catalog) = setup_test().await;

    let plan = plan_query(&schema, &catalog, MESSAGE_WITH_FORUM);
    let compiled = SqlCompiler::compile(&plan);

    // The fixture tables are tiny, so make seq scans prohibitively expensive: any predicate
    // that can't use an index (e.g. `col::text = $1`) would still show up as a Seq Scan.
    client.batch_execute("SET enable_seqscan = off").await.unwrap();

    let params: Vec<&(dyn ToSql + Sync)> = compiled.params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let explain_sql = format!("EXPLAIN (FORMAT JSON) {}", compiled.sql.trim_end_matches(';'));
    let row = client.query_one(&explain_sql, &params).await.expect("EXPLAIN failed");
    let explain: serde_json::Value = row.get(0);
    println!("{}", serde_json::to_string_pretty(&explain).unwrap());

    let mut nodes = Vec::new();
    collect_plan_nodes(&explain[0]["Plan"], &mut nodes);

    assert!(
        !nodes.iter().any(|(ty, _, _)| ty == "Seq Scan"),
        "expected no sequential scans, got {nodes:?}"
    );
    let uses_index = |idx: &str| nodes.iter().any(|(ty, _, name)| ty.starts_with("Index") && name == idx);
    assert!(uses_index("messages_pkey"), "root lookup should use messages_pkey: {nodes:?}");
    assert!(uses_index("forums_pkey"), "forum lateral should use forums_pkey: {nodes:?}");

    // And the query still returns the right data
    let result = ExecutionEngine::execute(&client, &plan).await.expect("Execution failed");
    assert_eq!(
        result["data"],
        json!({
            "message": {
                "id": "TWVzc2FnZTpjYTcwY2E3MC0wMDAwLTAwMDAtMDAwMC1jZWMxMTFhMGNhNzA=",
                "forum": { "id": "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3", "name": "Cats" }
            }
        })
    );
}

/// ORDER BY / LIMIT on lists must apply to the rows, not the aggregated JSON array,
/// at both the root and inside a lateral.
#[tokio::test]
async fn test_list_order_and_limit_apply_to_rows() {
    let (client, _schema, _catalog) = setup_test().await;

    let mut plan = OperationPlan::new();

    let mut messages = Step::pg_select("app_public", "messages", "t1", false);
    if let Step::PgSelect { fields, join_condition, order_by, limit, .. } = &mut messages {
        *fields = vec![SelectedField::column("body", "body")];
        *join_condition = Some(JoinCondition::new("forum_id", "id", "t0"));
        *order_by = Some("t1.\"body\" DESC".into());
        *limit = Some(2);
    }
    let messages_id = plan.add_step(messages);

    let mut forums = Step::pg_select("app_public", "forums", "t0", false);
    if let Step::PgSelect { fields, order_by, limit, .. } = &mut forums {
        *fields = vec![
            SelectedField::column("name", "name"),
            SelectedField::relation("messages", messages_id),
        ];
        *order_by = Some("t0.\"name\" ASC".into());
        *limit = Some(2);
    }
    let forums_id = plan.add_step(forums);
    plan.add_root_field("forums", forums_id);

    let mut names_only = Step::pg_select("app_public", "forums", "t2", false);
    if let Step::PgSelect { fields, order_by, limit, offset, .. } = &mut names_only {
        *fields = vec![SelectedField::column("name", "name")];
        *order_by = Some("t2.\"name\" DESC".into());
        *limit = Some(1);
        *offset = Some(1);
    }
    let names_id = plan.add_step(names_only);
    plan.add_root_field("middleForum", names_id);

    println!("Compiled SQL:\n{}", SqlCompiler::compile(&plan).sql);

    let result = ExecutionEngine::execute(&client, &plan).await.expect("Execution failed");
    assert_eq!(
        result["data"],
        json!({
            "forums": [
                {
                    "name": "Cats",
                    "messages": [
                        { "body": "Cats = awesome -- Cecilia" },
                        { "body": "Cats = awesome -- Bob" }
                    ]
                },
                {
                    "name": "Dogs",
                    "messages": [
                        { "body": "Dogs = awesome -- Cecilia" },
                        { "body": "Dogs = awesome -- Bob" }
                    ]
                }
            ],
            "middleForum": [{ "name": "Dogs" }]
        })
    );
}

/// Connections must page (LIMIT first + 1) before building node JSON or running nested
/// laterals, and only count all rows when totalCount is selected.
#[tokio::test]
async fn test_connection_pages_before_building_json() {
    let (client, schema, catalog) = setup_test().await;

    let without_count = plan_query(
        &schema,
        &catalog,
        "{ messagesConnection(first: 2) { pageInfo { hasNextPage } nodes { body forum { name } } } }",
    );
    let compiled = SqlCompiler::compile(&without_count);
    let sql = compiled.sql;
    println!("Compiled SQL:\n{}", sql);
    assert!(sql.contains("LIMIT $1::text::int8 + 1"), "page query should fetch first + 1 rows: {sql}");
    assert_eq!(compiled.params, ["2"]);
    assert!(!sql.contains("count(*) OVER"), "no full-scan window count: {sql}");
    assert!(!sql.contains("SELECT count(*) FROM"), "totalCount not selected: {sql}");

    let result = ExecutionEngine::execute(&client, &without_count).await.expect("Execution failed");
    let conn = &result["data"]["messagesConnection"];
    assert_eq!(conn["pageInfo"]["hasNextPage"], true);
    assert_eq!(conn["nodes"].as_array().map(Vec::len), Some(2), "{result}");
    assert!(conn["nodes"][0]["forum"]["name"].is_string(), "{result}");

    let with_count = plan_query(&schema, &catalog, "{ messagesConnection(first: 50) { totalCount pageInfo { hasNextPage } } }");
    let result = ExecutionEngine::execute(&client, &with_count).await.expect("Execution failed");
    assert_eq!(result["data"]["messagesConnection"]["totalCount"], 9, "{result}");
    assert_eq!(result["data"]["messagesConnection"]["pageInfo"]["hasNextPage"], false, "{result}");
}

/// Fields following the same foreign key from the same row share one join, whatever their
/// aliases or selections; the response still has every field with its own selection.
#[tokio::test]
async fn test_same_foreign_key_is_joined_once() {
    let (client, schema, catalog) = setup_test().await;

    let plan = plan_query(
        &schema,
        &catalog,
        r#"
        query {
            message(id: "ca70ca70-0000-0000-0000-cec111a0ca70") {
                forum { name }
                f2: forum { id }
                messages: forum { name }
            }
        }
    "#,
    );
    let sql = SqlCompiler::compile(&plan).sql;
    println!("Compiled SQL:\n{}", sql);
    assert_eq!(sql.matches("\"app_public\".\"forums\"").count(), 1, "one join for the forum: {sql}");
    assert!(!sql.contains("LATERAL"), "{sql}");

    let result = ExecutionEngine::execute(&client, &plan).await.expect("Execution failed");
    assert_eq!(
        result["data"]["message"],
        json!({
            "forum": { "name": "Cats" },
            "f2": { "id": "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3" },
            "messages": { "name": "Cats" }
        })
    );
}
