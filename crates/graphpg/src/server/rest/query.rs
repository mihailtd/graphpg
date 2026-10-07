//! The REST query grammar (decision record #59), parsed onto the planner's JSON argument shapes.
//!
//! - Filters: `?filter[col][op]=val` (OpenAPI `deepObject`), or `?filter={json}`
//! - Order: `?order=col:desc,col2:asc` or `?order=-col,col2`
//! - Keyset pagination: `?first=n&after=<cursor>` / `?last=n&before=<cursor>`; `?offset=n`
//! - Field selection: `?fields=a,b`; relation embedding: `?include=rel1,rel2.nested`
//! - Controls of an embedded relation, by its `include` path: `fields[posts]=id,title`,
//!   `filter[posts][status][eq]=x`, `order[posts]=-createdAt`, `first[posts]=5`,
//!   `offset[posts]=10`; `inner=posts` keeps the parents that have such an embedded row and
//!   `anti=posts` those that have none
//!
//! The parser is strict: an unknown parameter, operator, column or malformed value is a 400
//! instead of being ignored, so a typo can never turn into an unfiltered read.

use axum::http::HeaderMap;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::catalog::{ColumnMetadata, DatabaseCatalog, PgType, TableMetadata};

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
    /// Controls of the embedded relations, by `include` path (`posts`, `posts.comments`).
    #[serde(default)]
    pub embed: BTreeMap<String, EmbedControls>,
    /// `window`: window values of each row among all the matching rows, in the list's order
    /// (`rowNumber`, `rank`, `denseRank`, `percentRank`), in the row's `_window`.
    #[serde(default)]
    pub window: Vec<String>,
    /// `running`: columns whose running sums (a ledger's balance) the rows carry.
    #[serde(default)]
    pub running: Vec<String>,
    /// `partition`: group the window values within the rows sharing these values.
    #[serde(default)]
    pub partition: Vec<String>,
    /// `cursors=true`: each row carries its own cursor, in `_cursor`.
    #[serde(default)]
    pub cursors: bool,
    /// `asOf`: the point in time a temporal table is read at.
    #[serde(default)]
    pub as_of: Option<String>,
    /// `args[field][argument]=value`: the arguments of the computed fields (functions of a row)
    /// selected, by the field's path (`uniqueAuthorCount`, or `posts.score` in an embedding).
    #[serde(default)]
    pub field_args: BTreeMap<String, BTreeMap<String, String>>,
    /// `path[column]`: the JSON path (`$.a.b`) a `json` column is read through, by column
    /// (`meta`) or, in an embedded relation, by `include` path and column (`posts.meta`).
    #[serde(default)]
    pub json_paths: BTreeMap<String, String>,
    /// `include` paths whose parents must have a matching embedded row.
    #[serde(default)]
    pub inner: Vec<String>,
    /// Relation paths whose parents must have no matching row.
    #[serde(default)]
    pub anti: Vec<String>,
}

/// What an embedded relation is read with: its columns and, for a to-many relation, the rows
/// it keeps, their order and the page of them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmbedControls {
    pub fields: Option<Vec<String>>,
    pub filter: Option<serde_json::Value>,
    pub order_by: Option<serde_json::Value>,
    pub first: Option<usize>,
    pub offset: Option<usize>,
    /// `depth[rel]`: how many levels of a hierarchy (`ancestors`, `descendants`).
    #[serde(default)]
    pub depth: Option<usize>,
    /// `asOf[rel]`: the point in time the rows of a temporal table are read at.
    #[serde(default)]
    pub as_of: Option<String>,
}

impl EmbedControls {
    /// The relation is read as a connection: it is filtered, ordered or paged.
    pub fn paged(&self) -> bool {
        self.filter.is_some()
            || self.order_by.is_some()
            || self.first.is_some()
            || self.offset.is_some()
    }
}

/// Which endpoint the parameters belong to: a collection accepts all of them, a single item
/// only `fields` and `include`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Collection,
    Item,
}

/// Count preference for collection responses (RFC 7240).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CountPreference {
    #[default]
    None,
    /// `count=exact`: exact count scanning every matching row.
    Exact,
    /// `count=planned`: planner row estimate.
    Planned,
    /// `count=estimated`: exact count below `limits.count_threshold`, planner estimate above it.
    Estimated,
}

/// Request preferences (RFC 7240) the API honours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Preferences {
    /// `return=minimal`: write responses carry no body.
    pub return_minimal: bool,
    /// Total count preference: list responses carry `X-Total-Count`. Off by default because an
    /// exact count scans every matching row.
    pub count: CountPreference,
    /// `previous`: an update or upsert answers with the row as it was before the write too
    /// (`RETURNING OLD`, PostgreSQL 18).
    pub previous: bool,
}

impl Preferences {
    /// Whether any total count was requested.
    pub fn count_requested(&self) -> bool {
        self.count != CountPreference::None
    }

