//! Generates OpenAPI 3.1 documents from the database catalog and configuration.

use crate::catalog::{ColumnMetadata, DatabaseCatalog, PgType, TableMetadata};
use crate::config::Config;
use serde_json::json;
use std::collections::BTreeMap;

pub struct OpenApiGenerator;

impl OpenApiGenerator {
    /// Generates an OpenAPI 3.1 document from the introspected database catalog.
    pub fn generate(catalog: &DatabaseCatalog, config: &Config) -> serde_json::Value {
        let mut paths = serde_json::Map::new();
        let mut schemas = serde_json::Map::new();

        // 1. Hello-world endpoint (/rest)
        paths.insert(
            "/rest".to_string(),
            json!({
                "get": {
                    "summary": "Hello World",
                    "description": "REST API liveness / hello-world endpoint",
                    "operationId": "getHelloWorld",
                    "responses": {
                        "200": {
                            "description": "Successful response",
                            "content": {
                                "text/plain": {
                                    "schema": {
                                        "type": "string"
                                    }
                                }
                            }
                        }
                    }
                }
            }),
        );

        // 2. Add ProblemDetails schema to components
        schemas.insert(
            "ProblemDetails".to_string(),
            Self::generate_problem_details_schema(),
        );

        // 3. Generate component schemas and paths for accessible tables
        for table in &catalog.tables {
            // Check if table is omitted in config or catalog
            if let Some(entity_cfg) = config.entities.get(&table.name) {
                if entity_cfg.omit {
                    continue;
                }
            }

            let schema_name = &table.name;
            let table_schema = Self::generate_table_schema(table, catalog);
            schemas.insert(schema_name.clone(), table_schema);
            schemas.insert(
                format!("{}CreateInput", schema_name),
                Self::generate_create_input_schema(table, catalog),
            );
            schemas.insert(
                format!("{}UpdateInput", schema_name),
                Self::generate_update_input_schema(table, catalog),
            );
            schemas.insert(
                format!("{}Filter", schema_name),
                Self::generate_filter_schema(table, catalog),
            );

            // Determine allowed operations
            let mut read_allowed =
                table.operations.read || table.operations.list || table.operations.connection;
            let mut create_allowed = table.operations.create;
            let mut create_many_allowed = create_allowed && table.operations.create_many;
            let mut update_allowed = table.operations.update && !table.primary_key.is_empty();
            let mut delete_allowed = table.operations.delete && !table.primary_key.is_empty();
            let mut upsert_allowed =
                (create_allowed || update_allowed) && !table.primary_key.is_empty();

            if let Some(entity_cfg) = config.entities.get(&table.name) {
                for op in &entity_cfg.omit_operations {
                    match op {
                        crate::config::models::Operation::Read
                        | crate::config::models::Operation::List
                        | crate::config::models::Operation::Connection => {
                            read_allowed = false;
                        }
                        crate::config::models::Operation::Create => {
                            create_allowed = false;
                        }
                        crate::config::models::Operation::CreateMany => {
                            create_many_allowed = false;
                        }
                        crate::config::models::Operation::Update
                        | crate::config::models::Operation::UpdateMany => {
                            update_allowed = false;
                        }
                        crate::config::models::Operation::Delete
                        | crate::config::models::Operation::DeleteMany => {
                            delete_allowed = false;
                        }
                        crate::config::models::Operation::Upsert => {
                            upsert_allowed = false;
                        }
                    }
                }
            }

            let collection_path = format!("/rest/{}", table.name);
            let mut collection_ops = serde_json::Map::new();

            // 3.1 GET /rest/{table}
            if read_allowed {
                let mut get_op = json!({
                    "summary": format!("List {}", table.name),
                    "description": table.description.as_deref().unwrap_or(&format!("Retrieve rows from `{}`", table.name)),
                    "operationId": format!("list_{}", table.name),
                    "parameters": Self::generate_list_parameters(schema_name),
                    "responses": {
                        "200": {
                            "description": format!("A list of {} rows", table.name),
                            "headers": {
                                "Link": {
                                    "description": "RFC 8288 pagination links",
                                    "schema": { "type": "string" }
                                },
                                "x-total-count": {
                                    "description": "Total count of matching records",
                                    "schema": { "type": "integer" }
                                },
                                "ETag": {
                                    "description": "Row version entity tag",
                                    "schema": { "type": "string" }
                                }
                            },
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "array",
                                        "items": {
                                            "$ref": format!("#/components/schemas/{}", schema_name)
                                        }
                                    }
                                }
                            }
                        },
                        "400": {
                            "description": "Bad Request",
                            "content": {
                                "application/problem+json": {
                                    "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                }
                            }
                        }
                    }
                });

                if table.deprecation.is_some() {
                    get_op["deprecated"] = json!(true);
                }

                collection_ops.insert("get".to_string(), get_op);
            }

