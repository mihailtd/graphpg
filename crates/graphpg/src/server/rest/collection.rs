//! Writes to a collection: `PATCH` and `DELETE` of the rows a `filter` matches, `PUT` of many rows
//! (bulk upsert), and the `missing` choice of bulk writes. Single rows can also be patched with
//! RFC 6902 JSON Patch.
//!
//! All of it is planned onto the nodes GraphQL has (`update<Types>(where:)`,
//! `delete<Types>(where:)`, `create<Types>(inputs:, onConflict:)`), one statement in one
//! transaction. A write by filter affects at most `limits.bulk_rows` rows: above that it is
//! rolled back with `BULK_LIMIT_EXCEEDED`.

use super::*;

/// What a column a bulk write's rows do not set becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Missing {
    /// The column's default (or `NULL` without one): each row is inserted as it came.
    #[default]
    Default,
    /// `NULL`, in every row, for every column some row of the request sets.
    Null,
}

/// How a bulk upsert treats a row whose conflict key exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Resolution {
    /// Update the existing row from the proposed one.
    #[default]
    Merge,
    /// Leave the existing row as it is.
    Ignore,
}

/// The query parameters of a write that are not the filter grammar.
#[derive(Debug, Default)]
struct WriteParams {
    /// A write by filter without a filter is allowed.
    unfiltered: bool,
    missing: Missing,
    conflict: Option<Vec<String>>,
    resolution: Resolution,
    update: Option<Vec<String>>,
    /// The rest of the query string: the filter.
    rest: String,
}

/// Which of the parameters an endpoint takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    /// `PATCH` / `DELETE` of the rows a filter matches.
    ByFilter,
    /// `PUT` of rows, upserted.
    Upsert,
    /// `POST` of rows: only `missing` (other parameters are not looked at).
    Create,
}

fn write_params(raw: &str, endpoint: Endpoint) -> Result<WriteParams, ProblemDetails> {
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(&s.replace('+', " "))
            .decode_utf8()
            .map(|c| c.to_string())
            .map_err(|_| ProblemDetails::bad_request(format!("Invalid UTF-8 in query string: {s}")))
    };
    let mut params = WriteParams::default();
    let mut rest = Vec::new();
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (key, value) = (decode(key)?, decode(value)?);
        let invalid = |what: &str| ProblemDetails::invalid(what.to_string(), key.clone());
        match (key.as_str(), endpoint) {
            ("unfiltered", Endpoint::ByFilter) => {
                params.unfiltered = match value.as_str() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => return Err(invalid("`unfiltered` must be true or false")),
                }
            }
            ("missing", Endpoint::Upsert | Endpoint::Create) => {
                params.missing = match value.as_str() {
                    "default" => Missing::Default,
                    "null" => Missing::Null,
                    _ => return Err(invalid("`missing` must be `default` or `null`")),
                }
            }
            ("conflict", Endpoint::Upsert) => params.conflict = Some(query::split_list(&value)),
            ("resolution", Endpoint::Upsert) => {
                params.resolution = match value.as_str() {
                    "merge" => Resolution::Merge,
                    "ignore" => Resolution::Ignore,
                    _ => return Err(invalid("`resolution` must be `merge` or `ignore`")),
                }
            }
            ("update", Endpoint::Upsert) => params.update = Some(query::split_list(&value)),
            (_, Endpoint::Create) => {}
            (_, Endpoint::Upsert) => {
                return Err(ProblemDetails::bad_request(format!(
                    "Unknown query parameter `{key}`: a bulk upsert takes `conflict`, `resolution`, `update` and `missing`"
                )))
            }
            (_, Endpoint::ByFilter) => rest.push(pair),
        }
    }
    params.rest = rest.join("&");
    Ok(params)
}

