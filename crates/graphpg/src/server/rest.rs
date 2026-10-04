//! REST API request handling, query parameter parsing, and response shaping.
//!
//! Implements Story #58 (REST reads: filtering, ordering, pagination, and embedding)
//! using the protocol-neutral [`QueryPlanner`] and [`ResponseShaper`].
//!
//! Query parameters follow the grammar decided in Decision Record #59:
//! - Filters: `?filter[col][op]=val` (OpenAPI `deepObject` style)
//! - Order: `?order=col:desc,col2:asc` or `?order=-col,col2`
//! - Pagination: Keyset `?first=n&after=<cursor>` + offset `?limit=n&offset=n`
//! - Field selection: `?fields=a,b`
//! - Relation embedding: `?include=rel1,rel2`
//! - Complex search: `POST /rest/<resource>/search` with JSON filter body

use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use percent_encoding::{percent_decode_str, percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::Instrument;

/// RFC 3986 unreserved characters (alphanumeric, -, _, ., ~) left unencoded.
const CURSOR_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

use super::auth::AuthSessionContext;
use super::http::execution_error;
use super::ServerState;
use crate::catalog::{ColumnMetadata, DatabaseCatalog, FieldKind, PgType, TableMetadata};
use crate::compiler::SqlCompiler;
use crate::execution::ExecutionEngine;
use crate::planner::{QueryPlanner, RequestField, RequestOperation, ResponseShaper, RestPageInfo};

/// Parsed REST collection query options.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RestQuery {
    pub filter: Option<serde_json::Value>,
    pub order_by: Option<serde_json::Value>,
    pub first: Option<usize>,
    pub last: Option<usize>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub offset: Option<usize>,
    pub fields: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
}

/// Normalizes a filter operator string into the canonical name expected by [`QueryPlanner`].
pub fn normalize_operator(op: &str) -> &'static str {
    match op {
        "eq" | "equalTo" => "equalTo",
        "neq" | "ne" | "notEqualTo" => "notEqualTo",
        "gt" | "greaterThan" => "greaterThan",
        "gte" | "greaterThanOrEqualTo" => "greaterThanOrEqualTo",
        "lt" | "lessThan" => "lessThan",
        "lte" | "lessThanOrEqualTo" => "lessThanOrEqualTo",
        "in" => "in",
        "nin" | "notIn" => "notIn",
        "like" => "like",
        "nlike" | "notLike" => "notLike",
        "ilike" => "ilike",
        "nilike" | "notIlike" => "notIlike",
        "startsWith" | "startswith" => "startsWith",
        "notStartsWith" | "notstartswith" => "notStartsWith",
        "endsWith" | "endswith" => "endsWith",
        "notEndsWith" | "notendswith" => "notEndsWith",
        "contains" => "contains",
        "includes" => "includes",
        "is" | "isNull" => "isNull",
        "ancestorOf" => "ancestorOf",
        "descendantOf" => "descendantOf",
        "matches" => "matches",
        "matchesText" => "matchesText",
        "containsRange" => "containsRange",
        "overlaps" => "overlaps",
        "containedBy" => "containedBy",
        "isEmpty" => "isEmpty",
        "within" => "within",
        "withinDistance" => "withinDistance",
        "intersectsBox" => "intersectsBox",
        _ => "",
    }
}

/// Returns true if `op` is a recognized filter operator.
fn is_filter_operator(op: &str) -> bool {
    !normalize_operator(op).is_empty()
}

/// Recursively merges a `source` JSON object into `target`.
pub fn merge_json(target: &mut serde_json::Value, source: serde_json::Value) {
    match (target, source) {
        (serde_json::Value::Object(target_map), serde_json::Value::Object(source_map)) => {
            for (key, val) in source_map {
                if let Some(target_val) = target_map.get_mut(&key) {
                    merge_json(target_val, val);
                } else {
                    target_map.insert(key, val);
                }
            }
        }
        (target, source) => {
            *target = source;
        }
    }
}

