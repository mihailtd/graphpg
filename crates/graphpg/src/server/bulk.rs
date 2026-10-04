//! Bulk data in and out, streamed rather than built in memory:
//!
//! - `POST /export`: a GraphQL operation whose single root field is a connection, answered
//!   as its nodes one per line (NDJSON) or as CSV, read from a server-side portal in batches,
//!   so neither PostgreSQL nor the engine holds the whole result.
//! - `POST /import/{Type}`: `Create<Type>Input` rows, one JSON object per line (NDJSON),
//!   checked like mutation input and streamed into a binary `COPY` of a staging table, then
//!   inserted in one statement (converted to the columns' types as writes are) — in one
//!   transaction: every row is inserted, or none.
use super::http::ServerState;
use crate::catalog::DatabaseCatalog;
use crate::compiler::{SqlCompiler, WriteContext, WriteVerb};
use crate::execution::ExecutionError;
use crate::planner::{PlannerError, QueryPlanner};
use apollo_compiler::ExecutableDocument;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio_postgres::types::{ToSql, Type};

/// Rows fetched from the export's portal at a time.
const EXPORT_BATCH: i32 = 1000;

#[derive(Debug, Deserialize)]
pub struct ExportRequest {
    pub query: String,
    #[serde(rename = "operationName")]
    pub operation_name: Option<String>,
    pub variables: Option<serde_json::Value>,
    /// `ndjson` (default) or `csv`.
    pub format: Option<String>,
}

fn json_error(status: StatusCode, message: impl Into<String>, extensions: Option<serde_json::Value>) -> Response {
    let mut error = serde_json::json!({ "message": message.into() });
    if let Some(ext) = extensions {
        error["extensions"] = ext;
    }
    (status, Json(serde_json::json!({ "errors": [error] }))).into_response()
}

/// `POST /export`.
pub async fn export_handler(State(state): State<Arc<ServerState>>, request_headers: HeaderMap, Json(req): Json<ExportRequest>) -> Response {
    // Rows are read as the request's role, under its Row-Level Security policies
    let auth_ctx = match state.authenticate(&request_headers).await {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let csv = match req.format.as_deref() {
        None | Some("ndjson") => false,
        Some("csv") => true,
        Some(other) => return json_error(StatusCode::BAD_REQUEST, format!("unknown export format `{other}` (ndjson or csv)"), None),
    };
    let schema = state.current_schema();
    let catalog = state.current_catalog();
    let doc = match ExecutableDocument::parse_and_validate(&schema, &req.query, "export.graphql") {
        Ok(doc) => doc,
        Err(err) => return json_error(StatusCode::BAD_REQUEST, err.to_string(), None),
    };
    let operation = match doc.operations.get(req.operation_name.as_deref()) {
        Ok(op) if op.operation_type.is_query() => op,
        Ok(_) => return json_error(StatusCode::BAD_REQUEST, "exports run queries", None),
        Err(err) => return json_error(StatusCode::BAD_REQUEST, err.to_graphql_error(&doc.sources).message, None),
    };
    let planner = QueryPlanner::with_context(&catalog, &state.default_schema, Some(&doc), req.variables.as_ref());
    let plan = match planner.plan_operation(operation) {
        Ok(plan) => plan,
        Err(err) => return json_error(StatusCode::BAD_REQUEST, err.to_string(), None),
    };
    let Some(query) = SqlCompiler::compile_rows(&plan) else {
        return json_error(StatusCode::BAD_REQUEST, "an export's operation selects exactly one connection field (its `nodes`)", None);
    };

    let mut client = match state.read_pool(None).await {
        Ok((client, _)) => client,
        Err(message) => return json_error(StatusCode::SERVICE_UNAVAILABLE, message, None),
    };
    let (mut tx_out, rx) = futures::channel::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(async move {
        let result: Result<(), tokio_postgres::Error> = async {
            let transaction = client.transaction().await?;
            if let Some(auth) = &auth_ctx {
                auth.apply_on(&*transaction).await?;
            }
            let statement = transaction.prepare(&query.sql).await?;
            let params: Vec<&(dyn ToSql + Sync)> = query.params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
            let portal = transaction.bind(&statement, &params).await?;
            let mut header: Option<Vec<String>> = None;
            loop {
                let rows = transaction.query_portal(&portal, EXPORT_BATCH).await?;
                if rows.is_empty() {
                    break;
                }
                let mut chunk = String::new();
                for row in &rows {
                    let value: serde_json::Value = row.get(0);
                    if csv {
                        let keys = header.get_or_insert_with(|| {
                            let keys: Vec<String> = value.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                            chunk.push_str(&csv_line(keys.iter().map(|k| Some(k.clone()))));
                            keys
                        });
                        chunk.push_str(&csv_line(keys.iter().map(|k| csv_cell(value.get(k)))));
                    } else {
                        chunk.push_str(&value.to_string());
                        chunk.push('\n');
                    }
                }
                if tx_out.send(Ok(Bytes::from(chunk))).await.is_err() {
                    // The client went away
                    return Ok(());
                }
                if rows.len() < EXPORT_BATCH as usize {
                    break;
                }
            }
            transaction.commit().await
        }
        .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "export failed while streaming");
            // Rows already sent cannot be taken back: NDJSON gets a last error line, and the
            // stream then ends with an error so the transfer is visibly incomplete
            if !csv {
                let line = serde_json::json!({ "error": err.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| err.to_string()) });
                let _ = tx_out.send(Ok(Bytes::from(format!("{line}\n")))).await;
            }
            let _ = tx_out.send(Err(std::io::Error::other(err.to_string()))).await;
        }
    });

    let content_type = if csv { "text/csv; charset=utf-8" } else { "application/x-ndjson" };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    (StatusCode::OK, headers, Body::from_stream(rx)).into_response()
}

