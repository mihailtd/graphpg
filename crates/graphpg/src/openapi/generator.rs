//! Generates OpenAPI 3.1 documents from the database catalog and configuration.
//!
//! The document describes exactly what [`crate::server::rest`] does: the same tables (resolved
//! the same way), the same permission rules, the same query grammar and the same limits, so the
//! specification and the API cannot drift apart.

use crate::catalog::{ColumnMetadata, DatabaseCatalog, FieldKind, PgType, TableMetadata};
use crate::config::models::Operation;
use crate::config::Config;
use crate::server::rest::{
    check_table_permissions, resolve_table, MAX_BULK_ROWS, MAX_INCLUDES, MAX_INCLUDE_DEPTH,
    MAX_PAGE_SIZE, REST_PREFIX,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct OpenApiGenerator;

/// `{"$ref": "#/components/schemas/<name>"}`.
fn schema_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// A problem+json response.
fn problem(description: &str) -> Value {
    json!({
        "description": description,
        "content": { "application/problem+json": { "schema": schema_ref("ProblemDetails") } }
    })
}

/// The responses every operation can return, plus `extra` (status, description).
fn responses(success: Value, extra: &[(&str, &str)]) -> Value {
    let mut out = match success {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    for (status, description) in [
        (
            "400",
            "Bad Request: an unknown or malformed parameter, operator, field or body",
        ),
        ("401", "Unauthorized"),
        (
            "403",
            "Forbidden: the operation is not permitted on this resource",
        ),
    ]
    .iter()
    .chain(extra)
    {
        out.insert((*status).to_string(), problem(description));
    }
    out.insert("500".to_string(), problem("Internal Server Error"));
    Value::Object(out)
}

fn prefer_param(write: bool) -> Value {
    if write {
        json!({
            "name": "Prefer", "in": "header",
            "description": "`return=minimal` answers 204 without a body (default: `return=representation`)",
            "schema": { "type": "string", "enum": ["return=representation", "return=minimal"] }
        })
    } else {
        json!({
            "name": "Prefer", "in": "header",
            "description": "`count=exact` adds `X-Total-Count`; it costs a scan of every matching row, so it is off by default",
            "schema": { "type": "string", "enum": ["count=exact"] }
        })
    }
}

fn if_match_param(description: &str) -> Value {
    json!({
        "name": "If-Match", "in": "header",
        "description": description,
        "schema": { "type": "string" }
    })
}

/// Makes a schema accept `null` (OpenAPI 3.1 / JSON Schema 2020-12).
fn nullable(mut schema: Value) -> Value {
    match schema.get("type").cloned() {
        Some(Value::String(t)) => {
            schema["type"] = json!([t, "null"]);
            if let Some(Value::Array(values)) = schema.get_mut("enum") {
                values.push(Value::Null);
            }
            schema
        }
        Some(_) => schema,
        None => json!({ "anyOf": [schema, { "type": "null" }] }),
    }
}

/// A regex alternation of literal names.
fn alternation(names: &[String]) -> String {
    let escaped: Vec<String> = names.iter().map(|n| regex_escape(n)).collect();
    format!("({})", escaped.join("|"))
}

fn regex_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                vec![c]
            } else {
                vec!['\\', c]
            }
        })
        .collect()
}

impl OpenApiGenerator {
    /// Generates an OpenAPI 3.1 document, taking the first table's schema as the default one.
    pub fn generate(catalog: &DatabaseCatalog, config: &Config) -> Value {
        let default_schema = catalog
            .tables
            .first()
            .map(|t| t.schema.clone())
            .unwrap_or_default();
        Self::generate_for(catalog, config, &default_schema)
    }

