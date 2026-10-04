//! Row polymorphism exposed as GraphQL interfaces: Single Table Inheritance (discriminator
//! column), table-per-type inheritance (native `INHERITS` and shared-primary-key subtype
//! tables), and exclusive arcs whose targets may themselves be polymorphic.
//!
//! Uses the `interfaces_and_unions` fixture schema (tests/fixtures) plus a small
//! `polymorphism_fixture` schema created on demand.
mod common;
use graphpg::catalog::{CatalogInspector, CtiKind};
use graphpg::compiler::SqlCompiler;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::DatabaseCatalog;
use base64::Engine;
use serde_json::json;
use tokio_postgres::NoTls;


/// Created once (serialised by an advisory lock, since tests run concurrently).
const FIXTURE_SQL: &str = r#"
CREATE SCHEMA polymorphism_fixture;
SET search_path = polymorphism_fixture;

CREATE TABLE owners (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE shelters (id int PRIMARY KEY, name text NOT NULL);

-- Single Table Inheritance closed by a CHECK constraint
CREATE TABLE animals (
    id int PRIMARY KEY,
    kind text NOT NULL CHECK (kind IN ('dog', 'cat')),
    name text NOT NULL,
    breed text,
    indoor boolean,
    owner_id int REFERENCES owners(id)
);

-- Native INHERITS hierarchy
CREATE TABLE vehicles (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE cars (seats int NOT NULL, PRIMARY KEY (id)) INHERITS (vehicles);
CREATE TABLE trucks (payload_kg int NOT NULL, PRIMARY KEY (id)) INHERITS (vehicles);

-- Partitioning also uses pg_inherits, but is not polymorphism
CREATE TABLE readings (id int NOT NULL, taken_on date NOT NULL, PRIMARY KEY (id, taken_on)) PARTITION BY RANGE (taken_on);
CREATE TABLE readings_2026 PARTITION OF readings FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');

-- Exclusive arc: a note is about exactly one animal or shelter
CREATE TABLE notes (
    id int PRIMARY KEY,
    author_id int NOT NULL REFERENCES owners(id),
    animal_id int REFERENCES animals(id),
    shelter_id int REFERENCES shelters(id),
    body text NOT NULL,
    CHECK (num_nonnulls(animal_id, shelter_id) = 1)
);

INSERT INTO owners VALUES (1, 'Ada');
INSERT INTO shelters VALUES (1, 'Happy Paws');
INSERT INTO animals VALUES
    (1, 'dog', 'Rex', 'Beagle', NULL, 1),
    (2, 'cat', 'Tom', NULL, true, 1),
    (3, 'dog', 'Fido', 'Poodle', NULL, NULL);
INSERT INTO vehicles VALUES (1, 'Bike');
INSERT INTO cars VALUES (2, 'Mini', 4);
INSERT INTO trucks VALUES (3, 'Hauler', 9000);
INSERT INTO notes VALUES (1, 1, 1, NULL, 'Good boy'), (2, 1, NULL, 1, 'Nice staff');
"#;

async fn connect() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to local PostgreSQL 18.6");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    client
}

async fn ensure_fixture(client: &tokio_postgres::Client) {
    client.execute("SELECT pg_advisory_lock(424242)", &[]).await.unwrap();
    let exists: bool = client
        .query_one("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'polymorphism_fixture')", &[])
        .await
        .unwrap()
        .get(0);
    if !exists {
        client.batch_execute(&format!("BEGIN; {FIXTURE_SQL} COMMIT;")).await.expect("fixture setup failed");
    }
    client.execute("SELECT pg_advisory_unlock(424242)", &[]).await.unwrap();
}

async fn setup(schema_name: &str) -> (tokio_postgres::Client, apollo_compiler::validation::Valid<apollo_compiler::Schema>, DatabaseCatalog) {
    if schema_name == "interfaces_and_unions" {
        common::ensure_interfaces_fixture().await;
    }
    let client = connect().await;
    if schema_name == "polymorphism_fixture" {
        ensure_fixture(&client).await;
    }
    let catalog = CatalogInspector::new(&client).inspect_schema(schema_name).await.expect("inspection failed");
    let schema = SchemaBuilder::build(&catalog).expect("Failed to build valid GraphQL schema");
    (client, schema, catalog)
}

async fn run(
    client: &tokio_postgres::Client,
    schema: &apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: &DatabaseCatalog,
    db_schema: &str,
    query: &str,
) -> serde_json::Value {
    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(schema, query, "q.graphql").expect("Query validation failed");
    let plan = QueryPlanner::new(catalog, db_schema).plan(&doc).expect("Query planning failed");
    println!("SQL:\n{}", SqlCompiler::compile(&plan).sql);
    let result = ExecutionEngine::execute(client, &plan).await.expect("Execution failed");
    println!("Result:\n{}", serde_json::to_string_pretty(&result).unwrap());
    result["data"].clone()
}

/// Relay global ID for `<type_name>:<pk>`.
fn gid(type_name: &str, pk: i64) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{type_name}:{pk}"))
}