/// Parses a filter value according to the operator and column metadata.
fn parse_filter_value(
    val: &str,
    operator: &str,
    col_opt: Option<&ColumnMetadata>,
) -> serde_json::Value {
    if operator == "isNull" {
        return serde_json::Value::Bool(
            val.eq_ignore_ascii_case("true") || val == "1" || val.eq_ignore_ascii_case("null"),
        );
    }

    if operator == "in" || operator == "notIn" {
        if val.starts_with('[') && val.ends_with(']') {
            if let Ok(arr) = serde_json::from_str(val) {
                return arr;
            }
        }
        let items: Vec<serde_json::Value> = val
            .split(',')
            .map(|s| parse_scalar_value(s.trim(), col_opt))
            .collect();
        return serde_json::Value::Array(items);
    }

    parse_scalar_value(val, col_opt)
}

/// Parses an individual scalar literal value, using column type when known.
fn parse_scalar_value(val: &str, col_opt: Option<&ColumnMetadata>) -> serde_json::Value {
    if val.eq_ignore_ascii_case("null") {
        return serde_json::Value::Null;
    }

    if let Some(col) = col_opt {
        if col.pg_type == PgType::Bool {
            if let Ok(b) = val.parse::<bool>() {
                return serde_json::Value::Bool(b);
            }
            if val == "1" {
                return serde_json::Value::Bool(true);
            }
            if val == "0" {
                return serde_json::Value::Bool(false);
            }
        } else if col.pg_type.is_numeric() {
            if let Ok(n) = val.parse::<i64>() {
                return serde_json::Value::Number(n.into());
            }
            if let Ok(f) = val.parse::<f64>() {
                if let Some(num) = serde_json::Number::from_f64(f) {
                    return serde_json::Value::Number(num);
                }
            }
        }
    } else {
        if let Ok(b) = val.parse::<bool>() {
            return serde_json::Value::Bool(b);
        }
        if let Ok(n) = val.parse::<i64>() {
            return serde_json::Value::Number(n.into());
        }
        if let Ok(f) = val.parse::<f64>() {
            if let Some(num) = serde_json::Number::from_f64(f) {
                return serde_json::Value::Number(num);
            }
        }
    }

    serde_json::Value::String(val.to_string())
}

/// Extracts bracket path segments from a query key like `filter[name][eq]`.
fn parse_bracket_path(key: &str) -> Option<(String, Vec<String>)> {
    if let Some(first_bracket) = key.find('[') {
        if !key.ends_with(']') {
            return None;
        }
        let root = &key[..first_bracket];
        let mut segments = Vec::new();
        let rest = &key[first_bracket..];
        let mut in_bracket = false;
        let mut start = 0;
        for (i, c) in rest.char_indices() {
            if c == '[' {
                if in_bracket {
                    return None;
                }
                in_bracket = true;
                start = i + 1;
            } else if c == ']' {
                if !in_bracket {
                    return None;
                }
                in_bracket = false;
                segments.push(rest[start..i].to_string());
            }
        }
        if in_bracket {
            return None;
        }
        Some((root.to_string(), segments))
    } else {
        Some((key.to_string(), Vec::new()))
    }
}

/// Resolves column metadata traversing nested paths across relations.
fn resolve_column_for_path<'a>(
    path: &[String],
    table_meta: &'a TableMetadata,
    catalog: &'a DatabaseCatalog,
) -> Option<&'a ColumnMetadata> {
    if path.is_empty() {
        return None;
    }
    let mut current_table = table_meta;
    let mut i = 0;
    while i < path.len() {
        let segment = &path[i];
        if segment == "some" || segment == "every" || segment == "none" || segment == "count" {
            i += 1;
            continue;
        }
        if i == path.len() - 1 {
            return current_table
                .column_by_field(segment)
                .or_else(|| current_table.columns.iter().find(|c| &c.name == segment));
        }
        let rel_opt = catalog
            .object_fields(&current_table.schema, &current_table.name)
            .iter()
            .find(|f| f.name == *segment || f.name.eq_ignore_ascii_case(segment))
            .and_then(|f| match &f.kind {
                FieldKind::Relation(rel) | FieldKind::Connection(rel) => Some(rel.clone()),
                _ => None,
            });
        if let Some(rel) = rel_opt {
            if let Some(target) = catalog.find_table(&rel.target_schema, &rel.target_table) {
                current_table = target;
                i += 1;
                continue;
            }
        }
        return None;
    }
    None
}

