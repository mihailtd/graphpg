//! The REST query grammar (decision record #59), parsed onto the planner's JSON argument shapes.
//!
//! - Filters: `?filter[col][op]=val` (OpenAPI `deepObject`), or `?filter={json}`
//! - Order: `?order=col:desc,col2:asc` or `?order=-col,col2`
//! - Keyset pagination: `?first=n&after=<cursor>` / `?last=n&before=<cursor>`; `?offset=n`
//! - Field selection: `?fields=a,b`; relation embedding: `?include=rel1,rel2.nested`
//!
//! The parser is strict: an unknown parameter, operator, column or malformed value is a 400
//! instead of being ignored, so a typo can never turn into an unfiltered read.

use axum::http::HeaderMap;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};

use super::{MAX_INCLUDES, MAX_PAGE_SIZE};
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
}

/// Which endpoint the parameters belong to: a collection accepts all of them, a single item
/// only `fields` and `include`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Collection,
    Item,
}

/// Request preferences (RFC 7240) the API honours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Preferences {
    /// `return=minimal`: write responses carry no body.
    pub return_minimal: bool,
    /// `count=exact`: list responses carry `X-Total-Count`. Off by default because the count
    /// scans every matching row.
    pub count_exact: bool,
}

impl Preferences {
    /// The `Preference-Applied` header value for what was honoured.
    pub fn applied(&self, honoured_return: bool, honoured_count: bool) -> Option<String> {
        let mut parts = Vec::new();
        if honoured_return && self.return_minimal {
            parts.push("return=minimal");
        }
        if honoured_count && self.count_exact {
            parts.push("count=exact");
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
                prefs.count_exact = val.eq_ignore_ascii_case("exact");
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
        _ => "",
    }
}

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
fn parse_scalar_value(
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

/// Extracts bracket path segments from a query key like `filter[name][eq]`.
fn parse_bracket_path(key: &str) -> Option<(String, Vec<String>)> {
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
        if matches!(segment.as_str(), "some" | "every" | "none") {
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
fn segments_to_filter(
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
        } else if let Some((opt_name, _)) = options
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
        {
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

fn split_list(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Checks a `first` / `last` page size.
pub fn check_page_size(key: &str, n: usize) -> Result<usize, String> {
    if n == 0 || n > MAX_PAGE_SIZE {
        return Err(format!(
            "`{key}` must be between 1 and {MAX_PAGE_SIZE}, got {n}"
        ));
    }
    Ok(n)
}

fn page_size(key: &str, val: &str) -> Result<usize, String> {
    let n = val
        .parse::<usize>()
        .map_err(|_| format!("Invalid integer for `{key}`: {val}"))?;
    check_page_size(key, n)
}

/// Checks the `include` list's size; depth is checked when the relation tree is built.
pub fn check_include_count(include: &[String]) -> Result<(), String> {
    if include.len() > MAX_INCLUDES {
        return Err(format!(
            "`include` accepts at most {MAX_INCLUDES} relations"
        ));
    }
    Ok(())
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
                let include = split_list(&val);
                check_include_count(&include)?;
                query.include = Some(include);
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
            other => match parse_bracket_path(other) {
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
        assert!(p.return_minimal && p.count_exact);
        assert_eq!(
            p.applied(true, true).as_deref(),
            Some("return=minimal, count=exact")
        );
        let mut h = HeaderMap::new();
        h.insert(
            "prefer",
            "handling=lenient; return=representation".parse().unwrap(),
        );
        assert_eq!(parse_prefer_header(&h), Preferences::default());
    }

    #[test]
    fn page_sizes_are_bounded() {
        assert_eq!(page_size("first", "10"), Ok(10));
        assert!(page_size("first", "0").is_err());
        assert!(page_size("first", &(MAX_PAGE_SIZE + 1).to_string()).is_err());
        assert!(page_size("first", "x").is_err());
    }
}
