//! Foreign data wrappers as sources (docs/foreign-data.md): a `postgres_fdw` foreign table
//! (another PostgreSQL server; here a loopback connection) and a `file_fdw` one (a CSV file),
//! given keys and relations in `graphpg.yaml`, joined with local tables in one statement.
mod common;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use tower::ServiceExt;

const SCHEMA: &str = "fdw_app";

/// Keys and relations PostgreSQL cannot declare on foreign tables (or on a materialized view).
const CONFIG: &str = r#"
entities:
  invoices:                        # postgres_fdw
    primary_key: [id]
    foreign_keys:
      invoices_customer:
        columns: [customer_id]
        references: { table: customers }
        field_name: customer
        reverse_field_name: invoices
  countries:                       # file_fdw
    primary_key: [code]
  country_list:                    # a materialized view over the file
    primary_key: [code]
  customers:
    foreign_keys:
      customers_country:
        columns: [country_code]
        references: { table: country_list, columns: [code] }
        field_name: country
        reverse_field_name: customers
      customers_country_file:
        columns: [country_code]
        references: { table: countries, columns: [code] }
        field_name: countryFromFile
        omit_reverse: true
"#;

/// The foreign tables reachable only through their relations: no root fields.
const HIDDEN: &str = r#"
    omit_operations: [read, list, connection, create, update, delete]
"#;

/// Writes the CSV `countries` reads (on the database server) and fills the materialized view.
async fn prepare() {
    static ONCE: OnceCell<()> = OnceCell::const_new();
    ONCE.get_or_init(|| async {
        common::ensure_foreign_data_fixture().await;
        let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
            .batch_execute(
                "COPY (VALUES ('RO', 'Romania'), ('DE', 'Germany')) \
                 TO '/tmp/graphpg_fdw_countries.csv' WITH (FORMAT csv, HEADER); \
                 REFRESH MATERIALIZED VIEW fdw_app.country_list; \
                 ANALYZE fdw_app.countries;",
            )
            .await
            .unwrap();
    })
    .await;
}

async fn app_with(config: &str) -> axum::Router {
    prepare().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let config = Config::from_yaml(config).unwrap();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        schema,
        catalog,
        SCHEMA.to_string(),
        DebugOptions::default(),
    )
    .with_config(config)
    .with_introspection(true);
    create_router(Arc::new(state))
}

