//! Schema-driven smoke tests against the database and schema configured in `.env`
//! (`DATABASE_URL`, `DATABASE_SCHEMA`) and the engine config the server would load
//! (`ENGINE_CONFIG`, else the workspace's `graphpg.yaml`) — the largest schema we test with.
//!
//! Nothing here names a table: queries are generated from the catalog and the built GraphQL
//! schema, so the same tests run against any database.
mod common;
use apollo_compiler::schema::ExtendedType;
use graphpg::catalog::{CatalogInspector, Polymorphism};
use graphpg::config::Config;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::{DatabaseCatalog, ExecutionEngine};
use tokio_postgres::NoTls;

struct Env {
    client: tokio_postgres::Client,
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: DatabaseCatalog,
    db_schema: String,
}

async fn setup() -> Env {
    common::ensure_forum_fixture().await;
    let url = common::db_url().to_string();
    let db_schema = std::env::var("DATABASE_SCHEMA").unwrap_or_else(|_| "app_public".into());
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .expect("Failed to connect to PostgreSQL");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    let config_path = std::env::var("ENGINE_CONFIG")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../graphpg.yaml").to_string());
    let config = match std::path::Path::new(&config_path).is_file() {
        true => Config::load_from_file(&config_path, graphpg::config::Mode::Prod)
            .expect("engine config"),
        false => Config::default(),
    };
    let started = std::time::Instant::now();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config(&db_schema, &config)
        .await
        .expect("inspection failed");
    println!(
        "inspected {} relations in {:?}",
        catalog.tables.len(),
        started.elapsed()
    );
    let schema = SchemaBuilder::build(&catalog).expect("Failed to build valid GraphQL schema");
    Env {
        client,
        schema,
        catalog,
        db_schema,
    }
}

impl Env {
    async fn execute(&self, query: &str) -> Result<serde_json::Value, String> {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &self.schema,
            query,
            "q.graphql",
        )
        .map_err(|e| format!("validation: {e}"))?;
        let plan = QueryPlanner::new(&self.catalog, &self.db_schema)
            .plan(&doc)
            .map_err(|e| format!("planning: {e}"))?;
        let result = ExecutionEngine::execute(&self.client, &plan)
            .await
            .map_err(|e| format!("execution: {e}"))?;
        Ok(result["data"].clone())
    }

    /// Selection set covering every field of `type_name` callable without arguments:
    /// scalars directly, connections as `{ totalCount }`, other composites as `{ __typename }`.
    fn full_selection(&self, type_name: &str) -> String {
        let fields = match self.schema.types.get(type_name) {
            Some(ExtendedType::Object(obj)) => &obj.fields,
            Some(ExtendedType::Interface(iface)) => &iface.fields,
            _ => return "{ __typename }".to_string(),
        };
        let mut parts = vec!["__typename".to_string()];
        for (name, field) in fields {
            if field.arguments.iter().any(|a| a.is_required()) {
                continue;
            }
            let inner = field.ty.inner_named_type();
            match self.schema.types.get(inner.as_str()) {
                Some(ExtendedType::Scalar(_) | ExtendedType::Enum(_)) => {
                    parts.push(name.to_string())
                }
                Some(ExtendedType::Object(o)) if o.fields.contains_key("totalCount") => {
                    parts.push(format!("{name} {{ totalCount }}"))
                }
                Some(_) => parts.push(format!("{name} {{ __typename }}")),
                None => {}
            }
        }
        format!("{{ {} }}", parts.join(" "))
    }
}

