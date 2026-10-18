//! Constraint violations as typed errors: a unique, foreign key, check, exclusion or NOT NULL
//! violation is reported with the code of the violated constraint (a value of the table's
//! `<Type>ErrorCode` enum) and the input fields holding the offending values, instead of the
//! database's message.
use super::engine::ExecutionError;
use crate::catalog::{ConstraintKind, ConstraintMetadata, DatabaseCatalog, TableMetadata};
use crate::compiler::{InputRow, WriteContext, WriteVerb};
use serde::Serialize;
use tokio_postgres::error::{DbError, SqlState};

/// A write a constraint rejected.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConstraintViolation {
    pub message: String,
    /// `EMAIL_TAKEN`, `TEAM_NOT_FOUND`, `REFERENCED_BY_PLAYERS`, ...
    pub code: String,
    /// The enum `code` belongs to (`UserErrorCode`).
    #[serde(rename = "type")]
    pub type_name: String,
    /// The violated constraint (or unique index); `None` for NOT NULL.
    pub constraint: Option<String>,
    /// Input paths of the offending values (`input.email`); just the field names when the
    /// statement wrote several rows and the database did not say which one failed.
    pub fields: Vec<String>,
    /// Why the value fails the JSON Schema of a pg_jsonschema check: one entry per failed
    /// keyword, where it is in the value.
    #[serde(rename = "validationErrors", skip_serializing_if = "Vec::is_empty")]
    pub validation_errors: Vec<SchemaViolation>,
}

/// A part of a JSON value its JSON Schema rejects.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SchemaViolation {
    /// JSON Pointer into the value (`/telecom/0/system`; empty for the value itself).
    pub path: String,
    pub message: String,
}

impl ExecutionError {
    /// The constraint violation this error is, if any.
    pub fn violation(&self, catalog: &DatabaseCatalog) -> Option<ConstraintViolation> {
        match self {
            ExecutionError::Mutation { source, .. } => source.violation(catalog),
            ExecutionError::Write { error, context } => {
                violation(catalog, error.as_db_error()?, context.as_ref())
            }
            ExecutionError::Postgres(error) => violation(catalog, error.as_db_error()?, None),
            ExecutionError::StaleVersion { schema, table } => {
                let t = catalog.find_table(schema, table)?;
                Some(ConstraintViolation {
                    message: format!(
                        "`expectedVersion`: the `{}` changed since that version was read",
                        t.type_name
                    ),
                    code: "STALE_VERSION".to_string(),
                    type_name: error_enum(t),
                    constraint: None,
                    fields: vec!["expectedVersion".to_string()],
                    validation_errors: Vec::new(),
                })
            }
            _ => None,
        }
    }
}