    /// Generates an OpenAPI 3.1 document for the REST API of a server whose default schema is
    /// `default_schema` (the schema `/rest/<resource>` resolves names in first).
    pub fn generate_for(catalog: &DatabaseCatalog, config: &Config, default_schema: &str) -> Value {
        let mut paths = serde_json::Map::new();
        let mut schemas = serde_json::Map::new();

        paths.insert(
            REST_PREFIX.to_string(),
            json!({
                "get": {
                    "summary": "Hello World",
                    "description": "REST API liveness / hello-world endpoint",
                    "operationId": "getHelloWorld",
                    "responses": {
                        "200": {
                            "description": "Successful response",
                            "content": { "text/plain": { "schema": { "type": "string" } } }
                        }
                    }
                }
            }),
        );
        schemas.insert(
            "ProblemDetails".to_string(),
            Self::generate_problem_details_schema(),
        );

        // The tables the API serves: the one a resource name resolves to, not omitted
        let tables: Vec<&TableMetadata> = catalog
            .tables
            .iter()
            .filter(|t| {
                resolve_table(catalog, default_schema, &t.name)
                    .is_some_and(|resolved| std::ptr::eq(resolved, *t))
            })
            .filter(|t| !config.entities.get(&t.name).is_some_and(|e| e.omit))
            .collect();

        for table in &tables {
            let name = &table.name;
            schemas.insert(name.clone(), Self::generate_table_schema(table, catalog));
            schemas.insert(
                format!("{name}CreateInput"),
                Self::generate_create_input_schema(table, catalog),
            );
            schemas.insert(
                format!("{name}UpdateInput"),
                Self::generate_update_input_schema(table, catalog),
            );
            schemas.insert(
                format!("{name}Filter"),
                Self::generate_filter_schema(table, catalog, &tables),
            );
            schemas.insert(
                format!("{name}SearchRequest"),
                Self::generate_search_request_schema(table, catalog),
            );
            Self::generate_table_paths(table, catalog, config, &mut paths);
        }

        json!({
            "openapi": "3.1.0",
            "info": {
                "title": "graphpg REST API",
                "version": "1.0.0",
                "description": "REST API generated by graphpg from PostgreSQL database metadata"
            },
            "servers": [{ "url": "/", "description": "Current server" }],
            "security": [{ "bearerAuth": [] }],
            "paths": paths,
            "components": {
                "schemas": schemas,
                "securitySchemes": {
                    "bearerAuth": {
                        "type": "http",
                        "scheme": "bearer",
                        "bearerFormat": "JWT",
                        "description": "JWT bearer token"
                    }
                }
            }
        })
    }