/// A CSV record (RFC 4180): fields quoted when they hold separators, quotes or line breaks.
fn csv_line(fields: impl Iterator<Item = Option<String>>) -> String {
    let cells: Vec<String> = fields
        .map(|f| match f {
            None => String::new(),
            Some(text) if text.contains([',', '"', '\n', '\r']) => format!("\"{}\"", text.replace('"', "\"\"")),
            Some(text) => text,
        })
        .collect();
    format!("{}\r\n", cells.join(","))
}

/// A node field as a CSV cell: text as it is, null as empty, objects and lists as JSON.
fn csv_cell(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// `POST /import/{Type}`.
pub async fn import_handler(
    State(state): State<Arc<ServerState>>,
    Path(type_name): Path<String>,
    request_headers: HeaderMap,
    body: Body,
) -> Response {
    // Rows are inserted as the request's role, under its Row-Level Security policies
    let auth_ctx = match state.authenticate(&request_headers).await {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let catalog = state.current_catalog();
    let Some(table_name) = catalog.table_for_type(&state.default_schema, &type_name) else {
        return json_error(StatusCode::NOT_FOUND, format!("no type `{type_name}`"), None);
    };
    let Some(table) = catalog.find_table(&state.default_schema, table_name) else {
        return json_error(StatusCode::NOT_FOUND, format!("no type `{type_name}`"), None);
    };
    if !table.operations.create {
        return json_error(StatusCode::BAD_REQUEST, format!("`{type_name}` rows cannot be created"), None);
    }
    let mut client = match state.pool.get().await {
        Ok(client) => client,
        Err(err) => return json_error(StatusCode::SERVICE_UNAVAILABLE, format!("Database unavailable: {err}"), None),
    };
    match import(&state, &catalog, table, &mut client, auth_ctx.as_ref(), body).await {
        Ok(count) => (StatusCode::OK, Json(serde_json::json!({ "data": { "inserted": count } }))).into_response(),
        Err(ImportError::Input(line, err)) => {
            let errors: Vec<serde_json::Value> = match &err {
                PlannerError::Input(errors) => errors
                    .iter()
                    .map(|e| serde_json::json!({ "message": e.message, "extensions": { "code": e.code, "field": e.path, "line": line } }))
                    .collect(),
                other => vec![serde_json::json!({ "message": other.to_string(), "extensions": { "code": "BAD_USER_INPUT", "line": line } })],
            };
            (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "errors": errors }))).into_response()
        }
        Err(ImportError::Body(message)) => json_error(StatusCode::BAD_REQUEST, message, None),
        Err(ImportError::Database(err)) => {
            let err = ExecutionError::Write {
                error: err,
                context: Some(WriteContext { schema: table.schema.clone(), table: table.name.clone(), verb: WriteVerb::Insert, rows: Vec::new() }),
            };
            match err.violation(&catalog) {
                Some(v) => json_error(
                    StatusCode::OK,
                    format!("{} (nothing was imported)", v.message),
                    Some(serde_json::json!({ "code": v.code, "type": v.type_name, "constraint": v.constraint, "fields": v.fields })),
                ),
                None => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{err} (nothing was imported)"), None),
            }
        }
    }
}

