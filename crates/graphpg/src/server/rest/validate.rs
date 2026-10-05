//! Validation of REST input against the generated GraphQL input types.
//!
//! The planner reads arguments as JSON and silently ignores keys it does not know, so a typo
//! (`filter[name][eqq]`, `{"nme": "x"}`) would turn into an unfiltered read or a no-op write.
//! The input types of the schema describe exactly what each argument accepts, so REST checks
//! the JSON against them before planning: unknown keys and invalid enum values are rejected.

use apollo_compiler::schema::ExtendedType;
use apollo_compiler::{ast::Type, Schema};

use super::problem::ProblemDetails;

/// Which root operation type a field belongs to.
#[derive(Debug, Clone, Copy)]
pub enum Root {
    Query,
    Mutation,
}

/// Checks `value` against the type of argument `arg` of root field `field`.
///
/// `path` names the value in error messages (`filter`, `input`). A field or argument the
/// schema does not have is not an error here: the planner reports those.
pub fn validate_argument(
    schema: &Schema,
    root: Root,
    field: &str,
    arg: &str,
    value: &serde_json::Value,
    path: &str,
) -> Result<(), ProblemDetails> {
    let Some(definition) = root_field(schema, root, field) else {
        return Ok(());
    };
    let Some(argument) = definition.argument_by_name(arg) else {
        return Ok(());
    };
    check(schema, &argument.ty, value, path)
        .map_err(|(field, message)| ProblemDetails::invalid(message, field))
}

/// Whether root field `field` takes an argument `arg`.
pub fn has_argument(schema: &Schema, root: Root, field: &str, arg: &str) -> bool {
    root_field(schema, root, field)
        .and_then(|f| f.argument_by_name(arg))
        .is_some()
}

fn root_field<'a>(
    schema: &'a Schema,
    root: Root,
    field: &str,
) -> Option<&'a apollo_compiler::ast::FieldDefinition> {
    let root_name = match root {
        Root::Query => schema.schema_definition.query.as_ref(),
        Root::Mutation => schema.schema_definition.mutation.as_ref(),
    }?;
    match schema.types.get(root_name.as_str())? {
        ExtendedType::Object(object) => object.fields.get(field).map(|f| &***f),
        _ => None,
    }
}

fn check(
    schema: &Schema,
    ty: &Type,
    value: &serde_json::Value,
    path: &str,
) -> Result<(), (String, String)> {
    if value.is_null() {
        return Ok(());
    }
    match ty {
        Type::List(inner) | Type::NonNullList(inner) => match value {
            serde_json::Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    check(schema, inner, item, &format!("{path}[{i}]"))?;
                }
                Ok(())
            }
            // GraphQL coerces a single value into a one-element list
            other => check(schema, inner, other, path),
        },
        Type::Named(name) | Type::NonNullNamed(name) => match schema.types.get(name.as_str()) {
            Some(ExtendedType::InputObject(input)) => {
                let Some(object) = value.as_object() else {
                    return Err((path.to_string(), format!("`{path}` must be an object")));
                };
                for (key, member) in object {
                    let member_path = format!("{path}.{key}");
                    match input.fields.get(key.as_str()) {
                        None => {
                            let mut known: Vec<&str> =
                                input.fields.keys().map(|k| k.as_str()).collect();
                            known.sort_unstable();
                            return Err((
                                member_path.clone(),
                                format!(
                                    "unknown field `{member_path}`; `{path}` accepts: {}",
                                    known.join(", ")
                                ),
                            ));
                        }
                        Some(field) => check(schema, &field.ty, member, &member_path)?,
                    }
                }
                Ok(())
            }
            Some(ExtendedType::Enum(e)) => match value.as_str() {
                Some(s) if e.values.contains_key(s) => Ok(()),
                _ => Err((
                    path.to_string(),
                    format!("`{path}` must be one of the values of `{name}`"),
                )),
            },
            _ => Ok(()),
        },
    }
}
