//! REST API: reads (filtering, ordering, pagination, embedding) and writes (create, update,
//! upsert, delete, bulk and nested writes), planned onto the same `OperationPlan` as GraphQL.
//!
//! Every request is validated against the generated GraphQL input types before planning (see
//! [`validate`]), runs as one statement or one transaction, and fails with an RFC 9457
//! `application/problem+json` body (see [`problem`]). The grammar is in [`query`].

// Handlers fail with a `ProblemDetails` that is sent as the response, so it is never on a hot path
#![allow(clippy::result_large_err)]

mod aggregates;
mod collection;
mod functions;
mod media;
mod patch;
mod problem;
mod query;
mod search;
mod validate;

pub use aggregates::{rest_aggregate_get_handler, rest_aggregate_post_handler};
pub use collection::{
    rest_bulk_upsert_handler, rest_collection_delete_handler, rest_collection_update_handler,
};
pub use functions::{rest_rpc_handler, rpc_functions, RPC_RESOURCE};
pub use problem::ProblemDetails;
pub(crate) use query::{field_arguments, find_embeddable, EmbedKind, Embeddable};
pub use query::{
    merge_json, normalize_filter_json, normalize_operator, parse_order_string, parse_prefer_header,
    parse_query_params, EmbedControls, Preferences, QueryKind, RestQuery,
};
pub use search::{
    method_segment, rest_global_search_handler, rest_search_method_handler, GLOBAL_SEARCH_RESOURCE,
};

use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use percent_encoding::{percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use tracing::Instrument;

use super::auth::AuthSessionContext;
use super::ServerState;
use crate::catalog::{
    ColumnMetadata, DatabaseCatalog, FieldKind, MutationAction, MutationField, PgType,
    TableMetadata,
};
use crate::compiler::SqlCompiler;
use crate::config::models::Operation;
use crate::execution::ExecutionEngine;
use crate::planner::{QueryPlanner, RequestField, RequestOperation, ResponseShaper, RestPageInfo};
use media::Media;
use validate::{validate_argument, Root};

/// `GET /rest/node/{id}`: the row a Relay global ID (GraphQL's `id`) names. A table cannot use it.
pub const NODE_RESOURCE: &str = "node";

/// Path prefix of the REST API.
pub const REST_PREFIX: &str = "/rest";
pub use aggregates::MAX_GROUP_KEYS;

/// RFC 3986 unreserved characters (alphanumeric, -, _, ., ~) stay unencoded.
const URL_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

type RestResult = Result<Response, ProblemDetails>;

// ---------------------------------------------------------------------------------------------
// Resource, key and permission helpers
// ---------------------------------------------------------------------------------------------

/// Finds the table a resource name refers to: the default schema first, then any schema, then
/// case-insensitively or by its list field name. The OpenAPI generator uses the same rule for
/// the tables it describes.
pub fn resolve_table<'a>(
    catalog: &'a DatabaseCatalog,
    default_schema: &str,
    resource: &str,
) -> Option<&'a TableMetadata> {
    // `/rest/rpc/…` and `/rest/search` are not resources
    if resource == RPC_RESOURCE || resource == GLOBAL_SEARCH_RESOURCE || resource == NODE_RESOURCE {
        return None;
    }
    catalog
        .find_table(default_schema, resource)
        .or_else(|| catalog.find_table_any(resource))
        .or_else(|| {
            catalog
                .tables
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(resource) || t.list_field == resource)
        })
}

/// Finds the table a resource name refers to in `schema` alone (`/rest/{schema}/{resource}`):
/// by name, then case-insensitively or by its list field name.
pub fn resolve_table_in<'a>(
    catalog: &'a DatabaseCatalog,
    schema: &str,
    resource: &str,
) -> Option<&'a TableMetadata> {
    if resource == RPC_RESOURCE || resource == GLOBAL_SEARCH_RESOURCE || resource == NODE_RESOURCE {
        return None;
    }
    catalog.find_table(schema, resource).or_else(|| {
        catalog.tables.iter().find(|t| {
            t.schema == schema
                && (t.name.eq_ignore_ascii_case(resource) || t.list_field == resource)
        })
    })
}

/// The schemas with a path of their own, `/rest/{schema}/…`: the default one and those with
/// tables or functions, as long as the name can stand in a path (and is not `rpc` or `search`).
/// The router mounts these and the OpenAPI document describes them.
pub fn rest_schemas(catalog: &DatabaseCatalog, default_schema: &str) -> Vec<String> {
    let mut schemas = vec![default_schema.to_string()];
    for schema in catalog
        .tables
        .iter()
        .map(|t| &t.schema)
        .chain(catalog.functions.iter().map(|f| &f.schema))
    {
        if !schemas.contains(schema) {
            schemas.push(schema.clone());
        }
    }
    schemas.retain(|schema| {
        !schema.is_empty()
            && schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            && schema != RPC_RESOURCE
            && schema != GLOBAL_SEARCH_RESOURCE
            && schema != NODE_RESOURCE
    });
    schemas
}

/// A resource name as it is in a path under `/rest`: `schema/name` under a schema.
pub fn scoped(schema: &Option<String>, name: &str) -> String {
    match schema {
        Some(schema) => format!("{schema}/{name}"),
        None => name.to_string(),
    }
}

/// The path REST resources are under: `/rest`, or `/rest/{schema}` for a schema's own.
pub fn rest_base(schema: &Option<String>) -> String {
    match schema {
        Some(schema) => format!("{REST_PREFIX}/{schema}"),
        None => REST_PREFIX.to_string(),
    }
}

/// The schema a request's path names: set on the routes of `/rest/{schema}/…`.
#[derive(Debug, Clone)]
pub struct SchemaScope(pub String);

/// Path parameters, with the schema of the path (`None` for the unqualified routes).
pub struct Scoped<T>(pub Option<String>, pub T);

impl<S, T> axum::extract::FromRequestParts<S> for Scoped<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned + Send,
{
    type Rejection = <Path<T> as axum::extract::FromRequestParts<S>>::Rejection;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let schema = parts.extensions.get::<SchemaScope>().map(|s| s.0.clone());
        let Path(value) = Path::<T>::from_request_parts(parts, state).await?;
        Ok(Self(schema, value))
    }
}

/// Whether `operation` on `table` is allowed by the configuration and the catalog (which
/// reflects the connecting role's grants).
pub fn check_table_permissions(
    table: &TableMetadata,
    config: Option<&crate::config::Config>,
    operation: Operation,
) -> Result<(), ProblemDetails> {
    if let Some(entity) = config.and_then(|c| c.entities.get(&table.name)) {
        if entity.omit {
            return Err(ProblemDetails::not_found(format!(
                "Resource `{}` not found",
                table.name
            )));
        }
        if entity.omit_operations.contains(&operation) {
            return Err(ProblemDetails::forbidden(format!(
                "Operation {operation:?} on resource `{}` is forbidden",
                table.name
            )));
        }
    }

    let ops = &table.operations;
    let allowed = match operation {
        Operation::Read | Operation::List | Operation::Connection => {
            ops.read || ops.list || ops.connection
        }
        Operation::Create => ops.create,
        Operation::CreateMany => ops.create_many,
        Operation::Update | Operation::UpdateMany => ops.update,
        Operation::Delete | Operation::DeleteMany => ops.delete,
        Operation::Upsert => ops.create || ops.update,
    };
    if allowed {
        Ok(())
    } else {
        Err(ProblemDetails::forbidden(format!(
            "Operation {operation:?} on resource `{}` is not permitted",
            table.name
        )))
    }
}

/// Parses the `{key}` path segment: a raw key (composite keys comma-separated) or, for uuid and
/// numeric keys, the Relay global ID of this table's type.
pub fn parse_primary_key(
    key_str: &str,
    table: &TableMetadata,
) -> Result<serde_json::Value, String> {
    if table.primary_key.is_empty() {
        return Err(format!("Resource `{}` has no primary key", table.name));
    }

    if table.primary_key.len() == 1 {
        let col = table
            .get_column(&table.primary_key[0])
            .ok_or_else(|| format!("Primary key column `{}` not found", table.primary_key[0]))?;
        return match query::parse_key_scalar(key_str, col) {
            Ok(v) => Ok(v),
            Err(err) => {
                // Only keys that are not text can be told apart from a global ID
                let global = (col.pg_type == crate::catalog::PgType::Uuid
                    || col.pg_type.is_numeric())
                .then(|| crate::schema::SchemaBuilder::decode_global_id(key_str))
                .flatten()
                .filter(|(type_name, _)| *type_name == table.type_name);
                match global {
                    Some((_, raw)) => query::parse_key_scalar(&raw, col),
                    None => Err(err),
                }
            }
        };
    }

    let parts: Vec<&str> = key_str.split(',').map(str::trim).collect();
    if parts.len() != table.primary_key.len() {
        return Err(format!(
            "Composite key expects {} values separated by commas, got {}",
            table.primary_key.len(),
            parts.len()
        ));
    }
    let mut obj = serde_json::Map::new();
    for (part, pk_name) in parts.iter().zip(&table.primary_key) {
        let col = table
            .get_column(pk_name)
            .ok_or_else(|| format!("Primary key column `{pk_name}` not found"))?;
        obj.insert(col.field_name.clone(), query::parse_key_scalar(part, col)?);
    }
    Ok(serde_json::Value::Object(obj))
}

/// A row addressed in a path: by its primary key, or by one of its other unique keys
/// (`GET /rest/users/alice?by=username`, like GraphQL's `userByUsername`).
#[derive(Debug, Clone)]
pub(super) struct RowAddress {
    /// Columns of the unique key; empty for the primary key.
    pub unique: Vec<String>,
    /// `ByUsername`: the end of the names of the root fields of a unique key.
    suffix: String,
    /// The key as the arguments of the root fields.
    pub args: HashMap<String, serde_json::Value>,
    /// The parsed primary key: a body must agree with it. `None` for a unique key, whose
    /// columns can be updated.
    pub primary: Option<serde_json::Value>,
    /// The key as written in the path.
    pub label: String,
}

impl RowAddress {
    /// The primary key `key`.
    pub fn primary(table: &TableMetadata, key: &str) -> Result<Self, ProblemDetails> {
        let parsed = parse_primary_key(key, table).map_err(ProblemDetails::bad_request)?;
        Ok(Self {
            unique: Vec::new(),
            suffix: String::new(),
            args: key_args(table, &parsed),
            primary: Some(parsed),
            label: key.to_string(),
        })
    }

