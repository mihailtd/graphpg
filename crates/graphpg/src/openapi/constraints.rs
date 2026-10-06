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

/// The keywords a table's `CHECK` constraints give a column.
pub fn keywords(table: &TableMetadata, column: &ColumnMetadata) -> Map<String, Value> {
    keywords_of(&table.constraints, column)
}

fn keywords_of(constraints: &[ConstraintMetadata], column: &ColumnMetadata) -> Map<String, Value> {
    let mut out = Map::new();
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
    let pattern = Regex::new(&format!(r"^\(?{ident}\)? ~ '((?:[^']|'')*)'(?:::\w+)?$")).unwrap();

    for constraint in constraints.iter().filter(|c| {
        c.kind == ConstraintKind::Check && c.columns.len() == 1 && c.columns[0] == column.name
    }) {
        let Some(definition) = constraint.definition.trim().strip_prefix("CHECK ") else {
            continue;
        };
        let definition = cast.replace_all(definition, "$1");
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