/// The filter of a write by filter: only `filter` parameters.
fn write_filter(
    params: &WriteParams,
    table: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> Result<serde_json::Value, ProblemDetails> {
    let query = parse_query_params(&params.rest, QueryKind::Collection, table, catalog)
        .map_err(ProblemDetails::bad_request)?;
    let unrelated = [
        ("fields", query.fields.is_some()),
        ("include", query.include.is_some()),
        ("order", query.order_by.is_some()),
        ("first", query.first.is_some()),
        ("last", query.last.is_some()),
        ("after", query.after.is_some()),
        ("before", query.before.is_some()),
        ("offset", query.offset.is_some()),
        ("inner", !query.inner.is_empty()),
        ("anti", !query.anti.is_empty()),
        ("embedded controls", !query.embed.is_empty()),
    ];
    if let Some((name, _)) = unrelated.iter().find(|(_, present)| *present) {
        return Err(ProblemDetails::bad_request(format!(
            "`{name}` does not apply to a write: it takes a `filter` only"
        )));
    }
    match query.filter {
        Some(filter) => Ok(filter),
        None if params.unfiltered => Ok(serde_json::json!({})),
        None => Err(ProblemDetails::new(
            StatusCode::BAD_REQUEST,
            format!(
                "A write to `{}` without a `filter` would change every row: add a `filter`, or `unfiltered=true` to mean it",
                table.name
            ),
        )
        .with_code("FILTER_REQUIRED")),
    }
}

/// Sets every column some row sets, in all of them: `NULL` where a row does not.
pub(super) fn fill_missing(rows: &mut [serde_json::Value], missing: Missing) {
    if missing == Missing::Default {
        return;
    }
    let columns: std::collections::BTreeSet<String> = rows
        .iter()
        .filter_map(serde_json::Value::as_object)
        .flat_map(|row| row.keys().cloned())
        .collect();
    for row in rows.iter_mut().filter_map(serde_json::Value::as_object_mut) {
        for column in &columns {
            row.entry(column.clone()).or_insert(serde_json::Value::Null);
        }
    }
}

/// The answer to a write of many rows: the rows with the count in `X-Affected-Count`, or no
/// body with `Prefer: return=minimal`.
fn rows_response(ctx: &Ctx, rows: serde_json::Value, count: u64) -> Response {
    let mut out = HeaderMap::new();
    set_header(
        &mut out,
        header::HeaderName::from_static("x-affected-count"),
        &count.to_string(),
    );
    if ctx.prefs.return_minimal {
        if let Some(applied) = ctx.prefs.applied(true, false, false) {
            set_header(
                &mut out,
                header::HeaderName::from_static("preference-applied"),
                &applied,
            );
        }
        return (StatusCode::NO_CONTENT, out).into_response();
    }
    (StatusCode::OK, out, Json(rows)).into_response()
}

/// Runs a write of many rows and splits the payload into its rows and the affected count.
async fn run_many(
    ctx: &Ctx,
    table: &TableMetadata,
    m: &MutationField,
    args: HashMap<String, serde_json::Value>,
    method: &str,
) -> Result<(serde_json::Value, u64), ProblemDetails> {
    let rows_field = m.action.payload_field_name(table);
    let op = RequestOperation::mutation(vec![RequestField::new(&m.name)
        .with_arguments(args)
        .with_children(vec![
            RequestField::new(&rows_field).with_children(column_fields(&ctx.catalog, table)),
            RequestField::new("affectedCount"),
        ])]);
    let data = ctx.execute(&table.schema, op, method, true).await?;
    let payload =
        ResponseShaper::unwrap_single(&data, &m.name).map_err(|e| shaping_error(e, ctx.is_prod))?;
    let count = payload
        .get("affectedCount")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Ok((payload.get(&rows_field).cloned().unwrap_or_default(), count))
}

/// Handler for `PATCH /rest/{resource}?filter…`: sets the body's columns in the rows that match.
pub async fn rest_collection_update_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, resource): Scoped<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{}/{resource}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let table = ctx.table()?;
        ctx.allow(table, Operation::UpdateMany)?;
        if media::content_type(&headers).as_deref() == Some(media::JSON_PATCH) {
            return Err(ProblemDetails::unsupported_media_type(
                "A JSON Patch applies to one row (`PATCH /rest/{resource}/{key}`); here send `application/json` or `application/merge-patch+json`",
            ));
        }
        let params = write_params(raw_query.as_deref().unwrap_or(""), Endpoint::ByFilter)?;
        let filter = write_filter(&params, table, &ctx.catalog)?;
        let body = json_body(table, &headers, body, false)?;
        let set = serde_json::Value::Object(json_object(parse_json_body(&body)?)?);
        if set.as_object().is_some_and(|set| set.is_empty()) {
            return Err(ProblemDetails::bad_request(
                "Request body has no fields to update",
            ));
        }
        let m = mutation_field(
            &ctx.catalog,
            table,
            MutationAction::UpdateMany,
            "Updating by filter",
        )?;
        ctx.validate(Root::Mutation, &m.name, "where", &filter, "filter")?;
        ctx.validate(Root::Mutation, &m.name, "set", &set, "body")?;
        let args = HashMap::from([("where".to_string(), filter), ("set".to_string(), set)]);
        let (rows, count) = run_many(&ctx, table, &m, args, "PATCH").await?;
        Ok(rows_response(&ctx, rows, count))
    })
    .await
}

