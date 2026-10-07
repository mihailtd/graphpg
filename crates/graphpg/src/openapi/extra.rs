//! OpenAPI descriptions of what goes beyond a table's own routes: aggregates, the search
//! methods, global search and functions. Like the rest of the document they are built from the
//! same rules the handlers in [`crate::server::rest`] apply (permissions, names, limits).

use super::generator::{prefer_param, problem, responses, schema_ref, OpenApiGenerator};
use crate::catalog::{
    ColumnMetadata, DatabaseCatalog, FunctionArgument, FunctionMetadata, FunctionReturn, PgType,
    TableMetadata,
};
use crate::config::models::Operation;
use crate::config::Config;
use crate::server::rest::{
    check_table_permissions, method_segment, rpc_functions, GLOBAL_SEARCH_RESOURCE, MAX_GROUP_KEYS,
    REST_PREFIX, RPC_RESOURCE,
};
use apollo_compiler::ast::Type;
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::Schema;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Aggregate functions over a list of columns, and which columns each accepts.
const AGGREGATE_FUNCTIONS: [&str; 10] = [
    "min",
    "max",
    "sum",
    "avg",
    "distinctCount",
    "stddevSample",
    "stddevPopulation",
    "varianceSample",
    "variancePopulation",
    "median",
];

fn aggregate_columns(table: &TableMetadata, function: &str) -> Vec<String> {
    table
        .visible_columns()
        .filter(|c| match function {
            "min" | "max" => c.pg_type.is_orderable(),
            "distinctCount" => true,
            _ => c.pg_type.is_numeric(),
        })
        .map(|c| c.field_name.clone())
        .collect()
}

/// What `group` accepts: `column`, `column:unit`, `column:binned`.
fn group_values(table: &TableMetadata) -> Vec<String> {
    table
        .group_by_values()
        .into_iter()
        .map(|(_, column, unit)| match unit {
            None => column.field_name.clone(),
            Some("bin") => format!("{}:binned", column.field_name),
            Some(unit) => format!("{}:{unit}", column.field_name),
        })
        .collect()
}

fn column_list(columns: &[String], description: &str) -> Value {
    json!({
        "type": "array",
        "description": description,
        "items": { "type": "string", "enum": columns }
    })
}

fn json_content(schema: Value) -> Value {
    json!({ "application/json": { "schema": schema } })
}

/// A GraphQL input type as JSON Schema (scalars by name, enums, input objects).
fn graphql_schema(schema: &Schema, ty: &Type, depth: usize) -> Value {
    let inner = match ty {
        Type::List(inner) | Type::NonNullList(inner) => {
            json!({ "type": "array", "items": graphql_schema(schema, inner, depth) })
        }
        Type::Named(name) | Type::NonNullNamed(name) => graphql_named(schema, name.as_str(), depth),
    };
    if ty.is_non_null() {
        inner
    } else {
        super::generator::nullable(inner)
    }
}

fn graphql_named(schema: &Schema, name: &str, depth: usize) -> Value {
    match name {
        "Int" => return json!({ "type": "integer" }),
        "Float" => return json!({ "type": "number" }),
        "String" | "ID" => return json!({ "type": "string" }),
        "Boolean" => return json!({ "type": "boolean" }),
        _ => {}
    }
    match schema.types.get(name) {
        Some(ExtendedType::Enum(e)) => {
            let values: Vec<&str> = e.values.keys().map(|k| k.as_str()).collect();
            json!({ "type": "string", "enum": values })
        }
        Some(ExtendedType::InputObject(input)) if depth < 4 => {
            let mut properties = BTreeMap::new();
            let mut required = Vec::new();
            for (field, definition) in &input.fields {
                properties.insert(
                    field.to_string(),
                    graphql_schema(schema, &definition.ty, depth + 1),
                );
                if definition.ty.is_non_null() && definition.default_value.is_none() {
                    required.push(field.to_string());
                }
            }
            let mut object = json!({ "type": "object", "properties": properties, "additionalProperties": false });
            if !required.is_empty() {
                object["required"] = json!(required);
            }
            object
        }
        Some(ExtendedType::Scalar(_)) => match name {
            "UUID" => json!({ "type": "string", "format": "uuid" }),
            "Datetime" => json!({ "type": "string", "format": "date-time" }),
            "Date" => json!({ "type": "string", "format": "date" }),
            "JSON" | "JSONObject" => json!({}),
            _ => json!({ "type": "string" }),
        },
        _ => json!({}),
    }
}

