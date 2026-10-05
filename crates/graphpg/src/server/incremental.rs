use apollo_compiler::ast::Value;
use apollo_compiler::executable::{Field, Operation, Selection, SelectionSet};
use apollo_compiler::{ExecutableDocument, Node};
use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::channel::mpsc;
use futures::SinkExt;
use serde_json::Value as JsonValue;
use std::collections::HashSet;
use std::sync::Arc;

use crate::catalog::DatabaseCatalog;
use crate::compiler::SqlCompiler;
use crate::execution::engine::Connection;
use crate::execution::ExecutionEngine;
use crate::planner::QueryPlanner;
use crate::server::http::{
    debug_sql, execute_introspection, execution_error, planner_errors, response_key, GraphQLError,
    ServerState,
};
use crate::server::AuthSessionContext;
use apollo_compiler::validation::Valid;
use tracing::Instrument;

#[derive(Debug, Clone)]
pub struct IncrementalPlan {
    pub base_operation: Operation,
    pub targets: Vec<IncrementalTarget>,
}

#[derive(Debug, Clone)]
pub enum IncrementalTarget {
    Defer(ActiveDefer),
    Stream(ActiveStream),
}

/// A deferred fragment: `operation` re-reads the fields leading to it and selects the
/// fragment's fields at `path` (response keys; lists along it are expanded per element when
/// the patches are built).
#[derive(Debug, Clone)]
pub struct ActiveDefer {
    pub label: Option<String>,
    pub path: Vec<String>,
    pub operation: Operation,
}

/// A streamed list: `operation` re-reads the fields leading to it and selects the rows after
/// the first `initial_count`, whose list is at `path`.
#[derive(Debug, Clone)]
pub struct ActiveStream {
    pub label: Option<String>,
    pub path: Vec<String>,
    pub initial_count: usize,
    pub operation: Operation,
}

pub fn make_int_value(val: usize) -> Value {
    Value::Int((val as i32).into())
}

pub fn set_field_argument(field: &mut Field, name: &str, val: Value) {
    if let Some(arg) = field.arguments.iter_mut().find(|a| a.name.as_str() == name) {
        let mut new_arg = (**arg).clone();
        new_arg.value = Node::new(val);
        *arg = Node::new(new_arg);
    } else {
        field
            .arguments
            .push(Node::new(apollo_compiler::ast::Argument {
                name: apollo_compiler::Name::new(name).expect("valid argument name"),
                value: Node::new(val),
            }));
    }
}

/// Resolves an AST Value into a boolean, resolving variable references if necessary.
pub fn resolve_bool_value(
    val: &Value,
    variables: Option<&JsonValue>,
    operation: &Operation,
    default: bool,
) -> bool {
    match val {
        Value::Boolean(b) => *b,
        Value::Variable(var_name) => {
            let vname = var_name.as_str();
            if let Some(JsonValue::Object(map)) = variables {
                if let Some(JsonValue::Bool(b)) = map.get(vname) {
                    return *b;
                }
            }
            if let Some(var_def) = operation
                .variables
                .iter()
                .find(|v| v.name.as_str() == vname)
            {
                if let Some(def_val) = &var_def.default_value {
                    return resolve_bool_value(def_val, variables, operation, default);
                }
            }
            default
        }
        _ => default,
    }
}

/// Resolves an AST Value into a string.
pub fn resolve_string_value(
    val: &Value,
    variables: Option<&JsonValue>,
    operation: &Operation,
) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Variable(var_name) => {
            let vname = var_name.as_str();
            if let Some(JsonValue::Object(map)) = variables {
                if let Some(JsonValue::String(s)) = map.get(vname) {
                    return Some(s.clone());
                }
            }
            if let Some(var_def) = operation
                .variables
                .iter()
                .find(|v| v.name.as_str() == vname)
            {
                if let Some(def_val) = &var_def.default_value {
                    return resolve_string_value(def_val, variables, operation);
                }
            }
            None
        }
        _ => None,
    }
}