    fn generate_table_paths(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        config: &Config,
        paths: &mut serde_json::Map<String, Value>,
    ) {
        let name = &table.name;
        let allowed = |op| check_table_permissions(table, Some(config), op).is_ok();
        let has_key = !table.primary_key.is_empty();
        let read = allowed(Operation::Read);
        let create = allowed(Operation::Create);
        let create_many = create && allowed(Operation::CreateMany);
        let update = allowed(Operation::Update) && has_key;
        let delete = allowed(Operation::Delete) && has_key;
        let upsert = allowed(Operation::Upsert) && has_key;
        let row = schema_ref(name);
        let rows = json!({ "type": "array", "items": row });
        let json_content = |schema: Value| json!({ "application/json": { "schema": schema } });
        let deprecated = table.deprecation.is_some();

        let collection_path = format!("{REST_PREFIX}/{name}");
        let mut collection = serde_json::Map::new();

        if read {
            let mut op = json!({
                "summary": format!("List {name}"),
                "description": table.description.clone().unwrap_or_else(|| format!("Retrieve rows from `{name}`")),
                "operationId": format!("list_{name}"),
                "parameters": Self::generate_list_parameters(table, catalog),
                "responses": responses(json!({
                    "200": {
                        "description": format!("A list of {name} rows"),
                        "headers": {
                            "Link": {
                                "description": "RFC 8288 `next` / `prev` links carrying the cursors",
                                "schema": { "type": "string" }
                            },
                            "X-Total-Count": {
                                "description": "Number of matching rows; only with `Prefer: count=exact`",
                                "schema": { "type": "integer" }
                            },
                            "Preference-Applied": { "schema": { "type": "string" } }
                        },
                        "content": json_content(rows.clone())
                    }
                }), &[]),
            });
            if deprecated {
                op["deprecated"] = json!(true);
            }
            collection.insert("get".into(), op);
        }

        if create {
            let body = if create_many {
                json!({ "oneOf": [
                    schema_ref(&format!("{name}CreateInput")),
                    { "type": "array", "minItems": 1, "maxItems": MAX_BULK_ROWS,
                      "items": schema_ref(&format!("{name}CreateInput")) }
                ] })
            } else {
                schema_ref(&format!("{name}CreateInput"))
            };
            collection.insert("post".into(), json!({
                "summary": format!("Create {name}"),
                "description": format!(
                    "Create one row (object body) or, with an array body, up to {MAX_BULK_ROWS} rows in one transaction. Nested objects are nested writes, in the shape of the GraphQL input."
                ),
                "operationId": format!("create_{name}"),
                "parameters": [prefer_param(true)],
                "requestBody": { "required": true, "content": json_content(body) },
                "responses": responses(json!({
                    "201": {
                        "description": "Created: the row, or an array of rows for a bulk create",
                        "headers": {
                            "Location": { "description": "URL of the created row (single create)", "schema": { "type": "string" } },
                            "ETag": { "description": "Row version (single create)", "schema": { "type": "string" } },
                            "Preference-Applied": { "schema": { "type": "string" } }
                        },
                        "content": json_content(json!({ "oneOf": [row.clone(), rows.clone()] }))
                    },
                    "204": { "description": "Created (`Prefer: return=minimal`)" }
                }), &[
                    ("409", "Conflict: a unique constraint, with its typed `code`"),
                    ("413", "Content Too Large: more rows than a bulk create accepts"),
                    ("422", "Unprocessable Content: a validation failure, with its typed `code`"),
                ]),
            }));
        }
        if !collection.is_empty() {
            paths.insert(collection_path, Value::Object(collection));
        }

        if read {
            paths.insert(format!("{REST_PREFIX}/{name}/search"), json!({
                "post": {
                    "summary": format!("Search {name}"),
                    "description": "The list with the filter and pagination in a JSON body: `and` / `or` / `not`, column operators and relation filters. Continue a paged search by sending the `X-Next-Cursor` / `X-Previous-Cursor` response header back as `after` / `before`.",
                    "operationId": format!("search_{name}"),
                    "parameters": [prefer_param(false)],
                    "requestBody": {
                        "required": false,
                        "content": json_content(schema_ref(&format!("{name}SearchRequest")))
                    },
                    "responses": responses(json!({
                        "200": {
                            "description": format!("Search results for {name}"),
                            "headers": {
                                "X-Total-Count": { "description": "Only with `Prefer: count=exact`", "schema": { "type": "integer" } },
                                "X-Next-Cursor": { "description": "Cursor of the next page, if there is one", "schema": { "type": "string" } },
                                "X-Previous-Cursor": { "description": "Cursor of the previous page, if there is one", "schema": { "type": "string" } },
                                "Preference-Applied": { "schema": { "type": "string" } }
                            },
                            "content": json_content(rows.clone())
                        }
                    }), &[]),
                }
            }));
        }

        if !has_key {
            return;
        }
        let key_param = Self::key_parameter(table, catalog);
        let mut item = serde_json::Map::new();

        if read {
            let mut params = vec![
                key_param.clone(),
                Self::fields_param(table),
                Self::include_param(table, catalog),
            ];
            params.retain(|p| !p.is_null());
            item.insert("get".into(), json!({
                "summary": format!("Get {name} by key"),
                "description": format!("Retrieve a single row from `{name}` by primary key"),
                "operationId": format!("get_{name}_by_key"),
                "parameters": params,
                "responses": responses(json!({
                    "200": {
                        "description": format!("The requested {name} row"),
                        "headers": { "ETag": { "description": "Row version", "schema": { "type": "string" } } },
                        "content": json_content(row.clone())
                    }
                }), &[("404", "Not Found")]),
            }));
        }
        let write_responses = |ok: &str| {
            responses(
                json!({
                    "200": {
                        "description": ok,
                        "headers": {
                            "ETag": { "description": "Row version", "schema": { "type": "string" } },
                            "Preference-Applied": { "schema": { "type": "string" } }
                        },
                        "content": json_content(row.clone())
                    },
                    "204": { "description": "Done (`Prefer: return=minimal`)" }
                }),
                &[
                    ("404", "Not Found"),
                    ("409", "Conflict: a constraint, with its typed `code`"),
                    (
                        "412",
                        "Precondition Failed: the row changed since the `If-Match` version",
                    ),
                    (
                        "422",
                        "Unprocessable Content: a validation failure, with its typed `code`",
                    ),
                ],
            )
        };
        if update {
            item.insert("patch".into(), json!({
                "summary": format!("Update {name} by key"),
                "description": "Update the given fields. The key cannot be changed: a key in the body must match the URL.",
                "operationId": format!("update_{name}_by_key"),
                "parameters": [key_param.clone(),
                    if_match_param("Row version (the `ETag`) the update must apply to; 412 if the row changed"),
                    prefer_param(true)],
                "requestBody": { "required": true, "content": json_content(schema_ref(&format!("{name}UpdateInput"))) },
                "responses": write_responses("Updated"),
            }));
        }
        if upsert {
            item.insert("put".into(), json!({
                "summary": format!("Upsert {name} by key"),
                "description": "Insert the row or update its columns (200 either way). With `If-Match` the row must exist at that version, so it is an update.",
                "operationId": format!("upsert_{name}_by_key"),
                "parameters": [key_param.clone(),
                    if_match_param("Optional: require the row to exist at this version (the `ETag`)"),
                    prefer_param(true)],
                "requestBody": { "required": true, "content": json_content(schema_ref(&format!("{name}CreateInput"))) },
                "responses": write_responses("Row inserted or updated"),
            }));
        }
        if delete {
            item.insert("delete".into(), json!({
                "summary": format!("Delete {name} by key"),
                "description": format!("Delete a single row from `{name}` by primary key"),
                "operationId": format!("delete_{name}_by_key"),
                "parameters": [key_param,
                    if_match_param("Row version (the `ETag`) the delete must apply to; 412 if the row changed"),
                    prefer_param(true)],
                "responses": write_responses("Deleted: the row"),
            }));
        }
        if !item.is_empty() {
            paths.insert(format!("{REST_PREFIX}/{name}/{{key}}"), Value::Object(item));
        }
    }