/// Converts bracket segments and a string value into a nested filter JSON object.
fn segments_to_filter(
    segments: &[String],
    val: &str,
    table_meta: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> Option<serde_json::Value> {
    if segments.is_empty() {
        return None;
    }

    let (path, op_name) = if segments.len() >= 2 && is_filter_operator(segments.last().unwrap()) {
        let op = segments.last().unwrap().as_str();
        (&segments[..segments.len() - 1], normalize_operator(op))
    } else {
        (segments, "equalTo")
    };

    let col_opt = resolve_column_for_path(path, table_meta, catalog);
    let parsed_val = parse_filter_value(val, op_name, col_opt);
    let mut current = serde_json::json!({ op_name: parsed_val });

    for segment in path.iter().rev() {
        let mut map = serde_json::Map::new();
        map.insert(segment.clone(), current);
        current = serde_json::Value::Object(map);
    }

    Some(current)
}

/// Parses an order parameter string into a JSON array of enum values for the planner.
pub fn parse_order_string(
    order_str: &str,
    table_meta: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> serde_json::Value {
    let mut items = Vec::new();
    for raw in order_str.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let (name, dir) = if let Some(stripped) = raw.strip_prefix('-') {
            (stripped.trim(), "DESC")
        } else if let Some(stripped) = raw.strip_prefix('+') {
            (stripped.trim(), "ASC")
        } else if let Some((n, d)) = raw.split_once(':') {
            let dir = if d.trim().eq_ignore_ascii_case("desc") {
                "DESC"
            } else {
                "ASC"
            };
            (n.trim(), dir)
        } else {
            (raw, "ASC")
        };

        if name.eq_ignore_ascii_case("natural") {
            items.push("NATURAL".to_string());
        } else if name.eq_ignore_ascii_case("primary_key")
            || name.eq_ignore_ascii_case("primary_key_asc")
            || name.eq_ignore_ascii_case("primary_key_desc")
        {
            items.push(format!("PRIMARY_KEY_{dir}"));
        } else if name.ends_with("_ASC") || name.ends_with("_DESC") {
            items.push(name.to_uppercase());
        } else if let Some(col) = table_meta.columns.iter().find(|c| {
            c.field_name == name || c.name == name || c.field_name.eq_ignore_ascii_case(name)
        }) {
            items.push(format!("{}_{dir}", col.order_by_name()));
        } else if let Some((opt_name, _)) = catalog
            .order_options(table_meta)
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            items.push(format!("{opt_name}_{dir}"));
        } else {
            items.push(format!("{}_{dir}", name.to_uppercase()));
        }
    }

    serde_json::Value::Array(items.into_iter().map(serde_json::Value::String).collect())
}

/// Recursively normalizes filter operator keys in a JSON filter value.
pub fn normalize_filter_json(val: serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (k, v) in map {
                let norm_k = if is_filter_operator(&k) {
                    normalize_operator(&k).to_string()
                } else {
                    k
                };
                new_map.insert(norm_k, normalize_filter_json(v));
            }
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(normalize_filter_json).collect())
        }
        other => other,
    }
}

