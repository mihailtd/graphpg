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
async fn test_forum_fixture_connections_basics_limit3() {
    let (client, schema, catalog) = setup_test().await;

    // From Crystal fixture: grafast/dataplan-pg/__tests__/queries/connections/basics-limit3.test.graphql
    let query_str = r#"
        query {
            allMessagesConnection(first: 3) {
                edges {
                    cursor
                    node {
                        body
                        author {
                            username
                            gravatarUrl
                        }
                    }
                }
                pageInfo {
                    hasNextPage
                    hasPreviousPage
                    startCursor
                    endCursor
                }
                totalCount
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "basics_limit3.graphql",
    )
    .expect("Query validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Query planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for connections/basics-limit3:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan)
        .await
        .expect("Execution failed");

    println!("Result:\n{}", serde_json::to_string_pretty(&result).unwrap());

    let conn = &result["data"]["allMessagesConnection"];
    assert_eq!(conn["totalCount"], 9);

    let edges = conn["edges"].as_array().expect("edges array");
    assert_eq!(edges.len(), 3);

    let page_info = &conn["pageInfo"];
    assert_eq!(page_info["hasNextPage"], true);
    assert_eq!(page_info["hasPreviousPage"], false);

    let start_cursor = page_info["startCursor"].as_str().expect("startCursor");
    let end_cursor = page_info["endCursor"].as_str().expect("endCursor");
    assert_eq!(start_cursor, edges[0]["cursor"].as_str().unwrap());
    assert_eq!(end_cursor, edges[2]["cursor"].as_str().unwrap());

    // Verify node data
    for edge in edges {
        assert!(edge["node"]["body"].is_string());
        assert!(edge["node"]["author"]["username"].is_string());
    }
}

#[tokio::test]
async fn test_cursor_forward_pagination() {
    let (client, schema, catalog) = setup_test().await;

    // 1. Fetch first 2 records
    let first_query = r#"
        query {
            allMessagesConnection(first: 2) {
                edges {
                    cursor
                    node {
                        id
                        body
                    }
                }
                pageInfo {
                    endCursor
                }
            }
        }
    "#;

    let doc1 = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        first_query,
        "first_page.graphql",
    )
    .expect("Validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan1 = planner.plan(&doc1).expect("Planning failed");
    let res1 = ExecutionEngine::execute(&client, &plan1).await.expect("Execution failed");

    let first_edges = res1["data"]["allMessagesConnection"]["edges"].as_array().unwrap();
    assert_eq!(first_edges.len(), 2);
    let end_cursor = res1["data"]["allMessagesConnection"]["pageInfo"]["endCursor"]
        .as_str()
        .unwrap();

    // 2. Fetch next 2 records after end_cursor
    let second_query = format!(
        r#"
        query {{
            allMessagesConnection(first: 2, after: "{}") {{
                edges {{
                    cursor
                    node {{
                        id
                        body
                    }}
                }}
                pageInfo {{
                    hasNextPage
                    hasPreviousPage
                }}
            }}
        }}
    "#,
        end_cursor
    );

    let doc2 = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        &second_query,
        "second_page.graphql",
    )
    .expect("Validation failed");

    let plan2 = planner.plan(&doc2).expect("Planning failed");
    let res2 = ExecutionEngine::execute(&client, &plan2).await.expect("Execution failed");

    println!("Second page result:\n{}", serde_json::to_string_pretty(&res2).unwrap());

    let second_edges = res2["data"]["allMessagesConnection"]["edges"].as_array().unwrap();
    assert_eq!(second_edges.len(), 2);

    let second_page_info = &res2["data"]["allMessagesConnection"]["pageInfo"];
    assert_eq!(second_page_info["hasPreviousPage"], true);
    assert_eq!(second_page_info["hasNextPage"], true);

    // Verify disjoint sets of IDs
    let first_ids: Vec<&str> = first_edges
        .iter()
        .map(|e| e["node"]["id"].as_str().unwrap())
        .collect();
    let second_ids: Vec<&str> = second_edges
        .iter()
        .map(|e| e["node"]["id"].as_str().unwrap())
        .collect();

    for id in &second_ids {
        assert!(!first_ids.contains(id), "Second page IDs must not overlap with first page");
    }
}

#[tokio::test]
async fn test_connection_condition_filter() {
    let (client, schema, catalog) = setup_test().await;

    // Filter messages by condition: { featured: true }
    let query_str = r#"
        query {
            allMessagesConnection(condition: { featured: true }) {
                totalCount
                edges {
                    node {
                        id
                        body
                        featured
                    }
                }
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "condition_filter.graphql",
    )
    .expect("Validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for condition filter:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan).await.expect("Execution failed");

    println!("Result:\n{}", serde_json::to_string_pretty(&result).unwrap());

    let conn = &result["data"]["allMessagesConnection"];
    let edges = conn["edges"].as_array().unwrap();

    assert!(!edges.is_empty(), "Should have featured messages");
    for edge in edges {
        assert_eq!(edge["node"]["featured"], true);
    }
}

#[tokio::test]
async fn test_relational_nested_connection() {
    let (client, schema, catalog) = setup_test().await;

    // Nested relational connection: forums -> messagesConnection(first: 2)
    let query_str = r#"
        query {
            forums {
                name
                messagesConnection(first: 2) {
                    totalCount
                    pageInfo {
                        hasNextPage
                        startCursor
                        endCursor
                    }
                    edges {
                        cursor
                        node {
                            body
                        }
                    }
                }
            }
        }
    "#;

    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
        &schema,
        query_str,
        "nested_connection.graphql",
    )
    .expect("Validation failed");

    let planner = QueryPlanner::new(&catalog, "app_public");
    let plan = planner.plan(&doc).expect("Planning failed");

    let compiled = SqlCompiler::compile(&plan);
    println!("Compiled SQL for nested relational connection:\n{}", compiled.sql);

    let result = ExecutionEngine::execute(&client, &plan).await.expect("Execution failed");

    println!("Result:\n{}", serde_json::to_string_pretty(&result).unwrap());

    let forums = result["data"]["forums"].as_array().unwrap();
    assert_eq!(forums.len(), 3);

    for forum in forums {
        let conn = &forum["messagesConnection"];
        assert_eq!(conn["totalCount"], 3);
        assert_eq!(conn["pageInfo"]["hasNextPage"], true);

        let edges = conn["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 2);
    }
}

/// `totalCount` and `aggregates` share one scan of the filtered rows, with or without a page,
/// and stay correct when the page (or the whole result) is empty.
#[tokio::test]
async fn test_count_and_aggregates_share_one_scan() {
    let (client, schema, catalog) = setup_test().await;
    let run = |q: &'static str| {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(&schema, q, "q.graphql").expect("validation");
        QueryPlanner::new(&catalog, "app_public").plan(&doc).expect("planning")
    };
    let expected: i64 = client.query_one("SELECT count(*) FROM app_public.messages", &[]).await.unwrap().get(0);
    let (min, max): (String, String) = client
        .query_one("SELECT min(created_at)::text, max(created_at)::text FROM app_public.messages", &[])
        .await
        .map(|r| (r.get(0), r.get(1)))
        .unwrap();
    println!("expected count {expected}, min {min}, max {max}");

    // Stats only, stats + page, and an empty result
    for (q, page_len, total) in [
        ("{ messagesConnection { totalCount aggregates { min { createdAt } max { createdAt } } } }", None, expected),
        ("{ messagesConnection(first: 2) { totalCount aggregates { min { createdAt } max { createdAt } } pageInfo { hasNextPage } nodes { body } } }", Some(2), expected),
        ("{ messagesConnection(first: 2, condition: { body: \"no such message\" }) { totalCount aggregates { min { createdAt } } pageInfo { hasNextPage } nodes { body } } }", Some(0), 0),
    ] {
        let plan = run(q);
        let sql = SqlCompiler::compile(&plan).sql;
        println!("{sql}");
        assert_eq!(sql.matches("count(*)").count(), 1, "{sql}");
        assert_eq!(sql.matches("\"app_public\".\"messages\"").count(), if page_len.is_some() { 2 } else { 1 }, "stats scan + page scan: {sql}");

        let data = ExecutionEngine::execute(&client, &plan).await.expect("execution")["data"]["messagesConnection"].clone();
        println!("{data}");
        assert_eq!(data["totalCount"], total);
        if total > 0 {
            assert!(data["aggregates"]["min"]["createdAt"].is_string());
            assert!(data["aggregates"]["max"]["createdAt"].is_string());
        } else {
            assert!(data["aggregates"]["min"]["createdAt"].is_null());
        }
        if let Some(len) = page_len {
            assert_eq!(data["nodes"].as_array().unwrap().len(), len);
            assert_eq!(data["pageInfo"]["hasNextPage"], total > 2);
        }
    }
}

async fn run(
    client: &tokio_postgres::Client,
    schema: &apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: &graphpg::DatabaseCatalog,
    query: &str,
) -> Result<serde_json::Value, String> {
    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(schema, query, "q.graphql").map_err(|e| e.to_string())?;
    let plan = QueryPlanner::new(catalog, "app_public").plan(&doc).map_err(|e| e.to_string())?;
    ExecutionEngine::execute(client, &plan).await.map_err(|e| e.to_string())
}

fn ids(conn: &serde_json::Value) -> Vec<String> {
    conn["edges"].as_array().unwrap().iter().map(|e| e["node"]["databaseId"].as_str().unwrap().to_string()).collect()
}

/// One page of `messagesConnection`: the ids, the cursors of its first and last edge,
/// and `(hasPreviousPage, hasNextPage)`.
async fn page(
    client: &tokio_postgres::Client,
    schema: &apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: &graphpg::DatabaseCatalog,
    args: &str,
) -> (Vec<String>, Option<String>, Option<String>, (bool, bool)) {
    let query = format!(
        "{{ messagesConnection({args}) {{ edges {{ cursor node {{ databaseId }} }} \
         pageInfo {{ hasPreviousPage hasNextPage startCursor endCursor }} }} }}"
    );
    let res = run(client, schema, catalog, &query).await.unwrap_or_else(|e| panic!("{args}: {e}"));
    let conn = &res["data"]["messagesConnection"];
    let info = &conn["pageInfo"];
    let edges = conn["edges"].as_array().unwrap();
    if let (Some(first), Some(last)) = (edges.first(), edges.last()) {
        assert_eq!(info["startCursor"], first["cursor"], "{args}");
        assert_eq!(info["endCursor"], last["cursor"], "{args}");
    }
    (
        ids(conn),
        info["startCursor"].as_str().map(str::to_string),
        info["endCursor"].as_str().map(str::to_string),
        (info["hasPreviousPage"].as_bool().unwrap(), info["hasNextPage"].as_bool().unwrap()),
    )
}

#[tokio::test]
async fn test_keyset_pagination_follows_order_by() {
    let (client, schema, catalog) = setup_test().await;

    // `orderBy` and the equivalent SQL order. `featured` and `created_at` have ties,
    // `archived_at` is mostly NULL (NULLS LAST ascending, FIRST descending).
    let orders = [
        ("", "id ASC"),
        ("[PRIMARY_KEY_DESC]", "id DESC"),
        ("[FEATURED_DESC]", "featured DESC, id ASC"),
        ("[ARCHIVED_AT_ASC]", "archived_at ASC, id ASC"),
        ("[ARCHIVED_AT_DESC]", "archived_at DESC, id ASC"),
        ("[ARCHIVED_AT_DESC, FEATURED_ASC]", "archived_at DESC, featured ASC, id ASC"),
        ("[CREATED_AT_ASC, BODY_DESC]", "created_at ASC, body DESC, id ASC"),
        ("[FEATURED_ASC, PRIMARY_KEY_DESC]", "featured ASC, id DESC"),
    ];
    for (order_by, sql_order) in orders {
        let order_arg = if order_by.is_empty() { String::new() } else { format!(", orderBy: {order_by}") };
        let expected: Vec<String> = client
            .query(&format!("SELECT id::text FROM app_public.messages ORDER BY {sql_order}"), &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert!(expected.len() > 4, "fixture has enough rows to page through");

        // Forward: first 2 after each endCursor
        let mut seen = Vec::new();
        let mut after = None::<String>;
        loop {
            let cursor = after.as_ref().map(|c| format!(", after: \"{c}\"")).unwrap_or_default();
            let (page_ids, _, end, (has_prev, has_next)) = page(&client, &schema, &catalog, &format!("first: 2{order_arg}{cursor}")).await;
            assert_eq!(has_prev, after.is_some(), "{order_by} forward hasPreviousPage");
            seen.extend(page_ids);
            assert_eq!(has_next, seen.len() < expected.len(), "{order_by} forward hasNextPage");
            if !has_next {
                break;
            }
            after = end;
        }
        assert_eq!(seen, expected, "{order_by}: forward pages");

        // Backward: last 2 before each startCursor
        let mut seen: Vec<String> = Vec::new();
        let mut before = None::<String>;
        loop {
            let cursor = before.as_ref().map(|c| format!(", before: \"{c}\"")).unwrap_or_default();
            let (page_ids, start, _, (has_prev, has_next)) = page(&client, &schema, &catalog, &format!("last: 2{order_arg}{cursor}")).await;
            assert!(page_ids.len() <= 2);
            assert_eq!(has_next, before.is_some(), "{order_by} backward hasNextPage");
            seen.splice(0..0, page_ids);
            assert_eq!(has_prev, seen.len() < expected.len(), "{order_by} backward hasPreviousPage");
            if !has_prev {
                break;
            }
            before = start;
        }
        assert_eq!(seen, expected, "{order_by}: backward pages");

        // A window between two cursors, from either end
        let (_, _, after_second, _) = page(&client, &schema, &catalog, &format!("first: 2{order_arg}")).await;
        let (_, before_last, _, _) = page(&client, &schema, &catalog, &format!("last: 1{order_arg}")).await;
        let window = format!("{order_arg}, after: \"{}\", before: \"{}\"", after_second.unwrap(), before_last.unwrap());
        let middle = &expected[2..expected.len() - 1];
        assert_eq!(page(&client, &schema, &catalog, &format!("first: 100{window}")).await.0, middle, "{order_by}: first between");
        assert_eq!(page(&client, &schema, &catalog, &format!("last: 100{window}")).await.0, middle, "{order_by}: last between");
    }
}

#[tokio::test]
async fn test_keyset_pagination_on_citext_column() {
    let (client, schema, catalog) = setup_test().await;
    let names = |res: &serde_json::Value| -> Vec<String> {
        res["data"]["usersConnection"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["node"]["username"].as_str().unwrap().to_string())
            .collect()
    };
    let query = |args: &str| format!("{{ usersConnection({args}) {{ edges {{ cursor node {{ username }} }} }} }}");

    let first = run(&client, &schema, &catalog, &query("orderBy: [USERNAME_DESC], first: 1")).await.unwrap();
    assert_eq!(names(&first), ["Cecilia"]);
    let cursor = first["data"]["usersConnection"]["edges"][0]["cursor"].as_str().unwrap();
    let next = run(&client, &schema, &catalog, &query(&format!("orderBy: [USERNAME_DESC], first: 1, after: \"{cursor}\""))).await.unwrap();
    assert_eq!(names(&next), ["Bob"]);

    // `last` is the end of the order, not its start
    let last = run(&client, &schema, &catalog, &query("last: 1")).await.unwrap();
    assert_eq!(names(&last), ["Cecilia"]);
    let last = run(&client, &schema, &catalog, &query("orderBy: [USERNAME_DESC], last: 2")).await.unwrap();
    assert_eq!(names(&last), ["Bob", "Alice"]);

    // A cursor is only valid for the order it was issued for
    let default_cursor = run(&client, &schema, &catalog, &query("first: 1")).await.unwrap();
    let default_cursor = default_cursor["data"]["usersConnection"]["edges"][0]["cursor"].as_str().unwrap();
    let err = run(&client, &schema, &catalog, &query(&format!("orderBy: [USERNAME_DESC], after: \"{default_cursor}\""))).await.unwrap_err();
    assert!(err.contains("Invalid cursor"), "{err}");
}

#[tokio::test]
async fn test_offset_and_first_with_last() {
    let (client, schema, catalog) = setup_test().await;
    let expected: Vec<String> = client
        .query("SELECT id::text FROM app_public.messages ORDER BY featured DESC, id ASC", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let n = expected.len();
    let order = "orderBy: [FEATURED_DESC]";
    let get = |args: String| {
        let (client, schema, catalog) = (&client, &schema, &catalog);
        async move {
            let (ids, _, _, (prev, next)) = page(client, schema, catalog, &args).await;
            (ids, prev, next)
        }
    };

    // `offset` skips rows forward from the start or a cursor, backward from the end or a cursor
    assert_eq!(get(format!("{order}, first: 2, offset: 3")).await, (expected[3..5].to_vec(), true, true));
    assert_eq!(get(format!("{order}, first: 5, offset: {}", n - 2)).await, (expected[n - 2..].to_vec(), true, false));
    let (_, _, after, _) = page(&client, &schema, &catalog, &format!("{order}, first: 1")).await;
    let after = after.unwrap();
    assert_eq!(get(format!("{order}, first: 2, offset: 2, after: \"{after}\"")).await.0, expected[3..5]);
    assert_eq!(get(format!("{order}, last: 2, offset: 1")).await, (expected[n - 3..n - 1].to_vec(), true, true));
    assert_eq!(get(format!("{order}, offset: {n}")).await, (vec![], true, false));

    // `first` then `last`: the tail of the first page
    assert_eq!(get(format!("{order}, first: 4, last: 2")).await, (expected[2..4].to_vec(), true, true));
    assert_eq!(get(format!("{order}, first: 4, last: 9")).await, (expected[..4].to_vec(), false, true));
    assert_eq!(get(format!("{order}, first: 100, last: 2")).await, (expected[n - 2..].to_vec(), true, false));
    assert_eq!(get(format!("{order}, first: 3, last: 2, after: \"{after}\"")).await, (expected[2..4].to_vec(), true, true));
}

#[tokio::test]
async fn test_page_sizes_share_one_statement() {
    let (_client, schema, catalog) = setup_test().await;
    let sql = |query: String| {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(&schema, &query, "q.graphql").unwrap();
        SqlCompiler::compile(&QueryPlanner::new(&catalog, "app_public").plan(&doc).unwrap()).sql
    };
    // Sizes and offsets are parameters: only the arguments passed shape the statement
    for template in [
        "{ messagesConnection(first: {n}) { nodes { body } pageInfo { hasNextPage } } }",
        "{ messagesConnection(last: {n}, offset: {n}) { nodes { body } pageInfo { hasPreviousPage } } }",
        "{ messagesConnection(first: {n}, last: {n}) { nodes { body } pageInfo { hasPreviousPage } } }",
        "{ forums { messagesConnection(first: {n}) { nodes { body } } } }",
    ] {
        let (a, b) = (sql(template.replace("{n}", "2")), sql(template.replace("{n}", "7")));
        assert_eq!(a, b, "{template}");
    }
}

#[tokio::test]
async fn test_grouped_aggregates() {
    let (client, schema, catalog) = setup_test().await;
    let data = run(
        &client,
        &schema,
        &catalog,
        r#"{ messagesConnection(filter: { archivedAt: { isNull: true } }, first: 2) {
            totalCount
            nodes { body }
            byForum: groupedAggregates(groupBy: [FORUM_ID]) { keys count }
            byYear: groupedAggregates(groupBy: [FEATURED, CREATED_AT_TRUNCATED_TO_YEAR], having: { count: { greaterThanOrEqualTo: 2 } }) {
                keys count min { createdAt } max { createdAt }
            }
            none: groupedAggregates(groupBy: [FEATURED], having: { count: { greaterThan: 100 } }) { count }
        } }"#,
    )
    .await
    .unwrap();
    let conn = &data["data"]["messagesConnection"];
    assert_eq!(conn["nodes"].as_array().unwrap().len(), 2, "paging is unaffected");

    // The same groups straight from SQL
    let expected: Vec<serde_json::Value> = client
        .query(
            "SELECT forum_id::text, count(*)::int FROM app_public.messages WHERE archived_at IS NULL GROUP BY 1 ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| json!({ "keys": [r.get::<_, String>(0)], "count": r.get::<_, i32>(1) }))
        .collect();
    assert_eq!(conn["byForum"], json!(expected));
    let total: i64 = expected.iter().map(|g| g["count"].as_i64().unwrap()).sum();
    assert_eq!(conn["totalCount"], total, "totals come from the same scan");

    let expected: Vec<serde_json::Value> = client
        .query(
            "SELECT featured, to_json(date_trunc('year', created_at)), count(*)::int, to_json(min(created_at)), to_json(max(created_at))
             FROM app_public.messages WHERE archived_at IS NULL GROUP BY featured, date_trunc('year', created_at) HAVING count(*) >= 2
             ORDER BY featured, date_trunc('year', created_at)",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| {
            json!({
                "keys": [r.get::<_, bool>(0), r.get::<_, serde_json::Value>(1)],
                "count": r.get::<_, i32>(2),
                "min": { "createdAt": r.get::<_, serde_json::Value>(3) },
                "max": { "createdAt": r.get::<_, serde_json::Value>(4) },
            })
        })
        .collect();
    assert!(!expected.is_empty());
    assert_eq!(conn["byYear"], json!(expected));
    assert_eq!(conn["none"], json!([]));

    // Nested connections group their own rows; no rows, no groups
    let data = run(
        &client,
        &schema,
        &catalog,
        r#"{ forums { name messagesConnection { totalCount groupedAggregates(groupBy: [FEATURED]) { keys count } } }
             empty: messagesConnection(condition: { body: "no such message" }) { groupedAggregates(groupBy: [FEATURED]) { count } } }"#,
    )
    .await
    .unwrap();
    for forum in data["data"]["forums"].as_array().unwrap() {
        let conn = &forum["messagesConnection"];
        let sum: i64 = conn["groupedAggregates"].as_array().unwrap().iter().map(|g| g["count"].as_i64().unwrap()).sum();
        assert_eq!(conn["totalCount"].as_i64().unwrap(), sum, "{forum}");
    }
    assert_eq!(data["data"]["empty"]["groupedAggregates"], json!([]));

    // Unknown groupings are rejected by the schema
    let err = run(&client, &schema, &catalog, "{ messagesConnection { groupedAggregates(groupBy: [BODY_TRUNCATED_TO_DAY]) { count } } }").await.unwrap_err();
    assert!(err.contains("BODY_TRUNCATED_TO_DAY"), "{err}");
}