    fn key_parameter(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let (schema, description) = if table.primary_key.len() == 1 {
            (
                table
                    .get_column(&table.primary_key[0])
                    .map(|c| Self::pg_type_to_json_schema(&c.pg_type, catalog))
                    .unwrap_or_else(|| json!({ "type": "string" })),
                format!(
                    "Primary key of `{}` (the Relay global ID of the row is accepted too)",
                    table.name
                ),
            )
        } else {
            (
                json!({ "type": "string" }),
                format!(
                    "Primary key of `{}`: the values of ({}) separated by commas",
                    table.name,
                    table.primary_key.join(", ")
                ),
            )
        };
        json!({ "name": "key", "in": "path", "required": true, "description": description, "schema": schema })
    }

    fn visible_fields(table: &TableMetadata) -> Vec<String> {
        table
            .visible_columns()
            .map(|c| c.field_name.clone())
            .collect()
    }

    fn fields_param(table: &TableMetadata) -> Value {
        json!({
            "name": "fields", "in": "query", "style": "form", "explode": false,
            "description": "Columns to return (default: all)",
            "schema": { "type": "array", "items": { "type": "string", "enum": Self::visible_fields(table) } }
        })
    }

    /// The to-one and to-many relations (not their connections) that can be embedded.
    fn relation_names(table: &TableMetadata, catalog: &DatabaseCatalog) -> Vec<String> {
        catalog
            .object_fields(&table.schema, &table.name)
            .iter()
            .filter(|f| matches!(f.kind, FieldKind::Relation(_)))
            .map(|f| f.name.clone())
            .collect()
    }

    /// `include`, or null when the table has no relation to embed.
    fn include_param(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let relations = Self::relation_names(table, catalog);
        if relations.is_empty() {
            return Value::Null;
        }
        json!({
            "name": "include", "in": "query", "style": "form", "explode": false,
            "description": format!(
                "Relations to embed, with nested ones as dotted paths (`posts.author`): at most {MAX_INCLUDES} relations, {MAX_INCLUDE_DEPTH} levels deep. Relations: {}",
                relations.join(", ")
            ),
            "schema": {
                "type": "array",
                "maxItems": MAX_INCLUDES,
                "items": { "type": "string", "pattern": format!("^{}(\\.[A-Za-z_][A-Za-z0-9_]*)*$", alternation(&relations)) }
            }
        })
    }

