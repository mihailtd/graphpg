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