    /// The `Preference-Applied` header value for what was honoured.
    pub fn applied(
        &self,
        honoured_return: bool,
        honoured_count: bool,
        honoured_previous: bool,
    ) -> Option<String> {
        let mut parts = Vec::new();
        if honoured_return && self.return_minimal {
            parts.push("return=minimal");
        }
        if honoured_count {
            match self.count {
                CountPreference::Exact => parts.push("count=exact"),
                CountPreference::Planned => parts.push("count=planned"),
                CountPreference::Estimated => parts.push("count=estimated"),
                CountPreference::None => {}
            }
        }
        if honoured_previous && self.previous {
            parts.push("previous");
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Reads the `Prefer` header(s). Unknown preferences and values are ignored, as RFC 7240 says.
pub fn parse_prefer_header(headers: &HeaderMap) -> Preferences {
    let mut prefs = Preferences::default();
    for value in headers.get_all("prefer") {
        let Ok(value) = value.to_str() else { continue };
        for item in value.split(',') {
            let item = item.split(';').next().unwrap_or("").trim();
            // A preference without a value: `previous`
            if item.eq_ignore_ascii_case("previous") {
                prefs.previous = true;
                continue;
            }
            let Some((key, val)) = item.split_once('=') else {
                continue;
            };
            let (key, val) = (key.trim(), val.trim().trim_matches('"'));
            if key.eq_ignore_ascii_case("return") {
                if val.eq_ignore_ascii_case("minimal") {
                    prefs.return_minimal = true;
                } else if val.eq_ignore_ascii_case("representation") {
                    prefs.return_minimal = false;
                }
            } else if key.eq_ignore_ascii_case("count") {
                prefs.count = if val.eq_ignore_ascii_case("exact") {
                    CountPreference::Exact
                } else if val.eq_ignore_ascii_case("planned") {
                    CountPreference::Planned
                } else if val.eq_ignore_ascii_case("estimated") {
                    CountPreference::Estimated
                } else {
                    CountPreference::None
                };
            } else if key.eq_ignore_ascii_case("previous") {
                prefs.previous = val.eq_ignore_ascii_case("true");
            }
        }
    }
    prefs
}

/// Normalizes a filter operator string into the canonical name expected by the planner;
/// empty when `op` is not an operator.
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
        "regex" | "match" => "regex",
        "iregex" | "imatch" => "iregex",
        "isDistinctFrom" | "isdistinct" | "isDistinct" => "isDistinctFrom",
        "likeAny" => "likeAny",
        "likeAll" => "likeAll",
        "ilikeAny" => "ilikeAny",
        "ilikeAll" => "ilikeAll",
        "lessThanAny" | "ltAny" => "lessThanAny",
        "lessThanAll" | "ltAll" => "lessThanAll",
        "lessThanOrEqualToAny" | "lteAny" => "lessThanOrEqualToAny",
        "lessThanOrEqualToAll" | "lteAll" => "lessThanOrEqualToAll",
        "greaterThanAny" | "gtAny" => "greaterThanAny",
        "greaterThanAll" | "gtAll" => "greaterThanAll",
        "greaterThanOrEqualToAny" | "gteAny" => "greaterThanOrEqualToAny",
        "greaterThanOrEqualToAll" | "gteAll" => "greaterThanOrEqualToAll",
        "notContains" => "notContains",
        "strictlyLeftOf" => "strictlyLeftOf",
        "strictlyRightOf" => "strictlyRightOf",
        "adjacentTo" => "adjacentTo",
        "matchesPath" => "matchesPath",
        "existsPath" => "existsPath",
        "hasKey" => "hasKey",
        "hasAnyKeys" => "hasAnyKeys",
        "hasAllKeys" => "hasAllKeys",
        _ => "",
    }
}

pub(super) fn is_filter_operator(op: &str) -> bool {
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

/// Recursively normalizes filter operator keys in a JSON filter value. Keys that are not
/// operators are left as they are (the schema validation reports the unknown ones).
pub fn normalize_filter_json(val: serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let key = if is_filter_operator(&k) {
                        normalize_operator(&k).to_string()
                    } else {
                        k
                    };
                    (key, normalize_filter_json(v))
                })
                .collect(),
        ),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(normalize_filter_json).collect())
        }
        other => other,
    }
}

/// Parses an individual scalar literal, using the column type when known.
pub(super) fn parse_scalar_value(
    val: &str,
    col: Option<&ColumnMetadata>,
    what: &str,
) -> Result<serde_json::Value, String> {
    if val.eq_ignore_ascii_case("null") {
        return Ok(serde_json::Value::Null);
    }
    match col {
        Some(col) if col.pg_type == PgType::Bool => match val {
            "true" | "1" => Ok(true.into()),
            "false" | "0" => Ok(false.into()),
            _ => Err(format!("`{what}` must be a boolean, got `{val}`")),
        },
        Some(col) if col.pg_type.is_numeric() => {
            if let Ok(n) = val.parse::<i64>() {
                return Ok(n.into());
            }
            val.parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(serde_json::Value::Number)
                .ok_or_else(|| format!("`{what}` must be a number, got `{val}`"))
        }
        Some(_) => Ok(serde_json::Value::String(val.to_string())),
        None => {
            if let Ok(b) = val.parse::<bool>() {
                return Ok(b.into());
            }
            if let Ok(n) = val.parse::<i64>() {
                return Ok(n.into());
            }
            if let Some(num) = val
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
            {
                return Ok(serde_json::Value::Number(num));
            }
            Ok(serde_json::Value::String(val.to_string()))
        }
    }
}

/// Parses a scalar used as (part of) a primary key.
pub fn parse_key_scalar(val: &str, col: &ColumnMetadata) -> Result<serde_json::Value, String> {
    if col.pg_type == PgType::Uuid && uuid::Uuid::parse_str(val).is_err() {
        return Err(format!("Invalid UUID key: {val}"));
    }
    if col.pg_type.is_numeric() && val.parse::<f64>().is_err() {
        return Err(format!("Invalid numeric key: {val}"));
    }
    if col.pg_type == PgType::Uuid || !col.pg_type.is_numeric() {
        return Ok(serde_json::Value::String(val.to_string()));
    }
    parse_scalar_value(val, Some(col), "key")
}

