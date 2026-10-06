//! Functions: `GET` / `POST /rest/rpc/{function}`.
//!
//! Every function the schema exposes as a root field is callable by its SQL name. `STABLE` and
//! `IMMUTABLE` functions answer `GET` (arguments as query parameters) and `POST` (arguments in
//! the body's `args` object); `VOLATILE` ones, which change data, answer `POST` only. What a
//! function returns decides the response:
//!
//! | The function returns | Response |
//! | :--- | :--- |
//! | a set of rows of a table | a list of rows, filtered, ordered and paged like the table's own list |
//! | one row of a table | the row (or `null`) |
//! | a value (base type, enum, composite, array) or columns of its own | the JSON value, or a list for a set |
//! | `void` | `204` |
//!
//! Arguments use the GraphQL names (`camelCase`). A function result can be read and written in
//! one transaction like any mutation; an error is a problem+json like everywhere else.

use super::*;
use crate::catalog::{FunctionArgument, FunctionMetadata, FunctionReturn, PgType};
use axum::http::Method;

/// The path segment functions live under: `/rest/rpc/{function}`. A table cannot use it.
pub const RPC_RESOURCE: &str = "rpc";

/// The functions the REST API serves: root fields of the schema (not computed columns, not
/// omitted ones), by SQL name. When several schemas have one, the default schema's wins.
pub fn rpc_functions<'a>(
    catalog: &'a DatabaseCatalog,
    default_schema: &str,
) -> Vec<&'a FunctionMetadata> {
    let mut all: Vec<&FunctionMetadata> = [false, true]
        .into_iter()
        .flat_map(|mutation| catalog.function_root_fields(mutation))
        .filter(|f| !f.row_argument && catalog.function_is_exposed(f))
        .collect();
    all.sort_by_key(|f| f.schema != default_schema);
    let mut seen = std::collections::HashSet::new();
    all.retain(|f| seen.insert(f.name.clone()));
    all
}

/// What a function's result becomes.
enum Shape<'a> {
    /// Rows of a table, a set read as a connection (filter, order, pagination).
    Connection(&'a TableMetadata),
    /// One row, or the rows of a function that writes.
    Rows(&'a TableMetadata),
    Value,
    Record,
    Void,
}

fn shape<'a>(
    catalog: &'a DatabaseCatalog,
    f: &FunctionMetadata,
) -> Result<Shape<'a>, ProblemDetails> {
    Ok(match &f.returns {
        FunctionReturn::Table { schema, table } => {
            let table = catalog.find_table(schema, table).ok_or_else(|| {
                ProblemDetails::not_found(format!("The rows of `{}` are not available", f.name))
            })?;
            if f.returns_set && !f.volatile {
                Shape::Connection(table)
            } else {
                Shape::Rows(table)
            }
        }
        FunctionReturn::Value(_) => Shape::Value,
        FunctionReturn::Record => Shape::Record,
        FunctionReturn::Void => Shape::Void,
    })
}

/// A query-string value as the JSON the argument takes.
fn parse_argument_text(
    text: &str,
    arg: &FunctionArgument,
) -> Result<serde_json::Value, ProblemDetails> {
    let name = arg.graphql_name();
    let invalid = |e: String| ProblemDetails::invalid(e, name.clone());
    let column = arg.as_column();
    match &arg.pg_type {
        PgType::Array(element) if !text.starts_with('[') => {
            let element = ColumnMetadata {
                pg_type: element.as_ref().clone(),
                ..column
            };
            text.split(',')
                .map(|item| query::parse_scalar_value(item.trim(), Some(&element), &name))
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array)
                .map_err(invalid)
        }
        PgType::Array(_) | PgType::Json | PgType::Jsonb | PgType::Composite(_)
            if text.starts_with('{') || text.starts_with('[') =>
        {
            serde_json::from_str(text)
                .map_err(|e| invalid(format!("`{name}` is not valid JSON: {e}")))
        }
        PgType::Json | PgType::Jsonb | PgType::Composite(_) => {
            Ok(serde_json::Value::String(text.to_string()))
        }
        _ => query::parse_scalar_value(text, Some(&column), &name).map_err(invalid),
    }
}