    /// The row whose unique key `by` (column names, comma-separated) has the values `key`
    /// (comma-separated, in the order of `by`).
    pub fn unique(table: &TableMetadata, key: &str, by: &str) -> Result<Self, ProblemDetails> {
        let wanted: Vec<&str> = by
            .split(',')
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .collect();
        let found = table.lookup_keys().find(|(_, cols)| {
            cols.len() == wanted.len()
                && wanted
                    .iter()
                    .all(|w| cols.iter().any(|c| c.field_name == *w || c.name == *w))
        });
        let Some((_, cols)) = found else {
            let keys: Vec<String> = table
                .lookup_keys()
                .map(|(_, cols)| {
                    cols.iter()
                        .map(|c| c.field_name.clone())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            return Err(ProblemDetails::invalid(
                format!(
                    "`{by}` is not a unique key of `{}` that rows can be addressed by{}",
                    table.name,
                    if keys.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", keys.join("; "))
                    }
                ),
                "by",
            ));
        };
        // Values come in the order the key was named
        let ordered: Vec<&ColumnMetadata> = wanted
            .iter()
            .filter_map(|w| {
                cols.iter()
                    .copied()
                    .find(|c| c.field_name == *w || c.name == *w)
            })
            .collect();
        let values: Vec<&str> = key.split(',').map(str::trim).collect();
        if values.len() != ordered.len() {
            return Err(ProblemDetails::bad_request(format!(
                "`{by}` takes {} value(s) separated by commas, got {}",
                ordered.len(),
                values.len()
            )));
        }
        let mut args = HashMap::new();
        for (col, value) in ordered.iter().zip(values) {
            args.insert(
                col.field_name.clone(),
                query::parse_key_scalar(value, col).map_err(ProblemDetails::bad_request)?,
            );
        }
        Ok(Self {
            unique: cols.iter().map(|c| c.name.clone()).collect(),
            suffix: crate::catalog::models::unique_key_suffix(&cols),
            args,
            primary: None,
            label: key.to_string(),
        })
    }

    /// The address a path and its `by` parameter name.
    pub fn of(table: &TableMetadata, key: &str, by: Option<&str>) -> Result<Self, ProblemDetails> {
        match by {
            Some(by) => Self::unique(table, key, by),
            None => Self::primary(table, key),
        }
    }

    /// The root field that reads the row.
    pub fn read_root(&self, table: &TableMetadata) -> String {
        format!("{}{}", table.single_field, self.suffix)
    }
}

/// Takes the `by` parameter (the unique key a path addresses a row by) out of a query string.
pub(super) fn take_by(raw_query: Option<&str>) -> Result<(Option<String>, String), ProblemDetails> {
    let mut by = None;
    let mut rest = Vec::new();
    for pair in raw_query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "by" {
            by = Some(
                percent_encoding::percent_decode_str(&value.replace('+', " "))
                    .decode_utf8()
                    .map_err(|_| ProblemDetails::bad_request("Invalid UTF-8 in `by`"))?
                    .to_string(),
            );
        } else {
            rest.push(pair);
        }
    }
    Ok((by, rest.join("&")))
}