/// Resolves an AST Value into an integer (usize).
pub fn resolve_int_value(
    val: &Value,
    variables: Option<&JsonValue>,
    operation: &Operation,
    default: usize,
) -> usize {
    resolve_opt_int(val, variables, operation).unwrap_or(default)
}

/// Resolves an AST Value into an integer, `None` when it is null, missing or not one.
fn resolve_opt_int(
    val: &Value,
    variables: Option<&JsonValue>,
    operation: &Operation,
) -> Option<usize> {
    match val {
        Value::Int(i) => i.as_str().parse::<usize>().ok(),
        Value::Variable(var_name) => {
            let vname = var_name.as_str();
            if let Some(JsonValue::Object(map)) = variables {
                if let Some(value) = map.get(vname) {
                    return value.as_u64().map(|n| n as usize);
                }
            }
            let var_def = operation
                .variables
                .iter()
                .find(|v| v.name.as_str() == vname)?;
            resolve_opt_int(var_def.default_value.as_ref()?, variables, operation)
        }
        _ => None,
    }
}

/// How `@stream` pages a field: the initial payload reads its first `initial_count` rows
/// (from the field's own `offset`), the stream the rest.
struct StreamWindow {
    /// Response key of the connection's `nodes` or `edges`: the list streamed.
    items_key: String,
    /// `offset` of the streamed rows.
    offset: usize,
    /// `first` of the streamed rows: what remains of the field's own `first`, if it has one.
    rest: Option<usize>,
}

/// The [`StreamWindow`] of `field`, or `None` when it is delivered whole in the initial
/// payload: fields not paged by `first` and `offset` (plain lists take no arguments),
/// backward pages (`last`, `before`), selections of both `nodes` and `edges`, and fields whose
/// own `first` leaves nothing past `initial_count`.
fn stream_window(
    field: &Field,
    variables: Option<&JsonValue>,
    operation: &Operation,
    initial_count: usize,
) -> Option<StreamWindow> {
    let takes = |name: &str| {
        field
            .definition
            .arguments
            .iter()
            .any(|a| a.name.as_str() == name)
    };
    if !(takes("first") && takes("offset")) {
        return None;
    }
    let arg = |name: &str| field.arguments.iter().find(|a| a.name.as_str() == name);
    let int_arg =
        |name: &str| arg(name).and_then(|a| resolve_opt_int(&a.value, variables, operation));
    let backward = int_arg("last").is_some()
        || arg("before")
            .and_then(|a| resolve_string_value(&a.value, variables, operation))
            .is_some();
    if backward {
        return None;
    }
    let first = int_arg("first");
    if first.is_some_and(|f| f <= initial_count) {
        return None;
    }
    let lists: Vec<String> = field
        .selection_set
        .selections
        .iter()
        .filter_map(|s| match s {
            Selection::Field(f) if matches!(f.name.as_str(), "nodes" | "edges") => {
                Some(f.response_key().to_string())
            }
            _ => None,
        })
        .collect();
    let [items_key] = lists.as_slice() else {
        return None;
    };
    Some(StreamWindow {
        items_key: items_key.clone(),
        offset: int_arg("offset").unwrap_or(0) + initial_count,
        rest: first.map(|f| f - initial_count),
    })
}

/// Checks whether an operation contains any active @defer or @stream directives.
pub fn has_active_defer_or_stream(
    doc: &ExecutableDocument,
    operation: &Operation,
    variables: Option<&JsonValue>,
) -> bool {
    if !operation.operation_type.is_query() {
        return false;
    }
    let mut visited_fragments = HashSet::new();
    check_selection_set_for_directives(
        doc,
        operation,
        &operation.selection_set,
        variables,
        &mut visited_fragments,
    )
}

