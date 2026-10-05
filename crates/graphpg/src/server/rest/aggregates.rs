//! Aggregates: `GET` / `POST /rest/{resource}/aggregate`.
//!
//! The rows a filter selects, summarised: one object of totals, or with `group` one object per
//! group, in one scan of the table. The members are the same for the query string and the
//! JSON body:
//!
//! | | |
//! | :--- | :--- |
//! | `filter` | which rows, like a list's |
//! | `min`, `max`, `sum`, `avg`, `distinctCount`, `stddevSample`, `stddevPopulation`, `varianceSample`, `variancePopulation`, `median` | the columns to aggregate |
//! | `percentile` and `fraction` | columns and the fraction (0 to 1) of `percentile_cont` |
//! | `group` | what to group by: `column`, or `column:unit` with a unit of `minute` … `year` or `binned` |
//! | `having` | conditions on the groups: `{"count": {"greaterThan": 5}, "sum": {"total": {"greaterThan": 100}}}` |
//! | `timeZone`, `binWidth`, `binOrigin`, `fillGaps` | how groups are truncated or binned |
//!
//! Without `group` the response is `{"count": n, "sum": {"total": 12}, …}`; with it a list of
//! `{"keys": {"status": "paid"}, "count": n, "sum": {…}, …}`. Which aggregates and columns exist
//! is what the GraphQL schema offers for the table (sums of numbers, minimums of orderable
//! columns, …); asking for anything else is a 400.

use super::*;

/// Aggregate functions that take a list of columns.
const FUNCTIONS: [&str; 10] = [
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

/// Most values one request groups by.
pub const MAX_GROUP_KEYS: usize = 4;
/// Most groups a response holds; more is a 422 (narrow the filter or group more coarsely).
pub const MAX_GROUPS: usize = 10_000;

/// The members of an aggregate request.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct AggregateBody {
    filter: Option<serde_json::Value>,
    group: Option<Vec<String>>,
    min: Option<Vec<String>>,
    max: Option<Vec<String>>,
    sum: Option<Vec<String>>,
    avg: Option<Vec<String>>,
    distinct_count: Option<Vec<String>>,
    stddev_sample: Option<Vec<String>>,
    stddev_population: Option<Vec<String>>,
    variance_sample: Option<Vec<String>>,
    variance_population: Option<Vec<String>>,
    median: Option<Vec<String>>,
    percentile: Option<Vec<String>>,
    fraction: Option<f64>,
    having: Option<serde_json::Value>,
    time_zone: Option<String>,
    bin_width: Option<String>,
    bin_origin: Option<String>,
    fill_gaps: Option<bool>,
}

impl AggregateBody {
    /// The requested aggregate functions with their columns, in a stable order.
    fn functions(&self) -> Vec<(&'static str, &Vec<String>)> {
        let lists = [
            ("min", &self.min),
            ("max", &self.max),
            ("sum", &self.sum),
            ("avg", &self.avg),
            ("distinctCount", &self.distinct_count),
            ("stddevSample", &self.stddev_sample),
            ("stddevPopulation", &self.stddev_population),
            ("varianceSample", &self.variance_sample),
            ("variancePopulation", &self.variance_population),
            ("median", &self.median),
            ("percentile", &self.percentile),
        ];
        lists
            .into_iter()
            .filter_map(|(name, columns)| columns.as_ref().map(|c| (name, c)))
            .collect()
    }
}

/// `having[count][gt]=5`, `having[sum][total][gt]=100`: a path ending in an operator.
fn having_from_segments(segments: &[String], value: &str) -> Result<serde_json::Value, String> {
    let what = format!("having[{}]", segments.join("]["));
    let [path @ .., op] = segments else {
        return Err("`having` needs a path: `having[count][gt]=5`".to_string());
    };
    let op = query::normalize_operator(op);
    if path.is_empty() || op.is_empty() {
        return Err(format!(
            "`{what}` must name an aggregate and end with an operator"
        ));
    }
    let number = query::parse_scalar_value(value, None, &what)?;
    let mut current = serde_json::json!({ op: number });
    for segment in path.iter().rev() {
        current = serde_json::json!({ segment: current });
    }
    Ok(current)
}