impl OpenApiGenerator {
    // -----------------------------------------------------------------------------------------
    // Aggregates
    // -----------------------------------------------------------------------------------------

    pub(super) fn generate_aggregate_paths(
        table: &TableMetadata,
        config: &Config,
        schemas: &mut Map<String, Value>,
        paths: &mut Map<String, Value>,
    ) {
        if check_table_permissions(table, Some(config), Operation::Read).is_err()
            || !table.operations.connection
        {
            return;
        }
        let name = &table.name;
        let groups = group_values(table);
        let request_properties = Self::aggregate_properties(table, &groups);
        schemas.insert(
            format!("{name}AggregateRequest"),
            json!({
                "type": "object",
                "description": format!("Aggregate request for `{name}`; every member is optional, unknown members are a 400"),
                "properties": request_properties,
                "additionalProperties": false
            }),
        );

        let function_objects: BTreeMap<&str, Value> = AGGREGATE_FUNCTIONS
            .into_iter()
            .chain(["percentile"])
            .filter(|f| !aggregate_columns(table, if *f == "percentile" { "median" } else { f }).is_empty())
            .map(|f| (f, json!({ "type": "object", "additionalProperties": true, "description": format!("`{f}` of each requested column") })))
            .collect();
        let mut totals = json!({
            "type": "object",
            "description": "Totals of the filtered rows",
            "properties": { "count": { "type": "integer", "description": "Number of rows" } },
            "additionalProperties": false
        });
        let mut group = json!({
            "type": "object",
            "description": "One group of rows sharing the `group` values",
            "properties": {
                "keys": { "type": "object", "additionalProperties": true, "description": "The group's values by `group` item" },
                "count": { "type": "integer" }
            },
            "additionalProperties": false
        });
        for (function, schema) in function_objects {
            totals["properties"][function] = schema.clone();
            group["properties"][function] = schema;
        }
        schemas.insert(format!("{name}Aggregates"), totals);
        schemas.insert(format!("{name}AggregateGroup"), group);

        let mut params = Self::aggregate_parameters(table, &groups);
        params.retain(|p| !p.is_null());
        let result = json!({
            "oneOf": [
                schema_ref(&format!("{name}Aggregates")),
                { "type": "array", "maxItems": config.limits.groups, "items": schema_ref(&format!("{name}AggregateGroup")) }
            ]
        });
        let describe = format!(
            "Summarises the rows of `{name}`: one object of totals, or with `group` one object per group ({} from at most {MAX_GROUP_KEYS} values), in one scan.",
            match config.limits.groups {
                Some(max) => format!("at most {max}"),
                None => "any number".to_string(),
            }
        );
        let responses = |op: &str| {
            responses(
                json!({ "200": {
                    "description": format!("{op}: totals, or a list of groups"),
                    "content": json_content(result.clone())
                } }),
                &[(
                    "422",
                    "Unprocessable Content: more groups than a response holds",
                )],
            )
        };
        paths.insert(
            format!("{REST_PREFIX}/{name}/aggregate"),
            json!({
                "get": {
                    "summary": format!("Aggregate {name}"),
                    "description": describe,
                    "operationId": format!("aggregate_{name}"),
                    "parameters": params,
                    "responses": responses("Aggregated")
                },
                "post": {
                    "summary": format!("Aggregate {name} (body)"),
                    "description": format!("{describe} The members are those of the query string."),
                    "operationId": format!("aggregate_{name}_post"),
                    "requestBody": {
                        "required": false,
                        "content": json_content(schema_ref(&format!("{name}AggregateRequest")))
                    },
                    "responses": responses("Aggregated")
                }
            }),
        );
    }