#[tokio::test]
async fn test_every_table_list_and_connection_executes() {
    let env = setup().await;
    let mut failures = Vec::new();
    let mut executed = 0;

    for table in &env.catalog.tables {
        let selection = env.full_selection(&table.type_name);
        let mut queries = Vec::new();
        if table.operations.connection {
            queries.push(format!(
                "{{ {}(first: 3) {{ totalCount pageInfo {{ hasNextPage endCursor }} nodes {selection} }} }}",
                table.connection_field
            ));
        }
        if table.operations.list {
            queries.push(format!("{{ {} {selection} }}", table.list_field));
        }
        for query in queries {
            executed += 1;
            if let Err(e) = env.execute(&query).await {
                failures.push(format!("{}: {e}\n  query: {query}", table.name));
            }
        }
    }

    println!(
        "executed {executed} queries over {} tables",
        env.catalog.tables.len()
    );
    assert!(
        failures.is_empty(),
        "{} failing queries:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Every exclusive-arc row resolves to the member type of its non-null arc column, and the
/// fields requested for that type (via inline fragments) come from the referenced row.
#[tokio::test]
async fn test_exclusive_arcs_resolve_to_referenced_rows() {
    let env = setup().await;
    let mut checked_rows = 0;

    for arc in &env.catalog.exclusive_arcs {
        let arc_meta = env
            .catalog
            .find_table(&env.db_schema, &arc.table_name)
            .unwrap();
        let arc_type = &arc_meta.type_name;
        let [pk] = arc_meta.primary_key.as_slice() else {
            continue;
        };
        if !arc_meta.operations.list {
            continue;
        }

        let fragments: Vec<String> = (0..arc.target_tables.len())
            .map(|i| format!("... on {} {{ databaseId }}", arc.target_type_name(i)))
            .collect();
        let arc_fields: Vec<String> = arc
            .target_columns
            .iter()
            .map(|c| arc_meta.get_column(c).unwrap().field_name.clone())
            .collect();
        let query = format!(
            "{{ rows: {} {{ databaseId {} target {{ __typename {} }} }} }}",
            arc_meta.list_field,
            arc_fields.join(" "),
            fragments.join(" ")
        );
        let data = env
            .execute(&query)
            .await
            .unwrap_or_else(|e| panic!("{arc_type}: {e}\n{query}"));

        for row in data["rows"].as_array().unwrap() {
            let set: Vec<usize> = (0..arc.target_columns.len())
                .filter(|&i| !row[&arc_fields[i]].is_null())
                .collect();
            assert_eq!(
                set.len(),
                1,
                "{arc_type} {pk}={}: arc must have exactly one target",
                row["databaseId"]
            );
            let i = set[0];
            // A polymorphic target resolves to one of its concrete types
            let target = &arc.target_tables[i];
            let expected_types = match env.catalog.find_polymorphism(&env.db_schema, target) {
                Some(poly) => poly.member_type_names(),
                None => vec![arc.target_type_name(i)],
            };
            let typename = row["target"]["__typename"].as_str().unwrap_or_default();
            assert!(
                expected_types.iter().any(|t| t == typename),
                "{arc_type} row {}: {typename}",
                row["databaseId"]
            );
            assert_eq!(
                row["target"]["databaseId"], row[&arc_fields[i]],
                "{arc_type} row {}",
                row["databaseId"]
            );
            checked_rows += 1;
        }
    }
    println!(
        "checked {checked_rows} arc rows across {} arcs",
        env.catalog.exclusive_arcs.len()
    );
}

/// Every row of a polymorphic table (an interface) resolves to one of its concrete types, and
/// its global ID resolves back to the same concrete type through `node(id:)`.
#[tokio::test]
async fn test_polymorphic_rows_round_trip_through_node() {
    let env = setup().await;
    let polys: Vec<Polymorphism> = env
        .catalog
        .polymorphisms()
        .filter(|p| p.schema() == env.db_schema)
        .collect();
    println!("{} polymorphic tables", polys.len());

    for poly in polys {
        let members = poly.member_type_names();
        let table = env.catalog.find_table(poly.schema(), poly.table()).unwrap();
        if !table.operations.connection {
            continue;
        }
        let conn = table.connection_field.clone();
        let query = format!("{{ {conn}(first: 20) {{ nodes {{ __typename id }} }} }}");
        let data = env
            .execute(&query)
            .await
            .unwrap_or_else(|e| panic!("{conn}: {e}\n{query}"));
        for row in data[&conn]["nodes"].as_array().unwrap() {
            let typename = row["__typename"].as_str().unwrap();
            assert!(
                members.iter().any(|m| m == typename),
                "{conn}: unexpected {typename}"
            );
            let id = row["id"].as_str().unwrap();
            let node = env
                .execute(&format!("{{ node(id: \"{id}\") {{ __typename id }} }}"))
                .await
                .unwrap();
            assert_eq!(
                node["node"], *row,
                "{conn}: node(id:) must return the same object"
            );
        }
    }
}

/// Cursor pagination over every table's key (single or composite): the page after
/// `endCursor` continues without repeating any row of the first page.
#[tokio::test]
async fn test_every_table_paginates_by_cursor() {
    let env = setup().await;
    let mut paged = 0;
    for table in env
        .catalog
        .tables
        .iter()
        .filter(|t| t.operations.connection)
    {
        let conn = &table.connection_field;
        let page = |after: Option<&str>| {
            let after = after
                .map(|c| format!(", after: \"{c}\""))
                .unwrap_or_default();
            format!("{{ {conn}(first: 2{after}) {{ edges {{ cursor }} pageInfo {{ hasNextPage endCursor }} }} }}")
        };
        let first = env
            .execute(&page(None))
            .await
            .unwrap_or_else(|e| panic!("{conn}: {e}"));
        let info = &first[&conn]["pageInfo"];
        if info["hasNextPage"] != true {
            continue;
        }
        let second = env
            .execute(&page(info["endCursor"].as_str()))
            .await
            .unwrap_or_else(|e| panic!("{conn} (after): {e}"));
        let cursors = |d: &serde_json::Value| -> Vec<String> {
            d[&conn]["edges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["cursor"].as_str().unwrap().to_string())
                .collect()
        };
        let (a, b) = (cursors(&first), cursors(&second));
        assert!(!b.is_empty(), "{conn}: empty page after endCursor");
        assert!(
            b.iter().all(|c| !a.contains(c)),
            "{conn}: page 2 repeats page 1 rows"
        );
        paged += 1;
    }
    println!("paginated {paged} tables");
}