/// Reads the query string of `GET …/aggregate`.
fn body_from_query(
    raw: &str,
    table: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> Result<AggregateBody, ProblemDetails> {
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(&s.replace('+', " "))
            .decode_utf8_lossy()
            .to_string()
    };
    let mut map = serde_json::Map::new();
    let merge = |map: &mut serde_json::Map<String, serde_json::Value>,
                 key: &str,
                 value: serde_json::Value| {
        match map.get_mut(key) {
            Some(target) => merge_json(target, value),
            None => {
                map.insert(key.to_string(), value);
            }
        }
    };
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (key, value) = (decode(key), decode(value));
        let bad = |e: String| ProblemDetails::bad_request(e);
        match query::parse_bracket_path(&key) {
            Some((root, segments)) if root == "filter" && !segments.is_empty() => {
                let filter =
                    query::segments_to_filter(&segments, &value, table, catalog).map_err(bad)?;
                merge(&mut map, "filter", filter);
            }
            Some((root, segments)) if root == "having" && !segments.is_empty() => {
                merge(
                    &mut map,
                    "having",
                    having_from_segments(&segments, &value).map_err(bad)?,
                );
            }
            Some((root, segments)) if segments.is_empty() => match root.as_str() {
                "filter" => {
                    let parsed: serde_json::Value = serde_json::from_str(&value)
                        .map_err(|e| bad(format!("`filter` must be a JSON object: {e}")))?;
                    if !parsed.is_object() {
                        return Err(bad("`filter` must be a JSON object".to_string()));
                    }
                    merge(&mut map, "filter", normalize_filter_json(parsed));
                }
                "having" => {
                    let parsed: serde_json::Value = serde_json::from_str(&value)
                        .map_err(|e| bad(format!("`having` must be a JSON object: {e}")))?;
                    merge(&mut map, "having", parsed);
                }
                "fraction" => {
                    let fraction: f64 = value
                        .parse()
                        .map_err(|_| bad(format!("`fraction` must be a number, got `{value}`")))?;
                    map.insert(root, serde_json::json!(fraction));
                }
                "fillGaps" => {
                    let flag = match value.as_str() {
                        "true" | "1" | "" => true,
                        "false" | "0" => false,
                        _ => {
                            return Err(bad(format!(
                                "`fillGaps` must be true or false, got `{value}`"
                            )))
                        }
                    };
                    map.insert(root, serde_json::json!(flag));
                }
                "group" | "percentile" => {
                    map.insert(root, serde_json::json!(query::split_list(&value)));
                }
                name if FUNCTIONS.contains(&name) => {
                    map.insert(root, serde_json::json!(query::split_list(&value)));
                }
                // `timeZone`, `binWidth`, `binOrigin`, and unknown names (rejected below)
                _ => {
                    map.insert(root, serde_json::Value::String(value));
                }
            },
            _ => return Err(bad(format!("Unknown query parameter `{key}`"))),
        }
    }
    serde_json::from_value(serde_json::Value::Object(map))
        .map_err(|e| ProblemDetails::bad_request(format!("Invalid aggregate request: {e}")))
}