// ---- Detection & schema ---------------------------------------------------------------

#[tokio::test]
async fn test_detects_polymorphism_in_interfaces_and_unions() {
    let (_client, schema, catalog) = setup("interfaces_and_unions").await;

    // Enum-typed `type` column → STI. Variants would clash with other types (e.g. `Post`),
    // so the whole hierarchy is prefixed with the base type.
    let sti = catalog.find_sti("interfaces_and_unions", "single_table_items").expect("STI not detected");
    assert_eq!(sti.discriminator_column, "type");
    let names: Vec<&str> = sti.variants.iter().map(|v| v.type_name.as_str()).collect();
    assert_eq!(
        names,
        ["SingleTableItemTopic", "SingleTableItemPost", "SingleTableItemDivider", "SingleTableItemChecklist", "SingleTableItemChecklistItem"]
    );
    // NOT NULL discriminator: every row is one of the variants, no fallback type
    assert_eq!(sti.fallback_type_name, None);

    // Shared-primary-key subtype tables → table-per-type; they win over the `type` enum.
    let cti = catalog.find_cti("interfaces_and_unions", "relational_items").expect("CTI not detected");
    assert_eq!(cti.kind, CtiKind::SharedPrimaryKey);
    assert_eq!(cti.fallback_type_name, "GenericRelationalItem");
    let mut children: Vec<&str> = cti.children.iter().map(|c| c.table.as_str()).collect();
    children.sort();
    assert_eq!(
        children,
        ["relational_checklist_items", "relational_checklists", "relational_dividers", "relational_posts", "relational_topics"]
    );
    assert!(catalog.find_sti("interfaces_and_unions", "relational_items").is_none());
    assert!(catalog.find_cti("interfaces_and_unions", "union_items").is_some());

    // 1:1 extension tables with a single child and no discriminator are not subtypes
    assert!(catalog.find_cti("interfaces_and_unions", "people").is_none());

    let sdl = SchemaBuilder::generate_sdl(&catalog);
    assert!(sdl.contains("interface SingleTableItem implements Node {"), "{sdl}");
    assert!(sdl.contains("type SingleTableItemTopic implements Node & SingleTableItem {"));
    assert!(sdl.contains("interface RelationalItem implements Node {"));
    assert!(sdl.contains("type RelationalTopic implements Node & RelationalItem {"));
    assert!(sdl.contains("type GenericRelationalItem implements Node & RelationalItem {"));
    assert!(schema.types.contains_key("RelationalTopic"));
}

#[tokio::test]
async fn test_detects_check_sti_inherits_and_ignores_partitions() {
    let (_client, _schema, catalog) = setup("polymorphism_fixture").await;

    let sti = catalog.find_sti("polymorphism_fixture", "animals").expect("CHECK-based STI not detected");
    assert_eq!(sti.discriminator_column, "kind");
    let names: Vec<(&str, &str)> = sti.variants.iter().map(|v| (v.discriminator_value.as_str(), v.type_name.as_str())).collect();
    assert_eq!(names, [("dog", "Dog"), ("cat", "Cat")]);

    let cti = catalog.find_cti("polymorphism_fixture", "vehicles").expect("INHERITS not detected");
    assert_eq!(cti.kind, CtiKind::Inherits);
    assert_eq!(cti.children.len(), 2);

    assert!(catalog.find_cti("polymorphism_fixture", "readings").is_none(), "partitions are not subtypes");

    let sdl = SchemaBuilder::generate_sdl(&catalog);
    assert!(sdl.contains("interface Animal implements Node {"), "{sdl}");
    assert!(sdl.contains("type Dog implements Node & Animal {"));
    assert!(sdl.contains("type Cat implements Node & Animal {"));
    assert!(!sdl.contains("GenericAnimal"), "`kind` is NOT NULL, so no fallback type");
    assert!(sdl.contains("interface Vehicle implements Node {"));
    assert!(sdl.contains("type Car implements Node & Vehicle {"));
    assert!(sdl.contains("type GenericVehicle implements Node & Vehicle {"));
    // Unions cannot contain interfaces: a polymorphic arc target contributes its concrete types
    assert!(sdl.contains("union NotesTarget = Dog | Cat | Shelter"));
    // Root fields keep returning the (now interface) type
    assert!(sdl.contains("  animals: [Animal!]!"));
}