/// Handler for `DELETE /rest/{resource}?filter…`: deletes the rows that match.
pub async fn rest_collection_delete_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, resource): Scoped<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Response {
    respond(format!("{}/{resource}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let table = ctx.table()?;
        ctx.allow(table, Operation::DeleteMany)?;
        let params = write_params(raw_query.as_deref().unwrap_or(""), Endpoint::ByFilter)?;
        let filter = write_filter(&params, table, &ctx.catalog)?;
        let m = mutation_field(
            &ctx.catalog,
            table,
            MutationAction::DeleteMany,
            "Deleting by filter",
        )?;
        ctx.validate(Root::Mutation, &m.name, "where", &filter, "filter")?;
        let args = HashMap::from([("where".to_string(), filter)]);
        let (rows, count) = run_many(&ctx, table, &m, args, "DELETE").await?;
        Ok(rows_response(&ctx, rows, count))
    })
    .await
}

/// The rows of a bulk write's body: a non-empty array of objects within `limits.bulk_rows`.
pub(super) fn bulk_rows(
    ctx: &Ctx,
    body: serde_json::Value,
) -> Result<Vec<serde_json::Value>, ProblemDetails> {
    let serde_json::Value::Array(rows) = body else {
        return Err(ProblemDetails::bad_request(
            "Request body must be an array of objects",
        ));
    };
    if rows.is_empty() {
        return Err(ProblemDetails::bad_request(
            "A bulk write needs at least one row",
        ));
    }
    if let Some(max) = ctx.state.limits.bulk_rows {
        if rows.len() > max {
            return Err(ProblemDetails::from_limit(
                &crate::plan::LimitError::BulkRows {
                    rows: rows.len(),
                    max,
                },
            ));
        }
    }
    if let Some(i) = rows.iter().position(|r| !r.is_object()) {
        return Err(ProblemDetails::invalid(
            format!("Row {i} of the body must be a JSON object"),
            format!("body[{i}]"),
        ));
    }
    Ok(rows)
}

/// `missing` of a bulk create: `POST /rest/{resource}?missing=null`.
pub(super) fn create_missing(raw_query: Option<&str>) -> Result<Missing, ProblemDetails> {
    Ok(write_params(raw_query.unwrap_or(""), Endpoint::Create)?.missing)
}

