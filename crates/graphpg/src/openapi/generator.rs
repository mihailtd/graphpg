//! Generates OpenAPI 3.1 documents from the database catalog and configuration.
//!
//! The document describes exactly what [`crate::server::rest`] does: the same tables (resolved
//! the same way), the same permission rules, the same query grammar and the same limits, so the
//! specification and the API cannot drift apart.

use crate::catalog::{ColumnMetadata, DatabaseCatalog, FieldKind, PgType, TableMetadata};
use crate::config::models::LimitsConfig;
use crate::config::models::Operation;
use crate::config::Config;
use crate::server::rest::{check_table_permissions, resolve_table, rest_schemas, REST_PREFIX};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct OpenApiGenerator;

/// `{"$ref": "#/components/schemas/<name>"}`.
pub(super) fn schema_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// A problem+json response.
pub(super) fn problem(description: &str) -> Value {
    json!({
        "description": description,
        "content": { "application/problem+json": { "schema": schema_ref("ProblemDetails") } }
    })
}

/// The responses every operation can return, plus `extra` (status, description).
pub(super) fn responses(success: Value, extra: &[(&str, &str)]) -> Value {
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

pub(super) fn prefer_param(write: bool) -> Value {
    if write {
        json!({
            "name": "Prefer", "in": "header",
            "description": "`return=minimal` answers 204 without a body (default: `return=representation`)",
            "schema": { "type": "string", "enum": ["return=representation", "return=minimal"] }
        })
    } else {
        json!({
            "name": "Prefer", "in": "header",
            "description": "`count=exact`, `count=planned`, or `count=estimated` adds `X-Total-Count`; exact costs a scan of every matching row, so it is off by default",
            "schema": { "type": "string", "enum": ["count=exact", "count=planned", "count=estimated"] }
        })
    }
}

/// `Prefer` of an update or upsert: also `previous` (the row as it was, `RETURNING OLD`).
pub(super) fn prefer_previous_param() -> Value {
    json!({
        "name": "Prefer", "in": "header",
        "description": "`return=minimal` answers 204 without a body. `previous` answers `{\"row\": …, \"previous\": …}`: the row as it is now and as it was before the write (`null` for a row an upsert inserted); it needs PostgreSQL 18 and is ignored without it.",
        "schema": { "type": "string", "examples": ["return=minimal", "previous"] }
    })
}

fn if_match_param(description: &str) -> Value {
    json!({
        "name": "If-Match", "in": "header",
        "description": description,
        "schema": { "type": "string" }
    })
}

/// Makes a schema accept `null` (OpenAPI 3.1 / JSON Schema 2020-12).
/// `HEAD` of a `GET` operation: the same parameters and headers, no body.
fn head_of(get: &Value, operation_id: String) -> Value {
    let mut head = get.clone();
    head["summary"] = json!(format!(
        "{} (headers only)",
        get["summary"].as_str().unwrap_or("Read")
    ));
    head["operationId"] = json!(operation_id);
    if let Some(responses) = head["responses"].as_object_mut() {
        for response in responses.values_mut() {
            if let Some(response) = response.as_object_mut() {
                response.remove("content");
                response.remove("links");
            }
        }
    }
    head
}

/// `OPTIONS`: the methods the path answers to, and the request media types of its writes.
fn options_of(operation_id: String, media: &[(&str, &str)]) -> Value {
    let mut headers = json!({
        "Allow": { "description": "The methods this path answers to", "schema": { "type": "string" } }
    });
    for (name, description) in media {
        headers[*name] = json!({ "description": description, "schema": { "type": "string" } });
    }
    json!({
        "summary": "Methods and media types",
        "operationId": operation_id,
        "responses": responses(json!({
            "204": { "description": "The methods allowed", "headers": headers }
        }), &[])
    })
}

fn key_param_for_column(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
    OpenApiGenerator::key_parameter(table, catalog)
}

pub(super) fn nullable(mut schema: Value) -> Value {
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
pub(super) fn alternation(names: &[String]) -> String {
    let escaped: Vec<String> = names.iter().map(|n| regex_escape(n)).collect();
    format!("({})", escaped.join("|"))
}

/// The `filter` of a write by filter.
fn collection_filter_param(name: &str) -> Value {
    json!({
        "name": "filter", "in": "query", "style": "deepObject", "explode": true,
        "description": "The rows to write: the filter a list takes, `filter[column][operator]=value`. Required unless `unfiltered=true`",
        "schema": schema_ref(&format!("{name}Filter"))
    })
}

fn unfiltered_param() -> Value {
    json!({
        "name": "unfiltered", "in": "query",
        "description": "Allow the write without a `filter`, which writes every row",
        "schema": { "type": "boolean", "default": false }
    })
}

/// What a column the rows of a bulk write do not set becomes.
fn missing_param() -> Value {
    json!({
        "name": "missing", "in": "query",
        "description": "What a column some row of the array sets and another does not becomes in the latter: `default` (the column's default) or `null`",
        "schema": { "type": "string", "enum": ["default", "null"], "default": "default" }
    })
}

/// RFC 6902 JSON Patch: an array of operations.
fn json_patch_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "required": ["op", "path"],
            "properties": {
                "op": { "type": "string", "enum": ["add", "remove", "replace", "move", "copy", "test"] },
                "path": { "type": "string", "description": "JSON Pointer into the row: `/title`, `/settings/theme`" },
                "from": { "type": "string", "description": "JSON Pointer to move or copy from" },
                "value": {}
            },
            "additionalProperties": false
        }
    })
}

