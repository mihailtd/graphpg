//! Search: `POST /rest/{resource}/search/{method}` and the global `/rest/search`.
//!
//! Search is for combo boxes and global search: the best matches first, not paginated. Each
//! method a table's indexes support is its own endpoint (the GraphQL search fields):
//!
//! | Method | Index | Arguments |
//! | :--- | :--- | :--- |
//! | `fuzzy` | `pg_trgm` | `query`, `field`, `minSimilarity` |
//! | `semantic` | `pgvector` | `vector`, `field`, `minScore` |
//! | `bm25` | BM25 (ParadeDB) | `query`, `field` |
//! | `hybrid` | vector + lexical | `query`, `vector`, `lexicalWeight`, `vectorWeight`, `rrfK` |
//! | `full-text` | `tsvector` | `query`, `field` |
//! | `nearest` | PostGIS | `near`, `maxDistance`, … |
//!
//! The arguments are the ones of the GraphQL field, in the body; every search also takes
//! `first` (at most the configured `search.max_limit`), `filter` to scope the matches, and
//! `fields` to choose the columns. The response is `{"hits": [{"score": 0.9, "row": {…}}],
//! "hasMore": false}`. `GET` or `POST /rest/search` searches every searchable table at once;
//! each hit says which resource (`type`) its row is from.

use super::*;
use crate::catalog::SearchMethod;

/// The path segment of a search method.
pub fn method_segment(method: SearchMethod) -> &'static str {
    match method {
        SearchMethod::Fuzzy => "fuzzy",
        SearchMethod::Semantic => "semantic",
        SearchMethod::Bm25 => "bm25",
        SearchMethod::Hybrid => "hybrid",
        SearchMethod::FullText => "full-text",
        SearchMethod::Nearest => "nearest",
    }
}

/// Resource name of global search: `/rest/search`. A table cannot use it.
pub const GLOBAL_SEARCH_RESOURCE: &str = "search";

/// Arguments every search field has in common (they are not method arguments).
const COMMON_ARGUMENTS: [&str; 3] = ["first", "filter", "condition"];

