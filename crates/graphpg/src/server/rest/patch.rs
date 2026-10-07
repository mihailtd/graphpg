//! RFC 6902 JSON Patch applied to the document of a row (`application/json-patch+json`).
//!
//! The document is the row as the API shows it, so `/title` is the `title` column and
//! `/settings/theme` reaches into a JSON column.

use serde_json::Value;

/// Applies the operations (`add`, `remove`, `replace`, `move`, `copy`, `test`) in order to `doc`.
/// On an error `doc` is left as the failing operation found it: apply to a copy.
pub fn apply(doc: &mut Value, patch: &Value) -> Result<(), String> {
    let ops = patch
        .as_array()
        .ok_or("A JSON Patch is an array of operations")?;
    for (i, op) in ops.iter().enumerate() {
        apply_one(doc, op).map_err(|e| format!("operation {i}: {e}"))?;
    }
    Ok(())
}

fn apply_one(doc: &mut Value, op: &Value) -> Result<(), String> {
    let name = op
        .get("op")
        .and_then(Value::as_str)
        .ok_or("`op` is missing")?;
    let path = pointer(
        op.get("path")
            .and_then(Value::as_str)
            .ok_or("`path` is missing")?,
    )?;
    let value = || op.get("value").cloned().ok_or("`value` is missing");
    let from = || -> Result<Vec<String>, String> {
        pointer(
            op.get("from")
                .and_then(Value::as_str)
                .ok_or("`from` is missing")?,
        )
    };
    match name {
        "add" => add(doc, &path, value()?),
        "remove" => remove(doc, &path).map(|_| ()),
        "replace" => {
            // The target must exist
            get(doc, &path)?;
            remove(doc, &path)?;
            add(doc, &path, value()?)
        }
        "move" => {
            let from = from()?;
            if path.starts_with(&from) && path.len() > from.len() {
                return Err("cannot move a value into itself".into());
            }
            let moved = remove(doc, &from)?;
            add(doc, &path, moved)
        }
        "copy" => {
            let copied = get(doc, &from()?)?.clone();
            add(doc, &path, copied)
        }
        "test" => {
            if *get(doc, &path)? == value()? {
                Ok(())
            } else {
                Err(format!(
                    "the value at `/{}` is not the one tested",
                    path.join("/")
                ))
            }
        }
        other => Err(format!("unknown operation `{other}`")),
    }
}

/// A JSON Pointer (RFC 6901) as its unescaped tokens.
fn pointer(path: &str) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Err("a row cannot be replaced as a whole: name a column (`/title`)".into());
    }
    path.strip_prefix('/')
        .ok_or_else(|| format!("`{path}` is not a JSON Pointer"))
        .map(|rest| {
            rest.split('/')
                .map(|t| t.replace("~1", "/").replace("~0", "~"))
                .collect()
        })
}

fn get<'a>(doc: &'a Value, path: &[String]) -> Result<&'a Value, String> {
    let mut current = doc;
    for token in path {
        current = match current {
            Value::Object(map) => map.get(token),
            Value::Array(items) => token.parse::<usize>().ok().and_then(|i| items.get(i)),
            _ => None,
        }
        .ok_or_else(|| format!("`/{}` does not exist", path.join("/")))?;
    }
    Ok(current)
}

fn parent<'a>(doc: &'a mut Value, path: &'a [String]) -> Result<(&'a mut Value, &'a str), String> {
    let (last, rest) = path.split_last().ok_or("an empty path")?;
    let mut current = doc;
    for token in rest {
        current = match current {
            Value::Object(map) => map.get_mut(token),
            Value::Array(items) => token.parse::<usize>().ok().and_then(|i| items.get_mut(i)),
            _ => None,
        }
        .ok_or_else(|| format!("`/{}` does not exist", path.join("/")))?;
    }
    Ok((current, last.as_str()))
}

fn add(doc: &mut Value, path: &[String], value: Value) -> Result<(), String> {
    let (parent, last) = parent(doc, path)?;
    match parent {
        Value::Object(map) => {
            map.insert(last.to_string(), value);
            Ok(())
        }
        Value::Array(items) => {
            let index = if last == "-" {
                items.len()
            } else {
                last.parse::<usize>()
                    .ok()
                    .filter(|i| *i <= items.len())
                    .ok_or_else(|| format!("`{last}` is not an index of the array"))?
            };
            items.insert(index, value);
            Ok(())
        }
        _ => Err(format!(
            "`/{}` is not in an object or array",
            path.join("/")
        )),
    }
}

fn remove(doc: &mut Value, path: &[String]) -> Result<Value, String> {
    let (parent, last) = parent(doc, path)?;
    match parent {
        Value::Object(map) => map
            .remove(last)
            .ok_or_else(|| format!("`/{}` does not exist", path.join("/"))),
        Value::Array(items) => {
            let index = last
                .parse::<usize>()
                .ok()
                .filter(|i| *i < items.len())
                .ok_or_else(|| format!("`{last}` is not an index of the array"))?;
            Ok(items.remove(index))
        }
        _ => Err(format!("`/{}` does not exist", path.join("/"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn applies_the_operations_in_order() {
        let mut doc = json!({ "title": "a", "tags": ["x", "y"], "meta": { "k": 1 } });
        apply(
            &mut doc,
            &json!([
                { "op": "test", "path": "/title", "value": "a" },
                { "op": "replace", "path": "/title", "value": "b" },
                { "op": "add", "path": "/tags/-", "value": "z" },
                { "op": "add", "path": "/tags/0", "value": "w" },
                { "op": "remove", "path": "/tags/1" },
                { "op": "copy", "from": "/title", "path": "/subtitle" },
                { "op": "move", "from": "/meta/k", "path": "/n" },
                { "op": "add", "path": "/meta/a~1b", "value": true }
            ]),
        )
        .unwrap();
        assert_eq!(
            doc,
            json!({ "title": "b", "tags": ["w", "y", "z"], "meta": { "a/b": true },
                    "subtitle": "b", "n": 1 })
        );
    }

    #[test]
    fn a_failing_operation_is_named() {
        let mut doc = json!({ "title": "a" });
        for (patch, expected) in [
            (
                json!([{ "op": "test", "path": "/title", "value": "x" }]),
                "operation 0",
            ),
            (
                json!([{ "op": "remove", "path": "/nope" }]),
                "does not exist",
            ),
            (
                json!([{ "op": "replace", "path": "/nope", "value": 1 }]),
                "does not exist",
            ),
            (
                json!([{ "op": "add", "path": "/a/b", "value": 1 }]),
                "does not exist",
            ),
            (
                json!([{ "op": "frobnicate", "path": "/title" }]),
                "unknown operation",
            ),
            (
                json!([{ "op": "add", "path": "/title" }]),
                "`value` is missing",
            ),
            (
                json!([{ "op": "add", "path": "title", "value": 1 }]),
                "not a JSON Pointer",
            ),
            (
                json!([{ "op": "add", "path": "", "value": 1 }]),
                "a row cannot be replaced",
            ),
            (json!({ "op": "add" }), "array of operations"),
        ] {
            let err = apply(&mut doc, &patch).unwrap_err();
            assert!(err.contains(expected), "{patch}: {err}");
        }
    }
}