/// Parses raw query string parameters into [`RestQuery`].
pub fn parse_query_params(
    raw_query: &str,
    table_meta: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> Result<RestQuery, String> {
    let mut query = RestQuery::default();
    if raw_query.is_empty() {
        return Ok(query);
    }

    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_val) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = percent_decode_str(&raw_key.replace('+', " "))
            .decode_utf8_lossy()
            .to_string();
        let val = percent_decode_str(&raw_val.replace('+', " "))
            .decode_utf8_lossy()
            .to_string();

        match key.as_str() {
            "first" | "limit" => {
                let n = val
                    .parse::<usize>()
                    .map_err(|_| format!("Invalid integer for `{key}`: {val}"))?;
                query.first = Some(n);
            }
            "last" => {
                let n = val
                    .parse::<usize>()
                    .map_err(|_| format!("Invalid integer for `last`: {val}"))?;
                query.last = Some(n);
            }
            "after" => {
                query.after = Some(val);
            }
            "before" => {
                query.before = Some(val);
            }
            "offset" => {
                let n = val
                    .parse::<usize>()
                    .map_err(|_| format!("Invalid integer for `offset`: {val}"))?;
                query.offset = Some(n);
            }
            "fields" | "select" => {
                let fields: Vec<String> = val
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                query.fields = Some(fields);
            }
            "include" => {
                let incs: Vec<String> = val
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                query.include = Some(incs);
            }
            "order" | "orderBy" | "order_by" => {
                query.order_by = Some(parse_order_string(&val, table_meta, catalog));
            }
            "filter" => {
                if val.starts_with('{') && val.ends_with('}') {
                    let parsed: serde_json::Value = serde_json::from_str(&val)
                        .map_err(|e| format!("Invalid JSON for `filter`: {e}"))?;
                    let norm = normalize_filter_json(parsed);
                    if let Some(target) = &mut query.filter {
                        merge_json(target, norm);
                    } else {
                        query.filter = Some(norm);
                    }
                }
            }
            _ => {
                if let Some((root, segments)) = parse_bracket_path(&key) {
                    if root == "filter" && !segments.is_empty() {
                        if let Some(filt) = segments_to_filter(&segments, &val, table_meta, catalog)
                        {
                            if let Some(target) = &mut query.filter {
                                merge_json(target, filt);
                            } else {
                                query.filter = Some(filt);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(query)
}

/// Recursively builds nested [`RequestField`] relations for `include=...`.
fn build_include_field(
    catalog: &DatabaseCatalog,
    schema: &str,
    table_name: &str,
    parts: &[&str],
) -> Result<RequestField, String> {
    if parts.is_empty() {
        return Err("Empty relation name".to_string());
    }

    let rel_name = parts[0];
    let object_fields = catalog.object_fields(schema, table_name);
    let matched_field = object_fields
        .iter()
        .find(|f| f.name == rel_name || f.name.eq_ignore_ascii_case(rel_name))
        .ok_or_else(|| format!("Unknown relation `{rel_name}` for resource `{table_name}`"))?;

    let rel_info = match &matched_field.kind {
        FieldKind::Relation(rel) | FieldKind::Connection(rel) => rel,
        _ => {
            return Err(format!(
                "Field `{rel_name}` on `{table_name}` is not a relation"
            ));
        }
    };

    let target_table = catalog
        .find_table(&rel_info.target_schema, &rel_info.target_table)
        .ok_or_else(|| {
            format!(
                "Target table `{}.{}` not found for relation `{rel_name}`",
                rel_info.target_schema, rel_info.target_table
            )
        })?;

    if parts.len() == 1 {
        let target_columns: Vec<RequestField> = target_table
            .columns
            .iter()
            .filter(|c| !c.omit)
            .map(|c| RequestField::new(&c.field_name))
            .collect();
        Ok(RequestField::new(&matched_field.name).with_children(target_columns))
    } else {
        let nested_field = build_include_field(
            catalog,
            &rel_info.target_schema,
            &rel_info.target_table,
            &parts[1..],
        )?;
        let mut target_columns: Vec<RequestField> = target_table
            .columns
            .iter()
            .filter(|c| !c.omit)
            .map(|c| RequestField::new(&c.field_name))
            .collect();
        target_columns.push(nested_field);
        Ok(RequestField::new(&matched_field.name).with_children(target_columns))
    }
}

/// Constructs RFC 8288 `Link` headers for pagination (`rel="next"` and `rel="prev"`).
fn build_link_header(
    resource: &str,
    raw_query: Option<&str>,
    page_info: Option<&RestPageInfo>,
    query: &RestQuery,
) -> Option<String> {
    let pi = page_info?;
    let mut links = Vec::new();

    // Preserve existing params, but remove "after" and "before"
    let mut base_params: Vec<(String, String)> = Vec::new();
    if let Some(q) = raw_query.filter(|s| !s.is_empty()) {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (raw_k, raw_v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            let decoded_key = percent_decode_str(&raw_k.replace('+', " "))
                .decode_utf8_lossy()
                .to_string();
            if decoded_key != "after" && decoded_key != "before" {
                base_params.push((raw_k.to_string(), raw_v.to_string()));
            }
        }
    }

    // Next link:
    if pi.has_next_page {
        if let Some(end_cursor) = &pi.end_cursor {
            let mut params = base_params.clone();
            let enc_cursor = percent_encode(end_cursor.as_bytes(), CURSOR_ENCODE_SET).to_string();
            params.push(("after".to_string(), enc_cursor));
            if !params.iter().any(|(k, _)| k == "first" || k == "limit") {
                if let Some(first) = query.first {
                    params.push(("first".to_string(), first.to_string()));
                }
            }
            let query_string = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            links.push(format!("</rest/{resource}?{query_string}>; rel=\"next\""));
        }
    }

    // Prev link:
    if pi.has_previous_page {
        if let Some(start_cursor) = &pi.start_cursor {
            let mut params = base_params.clone();
            let enc_cursor = percent_encode(start_cursor.as_bytes(), CURSOR_ENCODE_SET).to_string();
            params.push(("before".to_string(), enc_cursor));
            if !params.iter().any(|(k, _)| k == "last") {
                if let Some(last) = query.last {
                    params.push(("last".to_string(), last.to_string()));
                } else if let Some(first) = query.first {
                    params.push(("last".to_string(), first.to_string()));
                }
            }
            let query_string = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            links.push(format!("</rest/{resource}?{query_string}>; rel=\"prev\""));
        }
    }

    if links.is_empty() {
        None
    } else {
        Some(links.join(", "))
    }
}

/// Executes a REST query against the database using [`QueryPlanner`].
async fn execute_rest_query(
    state: &Arc<ServerState>,
    resource: &str,
    method: &str,
    raw_query: Option<&str>,
    query: RestQuery,
    auth_ctx: &Option<AuthSessionContext>,
) -> Response {
    let catalog = state.current_catalog();

    // Look up table by resource name
    let table = match catalog
        .find_table(&state.default_schema, resource)
        .or_else(|| catalog.find_table_any(resource))
        .or_else(|| {
            catalog
                .tables
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(resource) || t.list_field == resource)
        }) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("Resource `{resource}` not found")
                })),
            )
                .into_response();
        }
    };

    // Permission checks
    if let Some(config) = &state.config {
        if let Some(entity_cfg) = config.entities.get(&table.name) {
            if entity_cfg.omit {
                return (StatusCode::NOT_FOUND, "Not Found").into_response();
            }
            if entity_cfg.omit_operations.iter().any(|op| {
                *op == crate::config::models::Operation::Read
                    || *op == crate::config::models::Operation::List
            }) {
                return (StatusCode::FORBIDDEN, "Forbidden").into_response();
            }
        }
    }

    if !table.operations.read && !table.operations.list && !table.operations.connection {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }

    // Determine row fields (projections & inclusions)
    let mut row_fields: Vec<RequestField> = Vec::new();
    if let Some(requested_fields) = &query.fields {
        for f in requested_fields {
            if let Some(col) = table
                .columns
                .iter()
                .find(|c| &c.field_name == f || &c.name == f)
            {
                if !col.omit {
                    row_fields.push(RequestField::new(&col.field_name));
                }
            } else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("Unknown field `{f}` for resource `{resource}`")
                    })),
                )
                    .into_response();
            }
        }
    } else {
        row_fields = table
            .columns
            .iter()
            .filter(|c| !c.omit)
            .map(|c| RequestField::new(&c.field_name))
            .collect();
    }

    // Embed relations if `include` was passed
    if let Some(included) = &query.include {
        for inc in included {
            let parts: Vec<&str> = inc.split('.').collect();
            match build_include_field(&catalog, &table.schema, &table.name, &parts) {
                Ok(field) => row_fields.push(field),
                Err(err_msg) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": err_msg })),
                    )
                        .into_response();
                }
            }
        }
    }

    // Build neutral request arguments
    let mut args: HashMap<String, serde_json::Value> = HashMap::new();
    if let Some(f) = query.filter.clone() {
        args.insert("filter".to_string(), f);
    }
    if let Some(o) = query.order_by.clone() {
        args.insert("orderBy".to_string(), o);
    }
    if let Some(first) = query.first {
        args.insert("first".to_string(), serde_json::json!(first));
    }
    if let Some(last) = query.last {
        args.insert("last".to_string(), serde_json::json!(last));
    }
    if let Some(after) = query.after.clone() {
        args.insert("after".to_string(), serde_json::json!(after));
    }
    if let Some(before) = query.before.clone() {
        args.insert("before".to_string(), serde_json::json!(before));
    }
    if let Some(offset) = query.offset {
        args.insert("offset".to_string(), serde_json::json!(offset));
    }

    // Determine root field: prefer connection when keyset pagination or page info requested
    let (root_field_name, request_field) = if table.operations.connection {
        (
            table.connection_field.clone(),
            ResponseShaper::build_list_request(
                &table.connection_field,
                args,
                row_fields,
                true,
                true,
            ),
        )
    } else if table.operations.list {
        (
            table.list_field.clone(),
            RequestField::new(&table.list_field)
                .with_arguments(args)
                .with_children(row_fields),
        )
    } else {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": format!("Listing `{resource}` is not permitted")
            })),
        )
            .into_response();
    };

    let request_op = RequestOperation::query(vec![request_field]);

    // Plan operation using protocol-neutral planner
    let planner = QueryPlanner::new(&catalog, &table.schema);
    let plan = match planner.plan_request(&request_op) {
        Ok(p) => p,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("Planning error: {err}")
                })),
            )
                .into_response();
        }
    };

    // Compile plan into single SQL statement
    let mut compiled = SqlCompiler::compile_operation(&plan);
    if state.sql_comments {
        compiled.tag(&[
            ("application", "graphpg"),
            ("rest_resource", resource),
            ("rest_method", method),
        ]);
    }
    let compiled = Arc::new(compiled);

    // Acquire pooled connection and execute
    let (client, _database) = match state.read_pool(None).await {
        Ok(acq) => acq,
        Err(err) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("Database unavailable: {err}")
                })),
            )
                .into_response();
        }
    };

    let span = tracing::info_span!("rest.execute", rest.resource = %resource);
    let task = {
        let compiled = compiled.clone();
        let auth_ctx = auth_ctx.clone();
        async move { ExecutionEngine::run_with_auth(&*client, &compiled, None, auth_ctx.as_ref()).await }
    };

    let result = match tokio::spawn(task.instrument(span)).await {
        Ok(res) => res,
        Err(err) => {
            tracing::error!(error = %err, "the operation's task failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "the operation failed" })),
            )
                .into_response();
        }
    };

    let data = match result {
        Ok(d) => d,
        Err(err) => {
            let (status, err_obj) = execution_error(&err, &catalog);
            return (
                status,
                Json(serde_json::json!({ "error": err_obj.message })),
            )
                .into_response();
        }
    };

    // Shape response and emit HTTP Link and X-Total-Count headers
    match ResponseShaper::unwrap_list(&data, &root_field_name) {
        Ok(shaped) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                "application/json; charset=utf-8".parse().unwrap(),
            );
            if let Some(total) = shaped.total_count {
                if let Ok(val) = total.to_string().parse() {
                    headers.insert("x-total-count", val);
                }
            }
            if let Some(link_val) =
                build_link_header(resource, raw_query, shaped.page_info.as_ref(), &query)
            {
                if let Ok(val) = link_val.parse() {
                    headers.insert(header::LINK, val);
                }
            }
            (StatusCode::OK, headers, Json(shaped.rows)).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Response shaping failed: {err}")
            })),
        )
            .into_response(),
    }
}