fn parse_filter_value(
    val: &str,
    operator: &str,
    col: Option<&ColumnMetadata>,
    what: &str,
) -> Result<serde_json::Value, String> {
    match operator {
        "isNull" => match val {
            "true" | "1" => Ok(true.into()),
            "false" | "0" => Ok(false.into()),
            _ => Err(format!("`{what}` must be true or false, got `{val}`")),
        },
        "contains" | "includes" | "containedBy" | "overlaps"
            if matches!(col.map(|c| &c.pg_type), Some(PgType::Array(_))) =>
        {
            list_value(
                val,
                col.and_then(|c| match &c.pg_type {
                    PgType::Array(inner) => Some(
                        crate::catalog::FunctionArgument {
                            name: c.name.clone(),
                            pg_type: (**inner).clone(),
                            has_default: false,
                        }
                        .as_column(),
                    ),
                    _ => None,
                })
                .as_ref(),
                what,
            )
        }
        "contains" | "includes" if matches!(col.map(|c| c.pg_type.is_range()), Some(true)) => {
            let elem_col = col
                .and_then(|c| c.pg_type.range_element())
                .map(|(inner, _)| {
                    crate::catalog::FunctionArgument {
                        name: col.map(|c| c.name.clone()).unwrap_or_default(),
                        pg_type: inner,
                        has_default: false,
                    }
                    .as_column()
                });
            parse_scalar_value(val, elem_col.as_ref(), what)
        }
        "strictlyLeftOf" | "strictlyRightOf" | "adjacentTo" | "overlaps" | "containedBy"
        | "equalTo" | "notEqualTo"
            if matches!(col.map(|c| c.pg_type.is_range()), Some(true)) =>
        {
            let elem_col = col
                .and_then(|c| c.pg_type.range_element())
                .map(|(inner, _)| {
                    crate::catalog::FunctionArgument {
                        name: col.map(|c| c.name.clone()).unwrap_or_default(),
                        pg_type: inner,
                        has_default: false,
                    }
                    .as_column()
                });
            parse_range_to_object(val, elem_col.as_ref(), what)
        }
        "contains" | "includes" | "containedBy" | "equalTo" | "notEqualTo"
            if matches!(col.map(|c| &c.pg_type), Some(PgType::Json | PgType::Jsonb)) =>
        {
            Ok(serde_json::from_str(val).unwrap_or_else(|_| val.into()))
        }
        "hasAnyKeys" | "hasAllKeys" | "likeAny" | "likeAll" | "ilikeAny" | "ilikeAll" => {
            let strings = list_value(val, None, what)?;
            Ok(match strings {
                serde_json::Value::Array(items) => serde_json::Value::Array(
                    items
                        .into_iter()
                        .map(|v| match v {
                            serde_json::Value::String(_) => v,
                            other => other.to_string().into(),
                        })
                        .collect(),
                ),
                other => other,
            })
        }
        op if op.ends_with("Any") || op.ends_with("All") => list_value(val, col, what),
        "in" | "notIn" => {
            if val.starts_with('[') && val.ends_with(']') {
                return serde_json::from_str(val)
                    .map_err(|e| format!("`{what}` is not a valid JSON array: {e}"));
            }
            val.split(',')
                .map(|s| parse_scalar_value(s.trim(), col, what))
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array)
        }
        _ => parse_scalar_value(val, col, what),
    }
}

/// A list value: a JSON array, or comma-separated scalars typed by `col`.
fn list_value(
    val: &str,
    col: Option<&ColumnMetadata>,
    what: &str,
) -> Result<serde_json::Value, String> {
    if val.starts_with('[') && val.ends_with(']') {
        return serde_json::from_str(val)
            .map_err(|e| format!("`{what}` is not a valid JSON array: {e}"));
    }
    val.split(',')
        .map(|s| parse_scalar_value(s.trim(), col, what))
        .collect::<Result<Vec<_>, _>>()
        .map(serde_json::Value::Array)
}

/// Parses a range value: JSON `{start: ..., end: ...}`, or PostgreSQL `[lower,upper)` string format.
fn parse_range_to_object(
    val: &str,
    elem_col: Option<&ColumnMetadata>,
    what: &str,
) -> Result<serde_json::Value, String> {
    let s = val.trim();
    if s.starts_with('{') {
        return serde_json::from_str(s).map_err(|e| format!("`{what}` is not valid JSON: {e}"));
    }
    if s.eq_ignore_ascii_case("empty") {
        return Ok(serde_json::json!({}));
    }
    if !((s.starts_with('[') || s.starts_with('(')) && (s.ends_with(']') || s.ends_with(')'))) {
        return Err(format!(
            "`{what}` must be a range object or `[lower,upper)` string, got `{val}`"
        ));
    }
    let lower_inclusive = s.starts_with('[');
    let upper_inclusive = s.ends_with(']');
    let inner = &s[1..s.len() - 1];
    let Some((lower_raw, upper_raw)) = inner.split_once(',') else {
        return Err(format!(
            "`{what}` must contain a comma separating bounds, got `{val}`"
        ));
    };
    let mut obj = serde_json::Map::new();
    let lower_raw = lower_raw.trim().trim_matches('"');
    if !lower_raw.is_empty() {
        let val = parse_scalar_value(lower_raw, elem_col, &format!("{what}.start"))?;
        obj.insert(
            "start".into(),
            serde_json::json!({ "value": val, "inclusive": lower_inclusive }),
        );
    }
    let upper_raw = upper_raw.trim().trim_matches('"');
    if !upper_raw.is_empty() {
        let val = parse_scalar_value(upper_raw, elem_col, &format!("{what}.end"))?;
        obj.insert(
            "end".into(),
            serde_json::json!({ "value": val, "inclusive": upper_inclusive }),
        );
    }
    Ok(serde_json::Value::Object(obj))
}

