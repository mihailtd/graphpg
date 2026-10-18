//! JSON Schema keywords (2020-12) from the `CHECK` constraints PostgreSQL reports for a
//! column: bounds (`minimum`, `exclusiveMaximum`, ...), `multipleOf`, `minLength` /
//! `maxLength`, `pattern`, `minItems` / `maxItems`. Only the simple forms are read, one
//! column at a time; anything else is left to the database, which checks every write anyway.
use crate::catalog::{ColumnMetadata, ConstraintKind, ConstraintMetadata, PgType, TableMetadata};
use regex::Regex;
use serde_json::{json, Map, Value};

/// Splits at the ` AND ` that are not inside parentheses.
fn conjuncts(expression: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut depth, mut start, mut quoted) = (0i32, 0usize, false);
    let bytes = expression.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            b' ' if !quoted && depth == 0 && expression[i..].starts_with(" AND ") => {
                parts.push(expression[start..i].to_string());
                start = i + 5;
                i += 4;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(expression[start..].to_string());
    parts
}

/// Removes parentheses that wrap the whole text.
fn unwrap(mut text: &str) -> &str {
    loop {
        let t = text.trim();
        if !(t.starts_with('(') && t.ends_with(')')) {
            return t;
        }
        let mut depth = 0;
        let mut quoted = false;
        for (i, c) in t.char_indices() {
            match c {
                '\'' => quoted = !quoted,
                '(' if !quoted => depth += 1,
                ')' if !quoted => {
                    depth -= 1;
                    if depth == 0 && i != t.len() - 1 {
                        return t;
                    }
                }
                _ => {}
            }
        }
        text = &t[1..t.len() - 1];
    }
}

fn number(text: &str) -> Option<Value> {
    if let Ok(i) = text.parse::<i64>() {
        return Some(json!(i));
    }
    text.parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .map(|f| json!(f))
}

/// The JSON Schema a pg_jsonschema check states for a column, as the OpenAPI document gives
/// it: inline, or (`component`) a component of its own the column refers to.
#[derive(Debug, Clone, PartialEq)]
pub struct StatedJsonSchema {
    /// The schema, in JSON Schema 2020-12 (the dialect of OpenAPI 3.1), without `$schema`.
    /// In a component, its own references (`#/$defs/Coding`) point into the component.
    pub schema: Value,
    /// Name of the component holding it: `Json.<table>.<column>` for a column's check whose
    /// schema refers to itself, `Json.<Domain>` for a domain's check (shared by its columns).
    pub component: Option<String>,
}

/// The JSON Schema a `CHECK (json[b]_matches_schema('<schema>', <column>))` (pg_jsonschema)
/// gives a column: a check of the column, or of the domain it is declared with
/// (`CHECK (jsonb_matches_schema('…', VALUE))`). See [`json_schema_of`] for the forms read.
pub fn json_schema(
    table: &TableMetadata,
    column: &ColumnMetadata,
    catalog: &crate::catalog::DatabaseCatalog,
) -> Option<StatedJsonSchema> {
    if let Some(schema) = table
        .constraints
        .iter()
        .filter(|c| c.kind == ConstraintKind::Check && c.columns == [column.name.clone()])
        .find_map(|c| json_schema_of(&c.definition, &column.name))
    {
        let (schema, refers) = to_openapi(schema);
        let component = refers.then(|| component_name(&[&table.name, &column.name]));
        let schema = match &component {
            Some(name) => in_component(schema, name),
            None => schema,
        };
        return Some(StatedJsonSchema { schema, component });
    }
    let domain = catalog.find_domain(column.domain.as_deref()?)?;
    let schema = domain
        .checks
        .iter()
        .find_map(|check| json_schema_of(check, "VALUE"))?;
    let name = if domain.type_name.is_empty() {
        &domain.config_name
    } else {
        &domain.type_name
    };
    let name = component_name(&[name]);
    let (schema, _) = to_openapi(schema);
    Some(StatedJsonSchema {
        schema: in_component(schema, &name),
        component: Some(name),
    })
}