// ---- Single Table Inheritance -----------------------------------------------------------

#[tokio::test]
async fn test_sti_rows_resolve_to_concrete_types_everywhere() {
    let (client, schema, catalog) = setup("polymorphism_fixture").await;
    let data = run(
        &client,
        &schema,
        &catalog,
        "polymorphism_fixture",
        r#"{
            animalsConnection(orderBy: [ID_ASC]) {
                nodes {
                    __typename
                    name
                    ... on Dog { breed }
                    ... on Cat { indoor owner { name } }
                }
            }
            tom: animal(id: 2) { __typename id }
            viaNode: node(id: "QW5pbWFsOjE=") { __typename id ... on Animal { name } ... on Dog { breed } }
        }"#,
    )
    .await;
    assert_eq!(
        data["animalsConnection"]["nodes"],
        json!([
            { "__typename": "Dog", "name": "Rex", "breed": "Beagle" },
            { "__typename": "Cat", "name": "Tom", "indoor": true, "owner": { "name": "Ada" } },
            { "__typename": "Dog", "name": "Fido", "breed": "Poodle" }
        ])
    );
    // Global IDs name the interface, so a row has a single ID and `node(id:)` resolves it
    assert_eq!(data["tom"], json!({ "__typename": "Cat", "id": gid("Animal", 2) }));
    assert_eq!(data["viaNode"], json!({ "__typename": "Dog", "id": gid("Animal", 1), "name": "Rex", "breed": "Beagle" }));
}

#[tokio::test]
async fn test_sti_through_relation() {
    let (client, schema, catalog) = setup("polymorphism_fixture").await;
    let data = run(
        &client,
        &schema,
        &catalog,
        "polymorphism_fixture",
        r#"{ owner(id: 1) { name animals { __typename name ... on Cat { indoor } } } }"#,
    )
    .await;
    let mut animals = data["owner"]["animals"].as_array().unwrap().clone();
    animals.sort_by_key(|a| a["name"].as_str().unwrap().to_string());
    assert_eq!(
        animals,
        vec![
            json!({ "__typename": "Dog", "name": "Rex" }),
            json!({ "__typename": "Cat", "name": "Tom", "indoor": true }),
        ]
    );
}

#[tokio::test]
async fn test_sti_enum_discriminator_fixture() {
    let (client, schema, catalog) = setup("interfaces_and_unions").await;
    let data = run(
        &client,
        &schema,
        &catalog,
        "interfaces_and_unions",
        r#"{
            singleTableItemsConnection(orderBy: [ID_ASC], first: 4) {
                nodes {
                    __typename
                    ... on SingleTableItemTopic { title }
                    ... on SingleTableItemDivider { title color }
                }
            }
        }"#,
    )
    .await;
    assert_eq!(
        data["singleTableItemsConnection"]["nodes"],
        json!([
            { "__typename": "SingleTableItemTopic", "title": "PostGraphile version 5" },
            { "__typename": "SingleTableItemTopic", "title": "Temporary test topic" },
            { "__typename": "SingleTableItemDivider", "title": "Headline features", "color": "green" },
            { "__typename": "SingleTableItemPost" }
        ])
    );
}

#[tokio::test]
async fn test_polymorphic_mutation_payload() {
    let (client, schema, catalog) = setup("polymorphism_fixture").await;
    client.batch_execute("BEGIN").await.unwrap();
    let data = run(
        &client,
        &schema,
        &catalog,
        "polymorphism_fixture",
        r#"mutation { createAnimal(input: { id: 99, kind: CAT, name: "Kit" }) { animal { __typename name ... on Cat { indoor } } } }"#,
    )
    .await;
    client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(data["createAnimal"]["animal"], json!({ "__typename": "Cat", "name": "Kit", "indoor": null }));
}

/// Fields selected on the interface are planned for every concrete type, but the relation
/// behind them is joined once for the row, not once per type.
#[tokio::test]
async fn test_interface_fields_join_once_per_row() {
    let (client, schema, catalog) = setup("polymorphism_fixture").await;
    let q = r#"{ animals { __typename name owner { name } notes { body } } }"#;
    let doc = apollo_compiler::ExecutableDocument::parse_and_validate(&schema, q, "q.graphql").unwrap();
    let plan = QueryPlanner::new(&catalog, "polymorphism_fixture").plan(&doc).unwrap();
    let sql = SqlCompiler::compile(&plan).sql;
    println!("{sql}");
    assert_eq!(sql.matches("\"polymorphism_fixture\".\"owners\"").count(), 1, "{sql}");
    assert_eq!(sql.matches("LEFT JOIN LATERAL").count(), 1, "notes lateral once: {sql}");

    let data = run(&client, &schema, &catalog, "polymorphism_fixture", q).await;
    let mut animals = data["animals"].as_array().unwrap().clone();
    animals.sort_by_key(|a| a["name"].as_str().unwrap().to_string());
    assert_eq!(
        animals,
        vec![
            json!({ "__typename": "Dog", "name": "Fido", "owner": null, "notes": [] }),
            json!({ "__typename": "Dog", "name": "Rex", "owner": { "name": "Ada" }, "notes": [{ "body": "Good boy" }] }),
            json!({ "__typename": "Cat", "name": "Tom", "owner": { "name": "Ada" }, "notes": [] }),
        ]
    );
}