    /// The members of an aggregate request, typed: the JSON body's, and the query string's.
    fn aggregate_properties(table: &TableMetadata, groups: &[String]) -> Value {
        let mut properties = Map::new();
        properties.insert(
            "filter".into(),
            schema_ref(&format!("{}Filter", table.name)),
        );
        properties.insert(
            "group".into(),
            json!({
                "type": "array", "maxItems": MAX_GROUP_KEYS,
                "description": "What to group by: a column, `column:unit` (minute, hour, day, week, month, quarter, year) or `column:binned`",
                "items": { "type": "string", "enum": groups }
            }),
        );
        for function in AGGREGATE_FUNCTIONS {
            let columns = aggregate_columns(table, function);
            if !columns.is_empty() {
                properties.insert(
                    function.into(),
                    column_list(&columns, &format!("Columns to take the `{function}` of")),
                );
            }
        }
        let numeric = aggregate_columns(table, "median");
        if !numeric.is_empty() {
            properties.insert(
                "percentile".into(),
                column_list(
                    &numeric,
                    "Columns to take the percentile of (needs `fraction`)",
                ),
            );
            properties.insert(
                "fraction".into(),
                json!({ "type": "number", "minimum": 0, "maximum": 1 }),
            );
        }
        properties.insert("having".into(), Self::having_schema(&numeric));
        properties.insert("timeZone".into(), json!({ "type": "string", "description": "Time zone dates are truncated in, such as `Europe/Paris`" }));
        properties.insert("binWidth".into(), json!({ "type": "string", "description": "Width of `:binned` groups, an ISO 8601 duration such as `PT15M`" }));
        properties.insert("binOrigin".into(), json!({ "type": "string", "format": "date-time", "description": "Where `:binned` groups start" }));
        properties.insert("fillGaps".into(), json!({ "type": "boolean", "description": "Include the empty groups between the first and the last" }));
        Value::Object(properties)
    }

    fn having_schema(numeric: &[String]) -> Value {
        let comparison = |ty: &str| {
            json!({
                "type": "object",
                "properties": {
                    "equalTo": { "type": ty }, "notEqualTo": { "type": ty },
                    "greaterThan": { "type": ty }, "greaterThanOrEqualTo": { "type": ty },
                    "lessThan": { "type": ty }, "lessThanOrEqualTo": { "type": ty }
                },
                "additionalProperties": false
            })
        };
        let columns: BTreeMap<&String, Value> =
            numeric.iter().map(|c| (c, comparison("number"))).collect();
        let by_column =
            json!({ "type": "object", "properties": columns, "additionalProperties": false });
        let mut properties = Map::new();
        properties.insert("count".into(), comparison("integer"));
        for function in ["sum", "avg", "min", "max"] {
            properties.insert(function.into(), by_column.clone());
        }
        json!({
            "type": "object",
            "description": "Conditions the groups must meet, all of them: `having[count][gt]=5`, `having[sum][total][gte]=100`",
            "properties": properties,
            "additionalProperties": false
        })
    }

    fn aggregate_parameters(table: &TableMetadata, groups: &[String]) -> Vec<Value> {
        let properties = Self::aggregate_properties(table, groups);
        let mut params = Vec::new();
        for (name, schema) in properties.as_object().into_iter().flatten() {
            let mut param = json!({ "name": name, "in": "query", "schema": schema });
            if let Some(description) = schema.get("description") {
                param["description"] = description.clone();
            }
            match name.as_str() {
                "filter" => {
                    param["style"] = json!("deepObject");
                    param["explode"] = json!(true);
                    param["description"] =
                        json!("Filter criteria, like a list's: `filter[column][operator]=value`");
                }
                "having" => {
                    param["style"] = json!("deepObject");
                    param["explode"] = json!(true);
                }
                _ if schema["type"] == "array" => {
                    param["style"] = json!("form");
                    param["explode"] = json!(false);
                }
                _ => {}
            }
            params.push(param);
        }
        params
    }