    /// Typed query parameters of the collection `GET`.
    fn generate_list_parameters(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let columns = Self::visible_fields(table);
        let order_item = format!("^[+-]?{}(:(asc|desc))?$", alternation(&columns));
        let mut params = vec![
            json!({
                "name": "filter", "in": "query", "style": "deepObject", "explode": true,
                "description": "Filter criteria: `filter[column][operator]=value`, through relations with `filter[relation][column]=value`. Unknown columns and operators are a 400. For `and` / `or` / `not` use `POST …/search`.",
                "schema": schema_ref(&format!("{}Filter", table.name))
            }),
            json!({
                "name": "order", "in": "query", "style": "form", "explode": false,
                "description": "Columns to order by: `column`, `-column` or `column:desc`",
                "schema": { "type": "array", "items": { "type": "string", "pattern": order_item } }
            }),
            json!({
                "name": "first", "in": "query",
                "description": format!("Page size for forward keyset pagination (default 100, at most {MAX_PAGE_SIZE})"),
                "schema": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_SIZE }
            }),
            json!({
                "name": "after", "in": "query",
                "description": "Cursor to continue after (from the `next` link)",
                "schema": { "type": "string" }
            }),
            json!({
                "name": "last", "in": "query",
                "description": format!("Page size for backward keyset pagination (at most {MAX_PAGE_SIZE})"),
                "schema": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_SIZE }
            }),
            json!({
                "name": "before", "in": "query",
                "description": "Cursor to continue before (from the `prev` link)",
                "schema": { "type": "string" }
            }),
            json!({
                "name": "offset", "in": "query",
                "description": "Rows to skip (offset pagination)",
                "schema": { "type": "integer", "minimum": 0 }
            }),
            Self::fields_param(table),
            Self::include_param(table, catalog),
            prefer_param(false),
        ];
        params.retain(|p| !p.is_null());
        Value::Array(params)
    }

    /// The body of `POST …/search`.
    fn generate_search_request_schema(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let columns = Self::visible_fields(table);
        let relations = Self::relation_names(table, catalog);
        let mut properties = json!({
            "filter": schema_ref(&format!("{}Filter", table.name)),
            "order": {
                "description": "Columns to order by: `column`, `-column` or `column:desc`",
                "oneOf": [
                    { "type": "string" },
                    { "type": "array", "items": { "type": "string" } }
                ]
            },
            "first": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_SIZE },
            "last": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_SIZE },
            "after": { "type": "string", "description": "Cursor (the `X-Next-Cursor` header of the previous page)" },
            "before": { "type": "string", "description": "Cursor (the `X-Previous-Cursor` header)" },
            "offset": { "type": "integer", "minimum": 0 },
            "fields": { "type": "array", "items": { "type": "string", "enum": columns } }
        });
        if !relations.is_empty() {
            properties["include"] = json!({
                "type": "array", "maxItems": MAX_INCLUDES,
                "items": { "type": "string", "pattern": format!("^{}(\\.[A-Za-z_][A-Za-z0-9_]*)*$", alternation(&relations)) }
            });
        }
        json!({
            "type": "object",
            "description": format!("Search request for `{}`; every member is optional, unknown members are a 400", table.name),
            "properties": properties,
            "additionalProperties": false
        })
    }

    /// RFC 9457 Problem Details JSON Schema.
    fn generate_problem_details_schema() -> Value {
        json!({
            "type": "object",
            "description": "RFC 9457 Problem Details object (`application/problem+json`)",
            "properties": {
                "type": { "type": "string", "description": "Always `about:blank`; the status and `code` carry the meaning" },
                "title": { "type": "string", "description": "Short human-readable summary of the status" },
                "status": { "type": "integer", "description": "HTTP status code" },
                "detail": { "type": "string", "description": "Human-readable explanation; raw database messages are masked in production" },
                "instance": { "type": "string", "format": "uri-reference", "description": "The request URL" },
                "code": { "type": "string", "description": "Stable machine-readable code, such as `EMAIL_TAKEN` or `STALE_VERSION`" },
                "fields": { "type": "array", "items": { "type": "string" }, "description": "Offending fields or parameters" },
                "requestId": { "type": "string", "description": "Correlates a masked error with the server log" }
            },
            "required": ["type", "title", "status", "detail"]
        })
    }

    /// The columns a client may write.
    fn writable(table: &TableMetadata) -> impl Iterator<Item = &ColumnMetadata> {
        table
            .visible_columns()
            .filter(|c| !c.is_generated && !c.readonly)
    }

    /// Input schema for row creation.
    fn generate_create_input_schema(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let mut properties = BTreeMap::new();
        let mut required = Vec::new();
        for col in Self::writable(table) {
            properties.insert(
                col.field_name.clone(),
                Self::column_to_json_schema(col, catalog),
            );
            if !col.is_nullable && col.default_value.is_none() {
                required.push(col.field_name.clone());
            }
        }
        let mut schema = json!({
            "type": "object",
            "description": format!("Input object for creating a `{}` row", table.name),
            "properties": properties,
            "additionalProperties": false
        });
        if !required.is_empty() {
            schema["required"] = json!(required);
        }
        schema
    }

    /// Input schema for a partial update: every writable column except the key.
    fn generate_update_input_schema(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let properties: BTreeMap<String, Value> = Self::writable(table)
            .filter(|c| !c.is_primary_key)
            .map(|c| {
                (
                    c.field_name.clone(),
                    Self::column_to_json_schema(c, catalog),
                )
            })
            .collect();
        json!({
            "type": "object",
            "description": format!("Input object for updating a `{}` row (partial)", table.name),
            "properties": properties,
            "additionalProperties": false
        })
    }

    /// Filter schema of the `filter` parameter and the search body.
    fn generate_filter_schema(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        served: &[&TableMetadata],
    ) -> Value {
        let mut properties = BTreeMap::new();
        for col in table.visible_columns() {
            let ty = Self::pg_type_to_json_schema(&col.pg_type, catalog);
            let mut ops = BTreeMap::new();
            for op in ["equalTo", "eq", "notEqualTo", "neq"] {
                ops.insert(op.to_string(), ty.clone());
            }
            let ordered = col.pg_type.is_numeric()
                || matches!(
                    col.pg_type,
                    PgType::Date
                        | PgType::Time
                        | PgType::TimeTz
                        | PgType::Timestamp
                        | PgType::TimestampTz
                );
            if ordered {
                for op in [
                    "greaterThan",
                    "gt",
                    "greaterThanOrEqualTo",
                    "gte",
                    "lessThan",
                    "lt",
                    "lessThanOrEqualTo",
                    "lte",
                ] {
                    ops.insert(op.to_string(), ty.clone());
                }
            }
            if matches!(
                col.pg_type,
                PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext
            ) {
                for op in ["like", "ilike", "startsWith", "endsWith"] {
                    ops.insert(op.to_string(), json!({ "type": "string" }));
                }
            }
            ops.insert("in".to_string(), json!({ "type": "array", "items": ty }));
            ops.insert("isNull".to_string(), json!({ "type": "boolean" }));
            properties.insert(
                col.field_name.clone(),
                json!({
                    "type": "object",
                    "properties": ops,
                    "description": format!("Filter criteria for `{}`", col.field_name)
                }),
            );
        }

        // Relation filters: a to-one relation takes the target's filter, a to-many one
        // `some` / `every` / `none` and a `count` comparison
        for field in catalog.object_fields(&table.schema, &table.name) {
            let FieldKind::Relation(rel) = &field.kind else {
                continue;
            };
            let Some(target) = served
                .iter()
                .find(|t| t.schema == rel.target_schema && t.name == rel.target_table)
            else {
                continue;
            };
            let target_filter = schema_ref(&format!("{}Filter", target.name));
            let schema = if rel.is_many {
                json!({
                    "type": "object",
                    "description": format!("Filter `{}` by its related rows", table.name),
                    "properties": {
                        "some": target_filter.clone(),
                        "every": target_filter.clone(),
                        "none": target_filter,
                        "count": {
                            "type": "object",
                            "properties": {
                                "equalTo": { "type": "integer" },
                                "greaterThan": { "type": "integer" },
                                "lessThan": { "type": "integer" }
                            }
                        }
                    }
                })
            } else {
                target_filter
            };
            properties.entry(field.name.clone()).or_insert(schema);
        }

        let own = schema_ref(&format!("{}Filter", table.name));
        properties.insert(
            "and".into(),
            json!({ "type": "array", "items": own.clone() }),
        );
        properties.insert(
            "or".into(),
            json!({ "type": "array", "items": own.clone() }),
        );
        properties.insert("not".into(), own);

        json!({
            "type": "object",
            "description": format!("Filter schema for `{}`", table.name),
            "properties": properties,
            "additionalProperties": false
        })
    }

    /// Converts a table's columns and metadata into an OpenAPI 3.1 / JSON Schema object.
    fn generate_table_schema(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
    ) -> serde_json::Value {
        let mut properties = BTreeMap::new();
        let mut required = Vec::new();

        // Rows are keyed by the field names of the API (`archivedAt`), not the column names
        for col in table.visible_columns() {
            properties.insert(
                col.field_name.clone(),
                Self::column_to_json_schema(col, catalog),
            );
            if !col.is_nullable {
                required.push(col.field_name.clone());
            }
        }

        let mut schema = json!({
            "type": "object",
            "properties": properties
        });

        if let Some(desc) = &table.description {
            schema["description"] = json!(desc);
        }

        if !required.is_empty() {
            schema["required"] = json!(required);
        }

        schema
    }

    /// Maps a column's PostgreSQL type to an OpenAPI 3.1 JSON Schema.
    fn column_to_json_schema(col: &ColumnMetadata, catalog: &DatabaseCatalog) -> serde_json::Value {
        let mut schema = if let Some(enum_key) = &col.enum_key {
            if let Some(enum_meta) = catalog.enums.iter().find(|e| &e.key == enum_key) {
                let vals: Vec<&str> = enum_meta.values.iter().map(|v| v.label.as_str()).collect();
                json!({ "type": "string", "enum": vals })
            } else {
                Self::pg_type_to_json_schema(&col.pg_type, catalog)
            }
        } else {
            Self::pg_type_to_json_schema(&col.pg_type, catalog)
        };

        // In OpenAPI 3.1 / JSON Schema 2020-12, nullable is represented as a union type: ["<type>", "null"]
        if col.is_nullable {
            schema = nullable(schema);
        }

        if let Some(desc) = &col.description {
            schema["description"] = json!(desc);
        }

        if let Some(dep) = &col.deprecation {
            schema["deprecated"] = json!(true);
            if schema.get("description").is_none() {
                schema["description"] = json!(format!("Deprecated: {dep}"));
            }
        }

        if let Some(def) = &col.default_value {
            schema["default"] = json!(def);
        }

        if let Some(max_len) = col.limits.max_length {
            schema["maxLength"] = json!(max_len);
        }

        if col.readonly {
            schema["readOnly"] = json!(true);
        }

        schema
    }

    /// Converts a [`PgType`] into its corresponding JSON Schema type and format.
    fn pg_type_to_json_schema(pg_type: &PgType, catalog: &DatabaseCatalog) -> serde_json::Value {
        match pg_type {
            PgType::Int2 | PgType::Int4 => json!({ "type": "integer", "format": "int32" }),
            PgType::Int8 => json!({ "type": "integer", "format": "int64" }),
            PgType::Float4 => json!({ "type": "number", "format": "float" }),
            PgType::Float8 | PgType::Numeric | PgType::Money => json!({ "type": "number" }),
            PgType::Bool => json!({ "type": "boolean" }),
            PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext => {
                json!({ "type": "string" })
            }
            PgType::Uuid => json!({ "type": "string", "format": "uuid" }),
            PgType::Date => json!({ "type": "string", "format": "date" }),
            PgType::Time | PgType::TimeTz => json!({ "type": "string", "format": "time" }),
            PgType::Timestamp | PgType::TimestampTz => {
                json!({ "type": "string", "format": "date-time" })
            }
            PgType::Interval => json!({ "type": "string", "format": "duration" }),
            PgType::Bytea => json!({ "type": "string", "format": "binary" }),
            PgType::Json | PgType::Jsonb => json!({ "type": "object" }),
            PgType::Inet | PgType::Cidr => json!({ "type": "string" }),
            PgType::MacAddr | PgType::MacAddr8 => json!({ "type": "string" }),
            PgType::Vector => json!({ "type": "array", "items": { "type": "number" } }),
            PgType::Array(inner) => {
                let inner_schema = Self::pg_type_to_json_schema(inner, catalog);
                json!({ "type": "array", "items": inner_schema })
            }
            PgType::Enum(name) => {
                if let Some(enum_meta) = catalog.enums.iter().find(|e| {
                    &e.config_name == name
                        || &e.key == name
                        || &e.type_name == name
                        || name.ends_with(&e.config_name)
                }) {
                    let vals: Vec<&str> =
                        enum_meta.values.iter().map(|v| v.label.as_str()).collect();
                    json!({ "type": "string", "enum": vals })
                } else {
                    json!({ "type": "string" })
                }
            }
            _ => json!({ "type": "string" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::models::{ColumnMetadata, RelationKind, TableMetadata};
    use crate::catalog::DatabaseCatalog;

    #[test]
    fn test_generate_minimal_openapi_document() {
        let mut catalog = DatabaseCatalog::default();
        catalog.tables.push(TableMetadata {
            schema: "public".to_string(),
            name: "users".to_string(),
            kind: RelationKind::Table,
            columns: vec![
                ColumnMetadata {
                    name: "id".to_string(),
                    pg_type: PgType::Int4,
                    is_nullable: false,
                    is_primary_key: true,
                    default_value: None,
                    is_generated: false,
                    description: Some("Primary key".to_string()),
                    limits: Default::default(),
                    generation: None,
                    deprecation: None,
                    field_name: "id".to_string(),
                    omit: false,
                    readonly: false,
                    enum_key: None,
                    domain: None,
                },
                ColumnMetadata {
                    name: "email".to_string(),
                    pg_type: PgType::Text,
                    is_nullable: true,
                    is_primary_key: false,
                    default_value: None,
                    is_generated: false,
                    description: None,
                    limits: Default::default(),
                    generation: None,
                    deprecation: None,
                    field_name: "email".to_string(),
                    omit: false,
                    readonly: false,
                    enum_key: None,
                    domain: None,
                },
            ],
            primary_key: vec!["id".to_string()],
            foreign_keys: vec![],
            description: Some("Users table".to_string()),
            type_name: "User".to_string(),
            list_field: "users".to_string(),
            single_field: "user".to_string(),
            connection_field: "usersConnection".to_string(),
            all_connection_field: None,
            operations: Default::default(),
            unique_keys: vec![],
            constraints: vec![],
            version_column: None,
            deprecation: None,
            federation_key: None,
            federation_shareable: false,
        });

        let config = Config::default();
        let doc = OpenApiGenerator::generate(&catalog, &config);

        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"]["title"], "graphpg REST API");

        // Paths include /rest, /rest/users, /rest/users/search, and /rest/users/{key}
        assert!(doc["paths"]["/rest"]["get"].is_object());
        assert!(doc["paths"]["/rest/users"]["get"].is_object());
        assert!(doc["paths"]["/rest/users"]["post"].is_object());
        assert!(doc["paths"]["/rest/users/search"]["post"].is_object());
        assert!(doc["paths"]["/rest/users/{key}"]["get"].is_object());
        assert!(doc["paths"]["/rest/users/{key}"]["patch"].is_object());
        assert!(doc["paths"]["/rest/users/{key}"]["put"].is_object());
        assert!(doc["paths"]["/rest/users/{key}"]["delete"].is_object());

        // Check GET /rest/users parameters include typed filter (deepObject)
        let get_params = doc["paths"]["/rest/users"]["get"]["parameters"]
            .as_array()
            .unwrap();
        assert!(get_params
            .iter()
            .any(|p| p["name"] == "filter" && p["style"] == "deepObject"));
        assert!(get_params.iter().any(|p| p["name"] == "order"));
        assert!(get_params.iter().any(|p| p["name"] == "first"));

        // Component schemas include users, usersCreateInput, usersUpdateInput, usersFilter, ProblemDetails
        let user_schema = &doc["components"]["schemas"]["users"];
        assert_eq!(user_schema["type"], "object");
        assert_eq!(user_schema["description"], "Users table");
        assert_eq!(user_schema["properties"]["id"]["type"], "integer");
        assert_eq!(user_schema["properties"]["id"]["format"], "int32");
        assert_eq!(
            user_schema["properties"]["email"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(user_schema["required"], json!(["id"]));

        let create_schema = &doc["components"]["schemas"]["usersCreateInput"];
        assert_eq!(create_schema["type"], "object");
        assert_eq!(create_schema["required"], json!(["id"]));

        let update_schema = &doc["components"]["schemas"]["usersUpdateInput"];
        assert_eq!(update_schema["type"], "object");
        assert!(update_schema.get("required").is_none());

        let filter_schema = &doc["components"]["schemas"]["usersFilter"];
        assert_eq!(filter_schema["type"], "object");
        assert!(filter_schema["properties"]["id"].is_object());
        assert!(filter_schema["properties"]["email"].is_object());

        let problem_schema = &doc["components"]["schemas"]["ProblemDetails"];
        assert_eq!(problem_schema["type"], "object");
        assert!(problem_schema["properties"]["status"].is_object());
        assert!(problem_schema["properties"]["detail"].is_object());
        assert!(problem_schema["properties"]["code"].is_object());

        // Security schemes include bearerAuth
        assert!(doc["components"]["securitySchemes"]["bearerAuth"].is_object());
    }

    #[test]
    fn nullable_schemas_accept_null() {
        assert_eq!(
            nullable(json!({ "type": "string" })),
            json!({ "type": ["string", "null"] })
        );
        // an enum must list null itself
        assert_eq!(
            nullable(json!({ "type": "string", "enum": ["a"] })),
            json!({ "type": ["string", "null"], "enum": ["a", null] })
        );
        assert_eq!(
            nullable(json!({ "$ref": "#/components/schemas/X" })),
            json!({ "anyOf": [{ "$ref": "#/components/schemas/X" }, { "type": "null" }] })
        );
    }
}