fn check_selection_set_for_directives(
    doc: &ExecutableDocument,
    operation: &Operation,
    selection_set: &SelectionSet,
    variables: Option<&JsonValue>,
    visited_fragments: &mut HashSet<String>,
) -> bool {
    for sel in &selection_set.selections {
        match sel {
            Selection::Field(field) => {
                for dir in field.directives.iter() {
                    if dir.name.as_str() == "stream" {
                        let if_cond = dir
                            .arguments
                            .iter()
                            .find(|a| a.name.as_str() == "if")
                            .map(|a| resolve_bool_value(&a.value, variables, operation, true))
                            .unwrap_or(true);
                        if if_cond {
                            return true;
                        }
                    }
                }
                if check_selection_set_for_directives(
                    doc,
                    operation,
                    &field.selection_set,
                    variables,
                    visited_fragments,
                ) {
                    return true;
                }
            }
            Selection::InlineFragment(inline) => {
                for dir in inline.directives.iter() {
                    if dir.name.as_str() == "defer" {
                        let if_cond = dir
                            .arguments
                            .iter()
                            .find(|a| a.name.as_str() == "if")
                            .map(|a| resolve_bool_value(&a.value, variables, operation, true))
                            .unwrap_or(true);
                        if if_cond {
                            return true;
                        }
                    }
                }
                if check_selection_set_for_directives(
                    doc,
                    operation,
                    &inline.selection_set,
                    variables,
                    visited_fragments,
                ) {
                    return true;
                }
            }
            Selection::FragmentSpread(spread) => {
                for dir in spread.directives.iter() {
                    if dir.name.as_str() == "defer" {
                        let if_cond = dir
                            .arguments
                            .iter()
                            .find(|a| a.name.as_str() == "if")
                            .map(|a| resolve_bool_value(&a.value, variables, operation, true))
                            .unwrap_or(true);
                        if if_cond {
                            return true;
                        }
                    }
                }
                let name = spread.fragment_name.as_str();
                if visited_fragments.insert(name.to_string()) {
                    if let Some(frag) = doc.fragments.get(name) {
                        if check_selection_set_for_directives(
                            doc,
                            operation,
                            &frag.selection_set,
                            variables,
                            visited_fragments,
                        ) {
                            return true;
                        }
                    }
                    visited_fragments.remove(name);
                }
            }
        }
    }
    false
}

/// Wraps selections into the chain of ancestor fields, leading from the root query.
fn wrap_in_ancestors(ancestors: &[Field], selections: SelectionSet) -> SelectionSet {
    if ancestors.is_empty() {
        return selections;
    }
    let mut current_selections = selections;
    for ancestor in ancestors.iter().rev() {
        let mut f = (*ancestor).clone();
        f.selection_set = current_selections;
        // Strip @defer and @stream from ancestor fields when building targeted query
        f.directives
            .retain(|d| d.name.as_str() != "stream" && d.name.as_str() != "defer");
        current_selections = SelectionSet {
            ty: ancestor.selection_set.ty.clone(),
            selections: vec![Selection::Field(Node::new(f))],
        };
    }
    current_selections
}

/// Strips directive with given name from a field.
fn strip_directive(field: &mut Field, dir_name: &str) {
    field.directives.retain(|d| d.name.as_str() != dir_name);
}

/// Analyzes an operation to build an IncrementalPlan containing the base operation
/// (with deferred fields omitted and streamed lists capped to initialCount)
/// and all subsequent targets (deferred fragments and stream patches).
pub fn analyze_incremental(
    doc: &ExecutableDocument,
    operation: &Operation,
    variables: Option<&JsonValue>,
) -> Option<IncrementalPlan> {
    if !operation.operation_type.is_query() {
        return None;
    }
    let mut current_path = Vec::new();
    let mut ancestors = Vec::new();
    let mut targets = Vec::new();

    let base_selection_set = process_selection_set(
        doc,
        operation,
        &operation.selection_set,
        variables,
        &mut current_path,
        &mut ancestors,
        &mut targets,
    );

    if targets.is_empty() {
        return None;
    }

    let mut base_operation = (*operation).clone();
    base_operation.selection_set = base_selection_set;

    Some(IncrementalPlan {
        base_operation,
        targets,
    })
}