/// `Json.<parts>`, with the characters a component name may not have (`[A-Za-z0-9._-]`
/// only) as `_`.
fn component_name(parts: &[&str]) -> String {
    let parts: Vec<String> = parts
        .iter()
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "._-".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        })
        .collect();
    format!("Json.{}", parts.join("."))
}

/// The JSON Schema a check states for `column` (`VALUE` in a domain's check). The forms read:
/// the call alone, `column IS NULL OR` the call (NULL passes the call anyway), and the call
/// `AND` other conditions (the value satisfies the schema, and more). The schema must be a
/// literal.
pub(crate) fn json_schema_of(definition: &str, column: &str) -> Option<Value> {
    let body = unwrap(definition.trim().strip_prefix("CHECK ")?);
    let body = without_null_guard(body, column);
    conjuncts(body)
        .iter()
        .find_map(|atom| matches_schema_call(unwrap(atom), column))
}

/// `body` without a `column IS NULL OR` (either side) around it.
fn without_null_guard<'b>(body: &'b str, column: &str) -> &'b str {
    let parts = split_top_level(body, " OR ");
    let [a, b] = parts.as_slice() else {
        return body;
    };
    let is_null = |part: &str| {
        let part = unwrap(part);
        part == format!("{column} IS NULL") || part == format!("\"{column}\" IS NULL")
    };
    match (is_null(a), is_null(b)) {
        (true, false) => unwrap(b),
        (false, true) => unwrap(a),
        _ => body,
    }
}

/// Splits at the `separator`s that are not inside parentheses nor quotes.
fn split_top_level<'b>(expression: &'b str, separator: &str) -> Vec<&'b str> {
    let mut parts = Vec::new();
    let (mut depth, mut start, mut quoted) = (0i32, 0usize, false);
    let bytes = expression.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            _ if !quoted && depth == 0 && expression[i..].starts_with(separator) => {
                parts.push(&expression[start..i]);
                start = i + separator.len();
                i += separator.len() - 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&expression[start..]);
    parts
}

/// The schema of `json[b]_matches_schema('<schema>', <column>)`, possibly schema-qualified.
fn matches_schema_call(call: &str, column: &str) -> Option<Value> {
    let open = call.find("matches_schema('")?;
    // The function, possibly schema-qualified: `jsonb_matches_schema`, `json_matches_schema`
    let function = &call[..open];
    let function = function.rsplit('.').next().unwrap_or(function);
    if !matches!(function, "jsonb_" | "json_") {
        return None;
    }
    // The literal, `''` an escaped quote
    let mut literal = String::new();
    let mut chars = call[open + "matches_schema('".len()..]
        .char_indices()
        .peekable();
    let rest = loop {
        let (i, c) = chars.next()?;
        if c == '\'' {
            if chars.peek().map(|(_, c)| *c) == Some('\'') {
                chars.next();
                literal.push('\'');
                continue;
            }
            break &call[open + "matches_schema('".len() + i + 1..];
        }
        literal.push(c);
    };
    let rest = rest
        .strip_prefix("::jsonb")
        .or_else(|| rest.strip_prefix("::json"))
        .unwrap_or(rest);
    let argument = rest.strip_prefix(", ")?.strip_suffix(')')?;
    // A domain's `VALUE`, or the column (cast when it is `json` and the function `jsonb_`)
    let argument = argument
        .strip_suffix("::jsonb")
        .or_else(|| argument.strip_suffix("::json"))
        .map(|a| a.trim_start_matches('(').trim_end_matches(')'))
        .unwrap_or(argument);
    let quoted = format!("\"{column}\"");
    if argument != column && argument != quoted {
        return None;
    }
    serde_json::from_str::<Value>(&literal)
        .ok()
        .filter(|v| v.is_object() || v.is_boolean())
}