/// Turns `{"or": {"0": ..., "1": ...}}`, which is what `filter[or][0][...]` builds, into the
/// lists `and` and `or` take.
pub(super) fn arrayify_groups(filter: serde_json::Value) -> serde_json::Value {
    match filter {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let value = arrayify_groups(value);
                    let numbered = |m: &serde_json::Map<String, serde_json::Value>| {
                        !m.is_empty() && m.keys().all(|k| k.parse::<usize>().is_ok())
                    };
                    match value {
                        serde_json::Value::Object(group)
                            if matches!(key.as_str(), "and" | "or") && numbered(&group) =>
                        {
                            let mut items: Vec<(usize, serde_json::Value)> = group
                                .into_iter()
                                .filter_map(|(k, v)| Some((k.parse().ok()?, v)))
                                .collect();
                            items.sort_by_key(|(i, _)| *i);
                            (
                                key,
                                serde_json::Value::Array(
                                    items.into_iter().map(|(_, v)| v).collect(),
                                ),
                            )
                        }
                        other => (key, other),
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

/// Extracts bracket path segments from a query key like `filter[name][eq]`.
pub(super) fn parse_bracket_path(key: &str) -> Option<(String, Vec<String>)> {
    let Some(first_bracket) = key.find('[') else {
        return Some((key.to_string(), Vec::new()));
    };
    if !key.ends_with(']') {
        return None;
    }
    let root = &key[..first_bracket];
    let rest = &key[first_bracket..];
    let (mut segments, mut in_bracket, mut start) = (Vec::new(), false, 0);
    for (i, c) in rest.char_indices() {
        match c {
            '[' if !in_bracket => {
                in_bracket = true;
                start = i + 1;
            }
            ']' if in_bracket => {
                in_bracket = false;
                segments.push(rest[start..i].to_string());
            }
            '[' | ']' => return None,
            _ => {}
        }
    }
    (!in_bracket).then(|| (root.to_string(), segments))
}

/// Resolves column metadata traversing nested paths across relations.
fn resolve_column_for_path<'a>(
    path: &[String],
    table_meta: &'a TableMetadata,
    catalog: &'a DatabaseCatalog,
) -> Option<&'a ColumnMetadata> {
    let mut current = table_meta;
    for (i, segment) in path.iter().enumerate() {
        if matches!(
            segment.as_str(),
            "some" | "every" | "none" | "and" | "or" | "not"
        ) || segment.parse::<usize>().is_ok()
        {
            continue;
        }
        if i == path.len() - 1 {
            return current
                .column_by_field(segment)
                .or_else(|| current.visible_columns().find(|c| &c.name == segment));
        }
        let rel = catalog.find_relation(&current.schema, &current.name, segment)?;
        current = catalog.find_table(&rel.target_schema, &rel.target_table)?;
    }
    None
}

/// Converts bracket segments and a string value into a nested filter JSON object.
pub(super) fn segments_to_filter(
    segments: &[String],
    val: &str,
    table_meta: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> Result<serde_json::Value, String> {
    let what = format!("filter[{}]", segments.join("]["));
    let (path, op_name) = match segments {
        [] => return Err("`filter` needs a column: `filter[column][operator]=value`".into()),
        [.., last] if segments.len() >= 2 && is_filter_operator(last) => {
            (&segments[..segments.len() - 1], normalize_operator(last))
        }
        _ => (segments, "equalTo"),
    };
    let col = resolve_column_for_path(path, table_meta, catalog);
    let mut current = serde_json::json!({ op_name: parse_filter_value(val, op_name, col, &what)? });
    for segment in path.iter().rev() {
        current = serde_json::json!({ segment: current });
    }
    Ok(current)
}

/// Parses an order string into the planner's `orderBy` enum values.
///
/// Items are `column`, `-column`, `+column`, `column:asc|desc`, `natural`, `primary_key`, or an
/// enum value such as `NAME_DESC`. An unknown column is an error.
pub fn parse_order_string(
    order_str: &str,
    table_meta: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> Result<serde_json::Value, String> {
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
            match d.trim().to_ascii_lowercase().as_str() {
                "desc" => (n.trim(), "DESC"),
                "asc" => (n.trim(), "ASC"),
                _ => return Err(format!("Invalid order direction `{d}` in `{raw}`")),
            }
        } else {
            (raw, "ASC")
        };

        let options = catalog.order_options(table_meta);
        let is_pk = ["primary_key", "primary_key_asc", "primary_key_desc"]
            .iter()
            .any(|p| name.eq_ignore_ascii_case(p));
        if name.eq_ignore_ascii_case("natural") {
            items.push("NATURAL".to_string());
        } else if is_pk {
            items.push(format!("PRIMARY_KEY_{dir}"));
        } else if name.ends_with("_ASC") || name.ends_with("_DESC") {
            items.push(name.to_string());
        } else if let Some(col) = table_meta
            .visible_columns()
            .find(|c| c.field_name == name || c.name == name)
        {
            items.push(format!("{}_{dir}", col.order_by_name()));
        } else if let Some(opt_name) = catalog
            .cti_columns(table_meta)
            .into_iter()
            .find(|(field, members)| field == name || members[0].1.name == name)
            .map(|(_, members)| members[0].1.order_by_name())
            .filter(|n| options.iter().any(|(o, _)| o == n))
        {
            // A column of a subtype of a table-per-type hierarchy
            items.push(format!("{opt_name}_{dir}"));
        } else if let Some(opt_name) = name.split_once('.').and_then(|(relation, column)| {
            // A column of the row a to-one relation leads to: `author.username`
            let wanted = format!(
                "{}_{}",
                crate::catalog::models::enum_value_name(relation),
                crate::catalog::models::enum_value_name(column)
            );
            options
                .iter()
                .find(|(n, target)| {
                    matches!(target, crate::catalog::OrderTarget::Related { .. })
                        && n.eq_ignore_ascii_case(&wanted)
                })
                .map(|(n, _)| n.clone())
        }) {
            items.push(format!("{opt_name}_{dir}"));
        } else if let Some((opt_name, _)) = options.into_iter().find(|(n, _)| {
            // `messages_count`, `MESSAGES_COUNT` and `messagesCount` are the same option
            n.eq_ignore_ascii_case(name) || *n == crate::catalog::models::enum_value_name(name)
        }) {
            items.push(format!("{opt_name}_{dir}"));
        } else {
            return Err(format!(
                "Unknown order column `{name}` for resource `{}`",
                table_meta.name
            ));
        }
    }
    Ok(serde_json::Value::Array(
        items.into_iter().map(serde_json::Value::String).collect(),
    ))
}

pub(super) fn split_list(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Checks a `first` / `last` page size is at least 1. The largest is a limit of the
/// deployment (`limits.page_size.max`), enforced on the plan.
pub fn check_page_size(key: &str, n: usize) -> Result<usize, String> {
    if n == 0 {
        return Err(format!("`{key}` must be at least 1"));
    }
    Ok(n)
}

fn page_size(key: &str, val: &str) -> Result<usize, String> {
    let n = val
        .parse::<usize>()
        .map_err(|_| format!("Invalid integer for `{key}`: {val}"))?;
    check_page_size(key, n)
}

/// The window values a row can carry (`window=rank`).
pub(super) const WINDOW_VALUES: [&str; 4] = ["rowNumber", "rank", "denseRank", "percentRank"];

/// Keys of a relation filter that filter the parent (`filter[posts][some][…]`), not the rows
/// embedded through the relation.
const PARENT_RELATION_FILTERS: [&str; 8] =
    ["some", "every", "none", "count", "sum", "avg", "min", "max"];

/// What kind of field `include` embeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmbedKind {
    /// A foreign-key relation (or the `history` of a temporal row).
    Relation,
    /// A function mounted on the table that returns rows of a table (a computed relationship).
    Mounted,
    /// `ancestors` / `descendants` of a hierarchy.
    Tree,
    /// The `target` of an exclusive arc: one row of one of several tables.
    Arc,
}

/// What `include` can embed.
pub(crate) struct Embeddable<'a> {
    /// The field of the table's type.
    pub name: String,
    /// A list of rows (a to-many relation, a function returning a set, a hierarchy).
    pub many: bool,
    /// The table of the rows (of the first target of an arc).
    pub target: &'a TableMetadata,
    pub kind: EmbedKind,
    /// A foreign-key relation, which `inner`, `anti` and the parent filters can follow.
    pub relation: bool,
    /// The tables an arc leads to, with the GraphQL type of each.
    pub arc_targets: Vec<(String, &'a TableMetadata)>,
}

/// The arguments of a function field of a table, the ones the request fills: those after the row,
/// apart from those the table's columns fill.
pub(crate) fn field_arguments(
    f: &crate::catalog::FunctionMetadata,
) -> Vec<&crate::catalog::FunctionArgument> {
    f.arguments
        .iter()
        .skip(usize::from(f.row_argument))
        .filter(|a| f.parent_column_for(&a.name).is_none())
        .collect()
}

/// The relation, computed relationship, hierarchy or arc `name` of `table`, matched
/// case-insensitively.
pub(crate) fn find_embeddable<'a>(
    catalog: &'a DatabaseCatalog,
    table: &'a TableMetadata,
    name: &str,
) -> Option<Embeddable<'a>> {
    catalog
        .object_fields(&table.schema, &table.name)
        .iter()
        .filter(|f| f.name == name || f.name.eq_ignore_ascii_case(name))
        .find_map(|f| match &f.kind {
            crate::catalog::FieldKind::Relation(rel) => Some(Embeddable {
                name: f.name.clone(),
                many: rel.is_many,
                target: catalog.find_table(&rel.target_schema, &rel.target_table)?,
                kind: EmbedKind::Relation,
                relation: true,
                arc_targets: Vec::new(),
            }),
            crate::catalog::FieldKind::Function(i) => {
                let func = &catalog.functions[*i];
                let crate::catalog::FunctionReturn::Table { schema, table } = &func.returns else {
                    return None;
                };
                (catalog.function_is_exposed(func) && !func.volatile).then_some(())?;
                Some(Embeddable {
                    name: f.name.clone(),
                    many: func.returns_set,
                    target: catalog.find_table(schema, table)?,
                    kind: EmbedKind::Mounted,
                    relation: false,
                    arc_targets: Vec::new(),
                })
            }
            crate::catalog::FieldKind::Tree(_) => Some(Embeddable {
                name: f.name.clone(),
                many: true,
                target: table,
                kind: EmbedKind::Tree,
                relation: false,
                arc_targets: Vec::new(),
            }),
            crate::catalog::FieldKind::ArcTarget => {
                let arc = catalog
                    .exclusive_arcs
                    .iter()
                    .find(|a| a.table_name == table.name)?;
                let targets: Vec<(String, &TableMetadata)> = arc
                    .branches(&table.schema)
                    .enumerate()
                    .filter_map(|(i, (_, schema, name, _))| {
                        Some((arc.target_type_name(i), catalog.find_table(schema, name)?))
                    })
                    .collect();
                let first = targets.first()?.1;
                Some(Embeddable {
                    name: f.name.clone(),
                    many: false,
                    target: first,
                    kind: EmbedKind::Arc,
                    relation: false,
                    arc_targets: targets,
                })
            }
            _ => None,
        })
}

