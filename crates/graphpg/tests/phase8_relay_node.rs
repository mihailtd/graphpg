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
    graphpg::catalog::DatabaseCatalog,
) {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    let inspector = CatalogInspector::new(&client);
    let catalog = inspector
        .inspect_schema("app_public")
        .await
        .expect("Failed to inspect schema");

    let valid_schema = SchemaBuilder::build(&catalog).expect("Failed to build schema");
    (client, valid_schema, catalog)
}

#[tokio::test]
async fn test_relay_node_interface_in_schema() {
    let (_client, schema, _catalog) = setup_test().await;

    // Verify Node interface exists in GraphQL schema
    let node_iface = schema.get_interface("Node");
    assert!(node_iface.is_some(), "Node interface must exist");
    let iface = node_iface.unwrap();
    assert!(
        iface.fields.contains_key("id"),
        "Node interface must have 'id' field"
    );

    // Verify Query.node exists
    let query_obj = schema.get_object("Query").unwrap();
    assert!(
        query_obj.fields.contains_key("node"),
        "Query.node must exist"
    );

    // Verify Forum implements Node
    let forum_obj = schema.get_object("Forum").unwrap();
    assert!(forum_obj
        .implements_interfaces
        .iter()
        .any(|i| i.as_str() == "Node"));
    assert!(forum_obj.fields.contains_key("id"));
    assert!(forum_obj.fields.contains_key("databaseId"));
}

#[tokio::test]
async fn test_query_node_by_global_id() {
    let (client, schema, catalog) = setup_test().await;

    // Global ID for Forum "Postgres" (id: f1700000-0000-0000-0000-000000000f17)
    let global_id =
        SchemaBuilder::encode_global_id("Forum", "f1700000-0000-0000-0000-000000000f17");

    let query_str = format!(
        r#"
        query {{
            node(id: "{}") {{
                __typename
                id
                ... on Forum {{
                    databaseId
                    name
                }}
            }}
        }}
        "#,
        global_id
    );

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        &query_str,
        "node_query.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for node(id:):\n{}", compiled.sql);

    assert!(
        compiled.sql.contains("t0.\"id\" = $1::text::uuid"),
        "{}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("'__typename': 'Forum'"),
        "{}",
        compiled.sql
    );

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");
    println!(
        "Result:\n{}",
        serde_json::to_string_pretty(&result).unwrap()
    );

    assert_eq!(
        result["data"]["node"],
        json!({
            "__typename": "Forum",
            "id": global_id,
            "databaseId": "f1700000-0000-0000-0000-000000000f17",
            "name": "Postgres"
        })
    );
}

#[tokio::test]
async fn test_query_node_nonexistent_returns_null() {
    let (client, schema, catalog) = setup_test().await;

    let nonexistent_id =
        SchemaBuilder::encode_global_id("Forum", "00000000-0000-0000-0000-000000000000");

    let query_str = format!(
        r#"
        query {{
            node(id: "{}") {{
                id
                ... on Forum {{
                    name
                }}
            }}
        }}
        "#,
        nonexistent_id
    );

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        &query_str,
        "node_null.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");
    assert!(
        result["data"]["node"].is_null(),
        "Non-existent entity should return null"
    );
}

#[tokio::test]
async fn test_query_node_invalid_id_returns_null_safely() {
    let (client, schema, catalog) = setup_test().await;

    let query_str = r#"
        query {
            node(id: "invalid-base64-not-a-node-id") {
                id
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "node_invalid.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");
    assert!(
        result["data"]["node"].is_null(),
        "Invalid node id should return null safely without error"
    );
}

/// Updates and deletes take the row's key, typed as `forum(id:)` takes it; their `ByNodeId`
/// variants take its global ID, refused when it is another type's.
#[tokio::test]
async fn test_mutations_by_key_and_by_node_id() {
    let (client, schema, catalog) = setup_test().await;
    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = |query: &str| {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &schema,
            query,
            "mutation.graphql",
        )
        .map_err(|e| e.to_string())?;
        planner.plan(&doc).map_err(|e| e.to_string())
    };
    let forum = "f1700000-0000-0000-0000-000000000f17";

    let by_key = plan(&format!(
        r#"mutation {{ updateForum(id: "{forum}", input: {{ name: "Postgres" }}) {{ forum {{ databaseId name }} }} }}"#
    ))
    .expect("by key");
    let result = ExecutionEngine::execute(&client, &by_key).await.unwrap();
    assert_eq!(result["data"]["updateForum"]["forum"]["databaseId"], forum);

    let node_id = SchemaBuilder::encode_global_id("Forum", forum);
    let by_node_id = plan(&format!(
        r#"mutation {{ updateForumByNodeId(nodeId: "{node_id}", input: {{ name: "Postgres" }}) {{ forum {{ id name }} }} }}"#
    ))
    .expect("by node ID");
    let result = ExecutionEngine::execute(&client, &by_node_id)
        .await
        .unwrap();
    assert_eq!(
        result["data"]["updateForumByNodeId"]["forum"],
        json!({ "id": node_id, "name": "Postgres" })
    );

    // A user's ID addresses no forum
    let user_id = SchemaBuilder::encode_global_id("User", forum);
    let err = plan(&format!(
        r#"mutation {{ deleteForumByNodeId(nodeId: "{user_id}") {{ forum {{ id }} }} }}"#
    ))
    .unwrap_err();
    assert!(
        err.contains("the ID of a `User`, not of a `Forum`"),
        "{err}"
    );
    let err =
        plan(r#"mutation { deleteForumByNodeId(nodeId: "nope") { forum { id } } }"#).unwrap_err();
    assert!(err.contains("not a global ID"), "{err}");

    // Key arguments, lookups and foreign keys take a global ID of their own type only
    for query in [
        format!(r#"mutation {{ deleteForum(id: "{user_id}") {{ forum {{ id }} }} }}"#),
        format!(r#"{{ forum(id: "{user_id}") {{ name }} }}"#),
        format!(
            r#"mutation {{ createMessage(input: {{ forumId: "{user_id}", authorId: "{user_id}", body: "x" }}) {{ message {{ id }} }} }}"#
        ),
        format!(
            r#"{{ messagesConnection(condition: {{ forumId: "{user_id}" }}) {{ totalCount }} }}"#
        ),
    ] {
        let err = plan(&query).unwrap_err();
        assert!(
            err.contains("is not the ID of `Forum` (it is the ID of a `User`)"),
            "{query}: {err}"
        );
    }
    let read = plan(&format!(r#"{{ forum(id: "{node_id}") {{ name }} }}"#)).expect("own type");
    let result = ExecutionEngine::execute(&client, &read).await.unwrap();
    assert_eq!(result["data"]["forum"]["name"], "Postgres");
}
