//! What the OpenAPI document infers from the database: JSON Schema keywords from `CHECK`
//! constraints, discriminators from Single Table Inheritance, and `links` from foreign keys.
mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::openapi::OpenApiGenerator;
use serde_json::{json, Value};
use tokio_postgres::NoTls;

async fn document(schema: &str) -> Value {
    match schema {
        "rest_extras" => common::ensure_rest_extras_fixture().await,
        _ => common::ensure_forum_fixture().await,
    }
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema(schema)
        .await
        .unwrap();
    OpenApiGenerator::generate_for(&catalog, &Config::default(), schema)
}

#[tokio::test]
async fn check_constraints_become_json_schema_keywords() {
    let doc = document("rest_extras").await;
    let row = &doc["components"]["schemas"]["bounded"]["properties"];
    assert_eq!(row["score"]["minimum"], 0);
    assert_eq!(row["score"]["maximum"], 100);
    assert_eq!(row["step"]["multipleOf"], 5);
    assert_eq!(row["rating"]["exclusiveMinimum"], 0);
    assert_eq!(row["rating"]["exclusiveMaximum"], 5);
    assert_eq!(row["code"]["pattern"], "^[A-Z]{3}$");
    assert_eq!(row["title"]["minLength"], 3);
    assert_eq!(row["title"]["maxLength"], 20);
    assert_eq!(row["tags"]["maxItems"], 5);
    // an OR is not read: the database still checks it
    assert!(row["other"].get("minimum").is_none(), "{}", row["other"]);

    // the same keywords on what a client sends
    let create = &doc["components"]["schemas"]["boundedCreateInput"]["properties"];
    assert_eq!(create["score"]["maximum"], 100);
    let update = &doc["components"]["schemas"]["boundedUpdateInput"]["properties"];
    assert_eq!(update["code"]["pattern"], "^[A-Z]{3}$");
}