/// `{"hits": [{"score", "row"}], "hasMore"}` from a search field's result.
fn shape_results(
    result: &serde_json::Value,
    row_of: impl Fn(&serde_json::Value) -> Option<(Option<String>, serde_json::Value)>,
) -> serde_json::Value {
    let hits: Vec<serde_json::Value> = result
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|hits| {
            hits.iter()
                .filter_map(|hit| {
                    let (kind, row) = row_of(hit.get("node")?)?;
                    let mut out = serde_json::Map::new();
                    out.insert(
                        "score".into(),
                        hit.get("score").cloned().unwrap_or_default(),
                    );
                    if let Some(kind) = kind {
                        out.insert("type".into(), serde_json::Value::String(kind));
                    }
                    out.insert("row".into(), row);
                    for extra in ["highlight", "distance"] {
                        if let Some(value) = hit.get(extra) {
                            out.insert(extra.into(), value.clone());
                        }
                    }
                    Some(serde_json::Value::Object(out))
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "hits": hits,
        "hasMore": result.get("hasMore").cloned().unwrap_or(serde_json::Value::Bool(false)),
    })
}

/// `POST /rest/{resource}/search/{method}`.
pub async fn rest_search_method_handler(
    State(state): State<Arc<ServerState>>,
    Scoped(schema, (resource, method)): Scoped<(String, String)>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(
        format!("{}/{resource}/search/{method}", rest_base(&schema)),
        async {
            let ctx = Ctx::new(&state, &headers, schema.clone(), resource).await?;
            let table = ctx.table()?;
            ctx.allow(table, Operation::Read)?;
            if raw_query.as_deref().is_some_and(|q| !q.is_empty()) {
                return Err(ProblemDetails::bad_request(
                    "Search takes its parameters in the JSON body, not the query string",
                ));
            }

            let available = ctx.catalog.search_fields(table);
            let (root, search_method) = available
                .iter()
                .find(|(_, m)| method_segment(*m) == method)
                .ok_or_else(|| {
                    let offered: Vec<&str> =
                        available.iter().map(|(_, m)| method_segment(*m)).collect();
                    ProblemDetails::not_found(format!(
                        "`{}` has no `{method}` search; it offers: {}",
                        table.name,
                        if offered.is_empty() {
                            "none".to_string()
                        } else {
                            offered.join(", ")
                        }
                    ))
                })?;

            let mut body = if body.is_empty() {
                serde_json::Map::new()
            } else {
                json_object(parse_json_body(&body)?)?
            };
            let fields = take_string_list(&mut body, "fields")?;
            let include = take_string_list(&mut body, "include")?;
            let highlight = take_highlight(&mut body, *search_method)?;

            let schema = ctx.state.current_schema();
            let accepted = validate::root_argument_names(&schema, Root::Query, root);
            let mut args = HashMap::new();
            for (name, value) in body {
                if !accepted.contains(&name) || name == "condition" {
                    let mut offered: Vec<&str> = accepted
                        .iter()
                        .map(String::as_str)
                        .filter(|a| *a != "condition")
                        .collect();
                    offered.extend(["fields", "include"]);
                    return Err(ProblemDetails::invalid(
                        format!(
                            "Unknown member `{name}` for a `{method}` search; it takes: {}",
                            offered.join(", ")
                        ),
                        name,
                    ));
                }
                let value = if name == "filter" {
                    normalize_filter_json(value)
                } else {
                    value
                };
                ctx.validate(Root::Query, root, &name, &value, &name)?;
                args.insert(name, value);
            }
            for required in validate::required_arguments(&schema, Root::Query, root) {
                if !args.contains_key(&required) && !COMMON_ARGUMENTS.contains(&required.as_str()) {
                    return Err(ProblemDetails::invalid(
                        format!("A `{method}` search needs `{required}`"),
                        required,
                    ));
                }
            }

            let selection = RestQuery {
                fields,
                include,
                ..Default::default()
            };
            let row_fields = select_fields(&ctx, table, &selection)?;
            // Full-text searches can mark the words that matched, nearest ones say how far
            let mut hit = vec![
                RequestField::new("score"),
                RequestField::new("node").with_children(row_fields),
            ];
            if let Some(arguments) = highlight {
                hit.push(RequestField::new("highlight").with_arguments(arguments));
            }
            if *search_method == SearchMethod::Nearest {
                hit.push(RequestField::new("distance"));
            }
            let request = RequestField::new(root)
                .with_arguments(args)
                .with_children(vec![
                    RequestField::new("hits").with_children(hit),
                    RequestField::new("hasMore"),
                ]);
            let data = ctx
                .execute(
                    &table.schema,
                    RequestOperation::query(vec![request]),
                    "POST",
                    false,
                )
                .await?;
            let result = ResponseShaper::unwrap_single(&data, root)
                .map_err(|e| shaping_error(e, ctx.is_prod))?;
            let out = shape_results(&result, |node| Some((None, node.clone())));
            Ok((StatusCode::OK, Json(out)).into_response())
        },
    )
    .await
}

/// `highlight` of a full-text search: `true`, or `{startSel, stopSel, maxWords, minWords,
/// maxFragments}`; the arguments of the hit's `highlight` field.
fn take_highlight(
    body: &mut serde_json::Map<String, serde_json::Value>,
    method: SearchMethod,
) -> Result<Option<HashMap<String, serde_json::Value>>, ProblemDetails> {
    let Some(value) = body.remove("highlight") else {
        return Ok(None);
    };
    if method != SearchMethod::FullText {
        return Err(ProblemDetails::invalid(
            "`highlight` marks the words of a full-text search that matched: it is for `full-text`",
            "highlight",
        ));
    }
    match value {
        serde_json::Value::Bool(true) => Ok(Some(HashMap::new())),
        serde_json::Value::Bool(false) | serde_json::Value::Null => Ok(None),
        serde_json::Value::Object(options) => {
            if let Some(unknown) = options.keys().find(|k| {
                !matches!(
                    k.as_str(),
                    "startSel" | "stopSel" | "maxWords" | "minWords" | "maxFragments"
                )
            }) {
                return Err(ProblemDetails::invalid(
                    format!("Unknown `highlight` option `{unknown}`: startSel, stopSel, maxWords, minWords, maxFragments"),
                    "highlight",
                ));
            }
            Ok(Some(options.into_iter().collect()))
        }
        _ => Err(ProblemDetails::invalid(
            "`highlight` is `true` or an object of options",
            "highlight",
        )),
    }
}

fn take_string_list(
    body: &mut serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<Option<Vec<String>>, ProblemDetails> {
    match body.remove(name) {
        None => Ok(None),
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                serde_json::Value::String(s) => Ok(s),
                _ => Err(ProblemDetails::invalid(
                    format!("`{name}` must be a list of strings"),
                    name,
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(ProblemDetails::invalid(
            format!("`{name}` must be a list of strings"),
            name,
        )),
    }
}

/// Global search over every searchable table: `GET /rest/search?query=…&first=…` or `POST`
/// with `{"query": "…", "first": 10}`.
pub async fn rest_global_search_handler(
    method: axum::http::Method,
    State(state): State<Arc<ServerState>>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{REST_PREFIX}/{GLOBAL_SEARCH_RESOURCE}"), async {
        let ctx = Ctx::new(&state, &headers, None, GLOBAL_SEARCH_RESOURCE.to_string()).await?;
        let mut params = serde_json::Map::new();
        if method == axum::http::Method::GET {
            for pair in raw_query
                .as_deref()
                .unwrap_or("")
                .split('&')
                .filter(|p| !p.is_empty())
            {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                let decode = |s: &str| {
                    percent_encoding::percent_decode_str(&s.replace('+', " "))
                        .decode_utf8_lossy()
                        .to_string()
                };
                let (key, value) = (decode(key), decode(value));
                match key.as_str() {
                    "query" => {
                        params.insert(key, serde_json::Value::String(value));
                    }
                    "first" => {
                        let n: usize = value.parse().map_err(|_| {
                            ProblemDetails::bad_request(format!(
                                "Invalid integer for `first`: {value}"
                            ))
                        })?;
                        params.insert(key, serde_json::json!(n));
                    }
                    _ => {
                        return Err(ProblemDetails::bad_request(format!(
                            "Unknown query parameter `{key}`"
                        )))
                    }
                }
            }
        } else {
            if raw_query.as_deref().is_some_and(|q| !q.is_empty()) {
                return Err(ProblemDetails::bad_request(
                    "POST takes its parameters in the JSON body, not the query string",
                ));
            }
            params = if body.is_empty() {
                serde_json::Map::new()
            } else {
                json_object(parse_json_body(&body)?)?
            };
            if let Some(unknown) = params
                .keys()
                .find(|k| !matches!(k.as_str(), "query" | "first"))
            {
                return Err(ProblemDetails::invalid(
                    format!("Unknown member `{unknown}`; global search takes `query` and `first`"),
                    unknown.clone(),
                ));
            }
        }
        let query = match params.get("query") {
            Some(serde_json::Value::String(q)) if !q.is_empty() => q.clone(),
            _ => {
                return Err(ProblemDetails::invalid(
                    "Global search needs a `query`",
                    "query",
                ))
            }
        };

        let root = ctx
            .catalog
            .global_search_field(&state.default_schema)
            .or_else(|| ctx.catalog.global_search_field_any())
            .ok_or_else(|| {
                ProblemDetails::not_found("Global search is not available: no table is searchable")
            })?;
        let tables: Vec<&TableMetadata> = ctx
            .catalog
            .global_search_tables()
            .into_iter()
            .map(|(table, _)| table)
            .filter(|t| ctx.allow(t, Operation::Read).is_ok())
            .collect();

        let mut node = vec![RequestField::new("__typename")];
        for table in &tables {
            node.extend(
                column_fields(&ctx.catalog, table)
                    .into_iter()
                    .map(|f| f.with_type_condition(&table.type_name)),
            );
        }
        let mut args = HashMap::from([("query".to_string(), serde_json::Value::String(query))]);
        if let Some(first) = params.get("first") {
            args.insert("first".to_string(), first.clone());
        }
        let request = RequestField::new(root)
            .with_arguments(args)
            .with_children(vec![
                RequestField::new("hits").with_children(vec![
                    RequestField::new("score"),
                    RequestField::new("node").with_children(node),
                ]),
                RequestField::new("hasMore"),
            ]);
        let schema = tables
            .first()
            .map_or(state.default_schema.as_str(), |t| t.schema.as_str());
        let data = ctx
            .execute(schema, RequestOperation::query(vec![request]), "GET", false)
            .await?;
        let result = ResponseShaper::unwrap_single(&data, root)
            .map_err(|e| shaping_error(e, ctx.is_prod))?;
        let out = shape_results(&result, |node| {
            let type_name = node.get("__typename")?.as_str()?;
            let table = tables.iter().find(|t| t.type_name == type_name)?;
            let mut row = node.clone();
            row.as_object_mut()?.remove("__typename");
            Some((Some(table.name.clone()), row))
        });
        Ok((StatusCode::OK, Json(out)).into_response())
    })
    .await
}