/// Maps a database error to the violation of a constraint the catalog knows.
pub fn violation(
    catalog: &DatabaseCatalog,
    db: &DbError,
    context: Option<&WriteContext>,
) -> Option<ConstraintViolation> {
    let (schema, table) = match (db.schema(), db.table()) {
        (Some(schema), Some(table)) => (schema, table),
        // A foreign data wrapper re-raises the remote error with its SQLSTATE and detail
        // only: the table is the one being written
        _ => {
            let context = context?;
            (context.schema.as_str(), context.table.as_str())
        }
    };
    let t = catalog.find_table(schema, table)?;
    let code = db.code();
    if *code == SqlState::NOT_NULL_VIOLATION {
        let col = t.get_column(db.column()?)?;
        let row = pick_row(context, t, &[], db.detail()).or_else(|| {
            // The row leaving the column NULL
            context?
                .rows
                .iter()
                .find(|r| r.values.iter().any(|(c, v)| *c == col.name && v.is_none()))
        });
        let field = field_path(row, &col.field_name);
        return Some(ConstraintViolation {
            message: format!("`{field}` is required: `{}` cannot be null", t.type_name),
            code: DatabaseCatalog::column_error_code(col, "REQUIRED"),
            type_name: error_enum(t),
            constraint: None,
            fields: vec![field],
            validation_errors: Vec::new(),
        });
    }
    // A check of a domain: PostgreSQL names the type, not the table nor the column
    if *code == SqlState::CHECK_VIOLATION {
        if let Some(datatype) = db.datatype() {
            return domain_violation(catalog, t, db, datatype, context);
        }
    }
    let kinds: &[ConstraintKind] = match code {
        c if *c == SqlState::UNIQUE_VIOLATION => {
            &[ConstraintKind::Unique, ConstraintKind::PrimaryKey]
        }
        c if *c == SqlState::FOREIGN_KEY_VIOLATION => &[ConstraintKind::ForeignKey],
        c if *c == SqlState::CHECK_VIOLATION => &[ConstraintKind::Check],
        c if *c == SqlState::EXCLUSION_VIOLATION => &[ConstraintKind::Exclusion],
        _ => return None,
    };
    let con = match db.constraint() {
        Some(name) => catalog.find_constraint(schema, table, name)?.1,
        // Unnamed (a foreign table's remote key): the constraint on the columns the error
        // detail quotes (`Key (id)=(1) already exists.`)
        None => {
            let (names, _) = db.detail().and_then(detail_key)?;
            let columns: Vec<&str> = names.split(", ").collect();
            t.constraints.iter().find(|c| {
                kinds.contains(&c.kind)
                    && c.columns.len() == columns.len()
                    && c.columns.iter().zip(&columns).all(|(a, b)| a == b)
            })?
        }
    };
    if !kinds.contains(&con.kind) {
        return None;
    }
    if con.kind == ConstraintKind::ForeignKey && references_side(con, t, context, db) {
        let (ref_schema, ref_table) = con.references.as_ref()?;
        let parent = catalog.find_table(ref_schema, ref_table)?;
        return Some(ConstraintViolation {
            message: format!(
                "the `{}` row is still referenced by `{}` rows (`{}`)",
                parent.type_name, t.type_name, con.name
            ),
            code: con.referenced_code.clone(),
            type_name: error_enum(parent),
            constraint: Some(con.name.clone()),
            fields: Vec::new(),
            validation_errors: Vec::new(),
        });
    }

    let mut row = pick_row(context, t, &con.columns, db.detail());
    // A pg_jsonschema check: why the value fails it, from the row whose value does
    let mut validation_errors = Vec::new();
    if let (ConstraintKind::Check, [column]) = (con.kind, con.columns.as_slice()) {
        if let Some(schema) = crate::openapi::constraints::json_schema_of(&con.definition, column) {
            let candidates: Vec<&InputRow> = match row {
                Some(r) => vec![r],
                None => context.map(|c| c.rows.iter().collect()).unwrap_or_default(),
            };
            for candidate in candidates {
                let errors = schema_violations(&schema, candidate, column);
                if !errors.is_empty() {
                    row = Some(candidate);
                    validation_errors = errors;
                    break;
                }
            }
        }
    }
    let fields: Vec<String> = con
        .columns
        .iter()
        .map(|c| {
            field_path(
                row,
                t.get_column(c).map(|c| c.field_name.as_str()).unwrap_or(c),
            )
        })
        .collect();
    let at = match fields.as_slice() {
        [] => String::new(),
        fields => format!(
            "{}: ",
            fields
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let what = match con.kind {
        ConstraintKind::PrimaryKey => {
            format!("a `{}` with this primary key already exists", t.type_name)
        }
        ConstraintKind::Unique => format!(
            "another `{}` already has this value (unique `{}`)",
            t.type_name, con.name
        ),
        ConstraintKind::ForeignKey => {
            let target = con
                .references
                .as_ref()
                .map(|(s, tb)| catalog.type_name_of(s, tb))
                .unwrap_or_default();
            format!("no `{target}` has this key (`{}`)", con.name)
        }
        ConstraintKind::Check => match con.check_expression() {
            Some(expr) => format!("check `{}` failed: `{expr}`", con.name),
            None => format!("check `{}` failed", con.name),
        },
        ConstraintKind::Exclusion => format!(
            "conflicts with another `{}` (exclusion `{}`)",
            t.type_name, con.name
        ),
    };
    Some(ConstraintViolation {
        message: format!("{at}{what}"),
        code: con.code.clone(),
        type_name: error_enum(t),
        constraint: Some(con.name.clone()),
        fields,
        validation_errors,
    })
}

/// A value a check of domain `datatype` rejected, in a write of `t`: the column of that
/// domain, in the row whose value fails a pg_jsonschema check of the domain (when it is one),
/// else the first row setting such a column.
fn domain_violation(
    catalog: &DatabaseCatalog,
    t: &TableMetadata,
    db: &DbError,
    datatype: &str,
    context: Option<&WriteContext>,
) -> Option<ConstraintViolation> {
    let domain = catalog.domains.iter().find(|d| {
        d.config_name == datatype
            && db
                .schema()
                .is_none_or(|s| d.key.starts_with(&format!("{}.", quote(s))))
    })?;
    let columns: Vec<_> = t
        .writable_columns()
        .filter(|c| c.domain.as_deref() == Some(domain.key.as_str()))
        .collect();
    let schema = domain
        .checks
        .iter()
        .find_map(|check| crate::openapi::constraints::json_schema_of(check, "VALUE"));
    let rows: Vec<&InputRow> = context.map(|c| c.rows.iter().collect()).unwrap_or_default();
    let sets =
        |row: &InputRow, column: &str| row.values.iter().any(|(c, v)| c == column && v.is_some());
    let mut found = None;
    if let Some(schema) = &schema {
        'rows: for row in &rows {
            for col in &columns {
                let errors = schema_violations(schema, row, &col.name);
                if !errors.is_empty() {
                    found = Some((Some(*row), *col, errors));
                    break 'rows;
                }
            }
        }
    }
    let (row, col, validation_errors) = match found {
        Some(found) => found,
        None => {
            let (row, col) = rows
                .iter()
                .find_map(|r| {
                    columns
                        .iter()
                        .find(|c| sets(r, &c.name))
                        .map(|c| (Some(*r), *c))
                })
                .or_else(|| columns.first().map(|c| (None, *c)))?;
            (row, col, Vec::new())
        }
    };
    let field = field_path(row, &col.field_name);
    let constraint = db.constraint().unwrap_or_default();
    Some(ConstraintViolation {
        message: format!(
            "`{field}`: the value fails check `{constraint}` of its type `{}`",
            domain.config_name
        ),
        code: DatabaseCatalog::column_error_code(col, "INVALID"),
        type_name: error_enum(t),
        constraint: Some(constraint.to_string()),
        fields: vec![field],
        validation_errors,
    })
}

/// `name` as PostgreSQL quotes an identifier (`format('%I')`): as it is when it is a plain
/// lower-case name.
fn quote(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// The parts of `row`'s value of `column` that `schema` rejects, as pg_jsonschema's own
/// validator (the `jsonschema` crate) reports them. Empty when the row does not set the
/// column, or the value passes (the database is the judge: this only explains its refusal).
fn schema_violations(
    schema: &serde_json::Value,
    row: &InputRow,
    column: &str,
) -> Vec<SchemaViolation> {
    let Some(Some(text)) = row.values.iter().find(|(c, _)| c == column).map(|(_, v)| v) else {
        return Vec::new();
    };
    let (Ok(value), Ok(validator)) = (
        serde_json::from_str::<serde_json::Value>(text),
        jsonschema::validator_for(schema),
    ) else {
        return Vec::new();
    };
    validator
        .iter_errors(&value)
        .map(|e| SchemaViolation {
            path: e.instance_path().to_string(),
            message: e.to_string(),
        })
        .collect()
}

fn error_enum(t: &TableMetadata) -> String {
    format!("{}ErrorCode", t.type_name)
}

/// `path.field` of an input row, or just `field` when the row is unknown.
fn field_path(row: Option<&InputRow>, field: &str) -> String {
    match row {
        Some(r) if !r.path.is_empty() => format!("{}.{field}", r.path),
        _ => field.to_string(),
    }
}

/// Whether a foreign key violation is on its referenced side: a write of the referenced
/// table would leave rows referring to nothing (a delete, or an update of the referenced
/// key), rather than a referencing row referring to a missing one.
fn references_side(
    con: &ConstraintMetadata,
    t: &TableMetadata,
    context: Option<&WriteContext>,
    db: &DbError,
) -> bool {
    match context {
        Some(ctx) => {
            let referencing = ctx.schema == t.schema && ctx.table == t.name;
            match ctx.verb {
                WriteVerb::Delete => true,
                WriteVerb::Insert => !referencing,
                WriteVerb::Update => {
                    !referencing
                        || !ctx
                            .rows
                            .iter()
                            .any(|r| r.values.iter().any(|(c, _)| con.columns.contains(c)))
                }
            }
        }
        // English server messages say so; others default to the referencing side
        None => db.detail().is_some_and(|d| d.contains("still referenced")),
    }
}

/// The input row a violation of `columns` comes from: the only row written, or the one
/// whose values the error detail (`Key (a, b)=(1, x) ...`) quotes.
fn pick_row<'c>(
    context: Option<&'c WriteContext>,
    t: &TableMetadata,
    columns: &[String],
    detail: Option<&str>,
) -> Option<&'c InputRow> {
    let rows = &context?.rows;
    if let [only] = rows.as_slice() {
        return Some(only);
    }
    let (names, values) = detail.and_then(detail_key)?;
    let wanted: Vec<&str> = names.split(", ").collect();
    if !wanted
        .iter()
        .all(|n| columns.iter().any(|c| c == n) || t.get_column(n).is_some())
    {
        return None;
    }
    let matching: Vec<&InputRow> = rows
        .iter()
        .filter(|r| {
            let row_values: Option<Vec<&str>> = wanted
                .iter()
                .map(|n| {
                    r.values
                        .iter()
                        .find(|(c, _)| c == n)
                        .and_then(|(_, v)| v.as_deref())
                })
                .collect();
            row_values.is_some_and(|v| v.join(", ") == values)
        })
        .collect();
    match matching.as_slice() {
        [row] => Some(row),
        _ => None,
    }
}

/// `("a, b", "1, x")` of a detail `Key (a, b)=(1, x) already exists.`
fn detail_key(detail: &str) -> Option<(&str, &str)> {
    let open = detail.find('(')?;
    let split = detail[open..].find(")=(")? + open;
    let close = detail.rfind(')')?;
    (close > split + 2).then(|| (&detail[open + 1..split], &detail[split + 3..close]))
}

#[cfg(test)]
mod tests {
    use super::detail_key;

    #[test]
    fn parses_error_detail_keys() {
        assert_eq!(
            detail_key("Key (email)=(a@b.c) already exists."),
            Some(("email", "a@b.c"))
        );
        assert_eq!(
            detail_key("Key (org, code)=(1, A) already exists."),
            Some(("org, code", "1, A"))
        );
        assert_eq!(
            detail_key("Key (team_id)=(99) is not present in table \"teams\"."),
            Some(("team_id", "99"))
        );
        assert_eq!(detail_key("Failing row contains (1, x)."), None);
    }
}