/// Splits the query string into function arguments and the remaining parameters.
fn split_query(
    raw: &str,
    f: &FunctionMetadata,
) -> Result<(HashMap<String, String>, String), ProblemDetails> {
    let names: Vec<String> = f.arguments.iter().map(|a| a.graphql_name()).collect();
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(&s.replace('+', " "))
            .decode_utf8_lossy()
            .to_string()
    };
    let (mut args, mut rest) = (HashMap::new(), Vec::new());
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(key);
        if names.contains(&key) {
            if args.insert(key.clone(), decode(value)).is_some() {
                return Err(ProblemDetails::invalid(
                    format!("Argument `{key}` is given twice"),
                    key,
                ));
            }
        } else {
            rest.push(pair);
        }
    }
    Ok((args, rest.join("&")))
}

/// Typed, validated arguments in the shape the planner takes.
fn bind_arguments(
    ctx: &Ctx,
    f: &FunctionMetadata,
    mut given: serde_json::Map<String, serde_json::Value>,
) -> Result<HashMap<String, serde_json::Value>, ProblemDetails> {
    let root = if f.volatile {
        Root::Mutation
    } else {
        Root::Query
    };
    let known: Vec<String> = f.arguments.iter().map(|a| a.graphql_name()).collect();
    if let Some(unknown) = given.keys().find(|k| !known.contains(k)) {
        return Err(ProblemDetails::invalid(
            format!(
                "Unknown argument `{unknown}` for `{}`; it takes: {}",
                f.name,
                if known.is_empty() {
                    "no arguments".to_string()
                } else {
                    known.join(", ")
                }
            ),
            format!("args.{unknown}"),
        ));
    }
    let mut out = HashMap::new();
    for arg in &f.arguments {
        let name = arg.graphql_name();
        match given.remove(&name) {
            Some(value) => {
                ctx.validate(root, &f.field_name, &name, &value, &format!("args.{name}"))?;
                out.insert(name, value);
            }
            None if !arg.has_default => {
                return Err(ProblemDetails::invalid(
                    format!("Missing argument `{name}` for `{}`", f.name),
                    format!("args.{name}"),
                ));
            }
            None => {}
        }
    }
    Ok(out)
}

fn method_not_allowed(f: &FunctionMetadata) -> ProblemDetails {
    let mut problem = ProblemDetails::new(
        StatusCode::METHOD_NOT_ALLOWED,
        format!("`{}` changes data, so it is called with POST", f.name),
    )
    .with_code("METHOD_NOT_ALLOWED");
    problem
        .headers
        .push((header::ALLOW, HeaderValue::from_static("POST")));
    problem
}