fn process_selection_set(
    doc: &ExecutableDocument,
    operation: &Operation,
    selection_set: &SelectionSet,
    variables: Option<&JsonValue>,
    current_path: &mut Vec<String>,
    ancestors: &mut Vec<Field>,
    targets: &mut Vec<IncrementalTarget>,
) -> SelectionSet {
    let mut out_selections = Vec::new();

    for sel in &selection_set.selections {
        match sel {
            Selection::Field(field_node) => {
                let mut field = (**field_node).clone();
                let field_key = field.response_key().as_str().to_string();

                // Check for @stream on this field
                let stream_dir = field
                    .directives
                    .iter()
                    .find(|d| d.name.as_str() == "stream");
                let active_stream = stream_dir.and_then(|dir| {
                    let if_cond = dir
                        .arguments
                        .iter()
                        .find(|a| a.name.as_str() == "if")
                        .map(|a| resolve_bool_value(&a.value, variables, operation, true))
                        .unwrap_or(true);
                    if !if_cond {
                        return None;
                    }
                    let initial_count = dir
                        .arguments
                        .iter()
                        .find(|a| {
                            a.name.as_str() == "initialCount" || a.name.as_str() == "initial_count"
                        })
                        .map(|a| resolve_int_value(&a.value, variables, operation, 0))
                        .unwrap_or(0);
                    let label = dir
                        .arguments
                        .iter()
                        .find(|a| a.name.as_str() == "label")
                        .and_then(|a| resolve_string_value(&a.value, variables, operation));
                    Some((initial_count, label))
                });

                let stream = active_stream.and_then(|(initial_count, label)| {
                    strip_directive(&mut field, "stream");
                    stream_window(field_node, variables, operation, initial_count)
                        .map(|w| (initial_count, label, w))
                });
                if let Some((initial_count, label, window)) = stream {
                    // 1. The initial payload reads the first `initial_count` rows from the
                    // field's own offset
                    set_field_argument(&mut field, "first", make_int_value(initial_count));

                    // 2. The stream reads the rest: after them, up to the field's own `first`
                    let mut stream_field = (**field_node).clone();
                    strip_directive(&mut stream_field, "stream");
                    set_field_argument(&mut stream_field, "offset", make_int_value(window.offset));
                    if let Some(rest) = window.rest {
                        set_field_argument(&mut stream_field, "first", make_int_value(rest));
                    }

                    // Build stream operation
                    let stream_sel_set = wrap_in_ancestors(
                        ancestors,
                        SelectionSet {
                            ty: selection_set.ty.clone(),
                            selections: vec![Selection::Field(Node::new(stream_field))],
                        },
                    );
                    let mut stream_op = (*operation).clone();
                    stream_op.selection_set = stream_sel_set;

                    let mut path = current_path.clone();
                    path.push(field_key.clone());
                    path.push(window.items_key);
                    targets.push(IncrementalTarget::Stream(ActiveStream {
                        label,
                        path,
                        initial_count,
                        operation: stream_op,
                    }));
                }

                // Recurse into child selections
                current_path.push(field_key);
                ancestors.push(field.clone());

                let child_selection_set = process_selection_set(
                    doc,
                    operation,
                    &field.selection_set,
                    variables,
                    current_path,
                    ancestors,
                    targets,
                );

                ancestors.pop();
                current_path.pop();

                field.selection_set = child_selection_set;
                out_selections.push(Selection::Field(Node::new(field)));
            }
            Selection::InlineFragment(inline) => {
                let defer_dir = inline
                    .directives
                    .iter()
                    .find(|d| d.name.as_str() == "defer");
                let active_defer = defer_dir.and_then(|dir| {
                    let if_cond = dir
                        .arguments
                        .iter()
                        .find(|a| a.name.as_str() == "if")
                        .map(|a| resolve_bool_value(&a.value, variables, operation, true))
                        .unwrap_or(true);
                    if !if_cond {
                        return None;
                    }
                    let label = dir
                        .arguments
                        .iter()
                        .find(|a| a.name.as_str() == "label")
                        .and_then(|a| resolve_string_value(&a.value, variables, operation));
                    Some(label)
                });

                if let Some(label) = active_defer {
                    // Deferred inline fragment: exclude from base, create targeted operation
                    let mut target_fragment = (**inline).clone();
                    target_fragment
                        .directives
                        .retain(|d| d.name.as_str() != "defer");

                    let target_sel_set = wrap_in_ancestors(
                        ancestors,
                        SelectionSet {
                            ty: inline.selection_set.ty.clone(),
                            selections: vec![Selection::InlineFragment(Node::new(target_fragment))],
                        },
                    );
                    let mut target_op = (*operation).clone();
                    target_op.selection_set = target_sel_set;
                    targets.push(IncrementalTarget::Defer(ActiveDefer {
                        label,
                        path: current_path.clone(),
                        operation: target_op,
                    }));
                } else {
                    // Non-deferred inline fragment: preserve and recurse
                    let mut inline_clone = (**inline).clone();
                    inline_clone.selection_set = process_selection_set(
                        doc,
                        operation,
                        &inline.selection_set,
                        variables,
                        current_path,
                        ancestors,
                        targets,
                    );
                    out_selections.push(Selection::InlineFragment(Node::new(inline_clone)));
                }
            }
            Selection::FragmentSpread(spread) => {
                let defer_dir = spread
                    .directives
                    .iter()
                    .find(|d| d.name.as_str() == "defer");
                let active_defer = defer_dir.and_then(|dir| {
                    let if_cond = dir
                        .arguments
                        .iter()
                        .find(|a| a.name.as_str() == "if")
                        .map(|a| resolve_bool_value(&a.value, variables, operation, true))
                        .unwrap_or(true);
                    if !if_cond {
                        return None;
                    }
                    let label = dir
                        .arguments
                        .iter()
                        .find(|a| a.name.as_str() == "label")
                        .and_then(|a| resolve_string_value(&a.value, variables, operation));
                    Some(label)
                });

                if let Some(label) = active_defer {
                    // Deferred fragment spread: expand named fragment into targeted operation
                    let frag_name = spread.fragment_name.as_str();
                    if let Some(frag) = doc.fragments.get(frag_name) {
                        let target_sel_set =
                            wrap_in_ancestors(ancestors, frag.selection_set.clone());
                        let mut target_op = (*operation).clone();
                        target_op.selection_set = target_sel_set;
                        targets.push(IncrementalTarget::Defer(ActiveDefer {
                            label,
                            path: current_path.clone(),
                            operation: target_op,
                        }));
                    }
                } else {
                    out_selections.push(sel.clone());
                }
            }
        }
    }

    SelectionSet {
        ty: selection_set.ty.clone(),
        selections: out_selections,
    }
}

