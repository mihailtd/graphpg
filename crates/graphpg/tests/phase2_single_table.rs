mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::compiler::SqlCompiler;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use serde_json::json;
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

#[tokio::test]
async fn test_fixture_field_aliases() {
    let (client, schema, catalog) = setup_test().await;

    // From Crystal fixture: grafast/dataplan-pg/__tests__/queries/super-simple/field-aliases.test.graphql
    let query_str = r#"
        query {
            forums {
                a: name
                b: name
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "field_aliases.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for field-aliases:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    // Verify aliasing matches Crystal's behavior: both aliases map to the same underlying column
    let forums = result["data"]["forums"]
        .as_array()
        .expect("forums should be an array");
    assert_eq!(forums.len(), 3);

    for item in forums {
        assert_eq!(item["a"], item["b"]);
    }

    let names: Vec<&str> = forums
        .iter()
        .map(|item| item["a"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Cats"));
    assert!(names.contains(&"Dogs"));
    assert!(names.contains(&"Postgres"));
}

#[tokio::test]
async fn test_fixture_single_record_via_arg() {
    let (client, schema, catalog) = setup_test().await;

    // From Crystal fixture: grafast/dataplan-pg/__tests__/queries/super-simple/single-record-via-arg.test.graphql
    let query_str = r#"
        query {
            forum(id: "f1700000-0000-0000-0000-000000000f17") {
                id
                name
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "single_record_via_arg.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for single-record-via-arg:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    // Returns Relay Global ID on id
    assert_eq!(
        result["data"]["forum"],
        json!({
            "id": "Rm9ydW06ZjE3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwZjE3",
            "name": "Postgres"
        })
    );
}

#[tokio::test]
async fn test_single_record_not_found_returns_null() {
    let (client, schema, catalog) = setup_test().await;

    let query_str = r#"
        query {
            forum(id: "00000000-0000-0000-0000-000000000000") {
                id
                name
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "not_found.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    assert_eq!(result["data"]["forum"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_multi_root_fields_single_query_roundtrip() {
    let (client, schema, catalog) = setup_test().await;

    let query_str = r#"
        query MultiRoot {
            users {
                id
                username
            }
            forums {
                name
            }
            specificForum: forum(id: "f1700000-0000-0000-0000-000000000f17") {
                name
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "multi_root.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for multi-root:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    // Verify all 3 root fields returned in a single data object
    let users = result["data"]["users"].as_array().expect("users array");
    let forums = result["data"]["forums"].as_array().expect("forums array");
    let specific_forum = &result["data"]["specificForum"];

    assert!(!users.is_empty(), "users should have items");
    assert!(!forums.is_empty(), "forums should have items");
    assert_eq!(specific_forum["name"], "Postgres");
}