// ---- Table-per-type -------------------------------------------------------------------

#[tokio::test]
async fn test_cti_native_inherits() {
    let (client, schema, catalog) = setup("polymorphism_fixture").await;
    let data = run(
        &client,
        &schema,
        &catalog,
        "polymorphism_fixture",
        r#"{
            vehiclesConnection(orderBy: [ID_ASC]) {
                nodes {
                    __typename
                    id
                    name
                    ... on Car { seats }
                    ... on Truck { payloadKg }
                }
            }
            cars { __typename id name seats }
        }"#,
    )
    .await;
    assert_eq!(
        data["vehiclesConnection"]["nodes"],
        json!([
            { "__typename": "GenericVehicle", "id": gid("Vehicle", 1), "name": "Bike" },
            { "__typename": "Car", "id": gid("Vehicle", 2), "name": "Mini", "seats": 4 },
            { "__typename": "Truck", "id": gid("Vehicle", 3), "name": "Hauler", "payloadKg": 9000 }
        ])
    );
    // The child table's own root returns the same object with the same ID
    assert_eq!(data["cars"], json!([{ "__typename": "Car", "id": gid("Vehicle", 2), "name": "Mini", "seats": 4 }]));
}

#[tokio::test]
async fn test_cti_shared_primary_key_fixture() {
    let (client, schema, catalog) = setup("interfaces_and_unions").await;
    let data = run(
        &client,
        &schema,
        &catalog,
        "interfaces_and_unions",
        r#"{
            relationalItemsConnection(orderBy: [ID_ASC], first: 6) {
                nodes {
                    __typename
                    type
                    ... on RelationalTopic { topicTitle: title }
                    ... on RelationalDivider { title color }
                    ... on RelationalPost { postTitle: title description }
                }
            }
            topic: relationalTopic(id: 1) { __typename id databaseId type title author { __typename } }
        }"#,
    )
    .await;
    assert_eq!(
        data["relationalItemsConnection"]["nodes"],
        json!([
            { "__typename": "RelationalTopic", "type": "TOPIC", "topicTitle": "PostGraphile version 5" },
            { "__typename": "RelationalTopic", "type": "TOPIC", "topicTitle": "Temporary test topic" },
            { "__typename": "RelationalDivider", "type": "DIVIDER", "title": "Headline features", "color": "green" },
            { "__typename": "RelationalPost", "type": "POST", "postTitle": "Better planning", "description": null },
            { "__typename": "RelationalPost", "type": "POST", "postTitle": "Easier to code", "description": null },
            { "__typename": "RelationalPost", "type": "POST", "postTitle": "More features", "description": "E.g. interfaces and unions" }
        ])
    );
    // Queried through its own table, a child still exposes the parent's fields and ID
    assert_eq!(
        data["topic"],
        json!({
            "__typename": "RelationalTopic",
            "id": gid("RelationalItem", 1),
            "databaseId": 1,
            "type": "TOPIC",
            "title": "PostGraphile version 5",
            "author": { "__typename": "People" }
        })
    );
}

// ---- Exclusive arcs ---------------------------------------------------------------------

#[tokio::test]
async fn test_exclusive_arc_with_polymorphic_target() {
    let (client, schema, catalog) = setup("polymorphism_fixture").await;
    let data = run(
        &client,
        &schema,
        &catalog,
        "polymorphism_fixture",
        r#"{
            notes {
                body
                target {
                    __typename
                    ... on Animal { name }
                    ... on Dog { breed }
                    ... on Shelter { name }
                }
            }
        }"#,
    )
    .await;
    let mut notes = data["notes"].as_array().unwrap().clone();
    notes.sort_by_key(|n| n["body"].as_str().unwrap().to_string());
    assert_eq!(
        notes,
        vec![
            json!({ "body": "Good boy", "target": { "__typename": "Dog", "name": "Rex", "breed": "Beagle" } }),
            json!({ "body": "Nice staff", "target": { "__typename": "Shelter", "name": "Happy Paws" } }),
        ]
    );
}