/// Formats a JSON value into an RFC 2046 multipart/mixed HTTP chunk.
fn format_multipart_chunk(json_val: &serde_json::Value) -> String {
    let body = serde_json::to_string(json_val).unwrap_or_default();
    format!("---\r\nContent-Type: application/json\r\n\r\n{}\r\n", body)
}

/// The values at `path` in `data`, each with its concrete path: a list along the way expands
/// into one entry per element (`["users", "posts"]` → `["users", 0, "posts"]`,
/// `["users", 1, "posts"]`, …), as does a list at the end of the path when `expand_last`.
/// Missing and null values have nothing to patch and are skipped.
fn locate<'v>(
    data: &'v JsonValue,
    path: &[String],
    expand_last: bool,
) -> Vec<(Vec<JsonValue>, &'v JsonValue)> {
    fn walk<'v>(
        value: &'v JsonValue,
        rest: &[String],
        expand_last: bool,
        prefix: &mut Vec<JsonValue>,
        out: &mut Vec<(Vec<JsonValue>, &'v JsonValue)>,
    ) {
        match (value, rest.split_first()) {
            (JsonValue::Null, _) => {}
            (JsonValue::Array(items), _) if !rest.is_empty() || expand_last => {
                for (i, item) in items.iter().enumerate() {
                    prefix.push(JsonValue::from(i));
                    walk(item, rest, expand_last, prefix, out);
                    prefix.pop();
                }
            }
            (_, None) => out.push((prefix.clone(), value)),
            (_, Some((segment, tail))) => {
                if let Some(next) = value.get(segment) {
                    prefix.push(JsonValue::String(segment.clone()));
                    walk(next, tail, expand_last, prefix, out);
                    prefix.pop();
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(data, path, expand_last, &mut Vec::new(), &mut out);
    out
}

/// A pooled connection inside the request's snapshot transaction: each payload's statements
/// nest in a savepoint (keeping the per-connection statement cache).
struct InSnapshot<'a>(&'a deadpool_postgres::ClientWrapper);

impl Connection for InSnapshot<'_> {
    fn client(&self) -> &tokio_postgres::Client {
        self.0
    }

    async fn prepare(&self, sql: &str) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
        Connection::prepare(self.0, sql).await
    }
}

/// Multipart chunks to the client; `false` once it disconnected.
struct Chunks(mpsc::Sender<Result<Bytes, std::io::Error>>);

impl Chunks {
    async fn send(&mut self, payload: &JsonValue) -> bool {
        self.0
            .send(Ok(Bytes::from(format_multipart_chunk(payload))))
            .await
            .is_ok()
    }

    async fn close(&mut self) {
        let _ = self.0.send(Ok(Bytes::from("-----\r\n"))).await;
    }
}

/// The incremental entry (or entries) of one deferred or streamed payload.
enum Patch {
    Data {
        path: Vec<JsonValue>,
        data: JsonValue,
    },
    Items {
        path: Vec<JsonValue>,
        items: JsonValue,
    },
}

/// One request's payloads: the initial one, then one per deferred fragment and streamed list.
struct Payloads<'r> {
    state: &'r ServerState,
    doc: &'r Valid<ExecutableDocument>,
    planner: QueryPlanner<'r>,
    catalog: &'r DatabaseCatalog,
    operation_label: &'r str,
    auth_ctx: Option<&'r AuthSessionContext>,
}