/// Arguments addressing one row by its parsed key.
fn key_args(table: &TableMetadata, key: &serde_json::Value) -> HashMap<String, serde_json::Value> {
    let mut args = HashMap::new();
    if table.primary_key.len() == 1 {
        if let Some(col) = table.get_column(&table.primary_key[0]) {
            args.insert(col.field_name.clone(), key.clone());
            args.insert("id".to_string(), key.clone());
        }
    } else if let Some(obj) = key.as_object() {
        args.extend(obj.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    args
}

/// The key members of a write body must agree with the URL: a row's key cannot be changed
/// through its own URL. Matching members are removed (the key travels as arguments).
fn strip_key_from_body(
    table: &TableMetadata,
    key: &serde_json::Value,
    body: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), ProblemDetails> {
    for pk in &table.primary_key {
        let Some(col) = table.get_column(pk) else {
            continue;
        };
        let Some(actual) = body.get(&col.field_name) else {
            continue;
        };
        let expected = if table.primary_key.len() == 1 {
            Some(key)
        } else {
            key.get(&col.field_name)
        };
        let normalized = match actual {
            serde_json::Value::String(s) if table.primary_key.len() == 1 => {
                parse_primary_key(s, table).ok()
            }
            serde_json::Value::String(s) => query::parse_key_scalar(s, col).ok(),
            other => Some(other.clone()),
        };
        if normalized.as_ref() != expected {
            return Err(ProblemDetails::invalid(
                format!(
                    "`{}` in the body does not match the key in the URL; a row's key cannot be changed",
                    col.field_name
                ),
                format!("body.{}", col.field_name),
            ));
        }
        body.remove(&col.field_name);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Field selection and embedding
// ---------------------------------------------------------------------------------------------

/// The selection that reads a value of `pg_type` whole: none for scalars, every attribute of a
/// composite (nested ones too), and `start` / `end` / `isEmpty` of a range (of each range of a
/// multirange). Without it the value would come back as an empty object.
fn value_selection(catalog: &DatabaseCatalog, pg_type: &PgType, depth: usize) -> Vec<RequestField> {
    let element = match pg_type {
        PgType::Array(inner) | PgType::Multirange(inner) => inner.as_ref(),
        other => other,
    };
    if element.is_range() || matches!(pg_type, PgType::Multirange(_)) {
        let bound = || {
            RequestField::new("").with_children(vec![
                RequestField::new("value"),
                RequestField::new("inclusive"),
            ])
        };
        let named = |name: &str| {
            let mut field = bound();
            field.name = name.to_string();
            field
        };
        return vec![named("start"), named("end"), RequestField::new("isEmpty")];
    }
    let PgType::Composite(key) = element else {
        return Vec::new();
    };
    if depth >= 4 {
        return Vec::new();
    }
    catalog
        .find_composite(key)
        .map(|composite| {
            composite
                .attributes
                .iter()
                .map(|a| {
                    RequestField::new(&a.field_name).with_children(value_selection(
                        catalog,
                        &a.pg_type,
                        depth + 1,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The request field for a column. The primary key of a Relay node table is the `databaseId`
/// field (the raw value), not the global `id`, so REST rows carry the real key under the
/// column's name.
fn column_field(
    catalog: &DatabaseCatalog,
    table: &TableMetadata,
    col: &ColumnMetadata,
) -> RequestField {
    if table.implements_node() && col.is_primary_key {
        RequestField::new("databaseId").with_alias(&col.field_name)
    } else {
        RequestField::new(&col.field_name).with_children(value_selection(catalog, &col.pg_type, 0))
    }
}

pub(crate) fn column_fields(catalog: &DatabaseCatalog, table: &TableMetadata) -> Vec<RequestField> {
    table
        .visible_columns()
        .map(|c| column_field(catalog, table, c))
        .collect()
}

#[derive(Default)]
struct IncludeTree(BTreeMap<String, IncludeTree>);

fn build_include_tree(include: &[String]) -> Result<IncludeTree, ProblemDetails> {
    let mut root = IncludeTree::default();
    for path in include {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(ProblemDetails::invalid(
                format!("Invalid `include` path `{path}`"),
                "include",
            ));
        }
        let mut node = &mut root;
        for part in parts {
            node = node.0.entry(part.to_string()).or_default();
        }
    }
    Ok(root)
}

/// Embedded field `name` of `table`: what it is, the table it leads to and the controls the
/// request gave it (`path` is the dotted `include` path).
struct Embed<'a> {
    field: String,
    kind: query::EmbedKind,
    target: &'a TableMetadata,
    arc_targets: Vec<(String, &'a TableMetadata)>,
    controls: Option<&'a EmbedControls>,
    /// Read as a connection (`postsConnection { nodes }`), whose rows are filtered, ordered
    /// or paged; the answer is unwrapped to the plain list.
    connection: bool,
}

fn resolve_embed<'a>(
    catalog: &'a DatabaseCatalog,
    table: &'a TableMetadata,
    name: &str,
    path: &str,
    embed: &'a BTreeMap<String, EmbedControls>,
) -> Result<Embed<'a>, ProblemDetails> {
    use query::EmbedKind;
    let found = query::find_embeddable(catalog, table, name).ok_or_else(|| {
        ProblemDetails::invalid(
            format!("Unknown relation `{name}` for resource `{}`", table.name),
            "include",
        )
    })?;
    let controls = embed.get(path);
    let paged = found.many && controls.is_some_and(EmbedControls::paged);
    // A relation is read as its connection when its rows are filtered, ordered or paged; a
    // function returning a set is a connection field already
    let connection = match found.kind {
        EmbedKind::Relation => paged,
        EmbedKind::Mounted => found.many,
        EmbedKind::Tree | EmbedKind::Arc => false,
    };
    if found.kind == EmbedKind::Relation
        && paged
        && !catalog
            .object_fields(&table.schema, &table.name)
            .iter()
            .any(|f| f.name == format!("{}Connection", found.name))
    {
        return Err(ProblemDetails::invalid(
            format!("`{path}` cannot be filtered, ordered or paged: it has no connection"),
            "include",
        ));
    }
    Ok(Embed {
        field: found.name,
        kind: found.kind,
        target: found.target,
        arc_targets: found.arc_targets,
        controls,
        connection,
    })
}

/// A computed field (a function of a row) selected by name: with the arguments the request
/// gave it (`args[field][argument]=value`), checked against the function. `None` for a field
/// that is not one, or returns rows of a table (that is `include`).
fn computed_field(
    catalog: &DatabaseCatalog,
    table: &TableMetadata,
    name: &str,
    path: &str,
    args: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<Option<RequestField>, ProblemDetails> {
    use crate::catalog::FunctionReturn;
    let Some(field) = catalog
        .object_fields(&table.schema, &table.name)
        .iter()
        .find(|f| f.name == name)
    else {
        return Ok(None);
    };
    let FieldKind::Function(i) = &field.kind else {
        return Ok(None);
    };
    let func = &catalog.functions[*i];
    if !catalog.function_is_exposed(func) || func.volatile {
        return Ok(None);
    }
    let children = match &func.returns {
        FunctionReturn::Value(pg_type) => value_selection(catalog, pg_type, 0),
        FunctionReturn::Record => func
            .return_columns
            .iter()
            .map(|c| {
                RequestField::new(crate::catalog::models::to_camel_case(&c.name))
                    .with_children(value_selection(catalog, &c.pg_type, 0))
            })
            .collect(),
        _ => return Ok(None),
    };
    let arguments = field_function_arguments(func, path, args)?;
    Ok(Some(
        RequestField::new(&field.name)
            .with_arguments(arguments)
            .with_children(children),
    ))
}

/// The arguments of a function field from `args[path][name]=value`: typed from the function,
/// and every one without a default is required.
fn field_function_arguments(
    func: &crate::catalog::FunctionMetadata,
    path: &str,
    args: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<HashMap<String, serde_json::Value>, ProblemDetails> {
    let given = args.get(path);
    let declared = query::field_arguments(func);
    let mut out = HashMap::new();
    for (name, text) in given.into_iter().flatten() {
        let argument = declared
            .iter()
            .find(|a| a.graphql_name() == *name)
            .ok_or_else(|| {
                let names: Vec<String> = declared.iter().map(|a| a.graphql_name()).collect();
                ProblemDetails::invalid(
                    format!(
                        "`{path}` has no argument `{name}`; it takes: {}",
                        if names.is_empty() {
                            "none".to_string()
                        } else {
                            names.join(", ")
                        }
                    ),
                    format!("args[{path}][{name}]"),
                )
            })?;
        out.insert(
            name.clone(),
            functions::parse_argument_text(text, argument)?,
        );
    }
    for argument in declared.iter().filter(|a| !a.has_default) {
        if !out.contains_key(&argument.graphql_name()) {
            return Err(ProblemDetails::invalid(
                format!(
                    "`{path}` needs the argument `{}`: `args[{path}][{}]=…`",
                    argument.graphql_name(),
                    argument.graphql_name()
                ),
                format!("args[{path}][{}]", argument.graphql_name()),
            ));
        }
    }
    Ok(out)
}

/// The columns of an embedded relation: `fields[path]`, else all. A name that is not a column
/// can be a computed field.
fn embedded_columns(
    catalog: &DatabaseCatalog,
    target: &TableMetadata,
    path: &str,
    fields: Option<&Vec<String>>,
    args: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<Vec<RequestField>, ProblemDetails> {
    let Some(requested) = fields.filter(|f| !f.is_empty()) else {
        return Ok(column_fields(catalog, target));
    };
    let mut out: Vec<RequestField> = Vec::new();
    for name in requested {
        let found = target
            .visible_columns()
            .find(|c| &c.field_name == name || &c.name == name);
        let field = match found {
            Some(col) => column_field(catalog, target, col),
            None => computed_field(catalog, target, name, &format!("{path}.{name}"), args)?
                .ok_or_else(|| {
                    ProblemDetails::invalid(
                        format!("Unknown field `{name}` for `{path}` ({})", target.name),
                        format!("fields[{path}]"),
                    )
                })?,
        };
        if !out.iter().any(|f| f.response_key() == field.response_key()) {
            out.push(field);
        }
    }
    Ok(out)
}

/// What an exclusive arc's `target` selects: `__typename` and, for each table it can lead to,
/// the requested columns it has (all, without `fields[…]`).
fn arc_fields(
    catalog: &DatabaseCatalog,
    targets: &[(String, &TableMetadata)],
    path: &str,
    fields: Option<&Vec<String>>,
) -> Result<Vec<RequestField>, ProblemDetails> {
    let mut out = vec![RequestField::new("__typename")];
    let requested = fields.filter(|f| !f.is_empty());
    if let Some(requested) = requested {
        if let Some(unknown) = requested.iter().find(|name| {
            !targets.iter().any(|(_, t)| {
                t.visible_columns()
                    .any(|c| &c.field_name == *name || &c.name == *name)
            })
        }) {
            return Err(ProblemDetails::invalid(
                format!("No table `{path}` leads to has a field `{unknown}`"),
                format!("fields[{path}]"),
            ));
        }
    }
    for (type_name, target) in targets {
        for col in target.visible_columns() {
            let wanted =
                requested.is_none_or(|r| r.iter().any(|n| *n == col.field_name || *n == col.name));
            if wanted {
                out.push(column_field(catalog, target, col).with_type_condition(type_name));
            }
        }
    }
    Ok(out)
}

fn include_fields(
    ctx: &Ctx,
    table: &TableMetadata,
    tree: &IncludeTree,
    query: &RestQuery,
    prefix: &str,
) -> Result<Vec<RequestField>, ProblemDetails> {
    use query::EmbedKind;
    let embed = &query.embed;
    let catalog = &ctx.catalog;
    let schema = ctx.state.current_schema();
    let mut out = Vec::new();
    for (name, sub) in &tree.0 {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let embedded = resolve_embed(catalog, table, name, &path, embed)?;
        let controls = embedded.controls.cloned().unwrap_or_default();

        // The arguments of the field: the controls, and a function's own
        let mut arguments: HashMap<String, serde_json::Value> = HashMap::new();
        if embedded.kind == EmbedKind::Mounted {
            let func = catalog
                .object_fields(&table.schema, &table.name)
                .iter()
                .find_map(|f| match &f.kind {
                    FieldKind::Function(i) if f.name == embedded.field => {
                        Some(&catalog.functions[*i])
                    }
                    _ => None,
                })
                .ok_or_else(|| ProblemDetails::bad_request("Unknown function field"))?;
            arguments = field_function_arguments(func, &path, &query.field_args)?;
        }
        let field_name = if embedded.connection && embedded.kind == EmbedKind::Relation {
            format!("{}Connection", embedded.field)
        } else {
            embedded.field.clone()
        };
        for (arg, value, label) in [
            ("filter", controls.filter.clone(), "filter"),
            ("orderBy", controls.order_by.clone(), "order"),
            (
                "first",
                controls.first.map(|n| serde_json::json!(n)),
                "first",
            ),
            (
                "offset",
                controls.offset.map(|n| serde_json::json!(n)),
                "offset",
            ),
            (
                "maxDepth",
                controls.depth.map(|n| serde_json::json!(n)),
                "depth",
            ),
            (
                "asOf",
                controls.as_of.clone().map(serde_json::Value::String),
                "asOf",
            ),
        ] {
            if let Some(value) = value {
                if !validate::field_has_argument(&schema, &table.type_name, &field_name, arg) {
                    return Err(ProblemDetails::invalid(
                        format!("`{label}[{path}]` does not apply to `{path}`"),
                        format!("{label}[{path}]"),
                    ));
                }
                validate::validate_field_argument(
                    &schema,
                    &table.type_name,
                    &field_name,
                    arg,
                    &value,
                    &format!("{label}[{path}]"),
                )?;
                arguments.insert(arg.to_string(), value);
            }
        }

        if embedded.kind == EmbedKind::Arc {
            let children = arc_fields(
                catalog,
                &embedded.arc_targets,
                &path,
                controls.fields.as_ref(),
            )?;
            out.push(RequestField::new(&embedded.field).with_children(children));
            continue;
        }
        let mut children = embedded_columns(
            catalog,
            embedded.target,
            &path,
            controls.fields.as_ref(),
            &query.field_args,
        )?;
        apply_json_paths(&mut children, embedded.target, &path, &query.json_paths);
        children.extend(include_fields(ctx, embedded.target, sub, query, &path)?);
        if !embedded.connection {
            out.push(
                RequestField::new(&embedded.field)
                    .with_arguments(arguments)
                    .with_children(children),
            );
            continue;
        }
        // Filtered, ordered or paged rows: the relation's connection, under the relation's name
        out.push(
            RequestField::new(field_name)
                .with_alias(&embedded.field)
                .with_arguments(arguments)
                .with_children(vec![RequestField::new("nodes").with_children(children)]),
        );
    }
    Ok(out)
}

/// Turns the `{"nodes": […]}` of the relations read as connections back into plain lists.
fn unwrap_embeds(
    catalog: &DatabaseCatalog,
    table: &TableMetadata,
    tree: &IncludeTree,
    embed: &BTreeMap<String, EmbedControls>,
    prefix: &str,
    row: &mut serde_json::Value,
) {
    let Some(object) = row.as_object_mut() else {
        return;
    };
    for (name, sub) in &tree.0 {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let Ok(embedded) = resolve_embed(catalog, table, name, &path, embed) else {
            continue;
        };
        let Some(value) = object.get_mut(&embedded.field) else {
            continue;
        };
        if embedded.connection {
            if let Some(nodes) = value.get_mut("nodes").map(serde_json::Value::take) {
                *value = nodes;
            }
        }
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    unwrap_embeds(catalog, embedded.target, sub, embed, &path, item);
                }
            }
            other => unwrap_embeds(catalog, embedded.target, sub, embed, &path, other),
        }
    }
}

/// [`unwrap_embeds`] for the rows of a read.
fn finish_rows(
    catalog: &DatabaseCatalog,
    table: &TableMetadata,
    query: &RestQuery,
    rows: &mut [serde_json::Value],
) {
    if query.include.as_ref().is_none_or(Vec::is_empty) {
        return;
    }
    let Ok(tree) = build_include_tree(query.include.as_deref().unwrap_or(&[])) else {
        return;
    };
    for row in rows {
        unwrap_embeds(catalog, table, &tree, &query.embed, "", row);
    }
}

/// Whether `name` is a computed column of `table` that `fields` can select: a function of the
/// row that returns a value or columns of its own.
pub(crate) fn computed_value_exists(
    catalog: &DatabaseCatalog,
    table: &TableMetadata,
    name: &str,
) -> bool {
    catalog
        .object_fields(&table.schema, &table.name)
        .iter()
        .any(|f| match &f.kind {
            FieldKind::Function(i) if f.name == name => {
                let func = &catalog.functions[*i];
                catalog.function_is_exposed(func)
                    && !func.volatile
                    && matches!(
                        func.returns,
                        crate::catalog::FunctionReturn::Value(_)
                            | crate::catalog::FunctionReturn::Record
                    )
            }
            _ => false,
        })
}

/// The fields of one row: the requested `fields` (default: all visible columns) and the
/// embedded `include` relations.
fn select_fields(
    ctx: &Ctx,
    table: &TableMetadata,
    query: &RestQuery,
) -> Result<Vec<RequestField>, ProblemDetails> {
    let (fields, include) = (query.fields.as_ref(), query.include.as_ref());
    let catalog = &*ctx.catalog;
    let cti = catalog.find_cti(&table.schema, &table.name);
    let mut out = match fields.filter(|f| !f.is_empty()) {
        None => {
            let mut selected = column_fields(catalog, table);
            if let Some(cti) = cti {
                if !selected.iter().any(|f| f.response_key() == "__typename") {
                    selected.push(RequestField::new("__typename"));
                }
                for child in &cti.children {
                    if let Some(child_table) = catalog.find_table(&child.schema, &child.table) {
                        for col in child_table.visible_columns() {
                            if !table
                                .visible_columns()
                                .any(|pc| pc.name == col.name || pc.field_name == col.field_name)
                            {
                                selected.push(
                                    column_field(catalog, child_table, col)
                                        .with_type_condition(&child.type_name),
                                );
                            }
                        }
                    }
                }
            }
            selected
        }
        Some(requested) => {
            let mut selected: Vec<RequestField> = Vec::new();
            if cti.is_some() && !selected.iter().any(|f| f.response_key() == "__typename") {
                selected.push(RequestField::new("__typename"));
            }
            for name in requested {
                if name == "__typename" {
                    if !selected.iter().any(|f| f.response_key() == "__typename") {
                        selected.push(RequestField::new("__typename"));
                    }
                    continue;
                }
                if let Some(col) = table
                    .visible_columns()
                    .find(|c| &c.field_name == name || &c.name == name)
                {
                    if !selected.iter().any(|f: &RequestField| {
                        f.response_key() == col.field_name && f.type_condition.is_none()
                    }) {
                        selected.push(column_field(catalog, table, col));
                    }
                } else if let Some(cti) = cti {
                    let mut found = false;
                    for child in &cti.children {
                        if let Some(child_table) = catalog.find_table(&child.schema, &child.table) {
                            if let Some(col) = child_table
                                .visible_columns()
                                .find(|c| &c.field_name == name || &c.name == name)
                            {
                                selected.push(
                                    column_field(catalog, child_table, col)
                                        .with_type_condition(&child.type_name),
                                );
                                found = true;
                            }
                        }
                    }
                    if !found {
                        return Err(ProblemDetails::invalid(
                            format!("Unknown field `{name}` for resource `{}`", table.name),
                            "fields",
                        ));
                    }
                } else if let Some(field) =
                    computed_field(catalog, table, name, name, &query.field_args)?
                {
                    if !selected
                        .iter()
                        .any(|f: &RequestField| f.response_key() == field.name)
                    {
                        selected.push(field);
                    }
                } else {
                    return Err(ProblemDetails::invalid(
                        format!("Unknown field `{name}` for resource `{}`", table.name),
                        "fields",
                    ));
                }
            }
            selected
        }
    };
    apply_json_paths(&mut out, table, "", &query.json_paths);
    if let Some(include) = include {
        out.extend(include_fields(
            ctx,
            table,
            &build_include_tree(include)?,
            query,
            "",
        )?);
    }
    Ok(out)
}

/// `path[column]=$.a.b`: the first item of a JSON path of a `json` / `jsonb` column, instead of
/// the whole value. `prefix` is the `include` path of the relation the columns belong to.
fn apply_json_paths(
    fields: &mut [RequestField],
    table: &TableMetadata,
    prefix: &str,
    paths: &BTreeMap<String, String>,
) {
    for field in fields.iter_mut() {
        let Some(col) = table
            .visible_columns()
            .find(|c| c.field_name == field.response_key())
        else {
            continue;
        };
        let key = |name: &str| {
            if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}.{name}")
            }
        };
        if let Some(path) = paths
            .get(&key(&col.field_name))
            .or_else(|| paths.get(&key(&col.name)))
        {
            field
                .arguments
                .insert("path".to_string(), serde_json::json!(path));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Request context and execution
// ---------------------------------------------------------------------------------------------

/// What every handler needs: the live catalog, the caller's identity and preferences.
struct Ctx {
    state: Arc<ServerState>,
    /// The schema of the path (`/rest/{schema}/…`); `None` for the unqualified `/rest/…`.
    schema: Option<String>,
    catalog: Arc<DatabaseCatalog>,
    auth: Option<AuthSessionContext>,
    prefs: Preferences,
    resource: String,
    is_prod: bool,
}

impl Ctx {
    async fn new(
        state: &Arc<ServerState>,
        headers: &HeaderMap,
        schema: Option<String>,
        resource: String,
    ) -> Result<Self, ProblemDetails> {
        if !state.openapi_enabled {
            return Err(ProblemDetails::not_found("The REST API is disabled"));
        }
        let auth = state
            .authenticate(headers)
            .await
            .map_err(ProblemDetails::from_auth_error)?;
        Ok(Self {
            catalog: state.current_catalog(),
            auth,
            prefs: parse_prefer_header(headers),
            is_prod: state.mask_errors,
            state: state.clone(),
            schema,
            resource,
        })
    }

    fn table(&self) -> Result<&TableMetadata, ProblemDetails> {
        let found = match &self.schema {
            Some(schema) => resolve_table_in(&self.catalog, schema, &self.resource),
            None => resolve_table(&self.catalog, &self.state.default_schema, &self.resource),
        };
        found.ok_or_else(|| {
            ProblemDetails::not_found(format!("Resource `{}` not found", self.resource))
        })
    }

    fn allow(&self, table: &TableMetadata, operation: Operation) -> Result<(), ProblemDetails> {
        check_table_permissions(table, self.state.config.as_ref(), operation)
    }

    /// Checks `value` (the argument `arg` of root field `field`) against the schema.
    fn validate(
        &self,
        root: Root,
        field: &str,
        arg: &str,
        value: &serde_json::Value,
        path: &str,
    ) -> Result<(), ProblemDetails> {
        validate_argument(&self.state.current_schema(), root, field, arg, value, path)
    }

    fn has_argument(&self, root: Root, field: &str, arg: &str) -> bool {
        validate::has_argument(&self.state.current_schema(), root, field, arg)
    }

    fn unavailable(&self, err: &dyn std::fmt::Display) -> ProblemDetails {
        ProblemDetails::internal(
            StatusCode::SERVICE_UNAVAILABLE,
            "DATABASE_UNAVAILABLE",
            "The database is unavailable",
            err,
            self.is_prod,
        )
    }

    /// Plans, compiles and runs `op`: reads on a replica when there is one, writes on the
    /// primary, each in a single statement or transaction.
    async fn execute(
        &self,
        schema: &str,
        op: RequestOperation,
        method: &str,
        write: bool,
    ) -> Result<serde_json::Value, ProblemDetails> {
        let planning = std::time::Instant::now();
        let plan = QueryPlanner::new(&self.catalog, schema)
            .with_limits(self.state.limits.plan_limits())
            .plan_request(&op)
            .map_err(|err| match err {
                crate::planner::PlannerError::Limit(limit) => ProblemDetails::from_limit(&limit),
                err => ProblemDetails::bad_request(format!("Planning error: {err}")),
            })?;

        let mut compiled = SqlCompiler::compile_operation(&plan);
        crate::telemetry::metrics().planning("rest", planning.elapsed().as_secs_f64());
        if self.state.sql_comments {
            compiled.tag(&[
                ("application", "graphpg"),
                ("rest_resource", &self.resource),
                ("rest_method", method),
            ]);
        }
        let (want_sql, want_explain) = (
            self.state.debug.sql || self.state.debug.explain,
            self.state.debug.explain,
        );
        if want_sql {
            let statements = crate::server::http::debug_sql(&compiled);
            let _ = DEBUG.try_with(|d| d.borrow_mut().sql.push(statements));
        }
        let compiled = Arc::new(compiled);
        let span = tracing::info_span!("rest.execute", rest.resource = %self.resource, rest.method = %method);
        let auth = self.auth.clone();

        let joined = if write {
            let client = self
                .state
                .pool
                .get()
                .await
                .map_err(|e| self.unavailable(&e))?;
            tokio::spawn(
                async move {
                    let mut plans = Vec::new();
                    let result = ExecutionEngine::run_with_auth(
                        &*client,
                        &compiled,
                        want_explain.then_some(&mut plans),
                        auth.as_ref(),
                    )
                    .await;
                    (result, plans)
                }
                .instrument(span),
            )
            .await
        } else {
            let (client, _) = self
                .state
                .read_pool(None)
                .await
                .map_err(|e| self.unavailable(&e))?;
            tokio::spawn(
                async move {
                    let mut plans = Vec::new();
                    let result = ExecutionEngine::run_with_auth(
                        &*client,
                        &compiled,
                        want_explain.then_some(&mut plans),
                        auth.as_ref(),
                    )
                    .await;
                    (result, plans)
                }
                .instrument(span),
            )
            .await
        };

        let (result, plans) = joined.map_err(|err| {
            ProblemDetails::internal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_SERVER_ERROR",
                "The operation failed",
                &err,
                self.is_prod,
            )
        })?;
        if !plans.is_empty() {
            let _ = DEBUG.try_with(|d| d.borrow_mut().explain.extend(plans));
        }
        result.map_err(|err| {
            ProblemDetails::from_execution_error(
                &err,
                &self.catalog,
                self.is_prod,
                &self.state.errors,
            )
        })
    }
}

/// The largest `X-Debug-SQL` / `X-Debug-Explain` header sent (see `respond`).
const MAX_DEBUG_HEADER_BYTES: usize = 8 * 1024;

tokio::task_local! {
    /// What `debug.sql` and `debug.explain` collect during a request: the statements and the
    /// plans, sent back in `X-Debug-SQL` and `X-Debug-Explain` headers.
    static DEBUG: std::cell::RefCell<DebugInfo>;
}

#[derive(Default)]
struct DebugInfo {
    sql: Vec<serde_json::Value>,
    explain: Vec<String>,
}

/// Turns a handler's result into a response, naming the request URL as the problem instance.
/// With `debug.sql` / `debug.explain` (dev) the statements and plans it ran are in headers.
async fn respond(instance: String, work: impl Future<Output = RestResult>) -> Response {
    let (result, debug) = DEBUG
        .scope(std::cell::RefCell::new(DebugInfo::default()), async {
            let result = work.await;
            (result, DEBUG.with(|d| std::mem::take(&mut *d.borrow_mut())))
        })
        .await;
    let mut response =
        result.unwrap_or_else(|problem| problem.with_instance(instance).into_response());
    // A header over 8 KiB is refused by common clients and proxies (Node's limit is 16 KiB
    // for all of them, Nginx's buffers 4 to 8 KiB): a larger value is served apart
    let encode = |value: &serde_json::Value| {
        let encoded = percent_encode(value.to_string().as_bytes(), URL_ENCODE_SET).to_string();
        if encoded.len() <= MAX_DEBUG_HEADER_BYTES {
            return encoded;
        }
        let id = super::debug::keep_oversized(value.clone());
        let pointer = serde_json::json!({
            "omitted": format!("{} bytes, too large for a header", encoded.len()),
            "at": format!("/debug/sql/{id}"),
        });
        percent_encode(pointer.to_string().as_bytes(), URL_ENCODE_SET).to_string()
    };
    if !debug.sql.is_empty() {
        set_header(
            response.headers_mut(),
            header::HeaderName::from_static("x-debug-sql"),
            &encode(&serde_json::Value::Array(debug.sql)),
        );
    }
    if !debug.explain.is_empty() {
        set_header(
            response.headers_mut(),
            header::HeaderName::from_static("x-debug-explain"),
            &encode(&serde_json::json!(debug.explain)),
        );
    }
    response
}

fn shaping_error(err: impl std::fmt::Display, is_prod: bool) -> ProblemDetails {
    ProblemDetails::internal(
        StatusCode::INTERNAL_SERVER_ERROR,
        "SHAPING_ERROR",
        "The response could not be shaped",
        &err,
        is_prod,
    )
}

/// The body of a write as JSON bytes, whatever it was sent as: JSON as it is, CSV as an array
/// of rows (create only), `multipart/form-data` as one object. Any other type is a `415`.
fn json_body(
    table: &TableMetadata,
    headers: &HeaderMap,
    body: axum::body::Bytes,
    csv: bool,
) -> Result<axum::body::Bytes, ProblemDetails> {
    let value = match media::content_type(headers).as_deref() {
        None | Some(media::JSON) => return Ok(body),
        Some(t) if t.ends_with("+json") => return Ok(body),
        Some(media::CSV) if csv => media::csv_to_rows(&body, table)?,
        Some(media::MULTIPART) => media::multipart_to_object(headers, &body, table)?,
        Some(other) => {
            return Err(ProblemDetails::unsupported_media_type(format!(
                "`{other}` is not accepted here: send {}",
                if csv {
                    "`application/json`, `text/csv` or `multipart/form-data`"
                } else {
                    "`application/json` or `multipart/form-data`"
                }
            )))
        }
    };
    Ok(serde_json::to_vec(&value)
        .map_err(|e| ProblemDetails::bad_request(e.to_string()))?
        .into())
}

fn parse_json_body(body: &[u8]) -> Result<serde_json::Value, ProblemDetails> {
    if body.is_empty() {
        return Err(ProblemDetails::bad_request("Request body cannot be empty"));
    }
    serde_json::from_slice(body)
        .map_err(|e| ProblemDetails::bad_request(format!("Invalid JSON request body: {e}")))
}

fn json_object(
    value: serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, ProblemDetails> {
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(ProblemDetails::bad_request(
            "Request body must be a JSON object",
        )),
    }
}

/// `If-Match`: one entity tag (weak or strong) naming the row version, or `*`.
fn parse_if_match(headers: &HeaderMap) -> Result<Option<String>, ProblemDetails> {
    let Some(raw) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let raw = raw
        .to_str()
        .map_err(|_| ProblemDetails::bad_request("Invalid `If-Match` header"))?
        .trim();
    if raw == "*" {
        return Ok(None);
    }
    if raw.contains(',') {
        return Err(ProblemDetails::bad_request(
            "`If-Match` accepts a single entity tag",
        ));
    }
    let tag = raw.strip_prefix("W/").unwrap_or(raw).trim_matches('"');
    Ok((!tag.is_empty()).then(|| tag.to_string()))
}

/// The row's entity tag (its version), removing a `rowVersion` the client did not select.
pub fn extract_etag(row: &mut serde_json::Value, table: &TableMetadata) -> Option<String> {
    let version = row
        .get("rowVersion")
        .and_then(|v| v.as_str().map(String::from))
        .or_else(|| {
            let column = table.version_column.as_ref()?;
            let v = row.get(column)?;
            Some(
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string()),
            )
        });
    if !table.columns.iter().any(|c| c.field_name == "rowVersion") {
        if let Some(obj) = row.as_object_mut() {
            obj.remove("rowVersion");
        }
    }
    version.map(|v| format!("\"{v}\""))
}

/// URL of a row: `/rest/<table>/<key>` (`/rest/<schema>/<table>/<key>` under a schema) with a composite key comma-separated.
fn location_of(
    schema: &Option<String>,
    table: &TableMetadata,
    row: &serde_json::Value,
) -> Option<String> {
    let parts = table
        .primary_key
        .iter()
        .map(|pk| {
            let col = table.get_column(pk)?;
            let value = row.get(&col.field_name)?;
            let text = value
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| value.to_string());
            Some(percent_encode(text.as_bytes(), URL_ENCODE_SET).to_string())
        })
        .collect::<Option<Vec<_>>>()?;
    let name = percent_encode(table.name.as_bytes(), URL_ENCODE_SET);
    Some(format!("{}/{name}/{}", rest_base(schema), parts.join(",")))
}

fn set_header(headers: &mut HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

/// What a single-row write returned.
struct Written {
    /// The row as it is now (`null` if no row matched).
    row: serde_json::Value,
    /// The row as it was before, when asked for with `Prefer: previous` and the database can
    /// say (PostgreSQL 18): `null` for a row an upsert inserted.
    previous: Option<serde_json::Value>,
}

impl From<serde_json::Value> for Written {
    fn from(row: serde_json::Value) -> Self {
        Self {
            row,
            previous: None,
        }
    }
}

/// A write response: the row (or no body for `return=minimal`) with `ETag` and `Location`;
/// with `Prefer: previous`, `{"row": …, "previous": …}`.
fn write_response(
    schema: &Option<String>,
    table: &TableMetadata,
    written: Written,
    status: StatusCode,
    prefs: Preferences,
    with_location: bool,
) -> Response {
    let Written { mut row, previous } = written;
    let mut headers = HeaderMap::new();
    if with_location {
        if let Some(location) = location_of(schema, table, &row) {
            set_header(&mut headers, header::LOCATION, &location);
        }
    }
    if let Some(etag) = extract_etag(&mut row, table) {
        set_header(&mut headers, header::ETAG, &etag);
    }
    if prefs.return_minimal {
        if let Some(applied) = prefs.applied(true, false, false) {
            set_header(
                &mut headers,
                header::HeaderName::from_static("preference-applied"),
                &applied,
            );
        }
        (StatusCode::NO_CONTENT, headers).into_response()
    } else if let Some(previous) = previous {
        if let Some(applied) = prefs.applied(false, false, true) {
            set_header(
                &mut headers,
                header::HeaderName::from_static("preference-applied"),
                &applied,
            );
        }
        let mut previous = previous;
        // The version of a row as it was is not stored any more
        if let Some(obj) = previous.as_object_mut() {
            obj.remove("rowVersion");
        }
        (
            status,
            headers,
            Json(serde_json::json!({ "row": row, "previous": previous })),
        )
            .into_response()
    } else {
        (status, headers, Json(row)).into_response()
    }
}

fn mutation_field(
    catalog: &DatabaseCatalog,
    table: &TableMetadata,
    action: MutationAction,
    what: &str,
) -> Result<MutationField, ProblemDetails> {
    mutation_field_by(catalog, table, action, what, &[])
}

/// The mutation field of `action` addressing rows by the unique key `unique` (column names; the
/// primary key when empty).
fn mutation_field_by(
    catalog: &DatabaseCatalog,
    table: &TableMetadata,
    action: MutationAction,
    what: &str,
    unique: &[String],
) -> Result<MutationField, ProblemDetails> {
    catalog
        .table_mutation_fields(table)
        .into_iter()
        .find(|m| {
            m.action == action
                && m.unique_key.len() == unique.len()
                && unique.iter().all(|u| m.unique_key.contains(u))
        })
        .ok_or_else(|| {
            ProblemDetails::bad_request(format!("{what} is not supported for `{}`", table.name))
        })
}

fn write_columns(catalog: &DatabaseCatalog, table: &TableMetadata) -> Vec<RequestField> {
    let mut columns = column_fields(catalog, table);
    if table.has_row_version() {
        columns.push(RequestField::new("rowVersion"));
    }
    columns
}

fn mutation_op(
    m: &MutationField,
    table: &TableMetadata,
    args: HashMap<String, serde_json::Value>,
    columns: Vec<RequestField>,
) -> RequestOperation {
    mutation_op_with(m, table, args, columns, None)
}

/// A mutation selecting the written rows and, when given, the payload field `previous` with the
/// rows as they were.
fn mutation_op_with(
    m: &MutationField,
    table: &TableMetadata,
    args: HashMap<String, serde_json::Value>,
    columns: Vec<RequestField>,
    previous: Option<(&str, Vec<RequestField>)>,
) -> RequestOperation {
    let payload = m.action.payload_field_name(table);
    let mut children = vec![RequestField::new(payload).with_children(columns)];
    if let Some((name, columns)) = previous {
        children.push(RequestField::new(name).with_children(columns));
    }
    RequestOperation::mutation(vec![RequestField::new(&m.name)
        .with_arguments(args)
        .with_children(children)])
}

/// Runs a single-row write. With `previous`, an update or upsert also reads the row as it was
/// (`RETURNING OLD`) when the database can.
async fn run_write(
    ctx: &Ctx,
    table: &TableMetadata,
    m: &MutationField,
    args: HashMap<String, serde_json::Value>,
    method: &str,
    previous: bool,
) -> Result<Written, ProblemDetails> {
    let previous_field = previous
        .then(|| {
            ctx.catalog
                .previous_field_name(table, m.action, table.has_upsert())
        })
        .flatten();
    let op = mutation_op_with(
        m,
        table,
        args,
        write_columns(&ctx.catalog, table),
        previous_field
            .as_deref()
            .map(|name| (name, column_fields(&ctx.catalog, table))),
    );
    let data = ctx.execute(&table.schema, op, method, true).await?;
    let payload = m.action.payload_field_name(table);
    let root =
        ResponseShaper::unwrap_single(&data, &m.name).map_err(|e| shaping_error(e, ctx.is_prod))?;
    Ok(Written {
        row: root.get(&payload).cloned().unwrap_or_default(),
        previous: previous_field.and_then(|name| root.get(&name).cloned()),
    })
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

struct Listing {
    rows: Vec<serde_json::Value>,
    total: Option<i64>,
    page_info: Option<RestPageInfo>,
    is_estimate: bool,
}

/// Where a list reads from: a table's own list or connection field, or a function returning
/// its rows (a connection) with the function's arguments.
struct ListTarget<'a> {
    table: &'a TableMetadata,
    root: &'a str,
    connection: bool,
    args: HashMap<String, serde_json::Value>,
}

impl<'a> ListTarget<'a> {
    /// A table's own root field: the connection when it has one, else the plain list.
    fn of_table(table: &'a TableMetadata) -> Result<Self, ProblemDetails> {
        let (root, connection) = if table.operations.connection {
            (&table.connection_field, true)
        } else if table.operations.list {
            (&table.list_field, false)
        } else {
            return Err(ProblemDetails::forbidden(format!(
                "Listing `{}` is not permitted",
                table.name
            )));
        };
        Ok(Self {
            table,
            root,
            connection,
            args: HashMap::new(),
        })
    }
}

async fn list(
    ctx: &Ctx,
    table: &TableMetadata,
    query: &RestQuery,
    method: &str,
) -> Result<Listing, ProblemDetails> {
    ctx.allow(table, Operation::Read)?;
    list_rows(ctx, ListTarget::of_table(table)?, query, method).await
}

/// Reads the rows through the connection's `edges` instead of its `nodes`, for what an edge
/// has: its cursor and the window values of its row.
fn edges_request(
    ctx: &Ctx,
    table: &TableMetadata,
    query: &RestQuery,
    root: &str,
    request: &mut RequestField,
) -> Result<(), ProblemDetails> {
    let schema = ctx.state.current_schema();
    let connection_type = validate::root_field_type_name(&schema, Root::Query, root);
    let edge_type = connection_type
        .as_deref()
        .and_then(|c| validate::field_type_name(&schema, c, "edges"));
    let has = |field: &str| {
        edge_type.as_deref().is_some_and(|t| {
            validate::object_field_names(&schema, t)
                .iter()
                .any(|f| f == field)
        })
    };
    let partition = query
        .partition
        .iter()
        .map(|g| aggregates::group_value(table, g).map(serde_json::Value::String))
        .collect::<Result<Vec<_>, _>>()?;
    let windowed = |mut field: RequestField| {
        if !partition.is_empty() {
            field = field.with_argument("partitionBy", serde_json::Value::Array(partition.clone()));
        }
        field
    };
    let Some(position) = request.children.iter().position(|c| c.name == "nodes") else {
        return Ok(());
    };
    let nodes = request.children.remove(position);
    let mut edge = Vec::new();
    if query.cursors {
        edge.push(RequestField::new("cursor"));
    }
    for name in &query.window {
        if !has(name) {
            return Err(ProblemDetails::invalid(
                format!("`{name}` is not available for `{}`", table.name),
                "window",
            ));
        }
        edge.push(windowed(RequestField::new(name)));
    }
    if !query.running.is_empty() {
        if !has("runningSum") {
            return Err(ProblemDetails::invalid(
                format!("`{}` has no numbers to sum", table.name),
                "running",
            ));
        }
        let columns = query
            .running
            .iter()
            .filter_map(|name| {
                table
                    .visible_columns()
                    .find(|c| &c.field_name == name || &c.name == name)
            })
            .map(|c| RequestField::new(&c.field_name))
            .collect();
        edge.push(windowed(
            RequestField::new("runningSum").with_children(columns),
        ));
    }
    edge.push(RequestField::new("node").with_children(nodes.children));
    request
        .children
        .insert(position, RequestField::new("edges").with_children(edge));
    Ok(())
}

/// Turns the `edges` of an answer into `nodes`: each row with its `_cursor` and `_window`.
fn rows_from_edges(data: &mut serde_json::Value, root: &str, query: &RestQuery) {
    let connection = match data.get_mut("data") {
        Some(inner) => inner.get_mut(root),
        None => data.get_mut(root),
    };
    let Some(connection) = connection.and_then(serde_json::Value::as_object_mut) else {
        return;
    };
    let Some(serde_json::Value::Array(edges)) = connection.remove("edges") else {
        return;
    };
    let rows: Vec<serde_json::Value> = edges
        .into_iter()
        .map(|mut edge| {
            let mut row = edge
                .get_mut("node")
                .map(serde_json::Value::take)
                .unwrap_or_default();
            if let Some(object) = row.as_object_mut() {
                if query.cursors {
                    object.insert(
                        "_cursor".into(),
                        edge.get("cursor").cloned().unwrap_or_default(),
                    );
                }
                let mut window = serde_json::Map::new();
                for name in query
                    .window
                    .iter()
                    .map(String::as_str)
                    .chain((!query.running.is_empty()).then_some("runningSum"))
                {
                    window.insert(
                        name.to_string(),
                        edge.get(name).cloned().unwrap_or_default(),
                    );
                }
                if !window.is_empty() {
                    object.insert("_window".into(), serde_json::Value::Object(window));
                }
            }
            row
        })
        .collect();
    connection.insert("nodes".into(), serde_json::Value::Array(rows));
}

async fn list_rows(
    ctx: &Ctx,
    target: ListTarget<'_>,
    query: &RestQuery,
    method: &str,
) -> Result<Listing, ProblemDetails> {
    let ListTarget {
        table,
        root,
        connection,
        args: initial_args,
    } = target;
    let mut args = initial_args.clone();
    let row_fields = select_fields(ctx, table, query)?;

    if let Some(filter) = &query.filter {
        ctx.validate(Root::Query, root, "filter", filter, "filter")?;
        args.insert("filter".into(), filter.clone());
    }
    if let Some(order) = &query.order_by {
        ctx.validate(Root::Query, root, "orderBy", order, "order")?;
        args.insert("orderBy".into(), order.clone());
    }
    for (name, value) in [
        ("first", query.first.map(|n| serde_json::json!(n))),
        ("last", query.last.map(|n| serde_json::json!(n))),
        ("after", query.after.clone().map(|s| serde_json::json!(s))),
        ("before", query.before.clone().map(|s| serde_json::json!(s))),
        ("offset", query.offset.map(|n| serde_json::json!(n))),
    ] {
        if let Some(value) = value {
            args.insert(name.into(), value);
        }
    }

    if let Some(as_of) = &query.as_of {
        if !ctx.has_argument(Root::Query, root, "asOf") {
            return Err(ProblemDetails::invalid(
                format!(
                    "`{}` has no temporal key: `asOf` does not apply",
                    table.name
                ),
                "asOf",
            ));
        }
        let value = serde_json::Value::String(as_of.clone());
        ctx.validate(Root::Query, root, "asOf", &value, "asOf")?;
        args.insert("asOf".into(), value);
    }

    let count_pref = ctx.prefs.count;
    let (include_total, estimate_total) = match count_pref {
        query::CountPreference::Planned => (true, true),
        query::CountPreference::Estimated => (true, true),
        query::CountPreference::Exact => (true, false),
        query::CountPreference::None => (false, false),
    };

    let edges = query.cursors || !query.window.is_empty() || !query.running.is_empty();
    if edges && !connection {
        return Err(ProblemDetails::bad_request(
            "`cursors`, `window` and `running` are for lists that are connections",
        ));
    }
    let mut request = if connection {
        ResponseShaper::build_list_request_ext(
            root,
            args.clone(),
            row_fields,
            true,
            include_total,
            estimate_total,
        )
    } else {
        RequestField::new(root)
            .with_arguments(args.clone())
            .with_children(row_fields)
    };
    if edges {
        edges_request(ctx, table, query, root, &mut request)?;
    }
    let mut data = ctx
        .execute(
            &table.schema,
            RequestOperation::query(vec![request]),
            method,
            false,
        )
        .await?;
    if edges {
        rows_from_edges(&mut data, root, query);
    }
    let mut shaped =
        ResponseShaper::unwrap_list(&data, root).map_err(|e| shaping_error(e, ctx.is_prod))?;
    finish_rows(&ctx.catalog, table, query, &mut shaped.rows);

    let mut total = shaped.total_count;
    let mut is_estimate = false;

    if include_total && connection {
        match count_pref {
            query::CountPreference::Planned => {
                is_estimate = true;
            }
            query::CountPreference::Exact => {
                is_estimate = false;
            }
            query::CountPreference::Estimated => {
                let has_next = shaped
                    .page_info
                    .as_ref()
                    .map(|p| p.has_next_page)
                    .unwrap_or(false);
                let is_start = query.offset.unwrap_or(0) == 0
                    && query.after.is_none()
                    && query.before.is_none();
                if !has_next && is_start {
                    total = Some(shaped.rows.len() as i64);
                    is_estimate = false;
                } else {
                    let est = total.unwrap_or(0);
                    let threshold = ctx.state.limits.count_threshold.unwrap_or(10_000) as i64;
                    if est <= threshold {
                        let mut count_args = initial_args;
                        if let Some(filter) = &query.filter {
                            count_args.insert("filter".into(), filter.clone());
                        }
                        let count_req = ResponseShaper::build_list_request_ext(
                            root,
                            count_args,
                            vec![],
                            false,
                            true,
                            false,
                        );
                        let count_data = ctx
                            .execute(
                                &table.schema,
                                RequestOperation::query(vec![count_req]),
                                method,
                                false,
                            )
                            .await?;
                        let count_shaped = ResponseShaper::unwrap_list(&count_data, root)
                            .map_err(|e| shaping_error(e, ctx.is_prod))?;
                        total = count_shaped.total_count;
                        is_estimate = false;
                    } else {
                        is_estimate = true;
                    }
                }
            }
            query::CountPreference::None => {
                is_estimate = false;
            }
        }
    }

    Ok(Listing {
        rows: shaped.rows,
        total,
        page_info: shaped.page_info,
        is_estimate,
    })
}

fn list_headers(listing: &Listing, prefs: Preferences) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(total) = listing.total.filter(|_| prefs.count_requested()) {
        set_header(
            &mut headers,
            header::HeaderName::from_static("x-total-count"),
            &total.to_string(),
        );
        if listing.is_estimate {
            set_header(
                &mut headers,
                header::HeaderName::from_static("x-total-count-estimate"),
                "true",
            );
        }
        if let Some(applied) = prefs.applied(false, true, false) {
            set_header(
                &mut headers,
                header::HeaderName::from_static("preference-applied"),
                &applied,
            );
        }
    }
    headers
}

/// The response of a list: the rows with the count (if asked for) and either a `Link` header
/// (a `GET` can be continued from its URL) or cursor headers (a search or function call with
/// a body cannot: the cursors are sent back as `after` / `before`).
fn listing_response(
    listing: Listing,
    prefs: Preferences,
    link: Option<String>,
    cursor_headers: bool,
    media: Media,
) -> Response {
    let mut out = list_headers(&listing, prefs);
    if let Some(link) = link {
        set_header(&mut out, header::LINK, &link);
    }
    if cursor_headers {
        if let Some(pi) = &listing.page_info {
            if let (true, Some(c)) = (pi.has_next_page, &pi.end_cursor) {
                set_header(
                    &mut out,
                    header::HeaderName::from_static("x-next-cursor"),
                    c,
                );
            }
            if let (true, Some(c)) = (pi.has_previous_page, &pi.start_cursor) {
                set_header(
                    &mut out,
                    header::HeaderName::from_static("x-previous-cursor"),
                    c,
                );
            }
        }
    }
    if media == Media::Ndjson {
        set_header(&mut out, header::CONTENT_TYPE, "application/x-ndjson");
        let mut lines = String::new();
        for row in &listing.rows {
            lines.push_str(&row.to_string());
            lines.push('\n');
        }
        return (StatusCode::OK, out, lines).into_response();
    }
    if media == Media::Csv {
        set_header(&mut out, header::CONTENT_TYPE, "text/csv; charset=utf-8");
        return (StatusCode::OK, out, media::rows_to_csv(&listing.rows, None)).into_response();
    }
    (StatusCode::OK, out, Json(listing.rows)).into_response()
}

/// RFC 8288 `Link` headers (`rel="next"` / `rel="prev"`) for a GET list: the request's own
/// parameters with the cursor replaced.
fn build_link_header(
    resource: &str,
    raw_query: Option<&str>,
    page_info: Option<&RestPageInfo>,
    query: &RestQuery,
) -> Option<String> {
    let pi = page_info?;
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(&s.replace('+', " "))
            .decode_utf8_lossy()
            .to_string()
    };
    let base: Vec<(&str, &str)> = raw_query
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .filter(|(k, _)| !matches!(decode(k).as_str(), "after" | "before"))
        .collect();
    let has = |names: &[&str]| {
        base.iter()
            .any(|(k, _)| names.contains(&decode(k).as_str()))
    };
    let render = |mut params: Vec<(String, String)>| {
        params.splice(
            0..0,
            base.iter().map(|(k, v)| (k.to_string(), v.to_string())),
        );
        let qs: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!("<{REST_PREFIX}/{resource}?{}>", qs.join("&"))
    };
    let enc = |c: &str| percent_encode(c.as_bytes(), URL_ENCODE_SET).to_string();

    let mut links = Vec::new();
    if let (true, Some(cursor)) = (pi.has_next_page, &pi.end_cursor) {
        let mut params = vec![("after".to_string(), enc(cursor))];
        if let (false, Some(first)) = (has(&["first"]), query.first) {
            params.push(("first".into(), first.to_string()));
        }
        links.push(format!("{}; rel=\"next\"", render(params)));
    }
    if let (true, Some(cursor)) = (pi.has_previous_page, &pi.start_cursor) {
        let mut params = vec![("before".to_string(), enc(cursor))];
        if !has(&["last"]) {
            if let Some(n) = query.last.or(query.first) {
                params.push(("last".into(), n.to_string()));
            }
        }
        links.push(format!("{}; rel=\"prev\"", render(params)));
    }
    (!links.is_empty()).then(|| links.join(", "))
}

/// Handler for `GET /rest/node/{id}`: GraphQL's `node(id:)`. A global ID names a table and a key,
/// so the answer is a redirect to the row's own URL (`303 See Other`, query string kept).
pub async fn rest_node_handler(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Response {
    respond(format!("{REST_PREFIX}/{NODE_RESOURCE}/{id}"), async {
        let ctx = Ctx::new(&state, &headers, None, NODE_RESOURCE.to_string()).await?;
        let not_found = || ProblemDetails::not_found(format!("No row has the global ID `{id}`"));
        let (type_name, key) =
            crate::schema::SchemaBuilder::decode_global_id(&id).ok_or_else(not_found)?;
        let (schema, name) = ctx
            .catalog
            .table_for_type_any(&type_name)
            .ok_or_else(not_found)?;
        let table = ctx.catalog.find_table(schema, name).ok_or_else(not_found)?;
        ctx.allow(table, Operation::Read)?;
        // The unqualified path when it names this table, else the path of its schema
        let owner = resolve_table(&ctx.catalog, &ctx.state.default_schema, &table.name)
            .is_some_and(|t| std::ptr::eq(t, table));
        let base = rest_base(&(!owner).then(|| table.schema.clone()));
        let encode = |s: &str| percent_encode(s.as_bytes(), URL_ENCODE_SET).to_string();
        let mut location = format!("{base}/{}/{}", encode(&table.name), encode(&key));
        if let Some(query) = raw_query.filter(|q| !q.is_empty()) {
            location.push('?');
            location.push_str(&query);
        }
        let mut out = HeaderMap::new();
        set_header(&mut out, header::LOCATION, &location);
        Ok((StatusCode::SEE_OTHER, out).into_response())
    })
    .await
}

/// Handler for `GET /rest/{resource}`.
pub async fn rest_list_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, resource): Scoped<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Response {
    respond(format!("{}/{resource}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let media = media::negotiate(&headers)?;
        let table = ctx.table()?;
        let query = parse_query_params(
            raw_query.as_deref().unwrap_or(""),
            QueryKind::Collection,
            table,
            &ctx.catalog,
        )
        .map_err(ProblemDetails::bad_request)?;
        // `Range: items=A-B` is the offset and size of the page
        let mut query = query;
        let range = media::parse_range(&headers)?;
        if let Some(range) = range {
            if query.first.is_some()
                || query.last.is_some()
                || query.after.is_some()
                || query.before.is_some()
                || query.offset.is_some()
            {
                return Err(ProblemDetails::bad_request(
                    "`Range` cannot be combined with `first`, `last`, `after`, `before` or `offset`",
                ));
            }
            query.offset = Some(range.first);
            if let Some(last) = range.last {
                // A page larger than the server allows is cut short: the `Content-Range` of the
                // answer says which rows came
                let size = last - range.first + 1;
                query.first = Some(match ctx.state.limits.page_size.max {
                    Some(max) => size.min(max),
                    None => size,
                });
            }
        }
        let mut listing = list(&ctx, table, &query, "GET").await?;
        if let Some(range) = range {
            if listing.rows.is_empty() && range.first > 0 {
                // Past the end: the answer names the size of the whole
                let mut counting =
                    Ctx::new(&state, &headers, schema.clone(), table.name.clone()).await?;
                counting.prefs.count = query::CountPreference::Exact;
                let first_row = RestQuery {
                    first: Some(1),
                    offset: None,
                    ..query.clone()
                };
                let total = list(&counting, table, &first_row, "GET").await?.total;
                return Err(ProblemDetails::range_not_satisfiable(
                    format!("Row {} is past the end of the list", range.first),
                    total,
                ));
            }
            // Only a requested count is known: `Content-Range` has `*` otherwise
            listing.total = listing.total.filter(|_| ctx.prefs.count_requested());
        }

        let link = build_link_header(
            &scoped(&ctx.schema, &table.name),
            raw_query.as_deref(),
            listing.page_info.as_ref(),
            &query,
        );
        let (first, count, total) = (
            query.offset.unwrap_or(0),
            listing.rows.len(),
            listing.total,
        );
        let mut response = listing_response(listing, ctx.prefs, link, false, media);
        let headers = response.headers_mut();
        headers.insert(
            header::ACCEPT_RANGES,
            HeaderValue::from_static(media::RANGE_UNIT),
        );
        if range.is_some() && count > 0 {
            let size = total.map_or_else(|| "*".to_string(), |n| n.to_string());
            if let Ok(value) = HeaderValue::from_str(&format!(
                "{} {first}-{}/{size}",
                media::RANGE_UNIT,
                first + count - 1
            )) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            // Everything there is, is not partial
            let whole = first == 0 && total.is_some_and(|n| n as usize == count);
            if !whole {
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            }
        }
        Ok(response)
    })
    .await
}

/// The JSON body of `POST /rest/{resource}/search`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchBody {
    filter: Option<serde_json::Value>,
    order: Option<OrderSpec>,
    first: Option<usize>,
    last: Option<usize>,
    after: Option<String>,
    before: Option<String>,
    offset: Option<usize>,
    fields: Option<Vec<String>>,
    include: Option<Vec<String>>,
    /// Controls of the embedded relations, by `include` path.
    #[serde(default)]
    embed: BTreeMap<String, EmbedBody>,
    #[serde(default)]
    inner: Vec<String>,
    #[serde(default)]
    anti: Vec<String>,
}

/// The controls of one embedded relation in a search body.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbedBody {
    fields: Option<Vec<String>>,
    filter: Option<serde_json::Value>,
    order: Option<OrderSpec>,
    first: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OrderSpec {
    One(String),
    Many(Vec<String>),
}

impl SearchBody {
    fn into_query(
        self,
        table: &TableMetadata,
        catalog: &DatabaseCatalog,
    ) -> Result<RestQuery, ProblemDetails> {
        let mut query = RestQuery {
            first: self.first,
            last: self.last,
            after: self.after,
            before: self.before,
            offset: self.offset,
            fields: self.fields,
            include: self.include,
            ..Default::default()
        };
        for (name, n) in [("first", query.first), ("last", query.last)] {
            if let Some(n) = n {
                query::check_page_size(name, n).map_err(ProblemDetails::bad_request)?;
            }
        }
        if let Some(filter) = self.filter {
            if !filter.is_object() {
                return Err(ProblemDetails::invalid(
                    "`filter` must be an object",
                    "filter",
                ));
            }
            query.filter = Some(normalize_filter_json(filter));
        }
        if let Some(order) = self.order {
            let items = match order {
                OrderSpec::One(s) => vec![s],
                OrderSpec::Many(v) => v,
            };
            let mut parsed = Vec::new();
            for item in items {
                match parse_order_string(&item, table, catalog)
                    .map_err(ProblemDetails::bad_request)?
                {
                    serde_json::Value::Array(values) => parsed.extend(values),
                    other => parsed.push(other),
                }
            }
            query.order_by = Some(serde_json::Value::Array(parsed));
        }
        query.inner = self.inner;
        query.anti = self.anti;
        for (path, controls) in self.embed {
            let target = query::walk_relation(catalog, table, &path)
                .map_err(|e| ProblemDetails::invalid(e, format!("embed.{path}")))?
                .target;
            let mut embedded = EmbedControls {
                fields: controls.fields,
                first: controls.first,
                offset: controls.offset,
                ..Default::default()
            };
            if let Some(n) = embedded.first {
                query::check_page_size("first", n).map_err(ProblemDetails::bad_request)?;
            }
            if let Some(filter) = controls.filter {
                if !filter.is_object() {
                    return Err(ProblemDetails::invalid(
                        "`filter` must be an object",
                        format!("embed.{path}.filter"),
                    ));
                }
                embedded.filter = Some(normalize_filter_json(filter));
            }
            if let Some(order) = controls.order {
                let items = match order {
                    OrderSpec::One(s) => vec![s],
                    OrderSpec::Many(v) => v,
                };
                let mut parsed = Vec::new();
                for item in items {
                    if let serde_json::Value::Array(values) =
                        parse_order_string(&item, target, catalog)
                            .map_err(ProblemDetails::bad_request)?
                    {
                        parsed.extend(values);
                    }
                }
                embedded.order_by = Some(serde_json::Value::Array(parsed));
            }
            query.embed.insert(path, embedded);
        }
        query::finish_embeds(&mut query, table, catalog).map_err(ProblemDetails::bad_request)?;
        Ok(query)
    }
}

/// Handler for `POST /rest/{resource}/search`: the same read as the list, with the filter and
/// pagination in a JSON body (`and` / `or` / `not`, relation filters).
pub async fn rest_search_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, resource): Scoped<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{}/{resource}/search", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let media = media::negotiate(&headers)?;
        let table = ctx.table()?;
        if raw_query.as_deref().is_some_and(|q| !q.is_empty()) {
            return Err(ProblemDetails::bad_request(
                "Search takes its parameters in the JSON body, not the query string",
            ));
        }
        let body: SearchBody = if body.is_empty() {
            SearchBody::default()
        } else {
            serde_json::from_slice(&body)
                .map_err(|e| ProblemDetails::bad_request(format!("Invalid search body: {e}")))?
        };
        let query = body.into_query(table, &ctx.catalog)?;
        let listing = list(&ctx, table, &query, "POST").await?;

        Ok(listing_response(listing, ctx.prefs, None, true, media))
    })
    .await
}

