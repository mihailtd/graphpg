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
async fn test_forum_fixture_relations_basics() {
    let (client, schema, catalog) = setup_test().await;

    // From Crystal fixture: grafast/dataplan-pg/__tests__/queries/relations/basics.test.graphql
    let query_str = r#"
        query {
            message(id: "ca70ca70-0000-0000-0000-cec111a0ca70") {
                id
                body
                forum {
                    id
                    name
                }
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "relations_basics.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for relations/basics:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    // Matches Crystal's grafast/dataplan-pg/__tests__/queries/relations/basics.json5 exactly:
    assert_eq!(
        result["data"],
        json!({
            "message": {
                "id": "TWVzc2FnZTpjYTcwY2E3MC0wMDAwLTAwMDAtMDAwMC1jZWMxMTFhMGNhNzA=",
                "body": "Cats = awesome -- Cecilia",
                "forum": {
                    "id": "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3",
                    "name": "Cats"
                }
            }
        })
    );
}

#[tokio::test]
async fn test_multiple_relations_on_single_record() {
    let (client, schema, catalog) = setup_test().await;

    // Both Many-to-One relations (forum and author) requested on Message
    let query_str = r#"
        query {
            message(id: "ca70ca70-0000-0000-0000-cec111a0ca70") {
                id
                body
                forum {
                    id
                    name
                }
                author {
                    id
                    username
                }
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "multiple_relations.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for multiple relations:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    assert_eq!(
        result["data"]["message"]["forum"],
        json!({
            "id": "Rm9ydW06Y2E3MDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwY2E3",
            "name": "Cats"
        })
    );
    assert_eq!(result["data"]["message"]["author"]["username"], "Cecilia");
}

#[tokio::test]
async fn test_one_to_many_relation_forum_messages() {
    let (client, schema, catalog) = setup_test().await;

    // One-to-Many: Forum -> Messages
    let query_str = r#"
        query {
            forum(id: "ca700000-0000-0000-0000-000000000ca7") {
                id
                name
                messages {
                    id
                    body
                }
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "one_to_many.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for 1:N relations:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    assert_eq!(result["data"]["forum"]["name"], "Cats");
    let messages = result["data"]["forum"]["messages"]
        .as_array()
        .expect("messages should be a JSON array");
    assert!(!messages.is_empty(), "Cats forum should have messages");

    // Verify all messages have id and body
    for msg in messages {
        assert!(msg["id"].is_string());
        assert!(msg["body"].is_string());
    }
}

#[tokio::test]
async fn test_three_level_deep_relations() {
    let (client, schema, catalog) = setup_test().await;

    // 3 levels: Forum -> Messages -> Author (User)
    let query_str = r#"
        query {
            forum(id: "ca700000-0000-0000-0000-000000000ca7") {
                name
                messages {
                    body
                    author {
                        username
                    }
                }
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "three_level.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for 3-level deep relation:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    assert_eq!(result["data"]["forum"]["name"], "Cats");
    let messages = result["data"]["forum"]["messages"]
        .as_array()
        .expect("messages array");
    assert!(!messages.is_empty());

    for msg in messages {
        let author = &msg["author"];
        assert!(author["username"].is_string());
    }
}

#[tokio::test]
async fn test_list_with_nested_relations_zero_n_plus_one() {
    let (client, schema, catalog) = setup_test().await;

    // All messages with their author and forum - entirely in 1 SQL query!
    let query_str = r#"
        query {
            messages {
                id
                body
                forum {
                    name
                }
                author {
                    username
                }
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "zero_n_plus_one.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for zero N+1 query:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    let messages = result["data"]["messages"]
        .as_array()
        .expect("messages array");
    assert!(!messages.is_empty());

    for msg in messages {
        assert!(msg["forum"]["name"].is_string());
        assert!(msg["author"]["username"].is_string());
    }
}