impl Payloads<'_> {
    /// Plans, compiles and runs one operation: its data and, in debug mode, its SQL.
    async fn run<C: Connection>(
        &self,
        conn: &C,
        operation: &Operation,
    ) -> Result<(JsonValue, Option<JsonValue>), Vec<GraphQLError>> {
        let plan = self
            .planner
            .plan_operation(operation)
            .map_err(|err| planner_errors(&err))?;
        let mut compiled = SqlCompiler::compile_operation(&plan);
        if self.state.sql_comments {
            compiled.tag(&[
                ("application", "graphpg"),
                ("graphql_operation", self.operation_label),
                ("graphql_type", "query"),
            ]);
        }
        let span =
            tracing::info_span!("graphql.execute", graphql.operation = %self.operation_label);
        let data = ExecutionEngine::run_with_auth(conn, &compiled, None, self.auth_ctx)
            .instrument(span)
            .await
            .map_err(|err| vec![execution_error(&err, self.catalog).1])?;
        Ok((
            data,
            (self.state.debug.sql || self.state.debug.explain).then(|| debug_sql(&compiled)),
        ))
    }

    /// The initial payload's data: introspection fields from the schema, the others from the
    /// database, in the order the root fields were selected.
    async fn initial<C: Connection>(
        &self,
        conn: &C,
        base: &Operation,
        variables: Option<&JsonValue>,
    ) -> Result<(JsonValue, Option<JsonValue>), Vec<GraphQLError>> {
        let root_fields = self
            .planner
            .flatten_selections(&base.selection_set, Some("Query"));
        let mut introspection = serde_json::Map::new();
        if root_fields.iter().any(|f| f.name.starts_with("__")) {
            let schema = self.state.current_schema();
            introspection = execute_introspection(&schema, self.doc, base, variables)
                .map_err(|message| vec![GraphQLError::new(message)])?;
        }
        let (mut sql_data, sql) = if root_fields.iter().any(|f| !f.name.starts_with("__")) {
            match self.run(conn, base).await? {
                (JsonValue::Object(map), sql) => (map, sql),
                (_, sql) => (serde_json::Map::new(), sql),
            }
        } else {
            (serde_json::Map::new(), None)
        };
        let mut data = serde_json::Map::new();
        for field in root_fields {
            let key = response_key(field);
            if data.contains_key(key) {
                continue;
            }
            if let Some(v) = introspection.remove(key).or_else(|| sql_data.remove(key)) {
                data.insert(key.to_string(), v);
            }
        }
        for (k, v) in sql_data {
            data.entry(k).or_insert(v);
        }
        Ok((JsonValue::Object(data), sql))
    }

    /// The patches of a deferred fragment or streamed list.
    async fn target<C: Connection>(
        &self,
        conn: &C,
        target: &IncrementalTarget,
    ) -> Result<(Vec<Patch>, Option<JsonValue>), Vec<GraphQLError>> {
        match target {
            IncrementalTarget::Defer(defer) => {
                let (data, sql) = self.run(conn, &defer.operation).await?;
                // A fragment under a list is deferred in each of its elements
                let patches = locate(&data, &defer.path, true)
                    .into_iter()
                    .map(|(path, value)| Patch::Data {
                        path,
                        data: value.clone(),
                    })
                    .collect();
                Ok((patches, sql))
            }
            IncrementalTarget::Stream(stream) => {
                let (data, sql) = self.run(conn, &stream.operation).await?;
                // A list under a list streams in each of its elements, after its initial rows
                let patches = locate(&data, &stream.path, false)
                    .into_iter()
                    .filter(|(_, items)| items.is_array())
                    .map(|(mut path, items)| {
                        path.push(JsonValue::from(stream.initial_count));
                        Patch::Items {
                            path,
                            items: items.clone(),
                        }
                    })
                    .collect();
                Ok((patches, sql))
            }
        }
    }
}