/// Handler for `GET /rest/{resource}/{key}`.
pub async fn rest_item_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, (resource, key)): Scoped<(String, String)>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Response {
    respond(format!("{}/{resource}/{key}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let table = ctx.table()?;
        ctx.allow(table, Operation::Read)?;
        let (by, raw_query) = take_by(raw_query.as_deref())?;
        let address = RowAddress::of(table, &key, by.as_deref())?;
        let query = parse_query_params(&raw_query, QueryKind::Item, table, &ctx.catalog)
            .map_err(ProblemDetails::bad_request)?;

        let mut fields = select_fields(&ctx, table, &query)?;
        if table.has_row_version() {
            fields.push(RequestField::new("rowVersion"));
        }
        let root = address.read_root(table);
        let mut arguments = address.args.clone();
        if let Some(as_of) = &query.as_of {
            if !ctx.has_argument(Root::Query, &root, "asOf") {
                return Err(ProblemDetails::invalid(
                    format!(
                        "`{}` has no temporal key: `asOf` does not apply",
                        table.name
                    ),
                    "asOf",
                ));
            }
            let value = serde_json::Value::String(as_of.clone());
            ctx.validate(Root::Query, &root, "asOf", &value, "asOf")?;
            arguments.insert("asOf".into(), value);
        }
        let request = RequestField::new(&root)
            .with_arguments(arguments)
            .with_children(fields);
        let data = ctx
            .execute(
                &table.schema,
                RequestOperation::query(vec![request]),
                "GET",
                false,
            )
            .await?;
        let mut row = ResponseShaper::unwrap_single(&data, &root)
            .map_err(|e| shaping_error(e, ctx.is_prod))?;
        finish_rows(&ctx.catalog, table, &query, std::slice::from_mut(&mut row));
        if row.is_null() {
            return Err(ProblemDetails::not_found(format!(
                "No `{}` found with key `{key}`",
                table.name
            )));
        }

        let mut out = HeaderMap::new();
        if let Some(etag) = extract_etag(&mut row, table) {
            set_header(&mut out, header::ETAG, &etag);
            if let Some(inm) = headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
            {
                let matches_etag = inm.trim() == "*"
                    || inm.split(',').any(|part| {
                        let part = part.trim();
                        let unquoted_part =
                            part.strip_prefix("W/").unwrap_or(part).trim_matches('"');
                        let unquoted_etag =
                            etag.strip_prefix("W/").unwrap_or(&etag).trim_matches('"');
                        part == etag || unquoted_part == unquoted_etag
                    });
                if matches_etag {
                    return Ok((StatusCode::NOT_MODIFIED, out).into_response());
                }
            }
        }
        Ok((StatusCode::OK, out, Json(row)).into_response())
    })
    .await
}