            // 3.2 POST /rest/{table} (create / bulk create)
            if create_allowed {
                let create_many = create_many_allowed;
                let request_body_schema = if create_many {
                    json!({
                        "oneOf": [
                            { "$ref": format!("#/components/schemas/{}CreateInput", schema_name) },
                            {
                                "type": "array",
                                "items": { "$ref": format!("#/components/schemas/{}CreateInput", schema_name) }
                            }
                        ]
                    })
                } else {
                    json!({ "$ref": format!("#/components/schemas/{}CreateInput", schema_name) })
                };

                let post_op = json!({
                    "summary": format!("Create {}", table.name),
                    "description": format!("Create a single row or bulk rows in `{}`", table.name),
                    "operationId": format!("create_{}", table.name),
                    "parameters": [
                        {
                            "name": "Prefer",
                            "in": "header",
                            "description": "Return representation or minimal response",
                            "schema": {
                                "type": "string",
                                "enum": ["return=representation", "return=minimal"],
                                "default": "return=representation"
                            }
                        }
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": request_body_schema
                            }
                        }
                    },
                    "responses": {
                        "201": {
                            "description": "Created",
                            "headers": {
                                "Location": {
                                    "description": "URI of created resource",
                                    "schema": { "type": "string" }
                                },
                                "ETag": {
                                    "description": "Row version entity tag",
                                    "schema": { "type": "string" }
                                }
                            },
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "$ref": format!("#/components/schemas/{}", schema_name)
                                    }
                                }
                            }
                        },
                        "204": {
                            "description": "Created (minimal representation requested)"
                        },
                        "400": {
                            "description": "Bad Request",
                            "content": {
                                "application/problem+json": {
                                    "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                }
                            }
                        },
                        "409": {
                            "description": "Conflict (constraint violation)",
                            "content": {
                                "application/problem+json": {
                                    "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                }
                            }
                        },
                        "422": {
                            "description": "Unprocessable Content (validation failure)",
                            "content": {
                                "application/problem+json": {
                                    "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                }
                            }
                        }
                    }
                });

                collection_ops.insert("post".to_string(), post_op);
            }

            if !collection_ops.is_empty() {
                paths.insert(collection_path, serde_json::Value::Object(collection_ops));
            }

            // 3.3 POST /rest/{table}/search
            if read_allowed {
                let search_path = format!("/rest/{}/search", table.name);
                paths.insert(
                    search_path,
                    json!({
                        "post": {
                            "summary": format!("Search {}", table.name),
                            "description": format!("Search rows in `{}` with structured filter and pagination criteria", table.name),
                            "operationId": format!("search_{}", table.name),
                            "requestBody": {
                                "required": true,
                                "content": {
                                    "application/json": {
                                        "schema": {
                                            "$ref": format!("#/components/schemas/{}Filter", schema_name)
                                        }
                                    }
                                }
                            },
                            "responses": {
                                "200": {
                                    "description": format!("Search results for {}", table.name),
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "type": "array",
                                                "items": {
                                                    "$ref": format!("#/components/schemas/{}", schema_name)
                                                }
                                            }
                                        }
                                    }
                                },
                                "400": {
                                    "description": "Bad Request",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                }
                            }
                        }
                    }),
                );
            }

            // 3.4 Single-item path: /rest/{table}/{key}
            if !table.primary_key.is_empty() {
                let item_path = format!("/rest/{}/{{key}}", table.name);
                let pk_schema = if table.primary_key.len() == 1 {
                    table
                        .get_column(&table.primary_key[0])
                        .map(|c| Self::pg_type_to_json_schema(&c.pg_type, catalog))
                        .unwrap_or_else(|| json!({ "type": "string" }))
                } else {
                    json!({ "type": "string" })
                };

                let key_parameter = json!({
                    "name": "key",
                    "in": "path",
                    "required": true,
                    "description": format!("Primary key of `{}`", table.name),
                    "schema": pk_schema
                });

                let mut item_ops = serde_json::Map::new();

                // GET /rest/{table}/{key}
                if read_allowed {
                    item_ops.insert(
                        "get".to_string(),
                        json!({
                            "summary": format!("Get {} by key", table.name),
                            "description": format!("Retrieve a single row from `{}` by primary key", table.name),
                            "operationId": format!("get_{}_by_key", table.name),
                            "parameters": [
                                key_parameter.clone(),
                                {
                                    "name": "fields",
                                    "in": "query",
                                    "description": "Comma-separated list of columns to select",
                                    "schema": { "type": "string" }
                                },
                                {
                                    "name": "include",
                                    "in": "query",
                                    "description": "Comma-separated list of relations to embed",
                                    "schema": { "type": "string" }
                                }
                            ],
                            "responses": {
                                "200": {
                                    "description": format!("The requested {} row", table.name),
                                    "headers": {
                                        "ETag": {
                                            "description": "Row version entity tag",
                                            "schema": { "type": "string" }
                                        }
                                    },
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "$ref": format!("#/components/schemas/{}", schema_name)
                                            }
                                        }
                                    }
                                },
                                "404": {
                                    "description": "Not Found",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                }
                            }
                        }),
                    );
                }

                // PATCH /rest/{table}/{key}
                if update_allowed {
                    item_ops.insert(
                        "patch".to_string(),
                        json!({
                            "summary": format!("Update {} by key", table.name),
                            "description": format!("Update fields of a `{}` row by primary key", table.name),
                            "operationId": format!("update_{}_by_key", table.name),
                            "parameters": [
                                key_parameter.clone(),
                                {
                                    "name": "If-Match",
                                    "in": "header",
                                    "description": "Optimistic concurrency version tag",
                                    "schema": { "type": "string" }
                                },
                                {
                                    "name": "Prefer",
                                    "in": "header",
                                    "description": "Return representation or minimal response",
                                    "schema": {
                                        "type": "string",
                                        "enum": ["return=representation", "return=minimal"],
                                        "default": "return=representation"
                                    }
                                }
                            ],
                            "requestBody": {
                                "required": true,
                                "content": {
                                    "application/json": {
                                        "schema": {
                                            "$ref": format!("#/components/schemas/{}UpdateInput", schema_name)
                                        }
                                    }
                                }
                            },
                            "responses": {
                                "200": {
                                    "description": "Updated",
                                    "headers": {
                                        "ETag": {
                                            "description": "Row version entity tag",
                                            "schema": { "type": "string" }
                                        }
                                    },
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "$ref": format!("#/components/schemas/{}", schema_name)
                                            }
                                        }
                                    }
                                },
                                "204": {
                                    "description": "Updated (minimal representation requested)"
                                },
                                "400": {
                                    "description": "Bad Request",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                },
                                "404": {
                                    "description": "Not Found",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                },
                                "409": {
                                    "description": "Conflict",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                },
                                "412": {
                                    "description": "Precondition Failed (version mismatch)",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                },
                                "422": {
                                    "description": "Unprocessable Content",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                }
                            }
                        }),
                    );
                }

                // PUT /rest/{table}/{key}
                if upsert_allowed {
                    item_ops.insert(
                        "put".to_string(),
                        json!({
                            "summary": format!("Upsert {} by key", table.name),
                            "description": format!("Insert or update a `{}` row by primary key", table.name),
                            "operationId": format!("upsert_{}_by_key", table.name),
                            "parameters": [
                                key_parameter.clone(),
                                {
                                    "name": "Prefer",
                                    "in": "header",
                                    "description": "Return representation or minimal response",
                                    "schema": {
                                        "type": "string",
                                        "enum": ["return=representation", "return=minimal"],
                                        "default": "return=representation"
                                    }
                                }
                            ],
                            "requestBody": {
                                "required": true,
                                "content": {
                                    "application/json": {
                                        "schema": {
                                            "$ref": format!("#/components/schemas/{}CreateInput", schema_name)
                                        }
                                    }
                                }
                            },
                            "responses": {
                                "200": {
                                    "description": "Row updated",
                                    "headers": {
                                        "ETag": {
                                            "description": "Row version entity tag",
                                            "schema": { "type": "string" }
                                        }
                                    },
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "$ref": format!("#/components/schemas/{}", schema_name)
                                            }
                                        }
                                    }
                                },
                                "201": {
                                    "description": "Row created",
                                    "headers": {
                                        "Location": {
                                            "description": "URI of created resource",
                                            "schema": { "type": "string" }
                                        },
                                        "ETag": {
                                            "description": "Row version entity tag",
                                            "schema": { "type": "string" }
                                        }
                                    },
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "$ref": format!("#/components/schemas/{}", schema_name)
                                            }
                                        }
                                    }
                                },
                                "204": {
                                    "description": "Upserted (minimal representation requested)"
                                },
                                "400": {
                                    "description": "Bad Request",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                },
                                "409": {
                                    "description": "Conflict",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                },
                                "422": {
                                    "description": "Unprocessable Content",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                }
                            }
                        }),
                    );
                }

                // DELETE /rest/{table}/{key}
                if delete_allowed {
                    item_ops.insert(
                        "delete".to_string(),
                        json!({
                            "summary": format!("Delete {} by key", table.name),
                            "description": format!("Delete a single row from `{}` by primary key", table.name),
                            "operationId": format!("delete_{}_by_key", table.name),
                            "parameters": [
                                key_parameter,
                                {
                                    "name": "If-Match",
                                    "in": "header",
                                    "description": "Optimistic concurrency version tag",
                                    "schema": { "type": "string" }
                                },
                                {
                                    "name": "Prefer",
                                    "in": "header",
                                    "description": "Return representation or minimal response",
                                    "schema": {
                                        "type": "string",
                                        "enum": ["return=representation", "return=minimal"],
                                        "default": "return=representation"
                                    }
                                }
                            ],
                            "responses": {
                                "200": {
                                    "description": "Deleted",
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "$ref": format!("#/components/schemas/{}", schema_name)
                                            }
                                        }
                                    }
                                },
                                "204": {
                                    "description": "Deleted (minimal representation requested)"
                                },
                                "404": {
                                    "description": "Not Found",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                },
                                "409": {
                                    "description": "Conflict",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                },
                                "412": {
                                    "description": "Precondition Failed (version mismatch)",
                                    "content": {
                                        "application/problem+json": {
                                            "schema": { "$ref": "#/components/schemas/ProblemDetails" }
                                        }
                                    }
                                }
                            }
                        }),
                    );
                }

                if !item_ops.is_empty() {
                    paths.insert(item_path, serde_json::Value::Object(item_ops));
                }
            }
        }

        // 4. Security schemes
        let security_schemes = json!({
            "bearerAuth": {
                "type": "http",
                "scheme": "bearer",
                "bearerFormat": "JWT",
                "description": "JWT bearer token"
            }
        });

        // 5. Assemble the complete OpenAPI 3.1 document
        json!({
            "openapi": "3.1.0",
            "info": {
                "title": "graphpg REST API",
                "version": "1.0.0",
                "description": "REST API generated by graphpg from PostgreSQL database metadata"
            },
            "servers": [
                {
                    "url": "/",
                    "description": "Current server"
                }
            ],
            "security": [
                {
                    "bearerAuth": []
                }
            ],
            "paths": paths,
            "components": {
                "schemas": schemas,
                "securitySchemes": security_schemes
            }
        })
    }

    /// Converts a table's columns and metadata into an OpenAPI 3.1 / JSON Schema object.
    fn generate_table_schema(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
    ) -> serde_json::Value {
        let mut properties = BTreeMap::new();
        let mut required = Vec::new();

        for col in &table.columns {
            if col.omit {
                continue;
            }

            let col_schema = Self::column_to_json_schema(col, catalog);
            properties.insert(col.name.clone(), col_schema);

            if !col.is_nullable && col.default_value.is_none() && !col.is_generated {
                required.push(col.name.clone());
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
            if let Some(type_val) = schema.get("type").and_then(|v| v.as_str()) {
                schema["type"] = json!([type_val, "null"]);
            }
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
            PgType::Inet | PgType::Cidr => json!({ "type": "string", "format": "ipv4" }),
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

    /// Generates typed query parameters for collection GET endpoints.
    fn generate_list_parameters(schema_name: &str) -> serde_json::Value {
        json!([
            {
                "name": "filter",
                "in": "query",
                "style": "deepObject",
                "explode": true,
                "description": "Filter criteria matching column operators",
                "schema": {
                    "$ref": format!("#/components/schemas/{}Filter", schema_name)
                }
            },
            {
                "name": "order",
                "in": "query",
                "description": "Ordering specification (e.g. 'created_at:desc' or '-created_at')",
                "schema": {
                    "type": "string"
                }
            },
            {
                "name": "first",
                "in": "query",
                "description": "Limit forward keyset pagination",
                "schema": {
                    "type": "integer",
                    "minimum": 1
                }
            },
            {
                "name": "after",
                "in": "query",
                "description": "Cursor for forward keyset pagination",
                "schema": {
                    "type": "string"
                }
            },
            {
                "name": "last",
                "in": "query",
                "description": "Limit backward keyset pagination",
                "schema": {
                    "type": "integer",
                    "minimum": 1
                }
            },
            {
                "name": "before",
                "in": "query",
                "description": "Cursor for backward keyset pagination",
                "schema": {
                    "type": "string"
                }
            },
            {
                "name": "offset",
                "in": "query",
                "description": "Offset pagination skip count",
                "schema": {
                    "type": "integer",
                    "minimum": 0
                }
            },
            {
                "name": "limit",
                "in": "query",
                "description": "Offset pagination limit count",
                "schema": {
                    "type": "integer",
                    "minimum": 1
                }
            },
            {
                "name": "fields",
                "in": "query",
                "description": "Comma-separated list of columns to select",
                "schema": {
                    "type": "string"
                }
            },
            {
                "name": "include",
                "in": "query",
                "description": "Comma-separated list of relations to embed",
                "schema": {
                    "type": "string"
                }
            }
        ])
    }

    /// Generates RFC 9457 Problem Details JSON Schema.
    fn generate_problem_details_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "description": "RFC 9457 Problem Details object",
            "properties": {
                "type": {
                    "type": "string",
                    "format": "uri",
                    "description": "URI reference identifying problem type"
                },
                "title": {
                    "type": "string",
                    "description": "Short human-readable summary of problem"
                },
                "status": {
                    "type": "integer",
                    "description": "HTTP status code"
                },
                "detail": {
                    "type": "string",
                    "description": "Human-readable explanation of error"
                },
                "instance": {
                    "type": "string",
                    "format": "uri-reference",
                    "description": "URI reference identifying specific occurrence"
                },
                "code": {
                    "type": "string",
                    "description": "Machine-readable error code"
                },
                "fields": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Offending fields or parameters"
                },
                "requestId": {
                    "type": "string",
                    "description": "Request correlation ID"
                }
            },
            "required": ["type", "title", "status", "detail"]
        })
    }

    /// Converts a table's writable columns into a JSON Schema for row creation.
    fn generate_create_input_schema(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
    ) -> serde_json::Value {
        let mut properties = BTreeMap::new();
        let mut required = Vec::new();

        for col in &table.columns {
            if col.omit || col.is_generated || col.readonly {
                continue;
            }

            let col_schema = Self::column_to_json_schema(col, catalog);
            properties.insert(col.field_name.clone(), col_schema);

            if !col.is_nullable && col.default_value.is_none() && !col.is_generated {
                required.push(col.field_name.clone());
            }
        }

        let mut schema = json!({
            "type": "object",
            "description": format!("Input object for creating a `{}` row", table.name),
            "properties": properties
        });

        if !required.is_empty() {
            schema["required"] = json!(required);
        }

        schema
    }

    /// Converts a table's writable columns into a JSON Schema for row partial update.
    fn generate_update_input_schema(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
    ) -> serde_json::Value {
        let mut properties = BTreeMap::new();

        for col in &table.columns {
            if col.omit || col.is_generated || col.readonly {
                continue;
            }

            let mut col_schema = Self::column_to_json_schema(col, catalog);
            if col.is_nullable {
                if let Some(type_val) = col_schema.get("type").and_then(|v| v.as_str()) {
                    col_schema["type"] = json!([type_val, "null"]);
                }
            }
            properties.insert(col.field_name.clone(), col_schema);
        }

        json!({
            "type": "object",
            "description": format!("Input object for updating a `{}` row (partial)", table.name),
            "properties": properties
        })
    }

    /// Generates filter schema for deepObject query parameters and search endpoint body.
    fn generate_filter_schema(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
    ) -> serde_json::Value {
        let mut properties = BTreeMap::new();

        for col in &table.columns {
            if col.omit {
                continue;
            }

            let col_type_schema = Self::pg_type_to_json_schema(&col.pg_type, catalog);
            let mut ops = BTreeMap::new();
            ops.insert("equalTo".to_string(), col_type_schema.clone());
            ops.insert("eq".to_string(), col_type_schema.clone());
            ops.insert("notEqualTo".to_string(), col_type_schema.clone());
            ops.insert("neq".to_string(), col_type_schema.clone());

            if col.pg_type.is_numeric()
                || matches!(
                    col.pg_type,
                    PgType::Date
                        | PgType::Time
                        | PgType::TimeTz
                        | PgType::Timestamp
                        | PgType::TimestampTz
                )
            {
                ops.insert("greaterThan".to_string(), col_type_schema.clone());
                ops.insert("gt".to_string(), col_type_schema.clone());
                ops.insert("greaterThanOrEqualTo".to_string(), col_type_schema.clone());
                ops.insert("gte".to_string(), col_type_schema.clone());
                ops.insert("lessThan".to_string(), col_type_schema.clone());
                ops.insert("lt".to_string(), col_type_schema.clone());
                ops.insert("lessThanOrEqualTo".to_string(), col_type_schema.clone());
                ops.insert("lte".to_string(), col_type_schema.clone());
            }

            if matches!(
                col.pg_type,
                PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext
            ) {
                ops.insert("like".to_string(), json!({ "type": "string" }));
                ops.insert("ilike".to_string(), json!({ "type": "string" }));
                ops.insert("startsWith".to_string(), json!({ "type": "string" }));
                ops.insert("endsWith".to_string(), json!({ "type": "string" }));
            }

            ops.insert(
                "in".to_string(),
                json!({ "type": "array", "items": col_type_schema }),
            );
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

        properties.insert(
            "and".to_string(),
            json!({
                "type": "array",
                "items": {
                    "$ref": format!("#/components/schemas/{}Filter", table.name)
                }
            }),
        );
        properties.insert(
            "or".to_string(),
            json!({
                "type": "array",
                "items": {
                    "$ref": format!("#/components/schemas/{}Filter", table.name)
                }
            }),
        );
        properties.insert(
            "not".to_string(),
            json!({
                "$ref": format!("#/components/schemas/{}Filter", table.name)
            }),
        );

        json!({
            "type": "object",
            "description": format!("Filter schema for `{}`", table.name),
            "properties": properties
        })
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
}