/// The enum value of `groupBy` for `column` or `column:unit`.
fn group_value(table: &TableMetadata, item: &str) -> Result<String, ProblemDetails> {
    let (name, unit) = match item.split_once(':') {
        Some((name, unit)) => (name.trim(), Some(unit.trim().to_ascii_lowercase())),
        None => (item.trim(), None),
    };
    let unit = unit.map(|u| if u == "binned" { "bin".to_string() } else { u });
    let values = table.group_by_values();
    let known: Vec<&str> = values
        .iter()
        .filter(|(_, c, _)| c.field_name == name || c.name == name)
        .map(|(_, _, u)| u.unwrap_or("none"))
        .collect();
    values
        .iter()
        .find(|(_, c, u)| (c.field_name == name || c.name == name) && *u == unit.as_deref())
        .map(|(value, _, _)| value.clone())
        .ok_or_else(|| {
            let detail = if known.is_empty() {
                format!(
                    "`{table_name}` cannot be grouped by `{name}`",
                    table_name = table.name
                )
            } else {
                format!(
                    "`{name}` cannot be grouped by `{}`; it can be grouped as: {}",
                    unit.as_deref().unwrap_or("itself"),
                    known
                        .iter()
                        .map(|u| match *u {
                            "none" => name.to_string(),
                            "bin" => format!("{name}:binned"),
                            u => format!("{name}:{u}"),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            ProblemDetails::invalid(detail, "group")
        })
}

/// Selections of the requested aggregate functions, checked against what the schema offers.
fn function_selections(
    ctx: &Ctx,
    table: &TableMetadata,
    aggregates_type: Option<&str>,
    body: &AggregateBody,
) -> Result<Vec<RequestField>, ProblemDetails> {
    let schema = ctx.state.current_schema();
    let mut out = Vec::new();
    for (function, columns) in body.functions() {
        if columns.is_empty() {
            continue;
        }
        let function_type =
            aggregates_type.and_then(|t| validate::field_type_name(&schema, t, function));
        if aggregates_type.is_some() && function_type.is_none() {
            return Err(ProblemDetails::invalid(
                format!("`{function}` is not available for `{}`", table.name),
                function,
            ));
        }
        let offered = function_type.map(|t| validate::object_field_names(&schema, &t));
        let mut children = Vec::new();
        for name in columns {
            let column = table
                .visible_columns()
                .find(|c| &c.field_name == name || &c.name == name)
                .ok_or_else(|| {
                    ProblemDetails::invalid(
                        format!("Unknown column `{name}` for resource `{}`", table.name),
                        function,
                    )
                })?;
            if offered
                .as_ref()
                .is_some_and(|o| !o.contains(&column.field_name))
            {
                return Err(ProblemDetails::invalid(
                    format!("`{function}` is not available for the column `{name}`"),
                    function,
                ));
            }
            children.push(RequestField::new(&column.field_name));
        }
        let mut field = RequestField::new(function).with_children(children);
        if function == "percentile" {
            let fraction = body
                .fraction
                .filter(|f| (0.0..=1.0).contains(f))
                .ok_or_else(|| {
                    ProblemDetails::invalid(
                        "`percentile` needs a `fraction` between 0 and 1",
                        "fraction",
                    )
                })?;
            field = field.with_argument("fraction", serde_json::json!(fraction));
        }
        out.push(field);
    }
    if body.fraction.is_some() && body.percentile.is_none() {
        return Err(ProblemDetails::invalid(
            "`fraction` is only used with `percentile`",
            "fraction",
        ));
    }
    Ok(out)
}

async fn aggregate(
    ctx: &Ctx,
    table: &TableMetadata,
    body: AggregateBody,
    method: &str,
) -> Result<serde_json::Value, ProblemDetails> {
    ctx.allow(table, Operation::Read)?;
    if !table.operations.connection {
        return Err(ProblemDetails::forbidden(format!(
            "Aggregates of `{}` are not available",
            table.name
        )));
    }
    let root = table.connection_field.as_str();
    let schema = ctx.state.current_schema();
    let connection_type = validate::root_field_type_name(&schema, Root::Query, root);
    let aggregates_type = connection_type
        .as_deref()
        .and_then(|c| validate::field_type_name(&schema, c, "aggregates"));

    let mut args = HashMap::new();
    if let Some(filter) = &body.filter {
        if !filter.is_object() {
            return Err(ProblemDetails::invalid(
                "`filter` must be an object",
                "filter",
            ));
        }
        let filter = normalize_filter_json(filter.clone());
        ctx.validate(Root::Query, root, "filter", &filter, "filter")?;
        args.insert("filter".to_string(), filter);
    }
    let functions = function_selections(ctx, table, aggregates_type.as_deref(), &body)?;

    let groups = body.group.clone().unwrap_or_default();
    let grouped = !groups.is_empty();
    if !grouped {
        if body.group.is_some() {
            return Err(ProblemDetails::invalid("`group` names no value", "group"));
        }
        for (name, present) in [
            ("having", body.having.is_some()),
            ("timeZone", body.time_zone.is_some()),
            ("binWidth", body.bin_width.is_some()),
            ("binOrigin", body.bin_origin.is_some()),
            ("fillGaps", body.fill_gaps.is_some()),
        ] {
            if present {
                return Err(ProblemDetails::invalid(
                    format!("`{name}` needs `group`"),
                    name,
                ));
            }
        }
    }
    if groups.len() > MAX_GROUP_KEYS {
        return Err(ProblemDetails::invalid(
            format!("`group` accepts at most {MAX_GROUP_KEYS} values"),
            "group",
        ));
    }

    let child = if grouped {
        let values = groups
            .iter()
            .map(|g| group_value(table, g).map(serde_json::Value::String))
            .collect::<Result<Vec<_>, _>>()?;
        let mut field = RequestField::new("groupedAggregates")
            .with_argument("groupBy", serde_json::Value::Array(values));
        if let Some(having) = &body.having {
            if !having.is_object() {
                return Err(ProblemDetails::invalid(
                    "`having` must be an object",
                    "having",
                ));
            }
            if let Some(connection_type) = &connection_type {
                validate::validate_field_argument(
                    &schema,
                    connection_type,
                    "groupedAggregates",
                    "having",
                    having,
                    "having",
                )?;
            }
            field = field.with_argument("having", having.clone());
        }
        for (name, value) in [
            ("timeZone", body.time_zone.clone()),
            ("binWidth", body.bin_width.clone()),
            ("binOrigin", body.bin_origin.clone()),
        ] {
            if let Some(value) = value {
                field = field.with_argument(name, serde_json::Value::String(value));
            }
        }
        if let Some(fill) = body.fill_gaps {
            field = field.with_argument("fillGaps", serde_json::json!(fill));
        }
        let mut children = vec![RequestField::new("keys"), RequestField::new("count")];
        children.extend(functions);
        field.with_children(children)
    } else {
        let mut children = vec![RequestField::new("totalCount").with_alias("count")];
        if !functions.is_empty() {
            children.push(RequestField::new("aggregates").with_children(functions));
        }
        // the connection field's children are the selections above
        let request = RequestField::new(root)
            .with_arguments(args)
            .with_children(children);
        let data = ctx
            .execute(
                &table.schema,
                RequestOperation::query(vec![request]),
                method,
                false,
            )
            .await?;
        let connection = ResponseShaper::unwrap_single(&data, root)
            .map_err(|e| shaping_error(e, ctx.is_prod))?;
        let mut out = serde_json::Map::new();
        out.insert(
            "count".to_string(),
            connection.get("count").cloned().unwrap_or_default(),
        );
        if let Some(serde_json::Value::Object(totals)) = connection.get("aggregates") {
            out.extend(totals.clone());
        }
        return Ok(serde_json::Value::Object(out));
    };

    let request = RequestField::new(root)
        .with_arguments(args)
        .with_children(vec![child]);
    let data = ctx
        .execute(
            &table.schema,
            RequestOperation::query(vec![request]),
            method,
            false,
        )
        .await?;
    let connection =
        ResponseShaper::unwrap_single(&data, root).map_err(|e| shaping_error(e, ctx.is_prod))?;
    let rows = connection
        .get("groupedAggregates")
        .and_then(|g| g.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.len() > MAX_GROUPS {
        return Err(ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "The request has more than {MAX_GROUPS} groups; narrow the filter or group by fewer or coarser values"
            ),
        )
        .with_code("TOO_MANY_GROUPS"));
    }
    Ok(serde_json::Value::Array(
        rows.into_iter()
            .map(|group| {
                let mut group = group;
                if let Some(obj) = group.as_object_mut() {
                    let keys = obj.remove("keys").and_then(|k| match k {
                        serde_json::Value::Array(values) => Some(values),
                        _ => None,
                    });
                    let keyed: serde_json::Map<String, serde_json::Value> = groups
                        .iter()
                        .zip(keys.unwrap_or_default())
                        .map(|(name, value)| (name.clone(), value))
                        .collect();
                    // `keys` first, as the group reads: keys, count, aggregates
                    let rest = std::mem::take(obj);
                    obj.insert("keys".to_string(), serde_json::Value::Object(keyed));
                    obj.extend(rest);
                }
                group
            })
            .collect(),
    ))
}

/// `GET /rest/{resource}/aggregate`.
pub async fn rest_aggregate_get_handler(
    State(state): State<Arc<ServerState>>,
    Path(resource): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Response {
    respond(format!("{REST_PREFIX}/{resource}/aggregate"), async {
        let ctx = Ctx::new(&state, &headers, resource).await?;
        let table = ctx.table()?;
        let body = body_from_query(raw_query.as_deref().unwrap_or(""), table, &ctx.catalog)?;
        let out = aggregate(&ctx, table, body, "GET").await?;
        Ok((StatusCode::OK, Json(out)).into_response())
    })
    .await
}

/// `POST /rest/{resource}/aggregate`.
pub async fn rest_aggregate_post_handler(
    State(state): State<Arc<ServerState>>,
    Path(resource): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    respond(format!("{REST_PREFIX}/{resource}/aggregate"), async {
        let ctx = Ctx::new(&state, &headers, resource).await?;
        let table = ctx.table()?;
        if raw_query.as_deref().is_some_and(|q| !q.is_empty()) {
            return Err(ProblemDetails::bad_request(
                "POST takes its parameters in the JSON body, not the query string",
            ));
        }
        let body: AggregateBody = if body.is_empty() {
            AggregateBody::default()
        } else {
            serde_json::from_slice(&body)
                .map_err(|e| ProblemDetails::bad_request(format!("Invalid aggregate body: {e}")))?
        };
        let out = aggregate(&ctx, table, body, "POST").await?;
        Ok((StatusCode::OK, Json(out)).into_response())
    })
    .await
}