enum ImportError {
    /// An invalid row, by line number (1-based).
    Input(usize, PlannerError),
    Body(String),
    Database(tokio_postgres::Error),
}

impl From<tokio_postgres::Error> for ImportError {
    fn from(err: tokio_postgres::Error) -> Self {
        ImportError::Database(err)
    }
}

/// Streams the rows of `body` into `table`; returns how many were inserted.
async fn import(
    state: &ServerState,
    catalog: &DatabaseCatalog,
    table: &crate::catalog::TableMetadata,
    client: &mut deadpool_postgres::Object,
    auth_ctx: Option<&super::AuthSessionContext>,
    body: Body,
) -> Result<u64, ImportError> {
    let planner = QueryPlanner::new(catalog, &state.default_schema);
    let transaction = client.transaction().await?;
    if let Some(auth) = auth_ctx {
        auth.apply_on(&*transaction).await?;
    }
    let mut stream = body.into_data_stream();
    let mut pending = Vec::<u8>::new();
    let mut line_no = 0usize;
    // Set by the first row: its fields are every row's
    let mut columns: Option<Vec<crate::plan::ColumnValue>> = None;
    let mut writer: Option<std::pin::Pin<Box<tokio_postgres::binary_copy::BinaryCopyInWriter>>> = None;

    let handle_line = |line: &[u8], line_no: usize| -> Result<Option<Vec<crate::plan::ColumnValue>>, ImportError> {
        let text = std::str::from_utf8(line).map_err(|_| ImportError::Body(format!("line {line_no} is not UTF-8")))?.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let row: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ImportError::Body(format!("line {line_no} is not a JSON object: {e}")))?;
        let values = planner.import_values(table, &row, &format!("line {line_no}")).map_err(|e| ImportError::Input(line_no, e))?;
        Ok(Some(values))
    };

    let mut finished = false;
    while !finished {
        let chunk = match stream.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(err)) => return Err(ImportError::Body(format!("reading the request body failed: {err}"))),
            None => {
                finished = true;
                Bytes::new()
            }
        };
        pending.extend_from_slice(&chunk);
        let mut lines: Vec<Vec<u8>> = Vec::new();
        while let Some(at) = pending.iter().position(|b| *b == b'\n') {
            lines.push(pending.drain(..=at).collect());
        }
        if finished && !pending.is_empty() {
            lines.push(std::mem::take(&mut pending));
        }
        for line in lines {
            line_no += 1;
            let Some(values) = handle_line(&line, line_no)? else { continue };
            if columns.is_none() {
                // Stage every value as text; the insert converts them
                let staging: Vec<String> = (0..values.len()).map(|j| format!("c{j} text")).collect();
                transaction
                    .batch_execute(&format!("CREATE TEMP TABLE _import ({}) ON COMMIT DROP", staging.join(", ")))
                    .await?;
                let sink = transaction.copy_in("COPY _import FROM STDIN (FORMAT binary)").await?;
                writer = Some(Box::pin(tokio_postgres::binary_copy::BinaryCopyInWriter::new(sink, &vec![Type::TEXT; values.len()])));
                columns = Some(values.clone());
            }
            let expected = columns.as_ref().expect("set above");
            let mut cells: Vec<Option<String>> = Vec::with_capacity(expected.len());
            for spec in expected {
                cells.push(values.iter().find(|v| v.column == spec.column).and_then(|v| SqlCompiler::value_text(&v.value)));
            }
            if values.len() != expected.len() || values.iter().any(|v| !expected.iter().any(|e| e.column == v.column)) {
                return Err(ImportError::Body(format!("line {line_no} must set the same fields as the first row")));
            }
            let refs: Vec<&(dyn ToSql + Sync)> = cells.iter().map(|c| c as &(dyn ToSql + Sync)).collect();
            writer.as_mut().expect("set above").as_mut().write(&refs).await?;
        }
    }
    let (Some(columns), Some(mut writer)) = (columns, writer) else {
        return Ok(0);
    };
    writer.as_mut().finish().await?;
    let inserted = transaction.execute(&SqlCompiler::import_statement(&table.schema, &table.name, &columns, "_import"), &[]).await?;
    transaction.commit().await?;
    Ok(inserted)
}