#[tokio::test]
async fn a_single_table_hierarchy_is_a_one_of_with_a_discriminator() {
    let doc = document("rest_extras").await;
    let schemas = &doc["components"]["schemas"];
    let pets = &schemas["pets"];
    assert_eq!(pets["discriminator"]["propertyName"], "kind");
    let mapping = pets["discriminator"]["mapping"].as_object().unwrap();
    assert_eq!(mapping.len(), 2, "{pets}");
    let members = pets["oneOf"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    for (value, target) in mapping {
        let name = target.as_str().unwrap().rsplit('/').next().unwrap();
        let variant = &schemas[name];
        assert_eq!(
            variant["allOf"][0]["$ref"],
            "#/components/schemas/pets.Base"
        );
        assert_eq!(variant["allOf"][1]["properties"]["kind"]["const"], *value);
    }
    // the base is the plain object, and everything that refers to `pets` gets the union
    assert_eq!(schemas["pets.Base"]["type"], "object");
    let list = &doc["paths"]["/rest/pets"]["get"]["responses"]["200"]["content"]
        ["application/json"]["schema"]["items"];
    assert_eq!(list["$ref"], "#/components/schemas/pets");
    // a table that is not a hierarchy has no union
    assert!(schemas["bounded"].get("oneOf").is_none());
}

#[tokio::test]
async fn foreign_keys_become_links() {
    let doc = document("app_public").await;
    let links = &doc["paths"]["/rest/messages/{key}"]["get"]["responses"]["200"]["links"];
    // to the rows this one points at: the key is read from the response
    assert_eq!(links["forums"]["operationId"], "get_forums_by_key");
    assert_eq!(
        links["forums"]["parameters"]["key"],
        "$response.body#/forumId"
    );
    assert_eq!(links["users"]["operationId"], "get_users_by_key");
    assert_eq!(
        links["users"]["parameters"]["key"],
        "$response.body#/authorId"
    );

    // to the rows that point at this one: a search filtered by this row's key
    let links = &doc["paths"]["/rest/forums/{key}"]["get"]["responses"]["200"]["links"];
    assert_eq!(links["messages"]["operationId"], "search_messages");
    assert_eq!(
        links["messages"]["requestBody"],
        json!({ "filter": { "forumId": { "eq": "$response.body#/id" } } })
    );
    // every link names an operation of the document
    let mut operations = std::collections::HashSet::new();
    for item in doc["paths"].as_object().unwrap().values() {
        for op in item.as_object().unwrap().values() {
            if let Some(id) = op["operationId"].as_str() {
                operations.insert(id.to_string());
            }
        }
    }
    for item in doc["paths"].as_object().unwrap().values() {
        for op in item.as_object().unwrap().values() {
            for response in op["responses"]
                .as_object()
                .into_iter()
                .flat_map(|r| r.values())
            {
                for link in response["links"]
                    .as_object()
                    .into_iter()
                    .flat_map(|l| l.values())
                {
                    let id = link["operationId"].as_str().unwrap();
                    assert!(operations.contains(id), "{id} is not an operation");
                }
            }
        }
    }
    // HEAD has no body, so no links
    assert!(
        doc["paths"]["/rest/forums/{key}"]["head"]["responses"]["200"]
            .get("links")
            .is_none()
    );
}

/// The document as served and exported: with the GraphQL schema, whose input types are the
/// create and update bodies.
async fn served_document(setup: &str, schema: &str) -> Value {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute("SELECT pg_advisory_lock(7461023)")
        .await
        .unwrap();
    client.batch_execute(setup).await.unwrap();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema(schema)
        .await
        .unwrap();
    client
        .batch_execute("SELECT pg_advisory_unlock(7461023)")
        .await
        .unwrap();
    let graphql = graphpg::schema::SchemaBuilder::build(&catalog).unwrap();
    OpenApiGenerator::generate_with_schema(&catalog, &Config::default(), schema, Some(&graphql))
}

const CONTRACT_FIXTURE: &str = "
DROP SCHEMA IF EXISTS openapi_contract CASCADE; CREATE SCHEMA openapi_contract;
CREATE DOMAIN openapi_contract.fhir_date AS text;
CREATE TYPE openapi_contract.state AS ENUM ('in progress', 'done');
CREATE TABLE openapi_contract.patients (
    id integer PRIMARY KEY,
    birth_date openapi_contract.fhir_date CHECK (birth_date ~ '^[0-9]{4}(-[0-9]{2})?$'),
    code varchar(10) CHECK (code ~ '^[A-Z]+$'),
    nickname varchar(20) CHECK (char_length(nickname) >= 2),
    score integer CHECK (score BETWEEN 0 AND 100),
    enabled boolean NOT NULL DEFAULT false,
    rank integer DEFAULT 5,
    status text DEFAULT 'active',
    state openapi_contract.state NOT NULL DEFAULT 'in progress',
    big bigint DEFAULT 9,
    token uuid DEFAULT gen_random_uuid(),
    created timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE openapi_contract.visits (
    id integer PRIMARY KEY,
    patient_id integer REFERENCES openapi_contract.patients,
    reason varchar(40) CHECK (char_length(reason) >= 3)
);";

#[tokio::test]
async fn checks_on_varchar_and_domain_columns_are_keywords() {
    // #216: PostgreSQL deparses these operands with a cast to text
    let doc = served_document(CONTRACT_FIXTURE, "openapi_contract").await;
    let row = &doc["components"]["schemas"]["patients"]["properties"];
    assert_eq!(
        row["birthDate"]["pattern"], "^[0-9]{4}(-[0-9]{2})?$",
        "{}",
        row["birthDate"]
    );
    assert_eq!(row["code"]["pattern"], "^[A-Z]+$", "{}", row["code"]);
    assert_eq!(row["nickname"]["minLength"], 2, "{}", row["nickname"]);
    assert_eq!(row["nickname"]["maxLength"], 20);
}

#[tokio::test]
async fn defaults_are_values_of_the_property_type() {
    // #213: constants as JSON values, expressions left to the database
    let doc = served_document(CONTRACT_FIXTURE, "openapi_contract").await;
    let schemas = &doc["components"]["schemas"];
    let row = &schemas["patients"]["properties"];
    assert_eq!(row["enabled"]["default"], json!(false));
    assert_eq!(row["rank"]["default"], json!(5));
    assert_eq!(row["status"]["default"], json!("active"));
    assert_eq!(row["state"]["default"], json!("IN_PROGRESS"));
    assert_eq!(row["big"]["default"], json!("9"));
    assert!(row["token"].get("default").is_none(), "{}", row["token"]);
    assert!(
        row["created"].get("default").is_none(),
        "{}",
        row["created"]
    );
    // and in what a client sends
    let multipart = &doc["paths"]["/rest/patients"]["post"]["requestBody"]["content"]
        ["multipart/form-data"]["schema"]["properties"];
    if let Some(enabled) = multipart.get("enabled") {
        assert_eq!(enabled["default"], json!(false), "{enabled}");
    }
    let create = &schemas["Input.CreatePatientInput"]["properties"];
    assert_eq!(create["rank"]["default"], json!(5), "{}", create["rank"]);
    // a field an update leaves out keeps its value: no default there
    let update = &schemas["Input.UpdatePatientInput"]["properties"];
    assert!(
        update["rank"].get("default").is_none(),
        "{}",
        update["rank"]
    );
}

#[tokio::test]
async fn write_bodies_keep_what_checks_state() {
    // #215: the GraphQL input types are the bodies; their column fields keep the keywords
    let doc = served_document(CONTRACT_FIXTURE, "openapi_contract").await;
    let schemas = &doc["components"]["schemas"];
    let body = &doc["paths"]["/rest/patients"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"];
    let body = serde_json::to_string(body).unwrap();
    assert!(body.contains("patientsCreateInput"), "{body}");
    assert_eq!(
        schemas["patientsCreateInput"]["$ref"],
        "#/components/schemas/Input.CreatePatientInput"
    );
    for input in ["Input.CreatePatientInput", "Input.UpdatePatientInput"] {
        let props = &schemas[input]["properties"];
        assert_eq!(props["score"]["minimum"], 0, "{input}: {}", props["score"]);
        assert_eq!(props["score"]["maximum"], 100, "{input}");
        assert_eq!(props["code"]["pattern"], "^[A-Z]+$", "{input}");
        assert_eq!(props["nickname"]["minLength"], 2, "{input}");
    }
    // a relation member keeps its shape, and the nested input its keywords
    let create = &schemas["Input.CreatePatientInput"]["properties"];
    assert!(
        create["visits"]
            .to_string()
            .contains("Input.VisitToManyInput"),
        "{}",
        create["visits"]
    );
    let visit = &schemas["Input.CreateVisitInput"]["properties"];
    assert_eq!(visit["reason"]["minLength"], 3, "{}", visit["reason"]);
    assert_eq!(visit["reason"]["maxLength"], 40);
    // required as GraphQL says: `enabled` has a default
    let required = schemas["Input.CreatePatientInput"]["required"].clone();
    assert!(!required.to_string().contains("enabled"), "{required}");
}