    // -----------------------------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------------------------

    pub(super) fn generate_search_method_paths(
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
        config: &Config,
        graphql: Option<&Schema>,
        paths: &mut Map<String, Value>,
    ) {
        if check_table_permissions(table, Some(config), Operation::Read).is_err() {
            return;
        }
        let name = &table.name;
        let limits = catalog.search_limits;
        for (root, method) in catalog.search_fields(table) {
            let segment = method_segment(method);
            let mut properties = Map::new();
            let mut required = Vec::new();
            let definition = graphql.and_then(|schema| {
                let query = schema.schema_definition.query.as_ref()?;
                match schema.types.get(query.as_str())? {
                    ExtendedType::Object(object) => object
                        .fields
                        .get(root.as_str())
                        .map(|f| (schema, f.clone())),
                    _ => None,
                }
            });
            if let Some((schema, field)) = &definition {
                {
                    for argument in &field.arguments {
                        let argument_name = argument.name.as_str();
                        if matches!(argument_name, "first" | "filter" | "condition") {
                            continue;
                        }
                        properties.insert(
                            argument_name.to_string(),
                            graphql_schema(schema, &argument.ty, 0),
                        );
                        if argument.ty.is_non_null() && argument.default_value.is_none() {
                            required.push(argument_name.to_string());
                        }
                    }
                }
            }
            properties.insert(
                "first".into(),
                json!({
                    "type": "integer", "minimum": 0, "maximum": limits.max,
                    "description": format!("Most hits to return (default {}, at most {})", limits.default, limits.max)
                }),
            );
            properties.insert("filter".into(), schema_ref(&format!("{name}Filter")));
            properties.insert("fields".into(), json!({ "type": "array", "items": { "type": "string", "enum": Self::visible_fields(table, catalog) } }));
            let relations = Self::relation_names(table, catalog);
            if !relations.is_empty() {
                properties.insert(
                    "include".into(),
                    json!({ "type": "array", "items": { "type": "string", "enum": relations } }),
                );
            }
            if method == crate::catalog::SearchMethod::FullText {
                properties.insert(
                    "highlight".into(),
                    json!({
                        "description": "Mark the words that matched in the text of the document (`ts_headline`): the hits then have a `highlight`. `true` for the defaults",
                        "oneOf": [
                            { "type": "boolean" },
                            {
                                "type": "object",
                                "properties": {
                                    "startSel": { "type": "string", "default": "<b>" },
                                    "stopSel": { "type": "string", "default": "</b>" },
                                    "maxWords": { "type": "integer" },
                                    "minWords": { "type": "integer" },
                                    "maxFragments": { "type": "integer", "description": "Fragments to show (0: the start of the text)" }
                                },
                                "additionalProperties": false
                            }
                        ]
                    }),
                );
            }
            let mut hit_properties =
                json!({ "score": { "type": "number" }, "row": schema_ref(name) });
            if method == crate::catalog::SearchMethod::FullText {
                hit_properties["highlight"] = json!({ "type": ["string", "null"], "description": "The matching fragments, the words marked (with `highlight`)" });
            }
            if method == crate::catalog::SearchMethod::Nearest {
                hit_properties["distance"] =
                    json!({ "type": "number", "description": "Distance from `near`" });
            }
            let mut body = json!({
                "type": "object",
                "description": format!("A `{segment}` search of `{name}`: the arguments of the GraphQL field `{root}`, `first`, `filter` to scope the matches and `fields`"),
                "properties": properties,
                "additionalProperties": definition.is_none()
            });
            if !required.is_empty() {
                body["required"] = json!(required);
            }
            paths.insert(
                format!("{REST_PREFIX}/{name}/search/{segment}"),
                json!({ "post": {
                    "summary": format!("Search {name} ({segment})"),
                    "description": format!("The best matches first, not paginated (`hasMore` says whether more rows matched). Backed by the GraphQL field `{root}`."),
                    "operationId": format!("search_{name}_{}", segment.replace('-', "_")),
                    "requestBody": { "required": !required.is_empty(), "content": json_content(body) },
                    "responses": responses(json!({ "200": {
                        "description": "Hits, best first",
                        "content": json_content(json!({
                            "type": "object",
                            "properties": {
                                "hits": { "type": "array", "items": {
                                    "type": "object",
                                    "properties": hit_properties,
                                    "required": ["score", "row"]
                                } },
                                "hasMore": { "type": "boolean" }
                            },
                            "required": ["hits", "hasMore"]
                        }))
                    } }), &[]),
                } }),
            );
        }
    }