/// What a dotted path (`posts.comments`) leads to: the embeddable at its end.
pub(super) fn walk_relation<'a>(
    catalog: &'a DatabaseCatalog,
    table: &'a TableMetadata,
    path: &str,
) -> Result<Embeddable<'a>, String> {
    let mut current = table;
    let mut found = None;
    for part in path.split('.') {
        let embeddable = find_embeddable(catalog, current, part)
            .ok_or_else(|| format!("Unknown relation `{part}` for resource `{}`", current.name))?;
        current = embeddable.target;
        found = Some(embeddable);
    }
    found.ok_or_else(|| "An empty relation path".to_string())
}

/// Every `include` path and the paths leading to it (`posts.comments` includes `posts`).
fn included_paths(include: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for path in include {
        let mut prefix = String::new();
        for part in path.split('.') {
            if !prefix.is_empty() {
                prefix.push('.');
            }
            prefix.push_str(part);
            out.insert(prefix.clone());
        }
    }
    out
}

/// Checks the embedded controls against `include`, and turns `inner` / `anti` into conditions
/// on the parent's filter.
pub(super) fn finish_embeds(
    query: &mut RestQuery,
    table: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> Result<(), String> {
    let included = included_paths(query.include.as_deref().unwrap_or(&[]));
    for (path, controls) in query.embed.iter_mut() {
        if !included.contains(path) {
            return Err(format!(
                "`{path}` has controls but is not in `include`: add `include={path}`"
            ));
        }
        let rel = walk_relation(catalog, table, path)?;
        if controls.paged() && !rel.many {
            return Err(format!(
                "`{path}` is a to-one relation: it has no rows to filter, order or page, only `fields`"
            ));
        }
        match rel.kind {
            EmbedKind::Tree if controls.first.is_some() || controls.offset.is_some() => {
                return Err(format!(
                    "`{path}` is a hierarchy: it takes `depth`, `filter` and `order`, not a page"
                ));
            }
            EmbedKind::Tree => {}
            _ if controls.depth.is_some() => {
                return Err(format!(
                    "`depth[{path}]` is for a hierarchy (`ancestors`, `descendants`)"
                ));
            }
            EmbedKind::Arc if controls.as_of.is_some() => {
                return Err(format!(
                    "`{path}` is a union of tables: `asOf` is for a table"
                ));
            }
            _ => {}
        }
        controls.filter = controls.filter.take().map(arrayify_groups);
    }
    for name in &query.window {
        if !WINDOW_VALUES.contains(&name.as_str()) {
            return Err(format!(
                "Unknown window value `{name}`; the values are: {}",
                WINDOW_VALUES.join(", ")
            ));
        }
    }
    for name in &query.running {
        let numeric = table
            .visible_columns()
            .any(|c| (c.field_name == *name || c.name == *name) && c.pg_type.is_numeric());
        if !numeric {
            return Err(format!(
                "`running={name}`: a running sum needs a number column of `{}`",
                table.name
            ));
        }
    }
    if !query.partition.is_empty() && query.window.is_empty() && query.running.is_empty() {
        return Err("`partition` needs `window` or `running`".to_string());
    }
    for (key, given) in &query.field_args {
        let (relation, field) = key.rsplit_once('.').unwrap_or(("", key));
        let owner = if relation.is_empty() {
            table
        } else {
            if !included.contains(relation) {
                return Err(format!(
                    "`args[{key}]` is not in `include`: add `include={relation}`"
                ));
            }
            walk_relation(catalog, table, relation)?.target
        };
        let func = catalog
            .object_fields(&owner.schema, &owner.name)
            .iter()
            .find_map(|f| match &f.kind {
                crate::catalog::FieldKind::Function(i) if f.name == field => {
                    Some(&catalog.functions[*i])
                }
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "`args[{key}]`: `{field}` is not a computed field of `{}`",
                    owner.name
                )
            })?;
        let declared = field_arguments(func);
        for name in given.keys() {
            if !declared.iter().any(|a| a.graphql_name() == *name) {
                return Err(format!(
                    "`args[{key}][{name}]`: `{field}` has no argument `{name}`"
                ));
            }
        }
    }
    for (key, path) in &query.json_paths {
        let (relation, column) = key.rsplit_once('.').unwrap_or(("", key));
        let owner = if relation.is_empty() {
            table
        } else {
            if !included.contains(relation) {
                return Err(format!(
                    "`path[{key}]` is not in `include`: add `include={relation}`"
                ));
            }
            walk_relation(catalog, table, relation)?.target
        };
        let col = owner
            .visible_columns()
            .find(|c| c.field_name == column || c.name == column);
        if !col.is_some_and(|c| matches!(c.pg_type, PgType::Json | PgType::Jsonb)) {
            return Err(format!(
                "`path[{key}]`: `{column}` is not a `json` column of `{}`",
                owner.name
            ));
        }
        if !path.starts_with('$') {
            return Err(format!(
                "`path[{key}]` must be a JSON path starting with `$` (`$.address.city`)"
            ));
        }
    }
    let mut conditions = Vec::new();
    for (param, paths) in [("inner", &query.inner), ("anti", &query.anti)] {
        for path in paths {
            let segments: Vec<&str> = path.split('.').collect();
            let present = presence_filter(query, catalog, table, &segments, "")
                .map_err(|e| format!("`{param}={path}`: {e}"))?;
            conditions.push(if param == "anti" {
                serde_json::json!({ "not": present })
            } else {
                present
            });
        }
    }
    if !conditions.is_empty() {
        if let Some(existing) = query.filter.take() {
            conditions.insert(0, existing);
        }
        query.filter = Some(serde_json::json!({ "and": conditions }));
    }
    Ok(())
}