/// Handler for `GET /rest/{resource}`.
pub async fn rest_list_handler(
    State(state): State<Arc<ServerState>>,
    Path(resource): Path<String>,
    RawQuery(raw_query): RawQuery,
    request_headers: HeaderMap,
) -> Response {
    if !state.openapi_enabled {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let auth_ctx = match state.authenticate(&request_headers).await {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };

    let catalog = state.current_catalog();
    let table = match catalog
        .find_table(&state.default_schema, &resource)
        .or_else(|| catalog.find_table_any(&resource))
        .or_else(|| {
            catalog
                .tables
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(&resource) || t.list_field == resource)
        }) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("Resource `{resource}` not found")
                })),
            )
                .into_response();
        }
    };

    let query = match parse_query_params(raw_query.as_deref().unwrap_or(""), table, &catalog) {
        Ok(q) => q,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": err })),
            )
                .into_response();
        }
    };

    execute_rest_query(
        &state,
        &resource,
        "GET",
        raw_query.as_deref(),
        query,
        &auth_ctx,
    )
    .await
}

/// Handler for `POST /rest/{resource}/search`.
pub async fn rest_search_handler(
    State(state): State<Arc<ServerState>>,
    Path(resource): Path<String>,
    RawQuery(raw_query): RawQuery,
    request_headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !state.openapi_enabled {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let auth_ctx = match state.authenticate(&request_headers).await {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };

    let catalog = state.current_catalog();
    let table = match catalog
        .find_table(&state.default_schema, &resource)
        .or_else(|| catalog.find_table_any(&resource))
        .or_else(|| {
            catalog
                .tables
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(&resource) || t.list_field == resource)
        }) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("Resource `{resource}` not found")
                })),
            )
                .into_response();
        }
    };

    let mut query = match parse_query_params(raw_query.as_deref().unwrap_or(""), table, &catalog) {
        Ok(q) => q,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": err })),
            )
                .into_response();
        }
    };

    // If body is present, extract filter and optional options from JSON body
    if !body.is_empty() {
        let body_json: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("Invalid JSON request body: {e}")
                    })),
                )
                    .into_response();
            }
        };

        if let Some(obj) = body_json.as_object() {
            if let Some(filter_val) = obj.get("filter") {
                query.filter = Some(normalize_filter_json(filter_val.clone()));
                if let Some(order_val) = obj.get("orderBy").or_else(|| obj.get("order")) {
                    if let Some(s) = order_val.as_str() {
                        query.order_by = Some(parse_order_string(s, table, &catalog));
                    } else if order_val.is_array() {
                        query.order_by = Some(order_val.clone());
                    }
                }
                if let Some(first_val) = obj.get("first").or_else(|| obj.get("limit")) {
                    if let Some(n) = first_val.as_u64() {
                        query.first = Some(n as usize);
                    }
                }
                if let Some(last_val) = obj.get("last") {
                    if let Some(n) = last_val.as_u64() {
                        query.last = Some(n as usize);
                    }
                }
                if let Some(after_val) = obj.get("after") {
                    if let Some(s) = after_val.as_str() {
                        query.after = Some(s.to_string());
                    }
                }
                if let Some(before_val) = obj.get("before") {
                    if let Some(s) = before_val.as_str() {
                        query.before = Some(s.to_string());
                    }
                }
                if let Some(offset_val) = obj.get("offset") {
                    if let Some(n) = offset_val.as_u64() {
                        query.offset = Some(n as usize);
                    }
                }
                if let Some(fields_val) = obj.get("fields") {
                    if let Some(arr) = fields_val.as_array() {
                        query.fields = Some(
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect(),
                        );
                    }
                }
                if let Some(inc_val) = obj.get("include") {
                    if let Some(arr) = inc_val.as_array() {
                        query.include = Some(
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect(),
                        );
                    }
                }
            } else {
                // Entire body is the filter object
                query.filter = Some(normalize_filter_json(body_json));
            }
        }
    }

    execute_rest_query(
        &state,
        &resource,
        "POST",
        raw_query.as_deref(),
        query,
        &auth_ctx,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_normalize_operator() {
        assert_eq!(normalize_operator("eq"), "equalTo");
        assert_eq!(normalize_operator("equalTo"), "equalTo");
        assert_eq!(normalize_operator("neq"), "notEqualTo");
        assert_eq!(normalize_operator("ne"), "notEqualTo");
        assert_eq!(normalize_operator("gt"), "greaterThan");
        assert_eq!(normalize_operator("gte"), "greaterThanOrEqualTo");
        assert_eq!(normalize_operator("lt"), "lessThan");
        assert_eq!(normalize_operator("lte"), "lessThanOrEqualTo");
        assert_eq!(normalize_operator("in"), "in");
        assert_eq!(normalize_operator("nin"), "notIn");
        assert_eq!(normalize_operator("like"), "like");
        assert_eq!(normalize_operator("ilike"), "ilike");
        assert_eq!(normalize_operator("startsWith"), "startsWith");
        assert_eq!(normalize_operator("startswith"), "startsWith");
        assert_eq!(normalize_operator("is"), "isNull");
        assert_eq!(normalize_operator("isNull"), "isNull");
    }

    #[test]
    fn test_parse_bracket_path() {
        assert_eq!(
            parse_bracket_path("filter[name][eq]"),
            Some((
                "filter".to_string(),
                vec!["name".to_string(), "eq".to_string()]
            ))
        );
        assert_eq!(
            parse_bracket_path("filter[author][username][eq]"),
            Some((
                "filter".to_string(),
                vec![
                    "author".to_string(),
                    "username".to_string(),
                    "eq".to_string()
                ]
            ))
        );
        assert_eq!(
            parse_bracket_path("filter"),
            Some(("filter".to_string(), vec![]))
        );
        assert_eq!(
            parse_bracket_path("order"),
            Some(("order".to_string(), vec![]))
        );
    }

    #[test]
    fn test_merge_json() {
        let mut target = json!({
            "age": { "greaterThan": 18 },
            "name": { "equalTo": "Alice" }
        });
        let source = json!({
            "age": { "lessThan": 65 },
            "city": { "equalTo": "London" }
        });
        merge_json(&mut target, source);
        assert_eq!(
            target,
            json!({
                "age": { "greaterThan": 18, "lessThan": 65 },
                "name": { "equalTo": "Alice" },
                "city": { "equalTo": "London" }
            })
        );
    }

    #[test]
    fn test_normalize_filter_json() {
        let input = json!({
            "or": [
                { "name": { "eq": "Alice" } },
                { "age": { "gt": 30, "lte": 50 } }
            ]
        });
        let normalized = normalize_filter_json(input);
        assert_eq!(
            normalized,
            json!({
                "or": [
                    { "name": { "equalTo": "Alice" } },
                    { "age": { "greaterThan": 30, "lessThanOrEqualTo": 50 } }
                ]
            })
        );
    }

    #[test]
    fn test_build_link_header() {
        let pi = RestPageInfo {
            has_next_page: true,
            has_previous_page: false,
            start_cursor: Some("cur_start".to_string()),
            end_cursor: Some("cur_end".to_string()),
        };
        let query = RestQuery {
            first: Some(10),
            ..Default::default()
        };

        let link = build_link_header("users", Some("first=10&order=name:desc"), Some(&pi), &query);
        assert_eq!(
            link,
            Some("</rest/users?first=10&order=name:desc&after=cur_end>; rel=\"next\"".to_string())
        );

        let pi_prev = RestPageInfo {
            has_next_page: false,
            has_previous_page: true,
            start_cursor: Some("cur_start".to_string()),
            end_cursor: Some("cur_end".to_string()),
        };
        let link_prev = build_link_header(
            "users",
            Some("first=10&after=cur_end"),
            Some(&pi_prev),
            &query,
        );
        assert_eq!(
            link_prev,
            Some("</rest/users?first=10&before=cur_start&last=10>; rel=\"prev\"".to_string())
        );
    }
}