/// Executes an incremental operation plan as `multipart/mixed` payloads: the initial one,
/// then one per deferred fragment and streamed list, in order.
///
/// Every payload reads one snapshot (a `REPEATABLE READ` read-only transaction): the deferred
/// and streamed queries re-read the lists the initial payload holds, so the list positions in
/// their paths match it. Queries run on a replica that has replayed `min_lsn` when given, as
/// in a single response.
#[allow(clippy::too_many_arguments)]
pub async fn execute_incremental_stream(
    state: Arc<ServerState>,
    doc: Arc<Valid<ExecutableDocument>>,
    plan: IncrementalPlan,
    variables: Option<serde_json::Value>,
    operation_label: String,
    auth_ctx: Option<AuthSessionContext>,
    min_lsn: Option<String>,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);

    tokio::spawn(async move {
        let mut chunks = Chunks(tx);
        let failed =
            |errors: Vec<GraphQLError>| serde_json::json!({ "errors": errors, "hasNext": false });

        let (client, database) = match state.read_pool(min_lsn.as_deref()).await {
            Ok(acquired) => acquired,
            Err(message) => {
                tracing::error!(error = %message, "incremental: failed to acquire database connection");
                chunks.send(&failed(vec![GraphQLError::new(message)])).await;
                chunks.close().await;
                return;
            }
        };
        if let Err(err) = client
            .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await
        {
            chunks
                .send(&failed(vec![GraphQLError::new(format!(
                    "Database unavailable: {err}"
                ))]))
                .await;
            chunks.close().await;
            return;
        }

        let catalog = state.current_catalog();
        let payloads = Payloads {
            state: &state,
            doc: &doc,
            planner: QueryPlanner::with_context(
                &catalog,
                &state.default_schema,
                Some(&**doc),
                variables.as_ref(),
            )
            .with_limits(state.limits.plan_limits()),
            catalog: &catalog,
            operation_label: &operation_label,
            auth_ctx: auth_ctx.as_ref(),
        };
        let conn = InSnapshot(&client);
        stream_payloads(
            &payloads,
            &conn,
            &plan,
            variables.as_ref(),
            database,
            &mut chunks,
        )
        .await;

        // The snapshot only read; a connection that cannot end it is not returned to the pool
        if client.batch_execute("ROLLBACK").await.is_err() {
            drop(deadpool_postgres::Object::take(client));
        }
        chunks.close().await;
    });

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/mixed; boundary=\"-\""),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));

    (StatusCode::OK, headers, Body::from_stream(rx)).into_response()
}