/// The `bytea` column a binary URL names, by field or column name.
fn binary_column<'a>(
    table: &'a TableMetadata,
    name: &str,
) -> Result<&'a ColumnMetadata, ProblemDetails> {
    table
        .columns
        .iter()
        .find(|c| !c.omit && (c.field_name == name || c.name == name))
        .ok_or_else(|| {
            ProblemDetails::not_found(format!("`{}` has no column `{name}`", table.name))
        })
        .and_then(|c| match c.pg_type {
            PgType::Bytea => Ok(c),
            _ => Err(ProblemDetails::not_found(format!(
                "`{name}` is not a binary (`bytea`) column: read it from `/rest/{}/{{key}}`",
                table.name
            ))),
        })
}

/// Handler for `GET /rest/{resource}/{key}/{column}`: the bytes of a `bytea` column, with a
/// media type recognized from their first bytes.
pub async fn rest_column_get_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, (resource, key, column)): Scoped<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    use base64::Engine;
    respond(
        format!("{}/{resource}/{key}/{column}", rest_base(&schema)),
        async {
            let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
            let table = ctx.table()?;
            ctx.allow(table, Operation::Read)?;
            let column = binary_column(table, &column)?;
            let parsed_key = parse_primary_key(&key, table).map_err(ProblemDetails::bad_request)?;
            let request = RequestField::new(&table.single_field)
                .with_arguments(key_args(table, &parsed_key))
                .with_children(vec![RequestField::new(&column.field_name)]);
            let data = ctx
                .execute(
                    &table.schema,
                    RequestOperation::query(vec![request]),
                    "GET",
                    false,
                )
                .await?;
            let row = ResponseShaper::unwrap_single(&data, &table.single_field)
                .map_err(|e| shaping_error(e, ctx.is_prod))?;
            let encoded = row
                .get(&column.field_name)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ProblemDetails::not_found(format!(
                        "`{}` of `{}` `{key}` has no value",
                        column.field_name, table.name
                    ))
                })?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|e| shaping_error(e, ctx.is_prod))?;
            let content_type = media::sniff(&bytes);
            if !media::accepts(&headers, content_type)
                && !media::accepts(&headers, media::OCTET_STREAM)
            {
                return Err(ProblemDetails::not_acceptable(format!(
                    "This value is `{content_type}`"
                )));
            }
            let mut out = HeaderMap::new();
            set_header(&mut out, header::CONTENT_TYPE, content_type);
            set_header(
                &mut out,
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            );
            Ok((StatusCode::OK, out, bytes).into_response())
        },
    )
    .await
}