/// Replaces the `$ref`s to components that do not exist with a plain object schema.
fn fix_dangling_refs(
    paths: &mut serde_json::Map<String, Value>,
    schemas: &mut serde_json::Map<String, Value>,
) {
    fn fix(value: &mut Value, names: &std::collections::HashSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(name) = map
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix("#/components/schemas/"))
                {
                    if !names.contains(name) {
                        *value = json!({ "type": "object" });
                        return;
                    }
                }
                map.values_mut().for_each(|v| fix(v, names));
            }
            Value::Array(items) => items.iter_mut().for_each(|v| fix(v, names)),
            _ => {}
        }
    }
    let names: std::collections::HashSet<String> = schemas.keys().cloned().collect();
    paths.values_mut().for_each(|v| fix(v, &names));
    schemas.values_mut().for_each(|v| fix(v, &names));
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
        Self::generate_with_schema(catalog, config, default_schema, None)
    }

    /// Like [`generate_for`](Self::generate_for), with the GraphQL schema the server serves:
    /// the arguments of search methods are typed from its input types.
    pub fn generate_with_schema(
        catalog: &DatabaseCatalog,
        config: &Config,
        default_schema: &str,
        graphql: Option<&apollo_compiler::Schema>,
    ) -> Value {
        let mut doc = Self::generate_unscoped(catalog, config, default_schema, graphql);
        // With several schemas, every resource and function also has the path of its own
        // schema, `/rest/{schema}/…`: the paths are fixed, one per schema, not a template
        let schemas = rest_schemas(catalog, default_schema);
        if schemas.len() > 1 {
            for schema in &schemas {
                let only = catalog.only_schema(schema);
                let scoped = Self::generate_unscoped(&only, config, schema, graphql);
                Self::merge_scoped(&mut doc, scoped, schema);
            }
        }
        if config.limits.rate_limit.enabled {
            Self::attach_rate_limits(&mut doc);
        }
        doc
    }

    /// With `limits.rate_limit` on: the `RateLimit-*` headers on every success, and a `429`
    /// (`RATE_LIMITED`, with `Retry-After`) on every operation. Off, the document is unchanged.
    fn attach_rate_limits(doc: &mut Value) {
        let header = |description: &str| json!({ "description": description, "schema": { "type": "integer", "minimum": 0 } });
        doc["components"]["headers"] = json!({
            "RateLimit-Limit": header("Requests the client's bucket holds (`limits.rate_limit`'s burst)"),
            "RateLimit-Remaining": header("Requests the client may still make now"),
            "RateLimit-Reset": header("Seconds until the client's bucket is full again"),
            "Retry-After": header("Seconds until the request may be retried")
        });
        let header_ref = |name: &str| json!({ "$ref": format!("#/components/headers/{name}") });
        doc["components"]["responses"]["RateLimited"] = json!({
            "description": "Too Many Requests: over the client's rate limit (`RATE_LIMITED`)",
            "headers": {
                "Retry-After": header_ref("Retry-After"),
                "RateLimit-Limit": header_ref("RateLimit-Limit"),
                "RateLimit-Remaining": header_ref("RateLimit-Remaining"),
                "RateLimit-Reset": header_ref("RateLimit-Reset")
            },
            "content": { "application/problem+json": { "schema": schema_ref("ProblemDetails") } }
        });
        let Some(paths) = doc["paths"].as_object_mut() else {
            return;
        };
        for item in paths.values_mut().filter_map(Value::as_object_mut) {
            for (method, operation) in item.iter_mut() {
                let is_operation = matches!(
                    method.as_str(),
                    "get" | "put" | "post" | "delete" | "options" | "head" | "patch"
                );
                let Some(responses) = operation
                    .get_mut("responses")
                    .and_then(Value::as_object_mut)
                    .filter(|_| is_operation)
                else {
                    continue;
                };
                for (status, response) in responses.iter_mut() {
                    if !status.starts_with('2') || response.get("$ref").is_some() {
                        continue;
                    }
                    let Some(response) = response.as_object_mut() else {
                        continue;
                    };
                    let headers = response.entry("headers").or_insert_with(|| json!({}));
                    if let Some(headers) = headers.as_object_mut() {
                        for name in ["RateLimit-Limit", "RateLimit-Remaining", "RateLimit-Reset"] {
                            headers.insert(name.to_string(), header_ref(name));
                        }
                    }
                }
                responses.insert(
                    "429".to_string(),
                    json!({ "$ref": "#/components/responses/RateLimited" }),
                );
            }
        }
    }

    /// Adds the paths and components of the document of one schema (`scoped`, written as if
    /// that schema were the only one) to `doc`, under `/rest/{schema}` and with the schema in
    /// the name of every component and operation.
    fn merge_scoped(doc: &mut Value, mut scoped: Value, schema: &str) {
        fn rename(value: &mut Value, schema: &str) {
            match value {
                Value::String(text) => {
                    if let Some(name) = text.strip_prefix("#/components/schemas/") {
                        if name != "ProblemDetails" {
                            *text = format!("#/components/schemas/{schema}.{name}");
                        }
                    }
                }
                Value::Array(items) => items.iter_mut().for_each(|v| rename(v, schema)),
                Value::Object(map) => {
                    for (key, v) in map.iter_mut() {
                        match (key.as_str(), &mut *v) {
                            ("operationId", Value::String(id)) => *id = format!("{schema}_{id}"),
                            _ => rename(v, schema),
                        }
                    }
                }
                _ => {}
            }
        }
        rename(&mut scoped, schema);
        let take = |scoped: &mut Value, pointer: &str| {
            scoped
                .pointer_mut(pointer)
                .and_then(Value::as_object_mut)
                .map(std::mem::take)
                .unwrap_or_default()
        };
        let paths = take(&mut scoped, "/paths");
        let components = take(&mut scoped, "/components/schemas");
        if let Some(target) = doc.pointer_mut("/paths").and_then(Value::as_object_mut) {
            for (path, item) in paths {
                // The root and the search over every resource are not a schema's
                if path == REST_PREFIX
                    || path == format!("{REST_PREFIX}/search")
                    || path.starts_with(&format!("{REST_PREFIX}/node/"))
                {
                    continue;
                }
                if let Some(rest) = path.strip_prefix(REST_PREFIX) {
                    target.insert(format!("{REST_PREFIX}/{schema}{rest}"), item);
                }
            }
        }
        if let Some(target) = doc
            .pointer_mut("/components/schemas")
            .and_then(Value::as_object_mut)
        {
            for (name, component) in components {
                if name != "ProblemDetails" {
                    target.insert(format!("{schema}.{name}"), component);
                }
            }
        }
    }

    fn generate_unscoped(
        catalog: &DatabaseCatalog,
        config: &Config,
        default_schema: &str,
        graphql: Option<&apollo_compiler::Schema>,
    ) -> Value {
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

        // GraphQL's `node(id:)`: a global ID names a table and a key
        if tables.iter().any(|t| t.implements_node()) {
            paths.insert(
                format!("{REST_PREFIX}/node/{{id}}"),
                json!({ "get": {
                    "summary": "Row by global ID",
                    "description": "The row a Relay global ID (the `id` GraphQL gives every row) names: a redirect to the row's own URL, query string kept",
                    "operationId": "get_node",
                    "parameters": [{
                        "name": "id", "in": "path", "required": true,
                        "description": "A Relay global ID: base64 of `<Type>:<key>`",
                        "schema": { "type": "string" }
                    }],
                    "responses": responses(json!({
                        "303": {
                            "description": "See Other: the row's URL",
                            "headers": { "Location": { "schema": { "type": "string" } } }
                        }
                    }), &[("404", "Not Found: no table has that type, or the ID is malformed")]),
                } }),
            );
        }

        let mut inputs = graphql.map(super::input_types::InputTypes::new);
        for table in &tables {
            let name = &table.name;
            let row_schema = Self::generate_table_schema(table, catalog);
            match Self::polymorphic_row_schemas(table, catalog, &row_schema) {
                Some(variants) => schemas.extend(variants),
                None => {
                    schemas.insert(name.clone(), row_schema);
                }
            }
            schemas.insert(
                format!("{name}WithPrevious"),
                json!({
                    "type": "object",
                    "description": "The row as it is now and as it was before the write (`Prefer: previous`)",
                    "properties": {
                        "row": schema_ref(name),
                        "previous": { "anyOf": [schema_ref(name), { "type": "null" }] }
                    },
                    "required": ["row", "previous"]
                }),
            );
            let (create, update) = Self::write_input_schemas(table, catalog, inputs.as_mut());
            schemas.insert(format!("{name}CreateInput"), create);
            schemas.insert(format!("{name}UpdateInput"), update);
            schemas.insert(
                format!("{name}Filter"),
                Self::generate_filter_schema(table, catalog, &tables),
            );
            schemas.insert(
                format!("{name}SearchRequest"),
                Self::generate_search_request_schema(table, catalog, &config.limits),
            );
            Self::generate_table_paths(table, catalog, config, &mut paths);
            Self::attach_error_codes(table, catalog, &mut paths, &mut schemas);
            Self::generate_aggregate_paths(table, config, &mut schemas, &mut paths);
            Self::generate_search_method_paths(table, catalog, config, graphql, &mut paths);
        }
        Self::generate_global_search_paths(catalog, config, &tables, &mut schemas, &mut paths);
        if let Some(inputs) = inputs {
            schemas.extend(inputs.into_components());
        }
        Self::generate_rpc_paths(catalog, config, default_schema, &tables, &mut paths);
        let webhooks = Self::generate_webhooks(&tables, config, &mut schemas);
        // The streaming endpoints belong to the GraphQL router, and need something to export
        if config.graphql.enabled && !tables.is_empty() {
            Self::generate_bulk_paths(&tables, &mut paths);
        }

        // The filter of a relation whose table the API does not serve has nothing to refer to
        fix_dangling_refs(&mut paths, &mut schemas);

        let mut doc = json!({
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
        });
        if !webhooks.is_empty() {
            doc["webhooks"] = Value::Object(webhooks);
        }
        doc
    }

    /// The codes a write of `table` can be rejected with (the GraphQL `<Type>ErrorCode` enum):
    /// `{table}ErrorCode` lists them with what each means, and `{table}Problem` is a problem whose
    /// `code` is one of them, the schema of the 400, 409 and 422 answers of its writes.
    fn attach_error_codes(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        paths: &mut serde_json::Map<String, Value>,
        schemas: &mut serde_json::Map<String, Value>,
    ) {
        let name = &table.name;
        let codes = catalog.error_codes(table);
        if codes.is_empty() {
            return;
        }
        let described: Vec<Value> = codes
            .iter()
            .map(|(code, doc)| json!({ "const": code, "description": doc }))
            .collect();
        schemas.insert(
            format!("{name}ErrorCode"),
            json!({
                "description": format!("Why a write of `{name}` rows can be rejected: the `code` of the 409 and 422 problems"),
                "type": "string",
                "oneOf": described
            }),
        );
        schemas.insert(
            format!("{name}Problem"),
            json!({
                "description": format!("A problem of a write to `{name}`; its `code` is one of `{name}ErrorCode` (or one of the general ones)"),
                "allOf": [
                    schema_ref("ProblemDetails"),
                    { "properties": { "code": { "anyOf": [schema_ref(&format!("{name}ErrorCode")), { "type": "string" }] } } }
                ]
            }),
        );
        for path in [
            format!("{REST_PREFIX}/{name}"),
            format!("{REST_PREFIX}/{name}/{{key}}"),
        ] {
            let Some(item) = paths.get_mut(&path).and_then(Value::as_object_mut) else {
                continue;
            };
            for method in ["post", "patch", "put", "delete"] {
                for status in ["400", "409", "422"] {
                    if let Some(content) = item.get_mut(method).and_then(|op| {
                        op.pointer_mut(&format!(
                            "/responses/{status}/content/application~1problem+json"
                        ))
                    }) {
                        *content = json!({ "schema": schema_ref(&format!("{name}Problem")) });
                    }
                }
            }
        }
    }

    /// `/export` and `/import/{type}`: streaming bulk data, next to the REST API (they share its
    /// authentication and Row-Level Security).
    fn generate_bulk_paths(tables: &[&TableMetadata], paths: &mut serde_json::Map<String, Value>) {
        paths.insert(
            "/export".to_string(),
            json!({
                "post": {
                    "summary": "Export a connection",
                    "description": "Streams every row a GraphQL query's single connection field (its `nodes`) selects, read from a server-side portal in batches, so neither side holds the whole result: for tables of any size, where a list is bounded by `limits.page_size`.",
                    "operationId": "export_rows",
                    "requestBody": { "required": true, "content": { "application/json": { "schema": {
                        "type": "object",
                        "required": ["query"],
                        "properties": {
                            "query": { "type": "string", "description": "A GraphQL query whose only root field is a connection" },
                            "operationName": { "type": "string" },
                            "variables": { "type": "object" },
                            "format": { "type": "string", "enum": ["ndjson", "csv"], "default": "ndjson" }
                        },
                        "additionalProperties": false
                    } } } },
                    "responses": responses(json!({
                        "200": {
                            "description": "The rows, one JSON object per line (or CSV)",
                            "content": {
                                "application/x-ndjson": { "schema": { "type": "string" } },
                                "text/csv": { "schema": { "type": "string" } }
                            }
                        }
                    }), &[("503", "Service Unavailable: the database is unavailable")]),
                }
            }),
        );
        let importable: Vec<&&TableMetadata> =
            tables.iter().filter(|t| t.operations.create).collect();
        if importable.is_empty() {
            return;
        }
        let types: Vec<String> = importable.iter().map(|t| t.type_name.clone()).collect();
        let one_of: Vec<Value> = importable
            .iter()
            .map(|t| schema_ref(&format!("{}CreateInput", t.name)))
            .collect();
        paths.insert(
            "/import/{type}".to_string(),
            json!({
                "post": {
                    "summary": "Import rows",
                    "description": "Inserts the rows of the body, one `Create<Type>Input` JSON object per line, checked like any write and streamed into a staging table, then inserted in one statement: in one transaction, so every row is inserted or none.",
                    "operationId": "import_rows",
                    "parameters": [{
                        "name": "type", "in": "path", "required": true,
                        "description": "The GraphQL type of the rows",
                        "schema": { "type": "string", "enum": types }
                    }],
                    "requestBody": { "required": true, "content": { "application/x-ndjson": { "schema": {
                        "type": "string",
                        "description": "One row per line, each a `CreateInput` of the type",
                        "examples": [one_of.clone()]
                    } } } },
                    "responses": responses(json!({
                        "200": {
                            "description": "Every row was inserted",
                            "content": { "application/json": { "schema": {
                                "type": "object",
                                "properties": { "data": { "type": "object", "properties": { "inserted": { "type": "integer" } } } }
                            } } }
                        }
                    }), &[("409", "Conflict: a unique constraint, with its typed `code`")]),
                }
            }),
        );
    }

    /// OpenAPI 3.1 `webhooks`: the change events the configuration declares for the tables the
    /// API serves (`entities.<table>.events`), one `<table>.<operation>` each, with a payload
    /// holding the row as it was (`old`) and as it is (`new`). Adds the payload components.
    fn generate_webhooks(
        tables: &[&TableMetadata],
        config: &Config,
        schemas: &mut serde_json::Map<String, Value>,
    ) -> serde_json::Map<String, Value> {
        use crate::config::models::EventOperation;
        let mut webhooks = serde_json::Map::new();
        for table in tables {
            let Some(events) = config
                .entities
                .get(&table.name)
                .and_then(|e| e.events.as_ref())
            else {
                continue;
            };
            let name = &table.name;
            let mut operations = events.operations.clone();
            operations.sort();
            operations.dedup();
            for op in operations {
                let component = Self::change_event_component(name, op, schemas);
                let event = format!("{name}.{}", op.as_str());
                let what = Self::change_event_what(op);
                let description = match &events.description {
                    Some(text) => format!("{what} in `{name}`. {text}"),
                    None => format!("{what} in `{name}`."),
                };
                webhooks.insert(
                    event.clone(),
                    Self::webhook_operation(
                        &format!("{event} event"),
                        &description,
                        &format!("webhook_{name}_{}", op.as_str()),
                        schema_ref(&component),
                    ),
                );
            }
        }
        // Event triggers (`events.triggers`): what is really sent, one webhook per trigger
        if config.events.enabled {
            for (trigger, t) in &config.events.triggers {
                let bare = t.table.rsplit('.').next().unwrap_or(&t.table);
                let Some(table) = tables.iter().find(|tb| {
                    tb.name == bare
                        && (!t.table.contains('.')
                            || t.table == format!("{}.{}", tb.schema, tb.name))
                }) else {
                    continue;
                };
                let mut operations: Vec<EventOperation> = t.operations.clone();
                operations.sort();
                operations.dedup();
                let refs: Vec<Value> = operations
                    .iter()
                    .map(|op| schema_ref(&Self::change_event_component(&table.name, *op, schemas)))
                    .collect();
                let ops: Vec<&str> = operations.iter().map(|o| o.as_str()).collect();
                let body = if refs.len() == 1 {
                    refs[0].clone()
                } else {
                    json!({ "oneOf": refs, "discriminator": { "propertyName": "op" } })
                };
                webhooks.insert(
                    trigger.clone(),
                    Self::webhook_operation(
                        &format!("{trigger} event trigger"),
                        &format!(
                            "Sent when `{}` rows change ({}), signed per Standard Webhooks (`webhook-id`, `webhook-timestamp`, `webhook-signature`). Delivered at least once: the event `id` is the same on every attempt.",
                            table.name,
                            ops.join(", ")
                        ),
                        &format!("event_{trigger}"),
                        body,
                    ),
                );
            }
            if !config.events.schedules.is_empty() {
                schemas.insert(
                    "ScheduleEvent".to_string(),
                    json!({
                        "type": "object",
                        "description": "An event of a schedule: a cron slot, or one scheduled from SQL",
                        "properties": {
                            "id": { "type": "string", "format": "uuid", "description": "Identifies the event, the same on every delivery attempt" },
                            "createdAt": { "type": "string", "format": "date-time" },
                            "schedule": { "type": "string", "description": "The schedule's name" },
                            "scheduledFor": { "type": ["string", "null"], "format": "date-time", "description": "The cron slot; null for an event scheduled from SQL" },
                            "payload": { "description": "The schedule's `payload`, or the one given to `schedule()`" }
                        },
                        "required": ["id", "createdAt", "schedule", "scheduledFor", "payload"]
                    }),
                );
            }
            for (name, s) in &config.events.schedules {
                let when = match &s.cron {
                    Some(cron) => {
                        format!("on the schedule `{cron}` (UTC), and when scheduled from SQL")
                    }
                    None => "when scheduled from SQL (`schedule()`)".to_string(),
                };
                webhooks.insert(
                    name.clone(),
                    Self::webhook_operation(
                        &format!("{name} schedule"),
                        &format!(
                            "Sent {when}, signed per Standard Webhooks. Delivered at least once."
                        ),
                        &format!("schedule_{name}"),
                        schema_ref("ScheduleEvent"),
                    ),
                );
            }
        }
        webhooks
    }

    fn change_event_what(op: crate::config::models::EventOperation) -> &'static str {
        use crate::config::models::EventOperation;
        match op {
            EventOperation::Insert => "A row was inserted",
            EventOperation::Update => "A row was updated",
            EventOperation::Delete => "A row was deleted",
        }
    }

    /// The payload component of a change of `table` (`<table><Op>Event`), added once.
    fn change_event_component(
        name: &str,
        op: crate::config::models::EventOperation,
        schemas: &mut serde_json::Map<String, Value>,
    ) -> String {
        use crate::config::models::EventOperation;
        let mut capitalized = op.as_str().to_string();
        capitalized[..1].make_ascii_uppercase();
        let component = format!("{name}{capitalized}Event");
        if schemas.contains_key(&component) {
            return component;
        }
        let (old, new) = match op {
            EventOperation::Insert => (json!({ "type": "null" }), schema_ref(name)),
            EventOperation::Update => (schema_ref(name), schema_ref(name)),
            EventOperation::Delete => (schema_ref(name), json!({ "type": "null" })),
        };
        let what = Self::change_event_what(op);
        schemas.insert(
            component.clone(),
            json!({
                "type": "object",
                "description": format!("{what} in `{name}`: the row as it was (`old`) and as it is (`new`)"),
                "properties": {
                    "id": { "type": "string", "format": "uuid", "description": "Identifies the event, the same on every delivery attempt" },
                    "createdAt": { "type": "string", "format": "date-time", "description": "When the change was committed" },
                    "table": {
                        "type": "object",
                        "properties": { "schema": { "type": "string" }, "name": { "type": "string", "const": name } },
                        "required": ["schema", "name"]
                    },
                    "op": { "type": "string", "const": op.as_str() },
                    "old": old,
                    "new": new
                },
                "required": ["id", "createdAt", "table", "op", "old", "new"]
            }),
        );
        component
    }

    fn webhook_operation(
        summary: &str,
        description: &str,
        operation_id: &str,
        body: Value,
    ) -> Value {
        json!({
            "post": {
                "summary": summary,
                "description": description,
                "operationId": operation_id,
                "requestBody": { "required": true, "content": {
                    "application/json": { "schema": body }
                } },
                "responses": {
                    "2XX": { "description": "The event was received: any 2xx acknowledges it" }
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
        let upsert_ops = allowed(Operation::Upsert);
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
                "parameters": Self::list_parameters_with_range(table, catalog, &config.limits),
                "responses": responses(json!({
                    "200": {
                        "description": format!("A list of {name} rows"),
                        "headers": {
                            "Accept-Ranges": {
                                "description": "`items`: the list takes `Range: items=A-B`",
                                "schema": { "type": "string", "const": "items" }
                            },
                            "Link": {
                                "description": "RFC 8288 `next` / `prev` links carrying the cursors",
                                "schema": { "type": "string" }
                            },
                            "X-Total-Count": {
                                "description": "Number of matching rows; with `Prefer: count=exact`, `count=planned`, or `count=estimated`",
                                "schema": { "type": "integer" }
                            },
                            "X-Total-Count-Estimate": {
                                "description": "`true` if `X-Total-Count` is a planner estimate",
                                "schema": { "type": "string" }
                            },
                            "Preference-Applied": { "schema": { "type": "string" } }
                        },
                        "content": {
                            "application/json": { "schema": rows.clone() },
                            "application/x-ndjson": { "schema": { "type": "string", "description": "One row per line" } },
                            "text/csv": { "schema": { "type": "string" } }
                        }
                    },
                    "206": {
                        "description": "Partial Content: the rows of `Range: items=A-B`",
                        "headers": {
                            "Content-Range": {
                                "description": "`items A-B/N`, with `*` for `N` unless `Prefer: count=...` asked for the count",
                                "schema": { "type": "string" }
                            },
                            "Link": { "schema": { "type": "string" } },
                            "X-Total-Count": { "schema": { "type": "integer" } },
                            "X-Total-Count-Estimate": {
                                "description": "`true` if `X-Total-Count` is a planner estimate",
                                "schema": { "type": "string" }
                            },
                            "Preference-Applied": { "schema": { "type": "string" } }
                        },
                        "content": {
                            "application/json": { "schema": rows.clone() },
                            "application/x-ndjson": { "schema": { "type": "string", "description": "One row per line" } },
                            "text/csv": { "schema": { "type": "string" } }
                        }
                    }
                }), &[("406", "Not Acceptable: the list is `application/json`, `application/x-ndjson` or `text/csv`"),
                      ("416", "Range Not Satisfiable: past the end of the list, with its size in `Content-Range: items */N`")]),
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
                    { "type": "array", "minItems": 1, "maxItems": config.limits.bulk_rows,
                      "items": schema_ref(&format!("{name}CreateInput")) }
                ] })
            } else {
                schema_ref(&format!("{name}CreateInput"))
            };
            collection.insert("post".into(), json!({
                "summary": format!("Create {name}"),
                "description": format!(
                    "Create one row (object body) or, with an array body, {} in one transaction. Nested objects are nested writes, in the shape of the GraphQL input.",
                    match config.limits.bulk_rows {
                        Some(max) => format!("up to {max} rows"),
                        None => "many rows".to_string(),
                    }
                ),
                "operationId": format!("create_{name}"),
                "parameters": if create_many { vec![prefer_param(true), missing_param()] } else { vec![prefer_param(true)] },
                "requestBody": { "required": true, "content": {
                    "application/json": { "schema": body },
                    "text/csv": { "schema": { "type": "string" } },
                    "multipart/form-data": { "schema": Self::multipart_schema(table, catalog) }
                } },
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
                    ("415", "Unsupported Media Type: send JSON, CSV rows or `multipart/form-data`"),
                    ("422", "Unprocessable Content: a validation failure, with its typed `code`"),
                ]),
            }));
        }
        let update_many = allowed(Operation::UpdateMany);
        let delete_many = allowed(Operation::DeleteMany);
        let bulk_upsert = upsert_ops && create_many && table.has_upsert();
        let limit_text = match config.limits.bulk_rows {
            Some(max) => format!("at most {max} rows (`limits.bulk_rows`): above that the write is rolled back with `BULK_LIMIT_EXCEEDED` (413)"),
            None => "any number of rows".to_string(),
        };
        let affected_headers = json!({
            "X-Affected-Count": { "description": "Number of rows written", "schema": { "type": "integer" } },
            "Preference-Applied": { "schema": { "type": "string" } }
        });
        if update_many {
            collection.insert("patch".into(), json!({
                "summary": format!("Update {name} by filter"),
                "description": format!("Set the given columns in every row the `filter` matches, in one statement and one transaction; {limit_text}. Without a `filter` the write is refused (`FILTER_REQUIRED`) unless `unfiltered=true`. A JSON Patch applies to one row (`PATCH …/{{key}}`)."),
                "operationId": format!("update_many_{name}"),
                "parameters": [collection_filter_param(name), unfiltered_param(), prefer_param(true)],
                "requestBody": { "required": true, "content": {
                    "application/json": { "schema": schema_ref(&format!("{name}UpdateInput")) },
                    "application/merge-patch+json": { "schema": schema_ref(&format!("{name}UpdateInput")) }
                } },
                "responses": responses(json!({
                    "200": {
                        "description": "The updated rows",
                        "headers": affected_headers.clone(),
                        "content": json_content(rows.clone())
                    },
                    "204": { "description": "Updated (`Prefer: return=minimal`); `X-Affected-Count` has the count", "headers": affected_headers.clone() }
                }), &[
                    ("409", "Conflict: a unique constraint, with its typed `code`"),
                    ("413", "Content Too Large: the filter matches more rows than `limits.bulk_rows`"),
                    ("415", "Unsupported Media Type: send JSON or a merge patch"),
                    ("422", "Unprocessable Content: a validation failure, with its typed `code`"),
                ]),
            }));
        }
        if delete_many {
            collection.insert("delete".into(), json!({
                "summary": format!("Delete {name} by filter"),
                "description": format!("Delete every row the `filter` matches, in one statement and one transaction; {limit_text}. Without a `filter` the write is refused (`FILTER_REQUIRED`) unless `unfiltered=true`."),
                "operationId": format!("delete_many_{name}"),
                "parameters": [collection_filter_param(name), unfiltered_param(), prefer_param(true)],
                "responses": responses(json!({
                    "200": {
                        "description": "The deleted rows",
                        "headers": affected_headers.clone(),
                        "content": json_content(rows.clone())
                    },
                    "204": { "description": "Deleted (`Prefer: return=minimal`); `X-Affected-Count` has the count", "headers": affected_headers.clone() }
                }), &[
                    ("409", "Conflict: a foreign key still refers to a row"),
                    ("413", "Content Too Large: the filter matches more rows than `limits.bulk_rows`"),
                ]),
            }));
        }
        if bulk_upsert {
            let keys: Vec<String> = table
                .conflict_keys()
                .map(|(_, cols)| {
                    cols.iter()
                        .map(|c| c.field_name.clone())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            let updatable: Vec<String> = table
                .writable_columns()
                .map(|c| c.field_name.clone())
                .collect();
            collection.insert("put".into(), json!({
                "summary": format!("Upsert many {name}"),
                "description": format!("Insert the rows of the array ({}), and for those whose `conflict` key exists either update them (`resolution=merge`, the default: the columns the rows set, or those of `update`) or leave them (`resolution=ignore`). One statement, one transaction.", limit_text),
                "operationId": format!("upsert_many_{name}"),
                "parameters": [
                    {
                        "name": "conflict", "in": "query", "style": "form", "explode": false,
                        "description": "The unique key rows are matched by: its columns, comma-separated (default: the primary key)",
                        "schema": { "type": "array", "items": { "type": "string" }, "examples": keys }
                    },
                    {
                        "name": "resolution", "in": "query",
                        "description": "What happens to a row whose key exists: `merge` updates it, `ignore` leaves it",
                        "schema": { "type": "string", "enum": ["merge", "ignore"], "default": "merge" }
                    },
                    {
                        "name": "update", "in": "query", "style": "form", "explode": false,
                        "description": "Columns a merge updates (default: those the rows set, but the key)",
                        "schema": { "type": "array", "items": { "type": "string", "enum": updatable } }
                    },
                    missing_param(),
                    prefer_param(true)
                ],
                "requestBody": { "required": true, "content": {
                    "application/json": { "schema": { "type": "array", "minItems": 1, "maxItems": config.limits.bulk_rows,
                        "items": schema_ref(&format!("{name}CreateInput")) } },
                    "text/csv": { "schema": { "type": "string" } }
                } },
                "responses": responses(json!({
                    "200": {
                        "description": "The rows as they are now (those `ignore` left are the existing ones)",
                        "headers": affected_headers.clone(),
                        "content": json_content(rows.clone())
                    },
                    "204": { "description": "Upserted (`Prefer: return=minimal`)", "headers": affected_headers.clone() }
                }), &[
                    ("409", "Conflict: a unique constraint other than the `conflict` key"),
                    ("413", "Content Too Large: more rows than `limits.bulk_rows`"),
                    ("422", "Unprocessable Content: a validation failure, with its typed `code`"),
                ]),
            }));
        }
        if let Some(get) = collection.get("get").cloned() {
            collection.insert("head".into(), head_of(&get, format!("head_{name}")));
        }
        if !collection.is_empty() {
            let mut media: Vec<(&str, &str)> = Vec::new();
            if create {
                media.push(("Accept-Post", "The request media types of a create"));
            }
            if update_many {
                media.push((
                    "Accept-Patch",
                    "`application/json`, `application/merge-patch+json`",
                ));
            }
            let media = &media[..];
            collection.insert(
                "options".into(),
                options_of(format!("options_{name}"), media),
            );
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
                                "X-Total-Count": { "description": "With `Prefer: count=exact`, `count=planned`, or `count=estimated`", "schema": { "type": "integer" } },
                                "X-Total-Count-Estimate": {
                                    "description": "`true` if `X-Total-Count` is a planner estimate",
                                    "schema": { "type": "string" }
                                },
                                "X-Next-Cursor": { "description": "Cursor of the next page, if there is one", "schema": { "type": "string" } },
                                "X-Previous-Cursor": { "description": "Cursor of the previous page, if there is one", "schema": { "type": "string" } },
                                "Preference-Applied": { "schema": { "type": "string" } }
                            },
                            "content": {
                                "application/json": { "schema": rows.clone() },
                                "application/x-ndjson": { "schema": { "type": "string", "description": "One row per line" } },
                                "text/csv": { "schema": { "type": "string" } }
                            }
                        }
                    }), &[("406", "Not Acceptable: the list is `application/json`, `application/x-ndjson` or `text/csv`")]),
                }
            }));
        }

        if !has_key {
            return;
        }
        let key_param = Self::key_parameter(table, catalog);
        let by = Self::by_parameter(table);
        let mut item = serde_json::Map::new();

        if read {
            let mut params = vec![
                key_param.clone(),
                by.clone(),
                Self::fields_param(table, catalog),
                Self::include_param(table, catalog),
            ];
            params.extend(Self::embed_parameters(
                table,
                catalog,
                &config.limits,
                false,
            ));
            params.retain(|p| !p.is_null());
            if table.has_row_version() {
                params.push(json!({
                    "name": "If-None-Match",
                    "in": "header",
                    "description": "Row version: returns 304 without a body when it matches",
                    "schema": { "type": "string" }
                }));
            }
            let links = Self::links_of(table, catalog, config);
            let mut ok_map = json!({
                "200": {
                    "description": format!("The requested {name} row"),
                    "headers": { "ETag": { "description": "Row version", "schema": { "type": "string" } } },
                    "content": json_content(row.clone())
                }
            });
            if table.has_row_version() {
                ok_map["304"] = json!({
                    "description": "Not Modified: the row has not changed since the version in `If-None-Match`",
                    "headers": { "ETag": { "description": "Row version", "schema": { "type": "string" } } }
                });
            }
            let mut get = json!({
                "summary": format!("Get {name} by key"),
                "description": format!("Retrieve a single row from `{name}` by primary key"),
                "operationId": format!("get_{name}_by_key"),
                "parameters": params,
                "responses": responses(ok_map, &[("404", "Not Found")]),
            });
            if !links.is_empty() {
                get["responses"]["200"]["links"] = Value::Object(links);
            }
            item.insert("get".into(), get);
        }
        let write_responses = |ok: &str, previous: bool| {
            let content = if previous {
                json!({ "oneOf": [row.clone(), schema_ref(&format!("{name}WithPrevious"))] })
            } else {
                row.clone()
            };
            responses(
                json!({
                    "200": {
                        "description": ok,
                        "headers": {
                            "ETag": { "description": "Row version", "schema": { "type": "string" } },
                            "Preference-Applied": { "schema": { "type": "string" } }
                        },
                        "content": json_content(content)
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
                "description": "Update the given fields: a JSON body (or an RFC 7396 merge patch: `null` sets a column to NULL), or an RFC 6902 JSON Patch (`application/json-patch+json`) applied to the row as the API shows it (`/title`, `/settings/theme`), answered with 422 `PATCH_FAILED` when an operation does not apply. The key cannot be changed: a key in the body must match the URL.",
                "operationId": format!("update_{name}_by_key"),
                "parameters": [key_param.clone(), by.clone(),
                    if_match_param("Row version (the `ETag`) the update must apply to; 412 if the row changed"),
                    prefer_previous_param()],
                "requestBody": { "required": true, "content": {
                    "application/json": { "schema": schema_ref(&format!("{name}UpdateInput")) },
                    "application/merge-patch+json": { "schema": schema_ref(&format!("{name}UpdateInput")) },
                    "application/json-patch+json": { "schema": json_patch_schema() },
                    "multipart/form-data": { "schema": Self::multipart_schema(table, catalog) }
                } },
                "responses": write_responses("Updated", true),
            }));
        }
        if upsert {
            item.insert("put".into(), json!({
                "summary": format!("Upsert {name} by key"),
                "description": "Insert the row or update its columns (200 either way). With `If-Match` the row must exist at that version, so it is an update.",
                "operationId": format!("upsert_{name}_by_key"),
                "parameters": [key_param.clone(),
                    if_match_param("Optional: require the row to exist at this version (the `ETag`)"),
                    prefer_previous_param()],
                "requestBody": { "required": true, "content": json_content(schema_ref(&format!("{name}CreateInput"))) },
                "responses": write_responses("Row inserted or updated", true),
            }));
        }
        if delete {
            item.insert("delete".into(), json!({
                "summary": format!("Delete {name} by key"),
                "description": format!("Delete a single row from `{name}` by primary key"),
                "operationId": format!("delete_{name}_by_key"),
                "parameters": [key_param, by,
                    if_match_param("Row version (the `ETag`) the delete must apply to; 412 if the row changed"),
                    prefer_param(true)],
                "responses": write_responses("Deleted: the row", false),
            }));
        }
        if let Some(get) = item.get("get").cloned() {
            item.insert("head".into(), head_of(&get, format!("head_{name}_by_key")));
        }
        if !item.is_empty() {
            let media: &[(&str, &str)] = if update {
                &[("Accept-Patch", "The request media types of an update")]
            } else {
                &[]
            };
            item.insert(
                "options".into(),
                options_of(format!("options_{name}_by_key"), media),
            );
            // A parameter a table has no use for (`by`) is null
            for operation in item.values_mut() {
                if let Some(params) = operation
                    .get_mut("parameters")
                    .and_then(Value::as_array_mut)
                {
                    params.retain(|p| !p.is_null());
                }
            }
            paths.insert(format!("{REST_PREFIX}/{name}/{{key}}"), Value::Object(item));
        }

        // The bytes of a `bytea` column
        let binary: Vec<String> = table
            .columns
            .iter()
            .filter(|c| !c.omit && c.pg_type == crate::catalog::PgType::Bytea)
            .map(|c| c.field_name.clone())
            .collect();
        if !binary.is_empty() && (read || update) {
            let column_param = json!({
                "name": "column", "in": "path", "required": true,
                "description": "A binary (`bytea`) column",
                "schema": { "type": "string", "enum": binary }
            });
            let bytes = json!({ "type": "string", "contentMediaType": "application/octet-stream" });
            let mut ops = serde_json::Map::new();
            if read {
                let get = json!({
                    "summary": format!("Get a binary column of {name}"),
                    "description": "The bytes of the column. The media type is recognized from the first bytes (`image/png`, `application/pdf`, ...), else `application/octet-stream`.",
                    "operationId": format!("get_{name}_column"),
                    "parameters": [key_param_for_column(table, catalog), column_param.clone()],
                    "responses": responses(json!({
                        "200": { "description": "The value", "content": { "*/*": { "schema": bytes.clone() } } }
                    }), &[("404", "Not Found: no such row, or the value is null"),
                          ("406", "Not Acceptable")]),
                });
                ops.insert("head".into(), head_of(&get, format!("head_{name}_column")));
                ops.insert("get".into(), get);
            }
            if update {
                ops.insert("put".into(), json!({
                    "summary": format!("Replace a binary column of {name}"),
                    "description": "The request body, of any media type, is the new value.",
                    "operationId": format!("put_{name}_column"),
                    "parameters": [key_param_for_column(table, catalog), column_param,
                        if_match_param("Optional: require the row to be at this version (the `ETag`)")],
                    "requestBody": { "required": true, "content": { "*/*": { "schema": bytes } } },
                    "responses": responses(json!({ "204": { "description": "Replaced" } }),
                        &[("404", "Not Found"), ("412", "Precondition Failed: the row changed since the `If-Match` version")]),
                }));
            }
            ops.insert(
                "options".into(),
                options_of(format!("options_{name}_column"), &[]),
            );
            paths.insert(
                format!("{REST_PREFIX}/{name}/{{key}}/{{column}}"),
                Value::Object(ops),
            );
        }
    }

    /// OpenAPI 3.1 `links` of a single-row response, inferred from the foreign keys: to the row a
    /// key points at (`get_<table>_by_key`, the key read from the response) and to the rows that
    /// point at this one (`search_<table>`, filtered by this row's key). Only keys of one
    /// column, only to resources the API serves and the caller may read.
    fn links_of(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        config: &Config,
    ) -> serde_json::Map<String, Value> {
        let readable = |t: &TableMetadata| {
            !t.primary_key.is_empty()
                && check_table_permissions(t, Some(config), Operation::Read).is_ok()
        };
        let field_of = |t: &TableMetadata, column: &str| {
            t.get_column(column)
                .filter(|c| !c.omit)
                .map(|c| c.field_name.clone())
        };
        let mut links = serde_json::Map::new();
        let mut add = |name: &str, constraint: &str, link: Value| {
            let key = if links.contains_key(name) {
                format!("{name}_{constraint}")
            } else {
                name.to_string()
            };
            links.insert(key, link);
        };

        // This row's foreign keys: the row they point at
        for fk in &table.foreign_keys {
            if fk.omit || fk.columns.len() != 1 || fk.foreign_columns.len() != 1 {
                continue;
            }
            let Some(target) = catalog
                .tables
                .iter()
                .find(|t| t.schema == fk.foreign_schema && t.name == fk.foreign_table)
            else {
                continue;
            };
            let (Some(from), true) = (
                field_of(table, &fk.columns[0]),
                readable(target) && target.primary_key == fk.foreign_columns,
            ) else {
                continue;
            };
            add(
                &target.name,
                &fk.constraint_name,
                json!({
                    "operationId": format!("get_{}_by_key", target.name),
                    "description": format!(
                        "The `{}` this `{}` refers to (`{}`)",
                        target.name, table.name, fk.columns[0]
                    ),
                    "parameters": { "key": format!("$response.body#/{from}") }
                }),
            );
        }

        // Rows whose foreign keys point at this row
        for other in &catalog.tables {
            if !readable(other) {
                continue;
            }
            for fk in &other.foreign_keys {
                if fk.omit_reverse
                    || fk.columns.len() != 1
                    || fk.foreign_columns.len() != 1
                    || fk.foreign_schema != table.schema
                    || fk.foreign_table != table.name
                {
                    continue;
                }
                let (Some(filter_field), Some(key_field)) = (
                    field_of(other, &fk.columns[0]),
                    field_of(table, &fk.foreign_columns[0]),
                ) else {
                    continue;
                };
                add(
                    &other.name,
                    &fk.constraint_name,
                    json!({
                        "operationId": format!("search_{}", other.name),
                        "description": format!(
                            "The `{}` rows that refer to this `{}` (`{}`)",
                            other.name, table.name, fk.columns[0]
                        ),
                        "requestBody": {
                            "filter": { filter_field: { "eq": format!("$response.body#/{key_field}") } }
                        }
                    }),
                );
            }
        }
        links
    }

    /// The schema of a `multipart/form-data` write: the create input, with the files of
    /// `bytea` columns as binary parts instead of Base64 text.
    fn multipart_schema(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let mut schema = Self::generate_create_input_schema(table, catalog);
        if let Some(properties) = schema["properties"].as_object_mut() {
            for column in table
                .columns
                .iter()
                .filter(|c| c.pg_type == crate::catalog::PgType::Bytea)
            {
                if let Some(property) = properties.get_mut(&column.field_name) {
                    *property =
                        json!({ "type": "string", "contentMediaType": "application/octet-stream" });
                }
            }
        }
        schema
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

    /// The fields of a row: its columns, and the columns of the subtypes of a table-per-type
    /// hierarchy.
    pub(super) fn visible_fields(table: &TableMetadata, catalog: &DatabaseCatalog) -> Vec<String> {
        table
            .visible_columns()
            .map(|c| c.field_name.clone())
            .chain(catalog.cti_columns(table).into_iter().map(|(name, _)| name))
            .chain(
                catalog
                    .object_fields(&table.schema, &table.name)
                    .iter()
                    .filter(|f| matches!(f.kind, FieldKind::Function(_)))
                    .filter(|f| crate::server::rest::computed_value_exists(catalog, table, &f.name))
                    .map(|f| f.name.clone()),
            )
            .collect()
    }

    pub(super) fn fields_param(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        json!({
            "name": "fields", "in": "query", "style": "form", "explode": false,
            "description": "Columns to return (default: all)",
            "schema": { "type": "array", "items": { "type": "string", "enum": Self::visible_fields(table, catalog) } }
        })
    }

    /// What a row of a list can carry besides its columns: its cursor, window values among the
    /// matching rows and running sums (the `edges` of the GraphQL connection).
    fn edge_parameters(table: &TableMetadata) -> Vec<Value> {
        let numeric: Vec<String> = table
            .visible_columns()
            .filter(|c| c.pg_type.is_numeric())
            .map(|c| c.field_name.clone())
            .collect();
        let groups: Vec<String> = table
            .group_by_values()
            .iter()
            .map(|(_, c, unit)| match unit {
                None => c.field_name.clone(),
                Some("bin") => format!("{}:binned", c.field_name),
                Some(unit) => format!("{}:{unit}", c.field_name),
            })
            .collect();
        let mut params = vec![
            json!({
                "name": "cursors", "in": "query",
                "description": "Each row carries its own cursor in `_cursor`: send it back as `after` / `before` to continue from that row",
                "schema": { "type": "boolean", "default": false }
            }),
            json!({
                "name": "window", "in": "query", "style": "form", "explode": false,
                "description": "Window values of each row among all the matching rows, in the list's order, pages before this one included: `_window.rowNumber`, `rank`, `denseRank`, `percentRank`",
                "schema": { "type": "array", "items": { "type": "string", "enum": ["rowNumber", "rank", "denseRank", "percentRank"] } }
            }),
            json!({
                "name": "partition", "in": "query", "style": "form", "explode": false,
                "description": "Number the rows (`window`, `running`) within the groups of rows sharing these values; `column`, `column:unit` for dates",
                "schema": { "type": "array", "items": { "type": "string", "enum": groups } }
            }),
        ];
        if !numeric.is_empty() {
            params.push(json!({
                "name": "running", "in": "query", "style": "form", "explode": false,
                "description": "Running sums of these columns up to each row, in the list's order (a ledger's balance): `_window.runningSum.<column>`",
                "schema": { "type": "array", "items": { "type": "string", "enum": numeric } }
            }));
        }
        params
    }

    /// `by`: address a row by one of its other unique keys instead of the primary key
    /// (`/rest/users/alice?by=username`), or null when the table has none.
    fn by_parameter(table: &TableMetadata) -> Value {
        let keys: Vec<String> = table
            .lookup_keys()
            .map(|(_, cols)| {
                cols.iter()
                    .map(|c| c.field_name.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect();
        if keys.is_empty() || !table.operations.read {
            return Value::Null;
        }
        json!({
            "name": "by", "in": "query",
            "description": "Address the row by this unique key instead of the primary key: the path then holds the key's values, comma-separated in this order",
            "schema": { "type": "string", "enum": keys }
        })
    }

    /// What a list can be ordered by besides the table's own columns, as the REST grammar spells
    /// it: `author.username` (a column of a to-one relation's row), `messagesCount` (the number
    /// of rows of a to-many relation), a computed column, and `natural` / `primary_key`.
    pub(super) fn order_names(table: &TableMetadata, catalog: &DatabaseCatalog) -> Vec<String> {
        use crate::catalog::{FunctionReturn, PgType};
        let mut names = vec!["natural".to_string(), "primary_key".to_string()];
        for field in catalog.object_fields(&table.schema, &table.name) {
            match &field.kind {
                FieldKind::Relation(rel) if rel.is_many => {
                    if !rel.history {
                        names.push(format!("{}Count", field.name));
                    }
                }
                FieldKind::Relation(rel) if rel.junction.is_none() => {
                    let Some(target) = catalog.find_table(&rel.target_schema, &rel.target_table)
                    else {
                        continue;
                    };
                    for col in target
                        .visible_columns()
                        .filter(|c| c.pg_type.is_orderable())
                    {
                        names.push(format!("{}.{}", field.name, col.field_name));
                    }
                }
                FieldKind::Function(i) => {
                    let f = &catalog.functions[*i];
                    let orderable = match &f.returns {
                        FunctionReturn::Value(t) => {
                            t.is_orderable() && !matches!(t, PgType::Array(_))
                        }
                        _ => false,
                    };
                    let no_other_args = f.arguments.iter().skip(1).all(|a| a.has_default);
                    if f.row_argument && orderable && !f.returns_set && no_other_args {
                        names.push(field.name.clone());
                    }
                }
                _ => {}
            }
        }
        names
    }

    /// The to-one and to-many relations (not their connections) and the computed relationships
    /// (functions mounted on the table that return rows) that can be embedded.
    pub(super) fn relation_names(table: &TableMetadata, catalog: &DatabaseCatalog) -> Vec<String> {
        Self::embeddables(table, catalog)
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    pub(super) fn embeddables<'a>(
        table: &'a TableMetadata,
        catalog: &'a DatabaseCatalog,
    ) -> Vec<crate::server::rest::Embeddable<'a>> {
        catalog
            .object_fields(&table.schema, &table.name)
            .iter()
            .filter(|f| {
                matches!(
                    f.kind,
                    FieldKind::Relation(_)
                        | FieldKind::Function(_)
                        | FieldKind::Tree(_)
                        | FieldKind::ArcTarget
                )
            })
            .filter_map(|f| crate::server::rest::find_embeddable(catalog, table, &f.name))
            .collect()
    }

    /// The controls of every embedded relation of `table`, one set of parameters each:
    /// `fields[rel]` and, for a list of rows, `filter[rel]`, `order[rel]`, `first[rel]` and
    /// `offset[rel]`; and, on a collection, `inner` and `anti`.
    pub(super) fn embed_parameters(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        limits: &LimitsConfig,
        collection: bool,
    ) -> Vec<Value> {
        let mut params = Vec::new();
        if table.period_key().is_some() {
            params.push(json!({
                "name": "asOf", "in": "query",
                "description": "Read the table as it was at this time: the versions whose period contains it",
                "schema": { "type": "string", "format": "date-time" }
            }));
        }
        let json_path = |column: &str, owner: &str| {
            json!({
                "name": format!("path[{column}]"), "in": "query",
                "description": format!("A JSON path (`$.address.city`): `{owner}` returns the first item it selects from the `json` column instead of the whole value"),
                "schema": { "type": "string", "pattern": "^\\$" }
            })
        };
        for col in table
            .visible_columns()
            .filter(|c| matches!(c.pg_type, PgType::Json | PgType::Jsonb))
        {
            params.push(json_path(&col.field_name, &col.field_name));
        }
        let embeddables = Self::embeddables(table, catalog);
        for e in &embeddables {
            let name = &e.name;
            let columns = Self::visible_fields(e.target, catalog);
            for col in e
                .target
                .visible_columns()
                .filter(|c| matches!(c.pg_type, PgType::Json | PgType::Jsonb))
            {
                params.push(json_path(
                    &format!("{name}.{}", col.field_name),
                    &col.field_name,
                ));
            }
            let columns = if e.kind == crate::server::rest::EmbedKind::Arc {
                // The union of the columns of the tables an arc leads to
                let mut all: Vec<String> = Vec::new();
                for (_, t) in &e.arc_targets {
                    for c in t.visible_columns() {
                        if !all.contains(&c.field_name) {
                            all.push(c.field_name.clone());
                        }
                    }
                }
                all
            } else {
                columns
            };
            params.push(json!({
                "name": format!("fields[{name}]"), "in": "query", "style": "form", "explode": false,
                "description": format!("Columns of the embedded `{name}` (default: all); `include={name}` first"),
                "schema": { "type": "array", "items": { "type": "string", "enum": columns } }
            }));
            // Temporal tables are read at a point in time
            if e.kind != crate::server::rest::EmbedKind::Arc && e.target.period_key().is_some() {
                params.push(json!({
                    "name": format!("asOf[{name}]"), "in": "query",
                    "description": format!("Read the embedded `{name}` as it was at this time: the versions whose period contains it"),
                    "schema": { "type": "string", "format": "date-time" }
                }));
            }
            if !e.many {
                continue;
            }
            if e.kind == crate::server::rest::EmbedKind::Tree {
                params.push(json!({
                    "name": format!("depth[{name}]"), "in": "query",
                    "description": format!("How many levels of the hierarchy `{name}` walks (default: all, cycles end it)"),
                    "schema": { "type": "integer", "minimum": 1 }
                }));
            }
            params.push(json!({
                "name": format!("filter[{name}]"), "in": "query", "style": "deepObject", "explode": true,
                "description": format!("Keeps only the embedded `{name}` rows that match, as `filter` does for a list: `filter[{name}][column][operator]=value`. The parents stay; `inner={name}` keeps those with a match"),
                "schema": schema_ref(&format!("{}Filter", e.target.name))
            }));
            params.push(json!({
                "name": format!("order[{name}]"), "in": "query", "style": "form", "explode": false,
                "description": format!("Order of the embedded `{name}` rows: `column`, `-column` or `column:desc`"),
                "schema": { "type": "array", "items": { "type": "string", "pattern": format!("^[+-]?{}(:(asc|desc))?$", alternation(&columns)) } }
            }));
            // A hierarchy is walked to its depth, not paged
            if e.kind != crate::server::rest::EmbedKind::Tree {
                params.push(json!({
                    "name": format!("first[{name}]"), "in": "query",
                    "description": format!("Number of embedded `{name}` rows (at most `limits.page_size.max`)"),
                    "schema": Self::page_schema(limits)
                }));
                params.push(json!({
                    "name": format!("offset[{name}]"), "in": "query",
                    "description": format!("Embedded `{name}` rows to skip"),
                    "schema": { "type": "integer", "minimum": 0 }
                }));
            }
        }
        // Computed fields (functions of a row) take arguments: `args[field][argument]`
        let mut owners: Vec<(String, &TableMetadata)> = vec![(String::new(), table)];
        owners.extend(
            embeddables
                .iter()
                .map(|e| (format!("{}.", e.name), e.target)),
        );
        for (prefix, owner) in owners {
            for field in catalog.object_fields(&owner.schema, &owner.name) {
                let FieldKind::Function(i) = &field.kind else {
                    continue;
                };
                let f = &catalog.functions[*i];
                if !catalog.function_is_exposed(f) || f.volatile {
                    continue;
                }
                for arg in crate::server::rest::field_arguments(f) {
                    params.push(json!({
                        "name": format!("args[{prefix}{}][{}]", field.name, arg.graphql_name()),
                        "in": "query",
                        "required": !arg.has_default,
                        "description": format!(
                            "Argument `{}` of the computed field `{}` of `{}` (select it with `fields={}`{})",
                            arg.graphql_name(), field.name, owner.name, field.name,
                            if prefix.is_empty() { String::new() } else { format!(" or `include={}`", prefix.trim_end_matches('.')) }
                        ),
                        "schema": Self::pg_type_to_json_schema(&arg.pg_type, catalog)
                    }));
                }
            }
        }
        let relations: Vec<String> = embeddables
            .iter()
            .filter(|e| e.relation)
            .map(|e| e.name.clone())
            .collect();
        if collection && !relations.is_empty() {
            for (name, text) in [
                ("inner", "Keeps the rows that have a row through each of these relations (matching its `filter[relation]`), like an inner join"),
                ("anti", "Keeps the rows that have no row through any of these relations (matching its `filter[relation]`)"),
            ] {
                params.push(json!({
                    "name": name, "in": "query", "style": "form", "explode": false,
                    "description": format!("{text}. Relations: {}", relations.join(", ")),
                    "schema": { "type": "array", "items": { "type": "string", "pattern": format!("^{}(\\.[A-Za-z_][A-Za-z0-9_]*)*$", alternation(&relations)) } }
                }));
            }
        }
        params
    }

    /// A page size: at least 1, at most `limits.page_size.max`.
    fn page_schema(limits: &LimitsConfig) -> Value {
        let mut schema = json!({ "type": "integer", "minimum": 1 });
        if let Some(max) = limits.page_size.max {
            schema["maximum"] = json!(max);
        }
        schema
    }

    /// `include`, or null when the table has no relation to embed.
    pub(super) fn include_param(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let relations = Self::relation_names(table, catalog);
        if relations.is_empty() {
            return Value::Null;
        }
        json!({
            "name": "include", "in": "query", "style": "form", "explode": false,
            "description": format!(
                "Relations to embed, with nested ones as dotted paths (`posts.author`), as deep as the depth and cost limits allow. Relations: {}",
                relations.join(", ")
            ),
            "schema": {
                "type": "array",
                "items": { "type": "string", "pattern": format!("^{}(\\.[A-Za-z_][A-Za-z0-9_]*)*$", alternation(&relations)) }
            }
        })
    }

    /// Typed query parameters of the collection `GET`.
    pub(super) fn generate_list_parameters(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        limits: &LimitsConfig,
    ) -> Value {
        let page = |what: &str| {
            let mut schema = json!({ "type": "integer", "minimum": 1 });
            let mut text = format!("{what} (default {}", limits.page_size.default);
            if let Some(max) = limits.page_size.max {
                schema["maximum"] = json!(max);
                text.push_str(&format!(", at most {max}"));
            }
            text.push(')');
            (text, schema)
        };
        let (first_text, first_schema) = page("Page size for forward keyset pagination");
        let (last_text, last_schema) = page("Page size for backward keyset pagination");
        let columns = Self::visible_fields(table, catalog);
        let mut orderable = columns.clone();
        orderable.extend(Self::order_names(table, catalog));
        let order_item = format!(
            "^[+-]?{}(:(asc|desc))?(:(nullsfirst|nullslast))?$",
            alternation(&orderable)
        );
        let mut params = vec![
            json!({
                "name": "filter", "in": "query", "style": "deepObject", "explode": true,
                "description": "Filter criteria: `filter[column][operator]=value`, through relations with `filter[relation][column]=value`. Lists take comma-separated values (`filter[id][in]=1,2`). Groups: `filter[or][0][age][lt]=18&filter[or][1][age][gt]=65`, and `filter[not][status][eq]=draft`. Unknown columns and operators are a 400. A body filter (`and` / `or` / `not`, any depth) is `POST …/search`.",
                "schema": schema_ref(&format!("{}Filter", table.name))
            }),
            json!({
                "name": "order", "in": "query", "style": "form", "explode": false,
                "description": "What to order by: `column`, `-column` or `column:desc`, and where NULLs go with `:nullsfirst` / `:nullslast` (by default last ascending, first descending). Besides the columns: a column of the row a to-one relation leads to (`author.username`), the number of rows of a to-many relation (`messagesCount`) and a computed column that returns an orderable value (`uniqueAuthorCount`); `natural` and `primary_key`",
                "schema": { "type": "array", "items": { "type": "string", "pattern": order_item } }
            }),
            json!({
                "name": "first", "in": "query",
                "description": first_text,
                "schema": first_schema
            }),
            json!({
                "name": "after", "in": "query",
                "description": "Cursor to continue after (from the `next` link)",
                "schema": { "type": "string" }
            }),
            json!({
                "name": "last", "in": "query",
                "description": last_text,
                "schema": last_schema
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
            Self::fields_param(table, catalog),
            Self::include_param(table, catalog),
            prefer_param(false),
        ];
        params.extend(Self::embed_parameters(table, catalog, limits, true));
        if table.operations.connection {
            params.extend(Self::edge_parameters(table));
        }
        params.retain(|p| !p.is_null());
        Value::Array(params)
    }

    /// The body of `POST …/search`.
    fn generate_search_request_schema(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        limits: &LimitsConfig,
    ) -> Value {
        json!({
            "type": "object",
            "description": format!("Search request for `{}`; every member is optional, unknown members are a 400", table.name),
            "properties": Self::search_request_properties(table, catalog, limits),
            "additionalProperties": false
        })
    }

    /// The members of a list request in a JSON body (a search, or a function's options).
    pub(super) fn search_request_properties(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        limits: &LimitsConfig,
    ) -> Value {
        let columns = Self::visible_fields(table, catalog);
        let relations = Self::relation_names(table, catalog);
        let mut properties = json!({
            "filter": schema_ref(&format!("{}Filter", table.name)),
            "order": {
                "description": "Columns to order by: `column`, `-column` or `column:desc`, with `:nullsfirst` / `:nullslast`",
                "oneOf": [
                    { "type": "string" },
                    { "type": "array", "items": { "type": "string" } }
                ]
            },
            "first": Self::page_schema(limits),
            "last": Self::page_schema(limits),
            "after": { "type": "string", "description": "Cursor (the `X-Next-Cursor` header of the previous page)" },
            "before": { "type": "string", "description": "Cursor (the `X-Previous-Cursor` header)" },
            "offset": { "type": "integer", "minimum": 0 },
            "fields": { "type": "array", "items": { "type": "string", "enum": columns } }
        });
        if !relations.is_empty() {
            properties["include"] = json!({
                "type": "array",
                "items": { "type": "string", "pattern": format!("^{}(\\.[A-Za-z_][A-Za-z0-9_]*)*$", alternation(&relations)) }
            });
            let path = format!("^{}(\\.[A-Za-z_][A-Za-z0-9_]*)*$", alternation(&relations));
            properties["embed"] = json!({
                "type": "object",
                "description": "Controls of the embedded relations, by `include` path: the columns, the filter, order and page of its rows",
                "propertyNames": { "pattern": path },
                "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "fields": { "type": "array", "items": { "type": "string" } },
                        "filter": { "type": "object" },
                        "order": {
                            "oneOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" } }]
                        },
                        "first": Self::page_schema(limits),
                        "offset": { "type": "integer", "minimum": 0 }
                    },
                    "additionalProperties": false
                }
            });
            for (name, what) in [
                (
                    "inner",
                    "Relations the rows must have a (matching) row through",
                ),
                (
                    "anti",
                    "Relations the rows must have no (matching) row through",
                ),
            ] {
                properties[name] = json!({
                    "type": "array",
                    "description": what,
                    "items": { "type": "string", "pattern": path }
                });
            }
        }
        properties
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

    /// The create and update body schemas of `table`: the GraphQL input types (with their
    /// nested writes) when the served schema is known, else the table's own columns.
    fn write_input_schemas(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        inputs: Option<&mut super::input_types::InputTypes<'_>>,
    ) -> (Value, Value) {
        let flat = (
            Self::generate_create_input_schema(table, catalog),
            Self::generate_update_input_schema(table, catalog),
        );
        let Some(inputs) = inputs else {
            return flat;
        };
        let fields = catalog.table_mutation_fields(table);
        let input_of = |inputs: &super::input_types::InputTypes<'_>, action, by_key: bool| {
            fields
                .iter()
                .find(|m| {
                    m.action == action && !m.by_node_id && (!by_key || m.unique_key.is_empty())
                })
                .and_then(|m| inputs.argument_type(&m.name, "input"))
        };
        let create = input_of(inputs, crate::catalog::MutationAction::Create, false);
        let update = input_of(inputs, crate::catalog::MutationAction::Update, true);
        (
            create.map_or(flat.0, |name| inputs.reference(&name)),
            update.map_or(flat.1, |name| inputs.reference(&name)),
        )
    }

    /// Input schema for row creation.
    fn generate_create_input_schema(table: &TableMetadata, catalog: &DatabaseCatalog) -> Value {
        let mut properties = BTreeMap::new();
        let mut required = Vec::new();
        for col in Self::writable(table) {
            properties.insert(
                col.field_name.clone(),
                Self::constrained_schema(table, col, catalog),
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
                    Self::constrained_schema(table, c, catalog),
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
        // Subtype columns: some subtype row matching is a match
        let subtype_columns = catalog.cti_columns(table);
        for col in table
            .visible_columns()
            .chain(subtype_columns.iter().map(|(_, members)| members[0].1))
        {
            let mut ty = Self::pg_type_to_json_schema(&col.pg_type, catalog);
            // Exact numbers are strings in responses, but a filter may be given either way
            if matches!(col.pg_type, PgType::Int8 | PgType::Numeric | PgType::Money) {
                ty = json!({ "type": ["number", "string"] });
            }
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
            ops.insert("isDistinctFrom".to_string(), ty.clone());
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
            let text_like = matches!(
                col.pg_type,
                PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext
            );
            if ordered || text_like {
                for op in [
                    "lessThan",
                    "lessThanOrEqualTo",
                    "greaterThan",
                    "greaterThanOrEqualTo",
                ] {
                    for quantifier in ["Any", "All"] {
                        ops.insert(
                            format!("{op}{quantifier}"),
                            json!({ "type": "array", "items": ty.clone() }),
                        );
                    }
                }
            }
            if let PgType::Array(_) = &col.pg_type {
                for op in ["contains", "containedBy", "overlaps"] {
                    ops.insert(op.to_string(), ty.clone());
                }
            }
            if col.pg_type.is_range() {
                for op in [
                    "containedBy",
                    "overlaps",
                    "strictlyLeftOf",
                    "strictlyRightOf",
                    "adjacentTo",
                ] {
                    ops.insert(op.to_string(), ty.clone());
                }
                if let Some((elem, _)) = col.pg_type.range_element() {
                    let elem_schema = Self::pg_type_to_json_schema(&elem, catalog);
                    ops.insert("contains".to_string(), elem_schema);
                }
            }
            if matches!(col.pg_type, PgType::Json | PgType::Jsonb) {
                for op in ["contains", "containedBy"] {
                    ops.insert(op.to_string(), json!({}));
                }
                for op in ["hasKey", "matchesPath", "existsPath"] {
                    ops.insert(op.to_string(), json!({ "type": "string" }));
                }
                for op in ["hasAnyKeys", "hasAllKeys"] {
                    ops.insert(
                        op.to_string(),
                        json!({ "type": "array", "items": { "type": "string" } }),
                    );
                }
            }
            // Spatial conditions on a geometry (`GeoJSONFilter`)
            if matches!(col.pg_type, PgType::Geometry | PgType::Geography) {
                let geometry = json!({ "type": "object", "description": "A GeoJSON geometry" });
                for op in ["intersects", "within", "contains"] {
                    ops.insert(op.to_string(), geometry.clone());
                }
                ops.insert(
                    "withinDistance".to_string(),
                    json!({
                        "type": "object",
                        "description": "Within a distance of a geometry (meters for geographies)",
                        "properties": { "geometry": geometry, "distance": { "type": "number" } },
                        "required": ["geometry", "distance"]
                    }),
                );
                ops.insert(
                    "intersectsBox".to_string(),
                    json!({
                        "type": "object",
                        "description": "Its bounding box intersects this box (WGS 84, srid 4326, by default)",
                        "properties": {
                            "xmin": { "type": "number" }, "ymin": { "type": "number" },
                            "xmax": { "type": "number" }, "ymax": { "type": "number" },
                            "srid": { "type": "integer" }
                        },
                        "required": ["xmin", "ymin", "xmax", "ymax"]
                    }),
                );
            }
            // `ltree` paths (`LTreeFilter`)
            if col.pg_type == PgType::Ltree {
                for (op, text) in [
                    ("ancestorOf", "Is this path or above it"),
                    ("descendantOf", "Is this path or below it"),
                    ("matches", "Matches an `lquery` pattern, e.g. `*.science.*`"),
                    (
                        "matchesText",
                        "Matches an `ltxtquery` of labels, e.g. `science & !biology`",
                    ),
                ] {
                    ops.insert(
                        op.to_string(),
                        json!({ "type": "string", "description": text }),
                    );
                }
            }
            // Full-text documents (`FullTextFilter`)
            if col.pg_type == PgType::TsVector {
                ops.insert(
                    "matches".to_string(),
                    json!({ "type": "string", "description": "A web-search-style query: words, \"quoted phrases\", `or`, `-excluded`" }),
                );
            }
            // Multiranges (`<Scalar>MultirangeFilter`): a set of ranges
            if let PgType::Multirange(range) = &col.pg_type {
                let range_schema = Self::pg_type_to_json_schema(range, catalog);
                ops.insert("containsRange".to_string(), range_schema.clone());
                ops.insert("overlaps".to_string(), range_schema.clone());
                ops.insert("containedBy".to_string(), range_schema);
                ops.insert("isEmpty".to_string(), json!({ "type": "boolean" }));
                if let Some((elem, _)) = range.range_element() {
                    ops.insert(
                        "contains".to_string(),
                        Self::pg_type_to_json_schema(&elem, catalog),
                    );
                }
            }
            if matches!(
                col.pg_type,
                PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext
            ) {
                for op in ["like", "ilike", "startsWith", "endsWith", "regex", "iregex"] {
                    ops.insert(op.to_string(), json!({ "type": "string" }));
                }
                for op in ["likeAny", "likeAll", "ilikeAny", "ilikeAll"] {
                    ops.insert(
                        op.to_string(),
                        json!({ "type": "array", "items": { "type": "string" } }),
                    );
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
                Self::constrained_schema(table, col, catalog),
            );
            if !col.is_nullable {
                required.push(col.field_name.clone());
            }
        }

        // The relations `include` embeds: present only when asked for
        let mut properties = properties;
        for e in Self::embeddables(table, catalog) {
            let target = if e.kind == crate::server::rest::EmbedKind::Arc {
                // One row of one of several tables, told apart by `__typename`
                json!({ "oneOf": e.arc_targets.iter().map(|(_, t)| schema_ref(&t.name)).collect::<Vec<_>>() })
            } else {
                schema_ref(&e.target.name)
            };
            let (schema, text) = if e.many {
                (json!({ "type": "array", "items": target }), "rows")
            } else {
                (json!({ "anyOf": [target, { "type": "null" }] }), "row")
            };
            let mut schema = schema;
            schema["description"] = json!(format!(
                "The related `{}` {text}; present when embedded with `include={}`",
                e.target.name, e.name
            ));
            properties.entry(e.name).or_insert(schema);
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

    /// The row schemas of a polymorphic table:
    /// - For Single Table Inheritance (STI): `<table>.Base` (the plain object), one `<table>.<Variant>`
    ///   per discriminator value, and `<table>` as the `oneOf` with a `discriminator` on the column.
    /// - For table-per-type (CTI): `<table>.Base` (parent columns), one `<table>.<Child>`
    ///   per subtype with child columns, `<table>.<Fallback>` for parent rows without a child,
    ///   and `<table>` as the `oneOf` with a `discriminator` on `__typename`.
    ///
    /// Returns `None` for any non-polymorphic table.
    fn polymorphic_row_schemas(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        base: &Value,
    ) -> Option<Vec<(String, Value)>> {
        if let Some(cti) = catalog.find_cti(&table.schema, &table.name) {
            let name = &table.name;
            let base_name = format!("{name}.Base");
            let mut out = vec![(base_name.clone(), base.clone())];
            let mut members = Vec::new();
            let mut mapping = serde_json::Map::new();

            for child in &cti.children {
                let variant_name = format!("{name}.{}", child.type_name);
                let mut props = serde_json::Map::new();
                props.insert(
                    "__typename".to_string(),
                    json!({ "const": child.type_name }),
                );
                let mut required = vec!["__typename".to_string()];

                if let Some(child_table) = catalog.find_table(&child.schema, &child.table) {
                    for col in child_table.visible_columns() {
                        if !table
                            .visible_columns()
                            .any(|pc| pc.name == col.name || pc.field_name == col.field_name)
                        {
                            props.insert(
                                col.field_name.clone(),
                                Self::constrained_schema(child_table, col, catalog),
                            );
                            if !col.is_nullable {
                                required.push(col.field_name.clone());
                            }
                        }
                    }
                }

                out.push((
                    variant_name.clone(),
                    json!({
                        "description": format!("A `{name}` row of subtype `{}`", child.type_name),
                        "allOf": [
                            schema_ref(&base_name),
                            {
                                "properties": props,
                                "required": required
                            }
                        ]
                    }),
                ));
                mapping.insert(
                    child.type_name.clone(),
                    json!(format!("#/components/schemas/{variant_name}")),
                );
                members.push(schema_ref(&variant_name));
            }

            let fallback_type_name = if !cti.fallback_type_name.is_empty() {
                &cti.fallback_type_name
            } else {
                "Fallback"
            };
            let fallback_name = format!("{name}.{fallback_type_name}");
            out.push((
                fallback_name.clone(),
                json!({
                    "description": format!("A `{name}` row whose subtype is none of the above"),
                    "allOf": [
                        schema_ref(&base_name),
                        {
                            "properties": {
                                "__typename": { "const": fallback_type_name }
                            },
                            "required": ["__typename"]
                        }
                    ]
                }),
            ));
            mapping.insert(
                fallback_type_name.to_string(),
                json!(format!("#/components/schemas/{fallback_name}")),
            );
            members.push(schema_ref(&fallback_name));

            let mut row = json!({
                "oneOf": members,
                "discriminator": { "propertyName": "__typename", "mapping": mapping }
            });
            if let Some(description) = &table.description {
                row["description"] = json!(description);
            }
            out.push((name.clone(), row));
            return Some(out);
        }

        let sti = catalog.find_sti(&table.schema, &table.name)?;
        let column = table
            .visible_columns()
            .find(|c| c.name == sti.discriminator_column)?;
        let field = &column.field_name;
        // Rows carry the value as the API names it (an enum value), not as the database does
        let enum_meta = column
            .enum_key
            .as_ref()
            .and_then(|key| catalog.enums.iter().find(|e| &e.key == key));
        let wire = |value: &str| {
            enum_meta
                .and_then(|e| e.values.iter().find(|v| v.label == value))
                .map_or_else(|| value.to_string(), |v| v.name.clone())
        };
        let name = &table.name;
        let base_name = format!("{name}.Base");
        let mut out = vec![(base_name.clone(), base.clone())];
        let mut members = Vec::new();
        let mut mapping = serde_json::Map::new();
        for variant in &sti.variants {
            let variant_name = format!("{name}.{}", variant.type_name);
            out.push((
                variant_name.clone(),
                json!({
                    "description": format!("A `{name}` row whose `{field}` is `{}`", wire(&variant.discriminator_value)),
                    "allOf": [
                        schema_ref(&base_name),
                        {
                            "properties": { field: { "const": wire(&variant.discriminator_value) } },
                            "required": [field]
                        }
                    ]
                }),
            ));
            mapping.insert(
                wire(&variant.discriminator_value),
                json!(format!("#/components/schemas/{variant_name}")),
            );
            members.push(schema_ref(&variant_name));
        }
        if let Some(fallback) = &sti.fallback_type_name {
            let fallback_name = format!("{name}.{fallback}");
            out.push((
                fallback_name.clone(),
                json!({
                    "description": format!("A `{name}` row whose `{field}` is none of the above"),
                    "allOf": [
                        schema_ref(&base_name),
                        { "properties": { field: { "type": "null" } } }
                    ]
                }),
            ));
            members.push(schema_ref(&fallback_name));
        }
        let mut row = json!({
            "oneOf": members,
            "discriminator": { "propertyName": field, "mapping": mapping }
        });
        if let Some(description) = &table.description {
            row["description"] = json!(description);
        }
        out.push((name.clone(), row));
        Some(out)
    }

    /// The parameters of a list, and the `Range` header.
    fn list_parameters_with_range(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        limits: &LimitsConfig,
    ) -> Value {
        let mut params = Self::generate_list_parameters(table, catalog, limits);
        if let Some(params) = params.as_array_mut() {
            params.push(json!({
                "name": "Range", "in": "header",
                "description": "A page as rows `A` to `B` (`items=0-24`, `items=25-`): answered with `206` and `Content-Range`, or `416` past the end. It does not combine with `first`, `last`, `after`, `before` or `offset`. Another unit or a suffix range is ignored.",
                "schema": { "type": "string", "pattern": "^items=\\d+-\\d*$" }
            }));
        }
        params
    }

    /// The schema of a column with the keywords its `CHECK` constraints state.
    fn constrained_schema(
        table: &TableMetadata,
        col: &ColumnMetadata,
        catalog: &DatabaseCatalog,
    ) -> Value {
        let mut schema = Self::column_to_json_schema(col, catalog);
        // pg_jsonschema: a JSON Schema check is the column's type
        if matches!(col.pg_type, PgType::Json | PgType::Jsonb)
            && catalog
                .integration(crate::catalog::integrations::Integration::PgJsonschema)
                .is_some()
        {
            if let Some(mut stated) = super::constraints::json_schema(table, col) {
                if let Some(object) = stated.as_object_mut() {
                    object.remove("$schema");
                }
                let mut typed = if col.is_nullable {
                    // NULL passes the check
                    json!({ "anyOf": [stated, { "type": "null" }] })
                } else {
                    stated
                };
                for keyword in ["description", "deprecated"] {
                    if let Some(value) = schema.get(keyword) {
                        typed[keyword] = value.clone();
                    }
                }
                return typed;
            }
        }
        for (keyword, value) in super::constraints::keywords(table, col) {
            // A `varchar(20)` limit and a `CHECK` on the length: the stricter one holds
            let tighter = match (schema.get(&keyword).and_then(Value::as_i64), value.as_i64()) {
                (Some(old), Some(new)) if keyword == "maxLength" || keyword == "maxItems" => {
                    new < old
                }
                (Some(old), Some(new)) if keyword == "minLength" || keyword == "minItems" => {
                    new > old
                }
                (Some(_), _) => false,
                _ => true,
            };
            if tighter {
                schema[&keyword] = value;
            }
        }
        schema
    }

    pub(super) fn column_to_json_schema(
        col: &ColumnMetadata,
        catalog: &DatabaseCatalog,
    ) -> serde_json::Value {
        let mut schema = if let Some(enum_key) = &col.enum_key {
            if let Some(enum_meta) = catalog.enums.iter().find(|e| &e.key == enum_key) {
                let vals: Vec<&str> = enum_meta.values.iter().map(|v| v.name.as_str()).collect();
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
    pub(super) fn pg_type_to_json_schema(
        pg_type: &PgType,
        catalog: &DatabaseCatalog,
    ) -> serde_json::Value {
        match pg_type {
            PgType::Int2 | PgType::Int4 => json!({ "type": "integer", "format": "int32" }),
            // 64-bit integers and decimals are strings in responses (JSON numbers are doubles)
            PgType::Int8 => json!({
                "type": "string",
                "pattern": "^-?[0-9]+$",
                "description": "A 64-bit integer, as a string"
            }),
            PgType::Float4 => json!({ "type": "number", "format": "float" }),
            PgType::Float8 => json!({ "type": "number" }),
            PgType::Numeric | PgType::Money => json!({
                "type": "string",
                "pattern": "^-?[0-9]+(\\.[0-9]+)?$",
                "description": "A decimal number, as a string (exact)"
            }),
            PgType::Bool => json!({ "type": "boolean" }),
            PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext => {
                json!({ "type": "string" })
            }
            PgType::Uuid => json!({ "type": "string", "format": "uuid" }),
            PgType::Date => json!({ "type": "string", "format": "date" }),
            PgType::TimeTz => json!({ "type": "string", "format": "time" }),
            PgType::Time => {
                json!({ "type": "string", "description": "A time of day without a time zone: `10:11:12`" })
            }
            PgType::TimestampTz => json!({ "type": "string", "format": "date-time" }),
            PgType::Timestamp => json!({
                "type": "string",
                "description": "A date and time without a time zone: `2026-01-02T03:04:05`"
            }),
            PgType::Interval => json!({ "type": "string", "format": "duration" }),
            PgType::Bytea => json!({ "type": "string", "contentEncoding": "base64" }),
            // Any JSON value: an object, an array, a string, ...
            PgType::Json | PgType::Jsonb => json!({}),
            PgType::Multirange(range) => json!({
                "type": "array",
                "items": Self::pg_type_to_json_schema(range, catalog)
            }),
            PgType::TstzRange
            | PgType::NumRange
            | PgType::DateRange
            | PgType::Int4Range
            | PgType::Int8Range => {
                let bound_type = match pg_type {
                    PgType::TstzRange => {
                        Self::pg_type_to_json_schema(&PgType::TimestampTz, catalog)
                    }
                    PgType::NumRange => Self::pg_type_to_json_schema(&PgType::Numeric, catalog),
                    PgType::DateRange => Self::pg_type_to_json_schema(&PgType::Date, catalog),
                    PgType::Int8Range => Self::pg_type_to_json_schema(&PgType::Int8, catalog),
                    _ => Self::pg_type_to_json_schema(&PgType::Int4, catalog),
                };
                let bound = json!({
                    "type": ["object", "null"],
                    "description": "A bound; null when the range is unbounded on that side",
                    "properties": { "value": bound_type, "inclusive": { "type": "boolean" } },
                    "required": ["value", "inclusive"]
                });
                json!({
                    "type": "object",
                    "description": "A range: `start` is inclusive and `end` exclusive unless `inclusive` says otherwise",
                    "properties": { "start": bound.clone(), "end": bound, "isEmpty": { "type": "boolean" } }
                })
            }
            PgType::Inet | PgType::Cidr => json!({ "type": "string" }),
            PgType::MacAddr | PgType::MacAddr8 => json!({ "type": "string" }),
            PgType::Vector => json!({ "type": "array", "items": { "type": "number" } }),
            PgType::Geometry | PgType::Geography => json!({
                "type": "object",
                "description": "A GeoJSON geometry"
            }),
            PgType::Array(inner) => {
                let inner_schema = Self::pg_type_to_json_schema(inner, catalog);
                json!({ "type": "array", "items": inner_schema })
            }
            PgType::Composite(key) => match catalog.find_composite(key) {
                Some(composite) => {
                    let properties: BTreeMap<String, Value> = composite
                        .attributes
                        .iter()
                        .map(|a| {
                            (
                                a.field_name.clone(),
                                Self::pg_type_to_json_schema(&a.pg_type, catalog),
                            )
                        })
                        .collect();
                    json!({ "type": "object", "properties": properties })
                }
                None => json!({ "type": "object" }),
            },
            PgType::Enum(name) => {
                if let Some(enum_meta) = catalog.enums.iter().find(|e| {
                    &e.config_name == name
                        || &e.key == name
                        || &e.type_name == name
                        || name.ends_with(&e.config_name)
                }) {
                    let vals: Vec<&str> =
                        enum_meta.values.iter().map(|v| v.name.as_str()).collect();
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
    fn event_triggers_and_schedules_are_documented_as_webhooks() {
        let catalog = users_catalog();
        let config = Config::from_yaml(
            "events:\n  triggers:\n    user_changed:\n      table: users\n      operations: [insert, update]\n      url: https://hooks.test/u\n  schedules:\n    nightly:\n      cron: '0 3 * * *'\n      url: https://hooks.test/n\n",
        )
        .unwrap();
        let doc = OpenApiGenerator::generate(&catalog, &config);
        let body = &doc["webhooks"]["user_changed"]["post"]["requestBody"]["content"]
            ["application/json"]["schema"];
        assert_eq!(
            body["oneOf"],
            json!([
                { "$ref": "#/components/schemas/usersInsertEvent" },
                { "$ref": "#/components/schemas/usersUpdateEvent" }
            ])
        );
        assert!(doc["components"]["schemas"]["usersUpdateEvent"]["properties"]["old"].is_object());
        assert_eq!(
            doc["webhooks"]["nightly"]["post"]["requestBody"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/ScheduleEvent"
        );
        // Off, nothing is documented
        let mut off = config.clone();
        off.events.enabled = false;
        assert!(OpenApiGenerator::generate(&catalog, &off)
            .get("webhooks")
            .is_none());
    }

    #[test]
    fn rate_limits_are_documented_only_when_on() {
        let catalog = users_catalog();
        let off = OpenApiGenerator::generate(&catalog, &Config::default());
        assert!(off["components"].get("headers").is_none());
        assert!(off.to_string().find("\"429\"").is_none());

        let mut config = Config::default();
        config.limits.rate_limit.enabled = true;
        let on = OpenApiGenerator::generate(&catalog, &config);
        let list = &on["paths"]["/rest/users"]["get"]["responses"];
        assert_eq!(list["429"]["$ref"], "#/components/responses/RateLimited");
        assert_eq!(
            list["200"]["headers"]["RateLimit-Remaining"]["$ref"],
            "#/components/headers/RateLimit-Remaining"
        );
        assert!(on["components"]["responses"]["RateLimited"]["headers"]["Retry-After"].is_object());
    }

    /// One `public.users` table: `id` (primary key) and `email`.
    fn users_catalog() -> DatabaseCatalog {
        let mut catalog = DatabaseCatalog::default();
        catalog.tables.push(TableMetadata {
            timescale: None,
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
        catalog
    }

    #[test]
    fn test_generate_minimal_openapi_document() {
        let catalog = users_catalog();
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

    #[test]
    fn the_filter_lists_the_operators_of_spatial_path_text_and_multirange_columns() {
        let column = |name: &str, pg_type: PgType| ColumnMetadata {
            name: name.to_string(),
            pg_type,
            is_nullable: true,
            is_primary_key: false,
            default_value: None,
            is_generated: false,
            description: None,
            limits: Default::default(),
            generation: None,
            deprecation: None,
            field_name: name.to_string(),
            omit: false,
            readonly: false,
            enum_key: None,
            domain: None,
        };
        let table = TableMetadata {
            timescale: None,
            schema: "public".to_string(),
            name: "places".to_string(),
            kind: RelationKind::Table,
            columns: vec![
                column("area", PgType::Geometry),
                column("path", PgType::Ltree),
                column("doc", PgType::TsVector),
                column("spans", PgType::Multirange(Box::new(PgType::Int4Range))),
            ],
            primary_key: vec![],
            foreign_keys: vec![],
            description: None,
            type_name: "Place".to_string(),
            list_field: "places".to_string(),
            single_field: "place".to_string(),
            connection_field: "placesConnection".to_string(),
            all_connection_field: None,
            operations: Default::default(),
            unique_keys: vec![],
            constraints: vec![],
            version_column: None,
            deprecation: None,
            federation_key: None,
            federation_shareable: false,
        };
        let mut catalog = DatabaseCatalog::default();
        catalog.tables.push(table.clone());
        let filter = OpenApiGenerator::generate_filter_schema(&table, &catalog, &[]);
        let ops = |column: &str| -> Vec<String> {
            let mut names: Vec<String> = filter["properties"][column]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            names.sort();
            names
        };
        for op in [
            "intersects",
            "within",
            "contains",
            "withinDistance",
            "intersectsBox",
        ] {
            assert!(ops("area").contains(&op.to_string()), "area: {op}");
        }
        for op in ["ancestorOf", "descendantOf", "matches", "matchesText"] {
            assert!(ops("path").contains(&op.to_string()), "path: {op}");
        }
        assert!(ops("doc").contains(&"matches".to_string()));
        for op in [
            "containsRange",
            "overlaps",
            "containedBy",
            "isEmpty",
            "contains",
        ] {
            assert!(ops("spans").contains(&op.to_string()), "spans: {op}");
        }
    }
}