/// Sends the payloads of `plan`, stopping early when the client disconnects.
async fn stream_payloads<C: Connection>(
    payloads: &Payloads<'_>,
    conn: &C,
    plan: &IncrementalPlan,
    variables: Option<&JsonValue>,
    database: &str,
    chunks: &mut Chunks,
) {
    let debug = payloads.state.debug.sql || payloads.state.debug.explain;
    let (data, sql) = match payloads
        .initial(conn, &plan.base_operation, variables)
        .await
    {
        Ok(initial) => initial,
        Err(errors) => {
            tracing::error!("incremental: the initial payload failed");
            chunks
                .send(&serde_json::json!({ "errors": errors, "hasNext": false }))
                .await;
            return;
        }
    };
    let mut initial = serde_json::json!({ "data": data, "hasNext": !plan.targets.is_empty() });
    if debug {
        initial["extensions"] = serde_json::json!({ "sql": sql, "database": database });
    }
    if !chunks.send(&initial).await {
        return;
    }

    let total = plan.targets.len();
    for (idx, target) in plan.targets.iter().enumerate() {
        let (label, static_path) = match target {
            IncrementalTarget::Defer(d) => (&d.label, &d.path),
            IncrementalTarget::Stream(s) => (&s.label, &s.path),
        };
        let entry = |mut fields: serde_json::Map<String, JsonValue>| {
            if let Some(label) = label {
                fields.insert("label".to_string(), JsonValue::String(label.clone()));
            }
            JsonValue::Object(fields)
        };
        let (entries, sql) = match payloads.target(conn, target).await {
            Ok((patches, sql)) => {
                let entries: Vec<JsonValue> = patches
                    .into_iter()
                    .map(|patch| {
                        let mut fields = serde_json::Map::new();
                        match patch {
                            Patch::Data { path, data } => {
                                fields.insert("path".to_string(), JsonValue::Array(path));
                                fields.insert("data".to_string(), data);
                            }
                            Patch::Items { path, items } => {
                                fields.insert("path".to_string(), JsonValue::Array(path));
                                fields.insert("items".to_string(), items);
                            }
                        }
                        entry(fields)
                    })
                    .collect();
                (entries, sql)
            }
            Err(errors) => {
                let mut fields = serde_json::Map::new();
                fields.insert("path".to_string(), serde_json::json!(static_path));
                fields.insert("errors".to_string(), serde_json::json!(errors));
                (vec![entry(fields)], None)
            }
        };
        let mut payload = serde_json::json!({ "hasNext": idx + 1 < total });
        // A fragment under an empty list has nothing to patch
        if !entries.is_empty() {
            payload["incremental"] = JsonValue::Array(entries);
        }
        if debug && sql.is_some() {
            payload["extensions"] = serde_json::json!({ "sql": sql });
        }
        if !chunks.send(&payload).await {
            return;
        }
    }
}