/// `GET` / `POST /rest/rpc/{function}`.
pub async fn rest_rpc_handler(
    method: Method,
    State(state): State<Arc<ServerState>>,
    Path(name): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{REST_PREFIX}/{RPC_RESOURCE}/{name}"), async {
        let ctx = Ctx::new(&state, &headers, name.clone()).await?;
        let f = rpc_functions(&ctx.catalog, &state.default_schema)
            .into_iter()
            .find(|f| f.name == name)
            .ok_or_else(|| ProblemDetails::not_found(format!("Function `{name}` not found")))?;
        let is_get = method == Method::GET;
        if is_get && f.volatile {
            return Err(method_not_allowed(f));
        }
        let shape = shape(&ctx.catalog, f)?;

        // Arguments and, for functions returning rows, the options of a list
        let (arguments, options): (serde_json::Map<_, _>, Options) = if is_get {
            let (args, rest) = split_query(raw_query.as_deref().unwrap_or(""), f)?;
            let mut typed = serde_json::Map::new();
            for arg in &f.arguments {
                if let Some(text) = args.get(&arg.graphql_name()) {
                    typed.insert(arg.graphql_name(), parse_argument_text(text, arg)?);
                }
            }
            (typed, Options::Query(rest))
        } else {
            if raw_query.as_deref().is_some_and(|q| !q.is_empty()) {
                return Err(ProblemDetails::bad_request(
                    "POST takes its arguments and options in the JSON body, not the query string",
                ));
            }
            let mut body = if body.is_empty() {
                serde_json::Map::new()
            } else {
                json_object(parse_json_body(&body)?)?
            };
            let args = match body.remove("args") {
                None => serde_json::Map::new(),
                Some(serde_json::Value::Object(args)) => args,
                Some(_) => return Err(ProblemDetails::invalid("`args` must be an object", "args")),
            };
            (args, Options::Body(body))
        };
        let args = bind_arguments(&ctx, f, arguments)?;

        match shape {
            Shape::Connection(table) => {
                ctx.allow(table, Operation::Read)?;
                let query = options.into_query(Some((table, QueryKind::Collection)), &ctx)?;
                let target = ListTarget {
                    table,
                    root: &f.field_name,
                    connection: true,
                    args,
                };
                let listing =
                    list_rows(&ctx, target, &query, if is_get { "GET" } else { "POST" }).await?;
                let link = is_get
                    .then(|| {
                        build_link_header(
                            &format!("{RPC_RESOURCE}/{}", f.name),
                            raw_query.as_deref(),
                            listing.page_info.as_ref(),
                            &query,
                        )
                    })
                    .flatten();
                Ok(listing_response(
                    listing,
                    ctx.prefs,
                    link,
                    !is_get,
                    Media::Json,
                ))
            }
            other => {
                let (children, table) = match &other {
                    Shape::Rows(table) => {
                        ctx.allow(table, Operation::Read)?;
                        let query = options.into_query(Some((table, QueryKind::Item)), &ctx)?;
                        (
                            select_fields(
                                &ctx.catalog,
                                table,
                                query.fields.as_ref(),
                                query.include.as_ref(),
                            )?,
                            Some(*table),
                        )
                    }
                    Shape::Value => {
                        options.into_query(None, &ctx)?;
                        let FunctionReturn::Value(pg_type) = &f.returns else {
                            unreachable!("a value shape")
                        };
                        (value_selection(&ctx.catalog, pg_type, 0), None)
                    }
                    Shape::Record => {
                        options.into_query(None, &ctx)?;
                        let columns = f
                            .return_columns
                            .iter()
                            .map(|c| {
                                RequestField::new(crate::catalog::models::to_camel_case(&c.name))
                                    .with_children(value_selection(&ctx.catalog, &c.pg_type, 0))
                            })
                            .collect();
                        (columns, None)
                    }
                    _ => {
                        options.into_query(None, &ctx)?;
                        (Vec::new(), None)
                    }
                };
                let field = RequestField::new(&f.field_name)
                    .with_arguments(args)
                    .with_children(children);
                let op = if f.volatile {
                    RequestOperation::mutation(vec![field])
                } else {
                    RequestOperation::query(vec![field])
                };
                let schema = table.map_or(f.schema.as_str(), |t| t.schema.as_str());
                let data = ctx
                    .execute(schema, op, if is_get { "GET" } else { "POST" }, f.volatile)
                    .await?;
                if matches!(other, Shape::Void) {
                    return Ok(StatusCode::NO_CONTENT.into_response());
                }
                let value = ResponseShaper::unwrap_single(&data, &f.field_name)
                    .map_err(|e| shaping_error(e, ctx.is_prod))?;
                Ok((StatusCode::OK, Json(value)).into_response())
            }
        }
    })
    .await
}

/// The list options of a function call: the rest of the query string (`GET`) or the body
/// without `args` (`POST`).
enum Options {
    Query(String),
    Body(serde_json::Map<String, serde_json::Value>),
}

impl Options {
    /// The options as a [`RestQuery`] for the rows of `target`; a function that returns
    /// anything else takes none.
    fn into_query(
        self,
        target: Option<(&TableMetadata, QueryKind)>,
        ctx: &Ctx,
    ) -> Result<RestQuery, ProblemDetails> {
        let Some((table, kind)) = target else {
            return match self {
                Options::Query(rest) if rest.is_empty() => Ok(RestQuery::default()),
                Options::Body(body) if body.is_empty() => Ok(RestQuery::default()),
                Options::Query(rest) => Err(ProblemDetails::bad_request(format!(
                    "Unknown query parameter in `{rest}`: this function takes only its arguments"
                ))),
                Options::Body(body) => Err(ProblemDetails::invalid(
                    format!(
                        "Unknown member `{}`: this function takes only `args`",
                        body.keys().next().map(String::as_str).unwrap_or_default()
                    ),
                    "body",
                )),
            };
        };
        match self {
            Options::Query(rest) => parse_query_params(&rest, kind, table, &ctx.catalog)
                .map_err(ProblemDetails::bad_request),
            Options::Body(body) => {
                if kind == QueryKind::Item {
                    if let Some(key) = body
                        .keys()
                        .find(|k| !matches!(k.as_str(), "fields" | "include"))
                    {
                        return Err(ProblemDetails::invalid(
                            format!("`{key}` is not supported for a function returning one row; use `fields` and `include`"),
                            key.clone(),
                        ));
                    }
                }
                serde_json::from_value::<SearchBody>(serde_json::Value::Object(body))
                    .map_err(|e| ProblemDetails::bad_request(format!("Invalid options: {e}")))?
                    .into_query(table, &ctx.catalog)
            }
        }
    }
}