/// Handler for `PUT /rest/{resource}/{key}/{column}`: the body, whatever its media type, is
/// the new value of a `bytea` column.
pub async fn rest_column_put_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, (resource, key, column)): Scoped<(String, String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    use base64::Engine;
    respond(
        format!("{}/{resource}/{key}/{column}", rest_base(&schema)),
        async {
            let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
            let table = ctx.table()?;
            let column = binary_column(table, &column)?;
            let input = serde_json::json!({
                column.field_name.clone(): base64::engine::general_purpose::STANDARD.encode(&body)
            });
            let if_match = parse_if_match(&headers)?;
            let address = RowAddress::primary(table, &key)?;
            update_row(
                &ctx,
                table,
                &address,
                input.to_string().as_bytes(),
                if_match,
                "PUT",
            )
            .await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        },
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// Handler for `POST /rest/{resource}`: create one row (object body) or many (array body).
/// Nested objects in the body are nested writes, in the shape of the GraphQL input.
pub async fn rest_create_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, resource): Scoped<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{}/{resource}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let table = ctx.table()?;
        ctx.allow(table, Operation::Create)?;
        let body = parse_json_body(&json_body(table, &headers, body, true)?)?;

        match body {
            rows @ serde_json::Value::Array(_) => {
                ctx.allow(table, Operation::CreateMany)?;
                let mut rows = collection::bulk_rows(&ctx, rows)?;
                collection::fill_missing(
                    &mut rows,
                    collection::create_missing(raw_query.as_deref())?,
                );
                let m = mutation_field(
                    &ctx.catalog,
                    table,
                    MutationAction::CreateMany,
                    "Bulk creation",
                )?;
                let inputs = serde_json::Value::Array(rows);
                ctx.validate(Root::Mutation, &m.name, "inputs", &inputs, "body")?;
                let args = HashMap::from([("inputs".to_string(), inputs)]);
                let data = ctx
                    .execute(
                        &table.schema,
                        mutation_op(&m, table, args, column_fields(&ctx.catalog, table)),
                        "POST",
                        true,
                    )
                    .await?;
                let rows = ResponseShaper::unwrap_mutation(
                    &data,
                    &m.name,
                    Some(&m.action.payload_field_name(table)),
                )
                .map_err(|e| shaping_error(e, ctx.is_prod))?;
                if ctx.prefs.return_minimal {
                    let mut out = HeaderMap::new();
                    if let Some(applied) = ctx.prefs.applied(true, false, false) {
                        set_header(
                            &mut out,
                            header::HeaderName::from_static("preference-applied"),
                            &applied,
                        );
                    }
                    return Ok((StatusCode::NO_CONTENT, out).into_response());
                }
                Ok((StatusCode::CREATED, Json(rows)).into_response())
            }
            serde_json::Value::Object(_) => {
                let m = mutation_field(&ctx.catalog, table, MutationAction::Create, "Creation")?;
                ctx.validate(Root::Mutation, &m.name, "input", &body, "body")?;
                let args = HashMap::from([("input".to_string(), body)]);
                let written = run_write(&ctx, table, &m, args, "POST", false).await?;
                Ok(write_response(
                    &ctx.schema,
                    table,
                    written,
                    StatusCode::CREATED,
                    ctx.prefs,
                    true,
                ))
            }
            _ => Err(ProblemDetails::bad_request(
                "Request body must be a JSON object or an array of objects",
            )),
        }
    })
    .await
}