/// The `onConflict` argument for the conflict target and resolution of a bulk upsert.
fn on_conflict(
    table: &TableMetadata,
    params: &WriteParams,
) -> Result<serde_json::Value, ProblemDetails> {
    let target: Vec<String> = match &params.conflict {
        Some(columns) => columns.clone(),
        None => table
            .conflict_keys()
            .find(|(key, _)| key.is_primary)
            .map(|(_, cols)| cols.iter().map(|c| c.field_name.clone()).collect())
            .ok_or_else(|| {
                ProblemDetails::bad_request(format!(
                    "`{}` has no primary key to upsert by: name the unique key with `conflict=column`",
                    table.name
                ))
            })?,
    };
    let key = table
        .conflict_keys()
        .find(|(_, cols)| {
            cols.len() == target.len()
                && target
                    .iter()
                    .all(|t| cols.iter().any(|c| &c.field_name == t || &c.name == t))
        })
        .map(|(key, cols)| crate::catalog::models::conflict_key_value(key, &cols))
        .ok_or_else(|| {
            let keys: Vec<String> = table
                .conflict_keys()
                .map(|(_, cols)| {
                    cols.iter()
                        .map(|c| c.field_name.clone())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            ProblemDetails::invalid(
                format!(
                    "`{}` is not a unique key of `{}`; the keys are: {}",
                    target.join(","),
                    table.name,
                    keys.join("; ")
                ),
                "conflict",
            )
        })?;
    if params.resolution == Resolution::Ignore {
        if params.update.is_some() {
            return Err(ProblemDetails::invalid(
                "`update` names the columns a merge updates: it does not go with `resolution=ignore`",
                "update",
            ));
        }
        return Ok(serde_json::json!({ "key": key, "action": "NOTHING" }));
    }
    let mut on_conflict = serde_json::json!({ "key": key, "action": "UPDATE" });
    if let Some(columns) = &params.update {
        let mut names = Vec::new();
        for name in columns {
            let col = table
                .writable_columns()
                .find(|c| &c.field_name == name || &c.name == name)
                .ok_or_else(|| {
                    ProblemDetails::invalid(format!("`{name}` is not a column to update"), "update")
                })?;
            names.push(serde_json::Value::String(col.order_by_name()));
        }
        on_conflict["update"] = serde_json::Value::Array(names);
    }
    Ok(on_conflict)
}

/// Handler for `PUT /rest/{resource}`: inserts the rows of the body, updating (`merge`) or
/// leaving (`ignore`) those whose `conflict` key exists.
pub async fn rest_bulk_upsert_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, resource): Scoped<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{}/{resource}", rest_base(&schema)), async {
        let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
        let table = ctx.table()?;
        ctx.allow(table, Operation::Upsert)?;
        ctx.allow(table, Operation::CreateMany)?;
        let params = write_params(raw_query.as_deref().unwrap_or(""), Endpoint::Upsert)?;
        let body = parse_json_body(&json_body(table, &headers, body, true)?)?;
        let mut rows = bulk_rows(&ctx, body)?;
        fill_missing(&mut rows, params.missing);
        let on_conflict = on_conflict(table, &params)?;

        let m = mutation_field(
            &ctx.catalog,
            table,
            MutationAction::CreateMany,
            "Bulk upsert",
        )?;
        if !ctx.has_argument(Root::Mutation, &m.name, "onConflict") {
            return Err(ProblemDetails::bad_request(format!(
                "`{}` has no unique key to upsert by",
                table.name
            )));
        }
        let inputs = serde_json::Value::Array(rows);
        ctx.validate(Root::Mutation, &m.name, "inputs", &inputs, "body")?;
        ctx.validate(
            Root::Mutation,
            &m.name,
            "onConflict",
            &on_conflict,
            "conflict",
        )?;
        let args = HashMap::from([
            ("inputs".to_string(), inputs),
            ("onConflict".to_string(), on_conflict),
        ]);
        let (rows, count) = run_many(&ctx, table, &m, args, "PUT").await?;
        Ok(rows_response(&ctx, rows, count))
    })
    .await
}

/// `PATCH /rest/{resource}/{key}` with a JSON Patch: reads the row, applies the operations to
/// its document and updates the columns that changed, as the plain `PATCH` does. Without an
/// `If-Match` the read and the write are two statements: a row changed in between is
/// overwritten only in the columns the patch touched.
pub(super) async fn json_patch_row(
    ctx: &Ctx,
    table: &TableMetadata,
    key: &str,
    body: &[u8],
    if_match: Option<String>,
) -> RestResult {
    ctx.allow(table, Operation::Update)?;
    let parsed_key = parse_primary_key(key, table).map_err(ProblemDetails::bad_request)?;
    let patch = parse_json_body(body)?;

    let request = RequestField::new(&table.single_field)
        .with_arguments(key_args(table, &parsed_key))
        .with_children(column_fields(&ctx.catalog, table));
    let data = ctx
        .execute(
            &table.schema,
            RequestOperation::query(vec![request]),
            "PATCH",
            false,
        )
        .await?;
    let current = ResponseShaper::unwrap_single(&data, &table.single_field)
        .map_err(|e| shaping_error(e, ctx.is_prod))?;
    let Some(row) = current.as_object() else {
        return Err(ProblemDetails::not_found(format!(
            "No `{}` row found with key `{key}`",
            table.name
        )));
    };

    let mut patched = current.clone();
    patch::apply(&mut patched, &patch).map_err(|e| {
        ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("The patch does not apply: {e}"),
        )
        .with_code("PATCH_FAILED")
    })?;
    let Some(patched) = patched.as_object() else {
        return Err(ProblemDetails::bad_request(
            "A patch cannot replace the row",
        ));
    };
    let mut changes = serde_json::Map::new();
    for (name, value) in patched {
        if row.get(name) != Some(value) {
            changes.insert(name.clone(), value.clone());
        }
    }
    for name in row.keys().filter(|name| !patched.contains_key(*name)) {
        changes.insert(name.clone(), serde_json::Value::Null);
    }
    if changes.is_empty() {
        return Ok((StatusCode::OK, Json(current)).into_response());
    }
    let changes =
        serde_json::to_vec(&changes).map_err(|e| ProblemDetails::bad_request(e.to_string()))?;
    update_row(ctx, table, key, &changes, if_match, "PATCH").await
}