    pub(super) fn generate_global_search_paths(
        catalog: &DatabaseCatalog,
        config: &Config,
        served: &[&TableMetadata],
        schemas: &mut Map<String, Value>,
        paths: &mut Map<String, Value>,
    ) {
        if catalog.global_search_field_any().is_none() {
            return;
        }
        let searchable: Vec<&TableMetadata> = catalog
            .global_search_tables()
            .into_iter()
            .map(|(table, _)| table)
            .filter(|t| served.iter().any(|s| std::ptr::eq(*s, *t)))
            .filter(|t| check_table_permissions(t, Some(config), Operation::Read).is_ok())
            .collect();
        if searchable.is_empty() {
            return;
        }
        let names: Vec<&str> = searchable.iter().map(|t| t.name.as_str()).collect();
        let rows: Vec<Value> = searchable.iter().map(|t| schema_ref(&t.name)).collect();
        schemas.insert(
            "GlobalSearchResults".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "hits": { "type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "score": { "type": "number" },
                            "type": { "type": "string", "enum": names, "description": "The resource the row is from" },
                            "row": { "oneOf": rows }
                        },
                        "required": ["score", "type", "row"]
                    } },
                    "hasMore": { "type": "boolean" }
                },
                "required": ["hits", "hasMore"]
            }),
        );
        let limits = catalog.search_limits;
        let first = json!({ "type": "integer", "minimum": 0, "maximum": limits.max });
        let ok = json!({ "200": {
            "description": "Hits across the searchable resources, best first",
            "content": json_content(schema_ref("GlobalSearchResults"))
        } });
        paths.insert(
            format!("{REST_PREFIX}/{GLOBAL_SEARCH_RESOURCE}"),
            json!({
                "get": {
                    "summary": "Search every searchable resource",
                    "description": format!("Searches {} at once, by full text or trigram similarity, best matches first (at most {} hits).", names.join(", "), limits.max),
                    "operationId": "global_search",
                    "parameters": [
                        { "name": "query", "in": "query", "required": true, "schema": { "type": "string", "minLength": 1 } },
                        { "name": "first", "in": "query", "schema": first }
                    ],
                    "responses": responses(ok.clone(), &[])
                },
                "post": {
                    "summary": "Search every searchable resource (body)",
                    "operationId": "global_search_post",
                    "requestBody": { "required": true, "content": json_content(json!({
                        "type": "object",
                        "properties": { "query": { "type": "string", "minLength": 1 }, "first": first },
                        "required": ["query"],
                        "additionalProperties": false
                    })) },
                    "responses": responses(ok, &[])
                }
            }),
        );
    }

    // -----------------------------------------------------------------------------------------
    // Functions
    // -----------------------------------------------------------------------------------------

    pub(super) fn generate_rpc_paths(
        catalog: &DatabaseCatalog,
        config: &Config,
        default_schema: &str,
        served: &[&TableMetadata],
        paths: &mut Map<String, Value>,
    ) {
        for f in rpc_functions(catalog, default_schema, None) {
            // the rows of a table the API does not serve cannot be described
            let table = match &f.returns {
                FunctionReturn::Table { schema, table } => {
                    match served
                        .iter()
                        .find(|t| &t.schema == schema && &t.name == table)
                    {
                        Some(t) => Some(*t),
                        None => continue,
                    }
                }
                _ => None,
            };
            if table
                .is_some_and(|t| check_table_permissions(t, Some(config), Operation::Read).is_err())
            {
                continue;
            }
            paths.insert(
                format!("{REST_PREFIX}/{RPC_RESOURCE}/{}", f.name),
                Self::function_path(catalog, f, table, &config.limits),
            );
        }
    }

    /// The JSON Schema of what a function returns.
    fn function_result(
        catalog: &DatabaseCatalog,
        f: &FunctionMetadata,
        table: Option<&TableMetadata>,
    ) -> Option<Value> {
        let set = |item: Value| {
            if f.returns_set {
                json!({ "type": "array", "items": item })
            } else {
                item
            }
        };
        match &f.returns {
            FunctionReturn::Void => None,
            FunctionReturn::Value(pg_type) => {
                let item = Self::pg_type_to_json_schema(pg_type, catalog);
                Some(if f.returns_set && !matches!(pg_type, PgType::Array(_)) {
                    json!({ "type": "array", "items": item })
                } else {
                    item
                })
            }
            FunctionReturn::Record => {
                let properties: BTreeMap<String, Value> = f
                    .return_columns
                    .iter()
                    .map(|c| {
                        (
                            crate::catalog::models::to_camel_case(&c.name),
                            Self::pg_type_to_json_schema(&c.pg_type, catalog),
                        )
                    })
                    .collect();
                Some(set(json!({ "type": "object", "properties": properties })))
            }
            FunctionReturn::Table { .. } => {
                let row = schema_ref(&table?.name);
                Some(if f.returns_set {
                    json!({ "type": "array", "items": row })
                } else {
                    json!({ "anyOf": [row, { "type": "null" }] })
                })
            }
        }
    }

    fn argument_schema(catalog: &DatabaseCatalog, arg: &FunctionArgument) -> Value {
        // A required argument cannot be null (`a: Int!`); one with a default can (`b: Int`)
        let column = ColumnMetadata {
            is_nullable: arg.has_default,
            ..arg.as_column()
        };
        Self::column_to_json_schema(&column, catalog)
    }

    fn function_path(
        catalog: &DatabaseCatalog,
        f: &FunctionMetadata,
        table: Option<&TableMetadata>,
        limits: &crate::config::models::LimitsConfig,
    ) -> Value {
        let connection = table.is_some() && f.returns_set && !f.volatile;
        let single_row = table.is_some() && !connection;
        let result = Self::function_result(catalog, f, table);

        // The arguments, typed from the function's own signature
        let mut arguments = Map::new();
        let mut required = Vec::new();
        for arg in &f.arguments {
            let name = arg.graphql_name();
            let mut schema = Self::argument_schema(catalog, arg);
            if let Some(description) = &arg.as_column().description {
                schema["description"] = json!(description);
            }
            arguments.insert(name.clone(), schema);
            if !arg.has_default {
                required.push(name);
            }
        }
        let mut args_schema =
            json!({ "type": "object", "properties": arguments, "additionalProperties": false });
        if !required.is_empty() {
            args_schema["required"] = json!(required);
        }

        // Query parameters (GET) and body (POST)
        let mut query_params: Vec<Value> = f
            .arguments
            .iter()
            .map(|arg| {
                let name = arg.graphql_name();
                let mut param = json!({
                    "name": name, "in": "query",
                    "required": !arg.has_default,
                    "schema": Self::argument_schema(catalog, arg)
                });
                if matches!(arg.pg_type, PgType::Array(_)) {
                    param["style"] = json!("form");
                    param["explode"] = json!(false);
                    param["description"] = json!("A comma-separated list, or a JSON array");
                }
                param
            })
            .collect();
        let mut body_properties = Map::new();
        body_properties.insert("args".into(), args_schema);
        if let Some(table) = table {
            if connection {
                if let Value::Array(list) = Self::generate_list_parameters(table, catalog, limits) {
                    query_params.extend(list);
                }
                if let Value::Object(options) =
                    Self::search_request_properties(table, catalog, limits)
                {
                    body_properties.extend(options);
                }
            } else if single_row {
                query_params.push(Self::fields_param(table, catalog));
                query_params.push(Self::include_param(table, catalog));
                query_params.extend(Self::embed_parameters(table, catalog, limits, false));
                body_properties.insert("fields".into(), json!({ "type": "array", "items": { "type": "string", "enum": Self::visible_fields(table, catalog) } }));
            }
        }
        query_params.retain(|p| !p.is_null());
        let mut body = json!({ "type": "object", "properties": body_properties, "additionalProperties": false });
        if !required.is_empty() {
            body["required"] = json!(["args"]);
        }
        let body_schema = if f.arguments.len() == 1
            && matches!(f.arguments[0].pg_type, PgType::Json | PgType::Jsonb)
        {
            let single_schema = Self::argument_schema(catalog, &f.arguments[0]);
            json!({
                "anyOf": [
                    body,
                    single_schema
                ]
            })
        } else {
            body
        };

        let success = match &result {
            None => json!({ "204": { "description": "Done" } }),
            Some(schema) => {
                let mut ok = json!({
                    "description": if connection { "A list of rows" } else { "The result" },
                    "content": json_content(schema.clone())
                });
                if connection {
                    ok["headers"] = json!({
                        "Link": { "description": "RFC 8288 `next` / `prev` links (GET)", "schema": { "type": "string" } },
                        "X-Next-Cursor": { "description": "Cursor of the next page (POST)", "schema": { "type": "string" } },
                        "X-Previous-Cursor": { "description": "Cursor of the previous page (POST)", "schema": { "type": "string" } },
                        "X-Total-Count": { "description": "With `Prefer: count=exact`, `count=planned`, or `count=estimated`", "schema": { "type": "integer" } },
                        "X-Total-Count-Estimate": { "description": "`true` if `X-Total-Count` is a planner estimate", "schema": { "type": "string" } }
                    });
                }
                json!({ "200": ok })
            }
        };
        let mut responses = responses(success, &[("404", "Not Found: no such function")]);
        if f.volatile {
            responses["409"] = problem("Conflict: a constraint, with its typed `code`");
            responses["422"] =
                problem("Unprocessable Content: a validation failure, with its typed `code`");
        }
        if connection {
            // reading from a table's rows
            responses["400"] = problem(
                "Bad Request: an unknown or malformed argument, parameter, operator or field",
            );
        }

        let summary = f
            .description
            .clone()
            .unwrap_or_else(|| format!("Call `{}`", f.name));
        let describe = |how: &str| {
            let mut text = format!("{summary}\n\n{how}");
            if f.volatile {
                text.push_str(" The function changes data, so it runs in a write transaction and is called with POST only.");
            }
            text
        };
        let mut operations = Map::new();
        if !f.volatile {
            let mut get = json!({
                "summary": summary,
                "description": describe("Arguments are query parameters."),
                "operationId": format!("rpc_{}", f.name),
                "parameters": query_params,
                "responses": responses.clone(),
            });
            if connection {
                // counts are opt-in like everywhere
                if let Some(parameters) = get["parameters"].as_array_mut() {
                    parameters.push(prefer_param(false));
                }
            }
            if f.deprecation.is_some() {
                get["deprecated"] = json!(true);
            }
            operations.insert("get".into(), get);
        }
        let mut post = json!({
            "summary": summary,
            "description": describe("Arguments are the `args` object of the body."),
            "operationId": if f.volatile { format!("rpc_{}", f.name) } else { format!("rpc_{}_post", f.name) },
            "requestBody": { "required": !required.is_empty(), "content": json_content(body_schema) },
            "responses": responses,
        });
        if f.deprecation.is_some() {
            post["deprecated"] = json!(true);
        }
        operations.insert("post".into(), post);
        Value::Object(operations)
    }
}