/// The schema in JSON Schema 2020-12, the dialect of OpenAPI 3.1: the keywords whose meaning
/// changed since the draft its `$schema` names are rewritten (array `items` is `prefixItems`,
/// `additionalItems` is `items`, `dependencies` is `dependentRequired` /
/// `dependentSchemas`, draft-04's boolean `exclusiveMinimum` / `exclusiveMaximum` are
/// numbers, and before 2019-09 a `$ref` ignores the keywords beside it), and `$schema`
/// is removed. Also whether the schema refers to itself (`"$ref": "#/$defs/Coding"`).
pub(crate) fn to_openapi(mut schema: Value) -> (Value, bool) {
    let draft = schema
        .get("$schema")
        .and_then(Value::as_str)
        .map(|uri| {
            if uri.contains("draft-04") {
                4
            } else if uri.contains("draft-06") {
                6
            } else if uri.contains("draft-07") {
                7
            } else if uri.contains("2019-09") {
                2019
            } else {
                2020
            }
        })
        .unwrap_or(2020);
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
    }
    let mut refers = false;
    walk(&mut schema, &mut |object| {
        if draft <= 7 && object.contains_key("$ref") {
            // Before 2019-09 a reference replaces the schema it is in
            object.retain(|k, _| {
                matches!(
                    k.as_str(),
                    "$ref"
                        | "definitions"
                        | "$defs"
                        | "$id"
                        | "id"
                        | "title"
                        | "description"
                        | "$comment"
                )
            });
        }
        if draft <= 2019 {
            if object.get("items").is_some_and(Value::is_array) {
                let items = object.remove("items").unwrap_or_default();
                object.insert("prefixItems".into(), items);
                if let Some(additional) = object.remove("additionalItems") {
                    object.insert("items".into(), additional);
                }
            } else {
                // Beside a single `items`, `additionalItems` meant nothing
                object.remove("additionalItems");
            }
        }
        if draft <= 7 {
            if let Some(Value::Object(dependencies)) = object.remove("dependencies") {
                let (mut required, mut schemas) = (Map::new(), Map::new());
                for (name, dependency) in dependencies {
                    if dependency.is_array() {
                        required.insert(name, dependency);
                    } else {
                        schemas.insert(name, dependency);
                    }
                }
                if !required.is_empty() {
                    object.insert("dependentRequired".into(), Value::Object(required));
                }
                if !schemas.is_empty() {
                    object.insert("dependentSchemas".into(), Value::Object(schemas));
                }
            }
        }
        if draft == 4 {
            for (flag, bound) in [
                ("exclusiveMinimum", "minimum"),
                ("exclusiveMaximum", "maximum"),
            ] {
                match object.get(flag) {
                    Some(Value::Bool(true)) => {
                        if let Some(value) = object.remove(bound) {
                            object.insert(flag.into(), value);
                        } else {
                            object.remove(flag);
                        }
                    }
                    Some(Value::Bool(false)) => {
                        object.remove(flag);
                    }
                    _ => {}
                }
            }
        }
        for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
            if object
                .get(keyword)
                .and_then(Value::as_str)
                .is_some_and(|r| r == "#" || r.starts_with("#/"))
            {
                refers = true;
            }
        }
    });
    (schema, refers)
}

/// The schema as the component `name`: its references to itself point into the component
/// (`#/$defs/Coding` → `#/components/schemas/<name>/$defs/Coding`), and its root `$id` is
/// gone (references are resolved in the document).
fn in_component(mut schema: Value, name: &str) -> Value {
    if let Some(object) = schema.as_object_mut() {
        object.remove("$id");
    }
    let base = format!("#/components/schemas/{name}");
    walk(&mut schema, &mut |object| {
        for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
            if let Some(Value::String(r)) = object.get_mut(keyword) {
                if r == "#" || r.starts_with("#/") {
                    *r = format!("{base}{}", &r[1..]);
                }
            }
        }
    });
    schema
}

