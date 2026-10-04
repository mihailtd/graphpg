mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::config::{EngineConfig, FederationConfig, RbacConfig};
use graphpg::schema::SchemaBuilder;
use tokio_postgres::NoTls;


async fn setup_test_roles(client: &tokio_postgres::Client) {
    let sql = r#"
        DO $$
        BEGIN
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rbac_reader_role') THEN
                CREATE ROLE rbac_reader_role;
            END IF;
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rbac_writer_role') THEN
                CREATE ROLE rbac_writer_role;
            END IF;
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rbac_col_role') THEN
                CREATE ROLE rbac_col_role;
            END IF;
        END
        $$;

        -- Reset permissions
        REVOKE ALL ON SCHEMA app_public FROM rbac_reader_role, rbac_writer_role, rbac_col_role;
        REVOKE ALL ON ALL TABLES IN SCHEMA app_public FROM rbac_reader_role, rbac_writer_role, rbac_col_role;

        -- 1. rbac_reader_role: SELECT only on users (no mutations, no forums, no messages)
        GRANT USAGE ON SCHEMA app_public TO rbac_reader_role;
        GRANT SELECT ON app_public.users TO rbac_reader_role;

        -- 2. rbac_writer_role: SELECT, INSERT, UPDATE, DELETE on forums only
        GRANT USAGE ON SCHEMA app_public TO rbac_writer_role;
        GRANT SELECT, INSERT, UPDATE, DELETE ON app_public.forums TO rbac_writer_role;

        -- 3. rbac_col_role: SELECT on only (id, name) on forums
        GRANT USAGE ON SCHEMA app_public TO rbac_col_role;
        GRANT SELECT (id, name) ON app_public.forums TO rbac_col_role;
    "#;
    client.batch_execute(sql).await.expect("Failed to setup test roles");
}

static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn ensure_test_roles(client: &tokio_postgres::Client) {
    INIT.get_or_init(|| async {
        setup_test_roles(client).await;
    })
    .await;
}

#[tokio::test]
async fn test_rbac_read_only_table_has_no_mutations_and_prunes_other_tables() {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    ensure_test_roles(&client).await;

    let inspector = CatalogInspector::new(&client);
    let mut config = EngineConfig::default();
    config.rbac = RbacConfig {
        enabled: true,
        role: Some("rbac_reader_role".to_string()),
    };

    let catalog = inspector
        .inspect_schema_with_config("app_public", &config)
        .await
        .expect("Failed to inspect schema with rbac config");

    let sdl = SchemaBuilder::generate_sdl(&catalog);

    // 1. User should be present for reading
    assert!(sdl.contains("type User"), "User type should exist in SDL");
    assert!(sdl.contains("users: [User!]!"), "Query.users should exist");
    assert!(sdl.contains("user(id: UUID!): User"), "Query.user should exist");

    // 2. Unpermitted tables (Forums, Messages) should be completely pruned
    assert!(!sdl.contains("type Forum"), "Forum should be pruned for rbac_reader_role");
    assert!(!sdl.contains("type Message"), "Message should be pruned for rbac_reader_role");
    assert!(!sdl.contains("forums:"), "Query.forums should not exist");
    assert!(!sdl.contains("messages:"), "Query.messages should not exist");

    // 3. User mutations must NOT exist (read-only role)
    assert!(!sdl.contains("createUser("), "createUser mutation must not exist");
    assert!(!sdl.contains("updateUser("), "updateUser mutation must not exist");
    assert!(!sdl.contains("deleteUser("), "deleteUser mutation must not exist");

    // Validates with apollo-compiler
    let schema = SchemaBuilder::build(&catalog).expect("Schema must validate");
    let schema_ref = schema.as_ref();
    assert!(schema_ref.get_object("User").is_some());
    assert!(schema_ref.get_object("Forum").is_none());
}