/// Updates the row at `key` with the members of `body` (`PATCH`, and `PUT` with `If-Match`).
async fn update_row(
    ctx: &Ctx,
    table: &TableMetadata,
    address: &RowAddress,
    body: &[u8],
    if_match: Option<String>,
    method: &str,
) -> RestResult {
    ctx.allow(table, Operation::Update)?;
    let mut input = json_object(parse_json_body(body)?)?;
    if let Some(primary) = &address.primary {
        strip_key_from_body(table, primary, &mut input)?;
    }
    if input.is_empty() {
        return Err(ProblemDetails::bad_request(
            "Request body has no fields to update",
        ));
    }

    let m = mutation_field_by(
        &ctx.catalog,
        table,
        MutationAction::Update,
        "Updating by this key",
        &address.unique,
    )?;
    let input = serde_json::Value::Object(input);
    ctx.validate(Root::Mutation, &m.name, "input", &input, "body")?;

    let mut args = address.args.clone();
    args.insert("input".into(), input);
    if let Some(version) = if_match {
        if !ctx.has_argument(Root::Mutation, &m.name, "expectedVersion") {
            return Err(ProblemDetails::bad_request(format!(
                "`{}` has no row version, so `If-Match` cannot be used",
                table.name
            )));
        }
        args.insert("expectedVersion".into(), serde_json::Value::String(version));
    }

    let written = run_write(ctx, table, &m, args, method, ctx.prefs.previous).await?;
    if written.row.is_null() {
        return Err(ProblemDetails::not_found(format!(
            "No `{}` row found with key `{}`",
            table.name, address.label
        )));
    }
    Ok(write_response(
        &ctx.schema,
        table,
        written,
        StatusCode::OK,
        ctx.prefs,
        false,
    ))
}