/// Calls `f` on every schema object of `schema`: the root, then those in its keywords that
/// hold schemas (`properties`, `items`, `allOf`, `$defs`, ...), after `f` changed them.
fn walk(schema: &mut Value, f: &mut impl FnMut(&mut Map<String, Value>)) {
    let Value::Object(object) = schema else {
        return;
    };
    f(object);
    for (keyword, value) in object.iter_mut() {
        match keyword.as_str() {
            "additionalProperties"
            | "additionalItems"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else"
            | "propertyNames"
            | "unevaluatedItems"
            | "unevaluatedProperties"
            | "contentSchema" => walk(value, f),
            "items" => match value {
                Value::Array(items) => items.iter_mut().for_each(|v| walk(v, f)),
                other => walk(other, f),
            },
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                if let Value::Array(items) = value {
                    items.iter_mut().for_each(|v| walk(v, f));
                }
            }
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas"
            | "dependencies" => {
                if let Value::Object(members) = value {
                    members.values_mut().for_each(|v| walk(v, f));
                }
            }
            _ => {}
        }
    }
}

/// The keywords a table's `CHECK` constraints give a column.
/// Those of the checks of the column's domain (`CHECK (VALUE ~ '…')`) complete them.
pub fn keywords(
    table: &TableMetadata,
    column: &ColumnMetadata,
    catalog: &crate::catalog::DatabaseCatalog,
) -> Map<String, Value> {
    let mut out = keywords_of(&table.constraints, column);
    let Some(domain) = column
        .domain
        .as_deref()
        .and_then(|k| catalog.find_domain(k))
    else {
        return out;
    };
    // A domain's check names its value `VALUE`
    let value = ColumnMetadata {
        name: "VALUE".to_string(),
        ..column.clone()
    };
    let checks: Vec<ConstraintMetadata> = domain
        .checks
        .iter()
        .map(|definition| ConstraintMetadata {
            name: String::new(),
            kind: ConstraintKind::Check,
            columns: vec![value.name.clone()],
            definition: definition.clone(),
            references: None,
            period: false,
            code: String::new(),
            referenced_code: String::new(),
        })
        .collect();
    for (keyword, v) in keywords_of(&checks, &value) {
        out.entry(keyword).or_insert(v);
    }
    out
}

fn keywords_of(constraints: &[ConstraintMetadata], column: &ColumnMetadata) -> Map<String, Value> {
    let mut out = Map::new();
    // Most columns have no `CHECK` of their own: the patterns below are compiled per column,
    // which on a schema of thousands of columns costs seconds when done for every one
    let checks: Vec<&ConstraintMetadata> = constraints
        .iter()
        .filter(|c| {
            c.kind == ConstraintKind::Check && c.columns.len() == 1 && c.columns[0] == column.name
        })
        .collect();
    if checks.is_empty() {
        return out;
    }
    let numeric = matches!(
        column.pg_type,
        PgType::Int2 | PgType::Int4 | PgType::Float4 | PgType::Float8
    );
    let text = matches!(
        column.pg_type,
        PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext
    );
    let array = matches!(column.pg_type, PgType::Array(_));
    let name = regex::escape(&column.name);
    let ident = format!("(?:\"{name}\"|{name})");
    let cast = Regex::new(
        r"\(?(-?\d+(?:\.\d+)?)\)?::(?:numeric|integer|bigint|smallint|double precision|real)",
    )
    .unwrap();
    let bound = Regex::new(&format!(r"^\(?{ident}\)? (>=|>|<=|<) (-?\d+(?:\.\d+)?)$")).unwrap();
    let multiple = Regex::new(&format!(r"^\(?{ident} % (\d+)\)? = 0$")).unwrap();
    let length = Regex::new(&format!(
        r"^(?:char_length|character_length|length)\({ident}\) (>=|>|<=|<|=) (\d+)$"
    ))
    .unwrap();
    let items = Regex::new(&format!(
        r"^(?:cardinality|array_length)\({ident}(?:, 1)?\) (>=|>|<=|<|=) (\d+)$"
    ))
    .unwrap();
    let pattern = Regex::new(&format!(r"^\(?{ident}\)? ~ '((?:[^']|'')*)'(?:::[\w ]+)?$")).unwrap();
    // PostgreSQL casts a `varchar` or domain operand to the operator's type, `(code)::text`
    let column_cast = Regex::new(&format!(
        r"\(({ident})\)::(?:text|character varying|varchar|character|bpchar|citext)(?:\(\d+\))?"
    ))
    .unwrap();

    for constraint in checks {
        let Some(definition) = constraint.definition.trim().strip_prefix("CHECK ") else {
            continue;
        };
        let mut definition = cast.replace_all(definition, "$1").into_owned();
        // Nested casts, `((code)::character varying)::text`, come off one at a time
        while let std::borrow::Cow::Owned(next) = column_cast.replace_all(&definition, "$1") {
            if next == definition {
                break;
            }
            definition = next;
        }
        for atom in conjuncts(unwrap(&definition)) {
            let atom = unwrap(&atom);
            if let (true, Some(c)) = (numeric, bound.captures(atom)) {
                let Some(value) = number(&c[2]) else { continue };
                let keyword = match &c[1] {
                    ">=" => "minimum",
                    ">" => "exclusiveMinimum",
                    "<=" => "maximum",
                    _ => "exclusiveMaximum",
                };
                out.insert(keyword.to_string(), value);
            } else if let (true, Some(c)) = (numeric, multiple.captures(atom)) {
                if let Some(step) = number(&c[1]).filter(|n| n.as_i64() != Some(0)) {
                    out.insert("multipleOf".into(), step);
                }
            } else if let (true, Some(c)) = (text, length.captures(atom)) {
                limit(&mut out, &c[1], &c[2], "minLength", "maxLength");
            } else if let (true, Some(c)) = (array, items.captures(atom)) {
                limit(&mut out, &c[1], &c[2], "minItems", "maxItems");
            } else if let (true, Some(c)) = (text, pattern.captures(atom)) {
                out.insert("pattern".into(), json!(c[1].replace("''", "'")));
            }
        }
    }
    out
}

