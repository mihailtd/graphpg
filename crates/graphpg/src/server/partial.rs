//! Partial results: a query whose statement fails because of one root field answers the
//! other root fields' data, with `null` and an error (with its `path`) for the failed ones.
//!
//! The one-statement design is kept where it matters: a query still runs as one statement,
//! and only when that statement fails with an error about the data (see
//! [`ExecutionError::is_data_error`]) are its root fields run again one by one, in one
//! snapshot (see [`ExecutionEngine::run_apart`]), to tell which fail. A failure of the
//! request as a whole (a timeout, the connection) still fails the request; mutations keep
//! their all-or-nothing transaction.
//!
//! What a failed root field does to `data` follows the GraphQL spec: a nullable field is
//! `null`; a non-null one (lists and connections are `[T!]!`) makes `data` itself `null`.
//! The `onError` request parameter (graphql-spec RFC 1163) changes that: `NULL` leaves the
//! failed field `null` whatever its type, `HALT` fails the whole request as before.
//!
//! [`ExecutionError::is_data_error`]: crate::execution::ExecutionError::is_data_error
//! [`ExecutionEngine::run_apart`]: crate::execution::ExecutionEngine::run_apart

/// What a field error does to the response (`onError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorBehavior {
    /// The spec's default: `null` in the field, propagated up through non-null types.
    #[default]
    Propagate,
    /// `null` in the field, whatever its type.
    Null,
    /// No data: the whole request fails.
    Halt,
}

impl ErrorBehavior {
    /// The behavior a request asks for; `None` means the default. The RFC's earlier names
    /// (`NO_PROPAGATE`, `ABORT`) are accepted too.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None => Ok(Self::Propagate),
            Some(v) => match v.to_ascii_uppercase().as_str() {
                "PROPAGATE" => Ok(Self::Propagate),
                "NULL" | "NO_PROPAGATE" => Ok(Self::Null),
                "HALT" | "ABORT" => Ok(Self::Halt),
                _ => Err(format!(
                    "unknown `onError` value `{v}` (PROPAGATE, NULL or HALT)"
                )),
            },
        }
    }
}

/// Whether a failed root field leaves `data` itself `null`: a non-null field propagates.
pub fn nulls_data(behavior: ErrorBehavior, non_null: bool) -> bool {
    match behavior {
        ErrorBehavior::Propagate => non_null,
        ErrorBehavior::Null => false,
        ErrorBehavior::Halt => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_rfc_values() {
        assert_eq!(ErrorBehavior::parse(None), Ok(ErrorBehavior::Propagate));
        assert_eq!(ErrorBehavior::parse(Some("null")), Ok(ErrorBehavior::Null));
        assert_eq!(
            ErrorBehavior::parse(Some("NO_PROPAGATE")),
            Ok(ErrorBehavior::Null)
        );
        assert_eq!(ErrorBehavior::parse(Some("HALT")), Ok(ErrorBehavior::Halt));
        assert_eq!(ErrorBehavior::parse(Some("ABORT")), Ok(ErrorBehavior::Halt));
        assert!(ErrorBehavior::parse(Some("IGNORE")).is_err());
    }

    #[test]
    fn non_null_fields_propagate_unless_asked_not_to() {
        assert!(!nulls_data(ErrorBehavior::Propagate, false));
        assert!(nulls_data(ErrorBehavior::Propagate, true));
        assert!(!nulls_data(ErrorBehavior::Null, true));
        assert!(nulls_data(ErrorBehavior::Halt, false));
    }
}
