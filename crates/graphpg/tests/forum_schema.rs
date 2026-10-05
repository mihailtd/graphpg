mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use tokio_postgres::NoTls;

#[tokio::test]
async fn test_introspect_and_generate_schema_from_forum_database() {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    // 1. Inspect app_public schema created from Crystal test fixtures
    let inspector = CatalogInspector::new(&client);
    let catalog = inspector
        .inspect_schema("app_public")
        .await
        .expect("Failed to inspect app_public schema");

    println!("Catalog inspected: {} tables found", catalog.tables.len());
    assert!(!catalog.tables.is_empty(), "Expected tables in app_public");

    // Verify key tables exist
    let user_table = catalog
        .find_table("app_public", "users")
        .expect("users table should exist");
    assert_eq!(user_table.primary_key, vec!["id"]);

    let forum_table = catalog
        .find_table("app_public", "forums")
        .expect("forums table should exist");
    assert_eq!(forum_table.primary_key, vec!["id"]);

    let message_table = catalog
        .find_table("app_public", "messages")
        .expect("messages table should exist");
    assert_eq!(message_table.primary_key, vec!["id"]);
    assert_eq!(
        message_table.foreign_keys.len(),
        2,
        "Expected 2 foreign keys on messages"
    );

    // 2. Build GraphQL Schema via apollo-compiler
    let valid_schema =
        SchemaBuilder::build(&catalog).expect("Failed to build valid GraphQL schema");

    // 3. Verify types exist in apollo-compiler schema
    let schema_ref = valid_schema.as_ref();
    assert!(
        schema_ref.get_object("User").is_some(),
        "User GraphQL type should exist"
    );
    assert!(
        schema_ref.get_object("Forum").is_some(),
        "Forum GraphQL type should exist"
    );
    assert!(
        schema_ref.get_object("Message").is_some(),
        "Message GraphQL type should exist"
    );

    let query_type = schema_ref
        .get_object("Query")
        .expect("Query root type should exist");
    assert!(
        query_type.fields.contains_key("users"),
        "Query.users field should exist"
    );
    assert!(
        query_type.fields.contains_key("user"),
        "Query.user(id:) field should exist"
    );
    assert!(
        query_type.fields.contains_key("messages"),
        "Query.messages field should exist"
    );

    // 4. Validate GraphQL query AST with apollo-compiler against our generated schema
    let test_query = r#"
        query TestQuery {
            users {
                id
                username
            }
            messages {
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
        &valid_schema,
        test_query,
        "test_query.graphql",
    )
    .expect("Query should be valid according to apollo-compiler");

    assert_eq!(doc.operations.iter().count(), 1);

    // 5. Snapshot test the generated SDL
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    insta::assert_snapshot!("forum_app_public_schema", sdl);

    // ... and the OpenAPI document, which a change to the REST API must update on purpose
    // (`cargo insta review`, or `INSTA_UPDATE=always cargo test`): a spec change fails CI
    // until it is committed
    let config = graphpg::config::Config::default();
    let openapi = graphpg::OpenApiGenerator::generate_with_schema(
        &catalog,
        &config,
        "app_public",
        Some(&valid_schema),
    );
    insta::assert_json_snapshot!("forum_app_public_openapi", openapi);
}