#[tokio::test]
async fn test_rbac_writer_role_has_full_mutations_for_permitted_table() {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    ensure_test_roles(&client).await;

    let inspector = CatalogInspector::new(&client);
    let mut config = EngineConfig::default();
    config.rbac = RbacConfig {
        enabled: true,
        role: Some("rbac_writer_role".to_string()),
    };

    let catalog = inspector
        .inspect_schema_with_config("app_public", &config)
        .await
        .expect("Failed to inspect schema with rbac config");

    let sdl = SchemaBuilder::generate_sdl(&catalog);

    // 1. Forum should be present with read queries and mutations
    assert!(sdl.contains("type Forum"), "Forum type should exist in SDL");
    assert!(sdl.contains("forums: [Forum!]!"), "Query.forums should exist");
    assert!(sdl.contains("createForum("), "createForum mutation must exist");
    assert!(sdl.contains("updateForum("), "updateForum mutation must exist");
    assert!(sdl.contains("deleteForum("), "deleteForum mutation must exist");

    // 2. Users and Messages must be pruned
    assert!(!sdl.contains("type User"), "User should be pruned for rbac_writer_role");
    assert!(!sdl.contains("type Message"), "Message should be pruned for rbac_writer_role");

    // Validates cleanly with apollo-compiler
    let schema = SchemaBuilder::build(&catalog).expect("Schema must validate");
    let schema_ref = schema.as_ref();
    assert!(schema_ref.get_object("Forum").is_some());
    assert!(schema_ref.get_object("User").is_none());
}

#[tokio::test]
async fn test_rbac_column_level_pruning() {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    ensure_test_roles(&client).await;

    let inspector = CatalogInspector::new(&client);
    let mut config = EngineConfig::default();
    config.rbac = RbacConfig {
        enabled: true,
        role: Some("rbac_col_role".to_string()),
    };

    let catalog = inspector
        .inspect_schema_with_config("app_public", &config)
        .await
        .expect("Failed to inspect schema with rbac config");

    let forum = catalog.find_table("app_public", "forums").expect("forums table found");
    let visible_cols: Vec<_> = forum.visible_columns().map(|c| c.name.as_str()).collect();

    // Permitted columns: id, name
    assert!(visible_cols.contains(&"id"), "id column should be visible");
    assert!(visible_cols.contains(&"name"), "name column should be visible");

    // Non-permitted columns (e.g. description, created_at, updated_at) must be omitted
    assert!(!visible_cols.contains(&"description"), "description should be pruned");

    let schema = SchemaBuilder::build(&catalog).expect("Schema must validate");
    let schema_ref = schema.as_ref();
    let forum_obj = schema_ref.get_object("Forum").expect("Forum object");
    assert!(forum_obj.fields.contains_key("name"));
    assert!(!forum_obj.fields.contains_key("description"));
}

#[tokio::test]
async fn test_rbac_and_federation_combined() {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    ensure_test_roles(&client).await;

    let inspector = CatalogInspector::new(&client);
    let mut config = EngineConfig::default();
    config.rbac = RbacConfig {
        enabled: true,
        role: Some("rbac_reader_role".to_string()),
    };
    config.federation = FederationConfig {
        enabled: true,
        version: Some("v2.3".to_string()),
    };

    let catalog = inspector
        .inspect_schema_with_config("app_public", &config)
        .await
        .expect("Failed to inspect schema with rbac and federation config");

    let subgraph_sdl = SchemaBuilder::generate_federation_subgraph_sdl(&catalog);

    // 1. Subgraph SDL has Federation v2 link
    assert!(subgraph_sdl.contains("https://specs.apollo.dev/federation/v2.3"));

    // 2. User entity is present and decorated with @key
    assert!(subgraph_sdl.contains("type User"));
    assert!(subgraph_sdl.contains("@key(fields: \"id\")"));

    // 3. Forum and Message are completely pruned from this subgraph
    assert!(!subgraph_sdl.contains("type Forum"));
    assert!(!subgraph_sdl.contains("type Message"));

    // 4. Runtime schema _Entity union contains ONLY permitted entities
    let runtime_sdl = SchemaBuilder::generate_sdl(&catalog);
    assert!(runtime_sdl.contains("union _Entity = User\n"));
    assert!(!runtime_sdl.contains("Forum"));

    let schema = SchemaBuilder::build(&catalog).expect("Schema must validate");
    assert!(schema.as_ref().get_object("User").is_some());
    assert!(schema.as_ref().get_object("Forum").is_none());
}