async fn app() -> axum::Router {
    app_with(CONFIG).await
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(request).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn graphql(app: &axum::Router, query: &str) -> Value {
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();
    let (status, body) = send(app, request).await;
    assert_eq!(status, StatusCode::OK, "{query}: {body}");
    assert!(body.get("errors").is_none(), "{query}: {body}");
    body["data"].clone()
}

async fn sdl(app: &axum::Router) -> String {
    let data = graphql(
        app,
        "{ __schema { queryType { fields { name } } mutationType { fields { name } } } }",
    )
    .await;
    data.to_string()
}

#[tokio::test]
async fn a_remote_table_joins_local_rows_both_ways() {
    let app = app().await;
    // Local -> foreign, nested in one statement
    let data = graphql(
        &app,
        "{ customer(id: 1) { name invoicesConnection(orderBy: [ID_ASC]) { totalCount nodes { amount status } } } }",
    )
    .await;
    assert_eq!(
        data["customer"],
        json!({ "name": "Acme", "invoicesConnection": { "totalCount": 2, "nodes": [
            { "amount": "100.00", "status": "PAID" },
            { "amount": "250.00", "status": "OPEN" }
        ] } })
    );
    // Foreign -> local
    let data = graphql(&app, "{ invoice(id: 3) { amount customer { name } } }").await;
    assert_eq!(
        data["invoice"],
        json!({ "amount": "75.50", "customer": { "name": "Globex" } })
    );
}

#[tokio::test]
async fn filters_and_aggregates_reach_into_the_remote_table() {
    let app = app().await;
    let data = graphql(
        &app,
        r#"{ customersConnection(orderBy: [ID_ASC], filter: { invoices: { some: { status: { equalTo: "OPEN" } } } }) { nodes { name } } }"#,
    )
    .await;
    let names: Vec<&str> = data["customersConnection"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Acme", "Globex"]);

    let data = graphql(
        &app,
        r#"{ invoicesConnection(filter: { customerId: { in: [1, 2] } }) { aggregates { sum { amount } } } }"#,
    )
    .await;
    assert_eq!(
        data["invoicesConnection"]["aggregates"]["sum"]["amount"],
        425.5
    );
}

#[tokio::test]
async fn writes_go_through_postgres_fdw() {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let count = || async {
        client
            .query_one(
                "SELECT count(*) FROM fdw_remote.invoices WHERE id = 900",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
    };
    let app = app().await;
    client
        .batch_execute("DELETE FROM fdw_remote.invoices WHERE id = 900")
        .await
        .unwrap();

    let create = r#"mutation { createInvoice(input: { id: 900, customerId: 3, amount: "9.99", status: "DRAFT" }) { invoice { databaseId customer { name } } } }"#;
    let data = graphql(&app, create).await;
    assert_eq!(
        data["createInvoice"]["invoice"],
        json!({ "databaseId": 900, "customer": { "name": "Initech" } })
    );
    assert_eq!(count().await, 1, "the row is on the remote side");

    // A remote constraint has no local definition, and postgres_fdw passes on its SQLSTATE
    // but not its name: no typed code (`ID_TAKEN`), masked like any database error
    let request = Request::post("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": create }).to_string()))
        .unwrap();
    let (_, body) = send(&app, request).await;
    assert_eq!(
        body["errors"][0]["extensions"]["code"], "INTERNAL_SERVER_ERROR",
        "{body}"
    );
    assert_eq!(count().await, 1, "the failed write changed nothing");

    let id = SchemaBuilder::encode_global_id("Invoice", "900");
    graphql(
        &app,
        &format!(r#"mutation {{ deleteInvoice(id: "{id}") {{ invoice {{ databaseId }} }} }}"#),
    )
    .await;
    assert_eq!(count().await, 0);
}

#[tokio::test]
async fn a_file_is_read_only_and_joins_directly_or_through_a_materialized_view() {
    let app = app().await;
    let data = graphql(
        &app,
        "{ countriesConnection(orderBy: [CODE_ASC]) { nodes { databaseId name } } }",
    )
    .await;
    assert_eq!(
        data["countriesConnection"]["nodes"],
        json!([{ "databaseId": "DE", "name": "Germany" }, { "databaseId": "RO", "name": "Romania" }])
    );
    // No writes are generated for a file
    let schema = sdl(&app).await;
    assert!(!schema.contains("createCountry"), "{schema}");
    assert!(schema.contains("createInvoice"));

    // Both relations answer the same; the materialized view has real indexes
    let data = graphql(
        &app,
        "{ customer(id: 2) { country { name } countryFromFile { name } } }",
    )
    .await;
    assert_eq!(
        data["customer"],
        json!({ "country": { "name": "Germany" }, "countryFromFile": { "name": "Germany" } })
    );
    let data = graphql(
        &app,
        r#"{ countryListById(code: "RO") { customersConnection(orderBy: [ID_ASC]) { nodes { name } } } }"#,
    )
    .await;
    assert_eq!(
        data["countryListById"]["customersConnection"]["nodes"],
        json!([{ "name": "Acme" }, { "name": "Initech" }])
    );
}

/// The recommended exposure when RLS matters: no root fields on the foreign table (RLS
/// cannot be enabled on one); its rows are reached through the local rows the request may
/// see, whose policies apply.
#[tokio::test]
async fn hidden_foreign_tables_are_reached_only_through_their_parents() {
    let config = CONFIG.replace(
        "  invoices:                        # postgres_fdw\n    primary_key: [id]\n",
        &format!("  invoices:\n    primary_key: [id]{HIDDEN}"),
    );
    assert_ne!(config, CONFIG, "the config was not changed");
    let app = app_with(&config).await;

    let schema = sdl(&app).await;
    for field in [
        "\"invoices\"",
        "\"invoice\"",
        "\"invoicesConnection\"",
        "createInvoice",
    ] {
        assert!(
            !schema.contains(field),
            "{field} is still exposed: {schema}"
        );
    }
    // Not by a forged global ID either
    let id = SchemaBuilder::encode_global_id("Invoice", "1");
    let data = graphql(&app, &format!(r#"{{ node(id: "{id}") {{ id }} }}"#)).await;
    assert_eq!(data["node"], Value::Null);
    // Through the parent, as before
    let data = graphql(&app, "{ customer(id: 2) { invoices { amount } } }").await;
    assert_eq!(data["customer"]["invoices"], json!([{ "amount": "75.50" }]));

    // REST: no collection of its own, embedded under the parent
    let (status, _) = send(
        &app,
        Request::get("/rest/invoices").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(
        matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
        "{status}"
    );
    let (status, body) = send(
        &app,
        Request::get("/rest/customers/2?include=invoices")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["invoices"][0]["amount"], "75.50", "{body}");
}

#[tokio::test]
async fn rest_lists_and_embeds_foreign_rows() {
    let app = app().await;
    let (status, body) = send(
        &app,
        Request::get("/rest/invoices?filter[status][eq]=OPEN&order=id")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<i64> = body
        .as_array()
        .unwrap_or_else(|| body["data"].as_array().unwrap())
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert!(ids.starts_with(&[2, 3]), "{body}");
}