/// The filter of the parents that have a row at the end of `segments`, matching the embedded
/// filters of the relations on the way.
fn presence_filter(
    query: &RestQuery,
    catalog: &DatabaseCatalog,
    table: &TableMetadata,
    segments: &[&str],
    prefix: &str,
) -> Result<serde_json::Value, String> {
    let (first, rest) = segments.split_first().ok_or("an empty relation path")?;
    let path = if prefix.is_empty() {
        first.to_string()
    } else {
        format!("{prefix}.{first}")
    };
    let found = walk_relation(catalog, table, first)?;
    if !found.relation {
        return Err(format!(
            "`{first}` is a computed relationship: parents can only be filtered by foreign-key relations"
        ));
    }
    let (name, many, target) = (found.name, found.many, found.target);
    let mut conditions = Vec::new();
    if let Some(filter) = query.embed.get(&path).and_then(|c| c.filter.clone()) {
        conditions.push(filter);
    }
    if !rest.is_empty() {
        conditions.push(presence_filter(query, catalog, target, rest, &path)?);
    }
    let inner = match conditions.len() {
        0 => serde_json::json!({}),
        1 => conditions.remove(0),
        _ => serde_json::json!({ "and": conditions }),
    };
    Ok(if many {
        serde_json::json!({ name: { "some": inner } })
    } else {
        serde_json::json!({ name: inner })
    })
}