/// Handler for `PATCH /rest/{resource}/{key}`.
pub async fn rest_update_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, (resource, key)): Scoped<(String, String)>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{}/{resource}/{key}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let table = ctx.table()?;
        ctx.allow(table, Operation::Update)?;
        let if_match = parse_if_match(&headers)?;
        let (by, rest) = take_by(raw_query.as_deref())?;
        if !rest.is_empty() {
            return Err(ProblemDetails::bad_request(
                "A row update takes its parameters in the body: only `by` goes in the query string",
            ));
        }
        let address = RowAddress::of(table, &key, by.as_deref())?;
        if media::content_type(&headers).as_deref() == Some(media::JSON_PATCH) {
            return collection::json_patch_row(&ctx, table, &address, &body, if_match).await;
        }
        let body = json_body(table, &headers, body, false)?;
        update_row(&ctx, table, &address, &body, if_match, "PATCH").await
    })
    .await
}

/// Handler for `PUT /rest/{resource}/{key}`: insert the row or replace its columns. With
/// `If-Match` the row must exist at that version, so it is an update instead.
pub async fn rest_upsert_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, (resource, key)): Scoped<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{}/{resource}/{key}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let table = ctx.table()?;
        let body = json_body(table, &headers, body, false)?;
        if headers.contains_key(header::IF_MATCH) {
            let if_match = parse_if_match(&headers)?;
            let address = RowAddress::primary(table, &key)?;
            return update_row(&ctx, table, &address, &body, if_match, "PUT").await;
        }

        ctx.allow(table, Operation::Upsert)?;
        let parsed_key = parse_primary_key(&key, table).map_err(ProblemDetails::bad_request)?;
        let mut input = json_object(parse_json_body(&body)?)?;
        strip_key_from_body(table, &parsed_key, &mut input)?;
        // The key of the URL is the key of the row
        if table.primary_key.len() == 1 {
            if let Some(col) = table.get_column(&table.primary_key[0]) {
                input.insert(col.field_name.clone(), parsed_key.clone());
            }
        } else if let Some(obj) = parsed_key.as_object() {
            input.extend(obj.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        let m = mutation_field(&ctx.catalog, table, MutationAction::Create, "Upserting")?;
        let input = serde_json::Value::Object(input);
        ctx.validate(Root::Mutation, &m.name, "input", &input, "body")?;

        let update_cols: Vec<serde_json::Value> = table
            .writable_columns()
            .filter(|c| !c.is_primary_key)
            .map(|c| serde_json::Value::String(c.order_by_name()))
            .collect();
        let on_conflict = if update_cols.is_empty() {
            serde_json::json!({ "key": "PRIMARY_KEY", "action": "NOTHING" })
        } else {
            serde_json::json!({ "key": "PRIMARY_KEY", "action": "UPDATE", "update": update_cols })
        };
        let args = HashMap::from([
            ("input".to_string(), input),
            ("onConflict".to_string(), on_conflict),
        ]);
        let written = run_write(&ctx, table, &m, args, "PUT", ctx.prefs.previous).await?;
        Ok(write_response(
            &ctx.schema,
            table,
            written,
            StatusCode::OK,
            ctx.prefs,
            false,
        ))
    })
    .await
}

/// Handler for `DELETE /rest/{resource}/{key}`.
pub async fn rest_delete_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, (resource, key)): Scoped<(String, String)>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Response {
    respond(format!("{}/{resource}/{key}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let table = ctx.table()?;
        ctx.allow(table, Operation::Delete)?;
        let (by, rest) = take_by(raw_query.as_deref())?;
        if !rest.is_empty() {
            return Err(ProblemDetails::bad_request(
                "A row delete takes no parameters but `by`",
            ));
        }
        let address = RowAddress::of(table, &key, by.as_deref())?;
        let if_match = parse_if_match(&headers)?;

        let m = mutation_field_by(
            &ctx.catalog,
            table,
            MutationAction::Delete,
            "Deleting by this key",
            &address.unique,
        )?;
        let mut args = address.args.clone();
        if let Some(version) = if_match {
            if !ctx.has_argument(Root::Mutation, &m.name, "expectedVersion") {
                return Err(ProblemDetails::bad_request(format!(
                    "`{}` has no row version, so `If-Match` cannot be used",
                    table.name
                )));
            }
            args.insert("expectedVersion".into(), serde_json::Value::String(version));
        }

        let data = ctx
            .execute(
                &table.schema,
                mutation_op(&m, table, args, column_fields(&ctx.catalog, table)),
                "DELETE",
                true,
            )
            .await?;
        let row = ResponseShaper::unwrap_mutation(
            &data,
            &m.name,
            Some(&m.action.payload_field_name(table)),
        )
        .map_err(|e| shaping_error(e, ctx.is_prod))?;
        if row.is_null() {
            return Err(ProblemDetails::not_found(format!(
                "No `{}` row found with key `{key}`",
                table.name
            )));
        }
        if ctx.prefs.return_minimal {
            let mut out = HeaderMap::new();
            if let Some(applied) = ctx.prefs.applied(true, false, false) {
                set_header(
                    &mut out,
                    header::HeaderName::from_static("preference-applied"),
                    &applied,
                );
            }
            return Ok((StatusCode::NO_CONTENT, out).into_response());
        }
        Ok((StatusCode::OK, Json(row)).into_response())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_headers_keep_the_request_parameters() {
        let pi = RestPageInfo {
            has_next_page: true,
            has_previous_page: false,
            start_cursor: Some("cur_start".into()),
            end_cursor: Some("cur_end".into()),
        };
        let query = RestQuery {
            first: Some(10),
            ..Default::default()
        };
        assert_eq!(
            build_link_header("users", Some("first=10&order=name:desc"), Some(&pi), &query),
            Some("</rest/users?first=10&order=name:desc&after=cur_end>; rel=\"next\"".to_string())
        );

        let pi = RestPageInfo {
            has_next_page: false,
            has_previous_page: true,
            ..pi
        };
        assert_eq!(
            build_link_header("users", Some("first=10&after=cur_end"), Some(&pi), &query),
            Some("</rest/users?first=10&before=cur_start&last=10>; rel=\"prev\"".to_string())
        );
    }

    #[test]
    fn if_match_forms() {
        let get = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::IF_MATCH, v.parse().unwrap());
            parse_if_match(&h)
        };
        assert_eq!(get("\"42\"").unwrap().as_deref(), Some("42"));
        assert_eq!(get("W/\"42\"").unwrap().as_deref(), Some("42"));
        assert_eq!(get("*").unwrap(), None);
        assert!(get("\"1\", \"2\"").is_err());
        assert_eq!(parse_if_match(&HeaderMap::new()).unwrap(), None);
    }
}