/// A comparison against a count, as the inclusive minimum and maximum of a size.
fn limit(out: &mut Map<String, Value>, op: &str, n: &str, min: &str, max: &str) {
    let Ok(n) = n.parse::<i64>() else { return };
    match op {
        ">=" => out.insert(min.to_string(), json!(n)),
        ">" => out.insert(min.to_string(), json!(n + 1)),
        "<=" => out.insert(max.to_string(), json!(n)),
        "<" => out.insert(max.to_string(), json!(n - 1)),
        _ => {
            out.insert(min.to_string(), json!(n));
            out.insert(max.to_string(), json!(n))
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_schema_checks() {
        let schema = json_schema_of(
            r#"CHECK (jsonb_matches_schema('{"type": "object", "properties": {"it''s": {"type": "integer"}}}'::json, settings))"#,
            "settings",
        )
        .unwrap();
        assert_eq!(schema["properties"]["it's"]["type"], "integer");
        assert!(json_schema_of(
            "CHECK (extensions.json_matches_schema('{\"type\": \"array\"}'::json, \"Data\"))",
            "Data"
        )
        .is_some());
        // Another column, an OR, or not a literal: not the column's type
        assert!(json_schema_of(
            "CHECK (jsonb_matches_schema('{}'::json, other))",
            "settings"
        )
        .is_none());
        assert!(json_schema_of(
            "CHECK ((jsonb_matches_schema('{}'::json, settings) OR (settings ? 'a'::text)))",
            "settings"
        )
        .is_none());
        // With other conditions, the value still satisfies the schema; NULL passes it anyway
        for check in [
            "CHECK ((jsonb_matches_schema('{\"type\": \"array\"}'::json, settings) AND (settings ? 'a'::text)))",
            "CHECK (((settings IS NULL) OR jsonb_matches_schema('{\"type\": \"array\"}'::json, settings)))",
            "CHECK ((jsonb_matches_schema('{\"type\": \"array\"}'::json, settings) OR (settings IS NULL)))",
        ] {
            assert_eq!(
                json_schema_of(check, "settings"),
                Some(json!({ "type": "array" })),
                "{check}"
            );
        }
        // A domain's check
        assert_eq!(
            json_schema_of(
                "CHECK (jsonb_matches_schema('{\"type\": \"object\"}'::json, VALUE))",
                "VALUE"
            ),
            Some(json!({ "type": "object" }))
        );
        assert!(json_schema_of(
            "CHECK (jsonb_matches_schema(schema_of(settings), settings))",
            "settings"
        )
        .is_none());
    }
    #[test]
    fn older_drafts_read_as_2020_12() {
        let (schema, refers) = to_openapi(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "definitions": { "Coding": { "type": "object" } },
            "type": "object",
            "properties": {
                "pair": { "type": "array", "items": [{ "type": "string" }, { "type": "integer" }], "additionalItems": false },
                "list": { "type": "array", "items": { "type": "string" }, "additionalItems": false },
                "coding": { "$ref": "#/definitions/Coding", "description": "kept", "type": "string" }
            },
            "dependencies": { "a": ["b"], "c": { "required": ["d"] } }
        }));
        assert!(refers);
        assert!(schema.get("$schema").is_none());
        let props = &schema["properties"];
        assert_eq!(
            props["pair"]["prefixItems"],
            json!([{ "type": "string" }, { "type": "integer" }])
        );
        assert_eq!(props["pair"]["items"], json!(false));
        assert_eq!(
            props["list"],
            json!({ "type": "array", "items": { "type": "string" } })
        );
        // Before 2019-09, the keywords beside a `$ref` meant nothing
        assert_eq!(
            props["coding"],
            json!({ "$ref": "#/definitions/Coding", "description": "kept" })
        );
        assert_eq!(schema["dependentRequired"], json!({ "a": ["b"] }));
        assert_eq!(
            schema["dependentSchemas"],
            json!({ "c": { "required": ["d"] } })
        );

        let (schema, refers) = to_openapi(json!({
            "$schema": "http://json-schema.org/draft-04/schema#",
            "type": "number", "minimum": 0, "exclusiveMinimum": true, "maximum": 9, "exclusiveMaximum": false
        }));
        assert!(!refers);
        assert_eq!(
            schema,
            json!({ "type": "number", "exclusiveMinimum": 0, "maximum": 9 })
        );

        // 2020-12 (or no `$schema`) is left as it is
        let same = json!({ "type": "array", "items": { "type": "string" }, "prefixItems": [{ "type": "string" }] });
        assert_eq!(to_openapi(same.clone()), (same, false));
    }

    #[test]
    fn a_component_refers_into_itself() {
        let schema = in_component(
            json!({
                "$id": "https://example.com/address",
                "$defs": { "Line": { "type": "string" } },
                "type": "object",
                "properties": {
                    "line": { "$ref": "#/$defs/Line" },
                    "parent": { "$ref": "#" },
                    "external": { "$ref": "https://example.com/other" },
                    "$ref": { "type": "string", "description": "a property named `$ref`" }
                }
            }),
            "Json.addresses.value",
        );
        assert!(schema.get("$id").is_none());
        let props = &schema["properties"];
        assert_eq!(
            props["line"]["$ref"],
            "#/components/schemas/Json.addresses.value/$defs/Line"
        );
        assert_eq!(
            props["parent"]["$ref"],
            "#/components/schemas/Json.addresses.value"
        );
        assert_eq!(props["external"]["$ref"], "https://example.com/other");
        assert_eq!(props["$ref"]["type"], "string");
        assert_eq!(component_name(&["my table", "col"]), "Json.my_table.col");
    }

    #[test]
    fn domain_checks_give_keywords() {
        let check = |definition: &str| ConstraintMetadata {
            name: String::new(),
            kind: ConstraintKind::Check,
            columns: vec!["VALUE".to_string()],
            definition: definition.to_string(),
            references: None,
            period: false,
            code: String::new(),
            referenced_code: String::new(),
        };
        let value = FunctionArgument {
            name: "VALUE".to_string(),
            pg_type: PgType::Text,
            has_default: false,
        }
        .as_column();
        assert_eq!(
            Value::Object(keywords_of(
                &[check("CHECK ((VALUE ~ '^[0-9]{4}$'::text))")],
                &value
            )),
            json!({ "pattern": "^[0-9]{4}$" })
        );
    }

    use crate::catalog::{ConstraintMetadata, FunctionArgument};

    fn of(column: &str, pg_type: PgType, check: &str) -> Value {
        let column = FunctionArgument {
            name: column.to_string(),
            pg_type,
            has_default: false,
        }
        .as_column();
        let constraint = ConstraintMetadata {
            name: "c".to_string(),
            kind: ConstraintKind::Check,
            columns: vec![column.name.clone()],
            definition: check.to_string(),
            references: None,
            period: false,
            code: String::new(),
            referenced_code: String::new(),
        };
        Value::Object(keywords_of(&[constraint], &column))
    }

    #[test]
    fn bounds_steps_lengths_items_and_patterns() {
        assert_eq!(
            of(
                "score",
                PgType::Int4,
                "CHECK (((score >= 0) AND (score <= 100)))"
            ),
            json!({ "minimum": 0, "maximum": 100 })
        );
        assert_eq!(
            of(
                "rating",
                PgType::Float8,
                "CHECK (((rating > (0)::numeric) AND (rating < (5)::numeric)))"
            ),
            json!({ "exclusiveMinimum": 0, "exclusiveMaximum": 5 })
        );
        assert_eq!(
            of("step", PgType::Int4, "CHECK ((((step % 5) = 0)))"),
            json!({ "multipleOf": 5 })
        );
        assert_eq!(
            of(
                "title",
                PgType::Text,
                "CHECK (((char_length(title) >= 3) AND (char_length(title) < 21)))"
            ),
            json!({ "minLength": 3, "maxLength": 20 })
        );
        assert_eq!(
            of(
                "tags",
                PgType::Array(Box::new(PgType::Text)),
                "CHECK ((cardinality(tags) <= 5))"
            ),
            json!({ "maxItems": 5 })
        );
        assert_eq!(
            of("code", PgType::Text, "CHECK ((code ~ '^[A-Z]{3}$'::text))"),
            json!({ "pattern": "^[A-Z]{3}$" })
        );
    }

    #[test]
    fn varchar_and_domain_operands_cast_to_text() {
        // As PostgreSQL 16 and 18 deparse checks on a domain over text and on varchar (#216)
        assert_eq!(
            of(
                "birth_date",
                PgType::Text,
                "CHECK (((birth_date)::text ~ '^[0-9]{4}$'::text))"
            ),
            json!({ "pattern": "^[0-9]{4}$" })
        );
        assert_eq!(
            of(
                "Code",
                PgType::Varchar,
                "CHECK (((\"Code\")::text ~ '^[A-Z]+$'::text))"
            ),
            json!({ "pattern": "^[A-Z]+$" })
        );
        assert_eq!(
            of(
                "name",
                PgType::Varchar,
                "CHECK (((char_length((name)::text) >= 2) AND (char_length((name)::text) <= 20)))"
            ),
            json!({ "minLength": 2, "maxLength": 20 })
        );
        assert_eq!(
            of(
                "code",
                PgType::Varchar,
                "CHECK ((((code)::character varying)::text ~ '^x$'::text))"
            ),
            json!({ "pattern": "^x$" })
        );
    }

    #[test]
    fn what_cannot_be_read_is_left_out() {
        // OR, other columns, 64-bit and decimal types (they are strings in JSON)
        assert_eq!(
            of("n", PgType::Int4, "CHECK (((n > 0) OR (n IS NULL)))"),
            json!({})
        );
        assert_eq!(of("lo", PgType::Int4, "CHECK ((lo < hi))"), json!({}));
        assert_eq!(of("big", PgType::Int8, "CHECK ((big >= 0))"), json!({}));
        assert_eq!(
            of("price", PgType::Numeric, "CHECK ((price >= (0)::numeric))"),
            json!({})
        );
        assert_eq!(of("n", PgType::Text, "CHECK ((n >= 0))"), json!({}));
    }
}