/// Parses raw query string parameters into [`RestQuery`].
pub fn parse_query_params(
    raw_query: &str,
    kind: QueryKind,
    table_meta: &TableMetadata,
    catalog: &DatabaseCatalog,
) -> Result<RestQuery, String> {
    let mut query = RestQuery::default();
    let decode = |s: &str| {
        percent_decode_str(&s.replace('+', " "))
            .decode_utf8()
            .map(|c| c.to_string())
            .map_err(|_| format!("Invalid UTF-8 in query string: {s}"))
    };
    // The paths `include` leads through, which controls such as `filter[posts][…]` refer to
    let mut included = BTreeSet::new();
    for pair in raw_query.split('&').filter(|p| !p.is_empty()) {
        let (raw_key, raw_val) = pair.split_once('=').unwrap_or((pair, ""));
        if decode(raw_key)? == "include" {
            included.extend(included_paths(&split_list(&decode(raw_val)?)));
        }
    }

    for pair in raw_query.split('&').filter(|p| !p.is_empty()) {
        let (raw_key, raw_val) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(raw_key)?;
        let val = decode(raw_val)?;

        let collection_only = |name: &str| -> Result<(), String> {
            if kind == QueryKind::Item {
                Err(format!(
                    "`{name}` is not supported when reading a single item; use `fields` and `include`"
                ))
            } else {
                Ok(())
            }
        };

        match key.as_str() {
            "fields" => query.fields = Some(split_list(&val)),
            "include" => {
                query.include = Some(split_list(&val));
            }
            "first" => {
                collection_only("first")?;
                query.first = Some(page_size("first", &val)?);
            }
            "last" => {
                collection_only("last")?;
                query.last = Some(page_size("last", &val)?);
            }
            "after" => {
                collection_only("after")?;
                query.after = Some(val);
            }
            "before" => {
                collection_only("before")?;
                query.before = Some(val);
            }
            "offset" => {
                collection_only("offset")?;
                query.offset = Some(
                    val.parse::<usize>()
                        .map_err(|_| format!("Invalid integer for `offset`: {val}"))?,
                );
            }
            "order" => {
                collection_only("order")?;
                query.order_by = Some(parse_order_string(&val, table_meta, catalog)?);
            }
            "filter" => {
                collection_only("filter")?;
                let parsed: serde_json::Value = serde_json::from_str(&val)
                    .map_err(|e| format!("`filter` must be a JSON object: {e}"))?;
                if !parsed.is_object() {
                    return Err("`filter` must be a JSON object".to_string());
                }
                let norm = normalize_filter_json(parsed);
                match &mut query.filter {
                    Some(target) => merge_json(target, norm),
                    None => query.filter = Some(norm),
                }
            }
            "asOf" => {
                query.as_of = Some(val);
            }
            "window" | "running" | "partition" | "cursors" => {
                collection_only(&key)?;
                match key.as_str() {
                    "window" => query.window = split_list(&val),
                    "running" => query.running = split_list(&val),
                    "partition" => query.partition = split_list(&val),
                    _ => {
                        query.cursors = match val.as_str() {
                            "true" | "1" => true,
                            "false" | "0" => false,
                            _ => return Err("`cursors` must be true or false".to_string()),
                        }
                    }
                }
            }
            "inner" | "anti" => {
                collection_only(&key)?;
                let paths = split_list(&val);
                if key == "inner" {
                    query.inner = paths;
                } else {
                    query.anti = paths;
                }
            }
            other => match parse_bracket_path(other) {
                // `filter[posts][status][eq]=x` filters the rows embedded through a to-many
                // relation in `include`; `filter[posts][some][…]` filters the parents
                Some((root, segments))
                    if root == "filter"
                        && segments.len() >= 2
                        && included.contains(&segments[0])
                        && !PARENT_RELATION_FILTERS.contains(&segments[1].as_str())
                        && walk_relation(catalog, table_meta, &segments[0])
                            .is_ok_and(|found| found.many) =>
                {
                    let target = walk_relation(catalog, table_meta, &segments[0])?.target;
                    let filt = segments_to_filter(&segments[1..], &val, target, catalog)?;
                    let controls = query.embed.entry(segments[0].clone()).or_default();
                    match &mut controls.filter {
                        Some(existing) => merge_json(existing, filt),
                        None => controls.filter = Some(filt),
                    }
                }
                Some((root, segments)) if root == "path" && segments.len() == 1 => {
                    query.json_paths.insert(segments[0].clone(), val);
                }
                Some((root, segments)) if root == "args" && segments.len() == 2 => {
                    query
                        .field_args
                        .entry(segments[0].clone())
                        .or_default()
                        .insert(segments[1].clone(), val);
                }
                Some((root, segments))
                    if segments.len() == 1 && matches!(root.as_str(), "asOf" | "depth") =>
                {
                    let path = segments[0].clone();
                    walk_relation(catalog, table_meta, &path)?;
                    let controls = query.embed.entry(path).or_default();
                    if root == "asOf" {
                        controls.as_of = Some(val);
                    } else {
                        controls.depth =
                            Some(val.parse::<usize>().ok().filter(|d| *d > 0).ok_or_else(
                                || format!("`depth` must be a positive integer, got `{val}`"),
                            )?);
                    }
                }
                Some((root, segments))
                    if segments.len() == 1
                        && matches!(root.as_str(), "fields" | "order" | "first" | "offset") =>
                {
                    let path = segments[0].clone();
                    let target = walk_relation(catalog, table_meta, &path)?.target;
                    let controls = query.embed.entry(path).or_default();
                    match root.as_str() {
                        "fields" => controls.fields = Some(split_list(&val)),
                        "order" => {
                            controls.order_by = Some(parse_order_string(&val, target, catalog)?)
                        }
                        "first" => controls.first = Some(page_size("first", &val)?),
                        _ => {
                            controls.offset = Some(
                                val.parse::<usize>()
                                    .map_err(|_| format!("Invalid integer for `offset`: {val}"))?,
                            )
                        }
                    }
                }
                Some((root, segments)) if root == "filter" => {
                    collection_only("filter")?;
                    let filt = segments_to_filter(&segments, &val, table_meta, catalog)?;
                    match &mut query.filter {
                        Some(target) => merge_json(target, filt),
                        None => query.filter = Some(filt),
                    }
                }
                _ => return Err(format!("Unknown query parameter `{other}`")),
            },
        }
    }

    query.filter = query.filter.take().map(arrayify_groups);
    finish_embeds(&mut query, table_meta, catalog)?;
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_operators() {
        assert_eq!(normalize_operator("eq"), "equalTo");
        assert_eq!(normalize_operator("nin"), "notIn");
        assert_eq!(normalize_operator("startswith"), "startsWith");
        assert_eq!(normalize_operator("is"), "isNull");
        assert_eq!(normalize_operator("eqq"), "");
    }

    #[test]
    fn parses_bracket_paths() {
        assert_eq!(
            parse_bracket_path("filter[author][username][eq]"),
            Some((
                "filter".to_string(),
                vec!["author".into(), "username".into(), "eq".into()]
            ))
        );
        assert_eq!(parse_bracket_path("order"), Some(("order".into(), vec![])));
        assert_eq!(parse_bracket_path("filter[a"), None);
        assert_eq!(parse_bracket_path("filter[[a]]"), None);
    }

    #[test]
    fn merges_filters() {
        let mut target = json!({ "age": { "greaterThan": 18 }, "name": { "equalTo": "A" } });
        merge_json(
            &mut target,
            json!({ "age": { "lessThan": 65 }, "city": { "equalTo": "L" } }),
        );
        assert_eq!(
            target,
            json!({
                "age": { "greaterThan": 18, "lessThan": 65 },
                "name": { "equalTo": "A" },
                "city": { "equalTo": "L" }
            })
        );
    }

    #[test]
    fn normalizes_filter_json() {
        let out = normalize_filter_json(json!({
            "or": [{ "name": { "eq": "A" } }, { "age": { "gt": 30, "lte": 50 } }]
        }));
        assert_eq!(
            out,
            json!({
                "or": [
                    { "name": { "equalTo": "A" } },
                    { "age": { "greaterThan": 30, "lessThanOrEqualTo": 50 } }
                ]
            })
        );
    }

    #[test]
    fn prefer_header_is_parsed() {
        let mut h = HeaderMap::new();
        h.insert("prefer", "return=minimal, count=exact".parse().unwrap());
        let p = parse_prefer_header(&h);
        assert!(p.return_minimal);
        assert_eq!(p.count, CountPreference::Exact);
        assert_eq!(
            p.applied(true, true, true).as_deref(),
            Some("return=minimal, count=exact")
        );
        h.insert("prefer", "count=planned".parse().unwrap());
        let p = parse_prefer_header(&h);
        assert_eq!(p.count, CountPreference::Planned);
        assert!(p.count_requested());
        assert_eq!(
            p.applied(false, true, false).as_deref(),
            Some("count=planned")
        );
        h.insert("prefer", "count=estimated".parse().unwrap());
        let p = parse_prefer_header(&h);
        assert_eq!(p.count, CountPreference::Estimated);
        assert!(p.count_requested());
        assert_eq!(
            p.applied(false, true, false).as_deref(),
            Some("count=estimated")
        );
        h.insert("prefer", "previous".parse().unwrap());
        assert!(parse_prefer_header(&h).previous);
        h.insert(
            "prefer",
            "previous=true, return=representation".parse().unwrap(),
        );
        assert!(parse_prefer_header(&h).previous);
        h.insert("prefer", "previous=false".parse().unwrap());
        assert!(!parse_prefer_header(&h).previous);
        let mut h = HeaderMap::new();
        h.insert(
            "prefer",
            "handling=lenient; return=representation".parse().unwrap(),
        );
        assert_eq!(parse_prefer_header(&h), Preferences::default());
    }

    #[test]
    fn page_sizes_are_positive_numbers() {
        assert_eq!(page_size("first", "10"), Ok(10));
        assert!(page_size("first", "0").is_err());
        assert!(page_size("first", "x").is_err());
    }
}
