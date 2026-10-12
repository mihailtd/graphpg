//! Persisted queries (`graphql.persisted_queries`): operations sent by reference instead of
//! by text, and the allow-list of operations a production deployment runs.
//!
//! - A request names an operation by `extensions.persistedQuery.sha256Hash` (Apollo's
//!   protocol: the SHA-256 of the text, in hex), or by `documentId` (an id of the manifest).
//! - The manifest is a JSON file read at startup: an Apollo persisted query manifest
//!   (`{"operations": [{"id", "body"}]}`) or a map of ids to texts, as GraphQL Code
//!   Generator and Relay write. Its operations are validated against the schema at startup.
//! - Automatic persisted queries (APQ): an unknown hash is answered `PERSISTED_QUERY_NOT_FOUND`,
//!   the client sends the hash with the text, which registers it in this instance's memory
//!   (bounded, cleared when full, kept across schema reloads: a stale text fails validation
//!   like any other).
//! - With `allow_list` only the manifest's operations run, by id, by hash, or as text whose
//!   hash is listed; anything else is `OPERATION_NOT_ALLOWED` before it is parsed.
//! - Off, a request naming a persisted query is answered `PERSISTED_QUERY_NOT_SUPPORTED`:
//!   Apollo clients then send the text alone.
//!
//! The resolved text goes down the same path as any other: the document and compiled
//! operation caches are keyed by it, so a persisted query is planned once like the rest.
use crate::config::models::PersistedQueriesConfig;
use apollo_compiler::validation::Valid;
use apollo_compiler::{ExecutableDocument, Schema};
use axum::http::StatusCode;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Operations registered by clients kept at most.
pub const MAX_REGISTERED: usize = 4096;

/// The SHA-256 of `text`, in lowercase hex: the hash APQ clients send.
pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Why a request's operation cannot be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl Rejection {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn not_found() -> Self {
        // Apollo clients look for this message, or the code, to send the text
        Self::new(
            StatusCode::OK,
            "PERSISTED_QUERY_NOT_FOUND",
            "PersistedQueryNotFound",
        )
    }
}

/// The known operations: the manifest's, and those clients registered.
pub struct PersistedQueries {
    /// Clients may register operations (APQ).
    automatic: bool,
    /// Only the manifest's operations run.
    allow_list: bool,
    /// The manifest's operations by id.
    by_id: HashMap<String, Arc<str>>,
    /// The manifest's operations by the SHA-256 of their text.
    by_hash: HashMap<String, Arc<str>>,
    /// Operations clients registered, by hash.
    registered: Mutex<HashMap<String, Arc<str>>>,
}

impl PersistedQueries {
    /// The persisted queries of `config`, the manifest read from its file; `None` when off.
    pub fn new(config: &PersistedQueriesConfig) -> Result<Option<Arc<Self>>, String> {
        if !config.enabled {
            return Ok(None);
        }
        let manifest = match &config.manifest {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|err| format!("persisted query manifest {path}: {err}"))?,
            ),
            None => None,
        };
        Self::with_manifest(config, manifest.as_deref())
            .map(|pq| Some(Arc::new(pq)))
            .map_err(|err| match &config.manifest {
                Some(path) => format!("persisted query manifest {path}: {err}"),
                None => err,
            })
    }

    /// The persisted queries of `config` with `manifest` (JSON text) as the manifest.
    pub fn with_manifest(
        config: &PersistedQueriesConfig,
        manifest: Option<&str>,
    ) -> Result<Self, String> {
        let mut by_id = HashMap::new();
        let mut by_hash = HashMap::new();
        if let Some(manifest) = manifest {
            for (id, body) in parse_manifest(manifest)? {
                let body: Arc<str> = body.into();
                by_hash.insert(sha256_hex(&body), body.clone());
                by_id.insert(id, body);
            }
        }
        Ok(Self {
            automatic: config.automatic && !config.allow_list,
            allow_list: config.allow_list,
            by_id,
            by_hash,
            registered: Mutex::default(),
        })
    }

    /// Operations in the manifest.
    pub fn manifest_len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether only the manifest's operations run.
    pub fn allow_list(&self) -> bool {
        self.allow_list
    }

    /// The manifest's operations that do not validate against `schema`, one line each.
    pub fn check(&self, schema: &Valid<Schema>) -> Vec<String> {
        let mut problems: Vec<String> = self
            .by_id
            .iter()
            .filter_map(|(id, body)| {
                ExecutableDocument::parse_and_validate(schema, &**body, "manifest.graphql")
                    .err()
                    .map(|err| format!("operation `{id}`: {}", err.errors))
            })
            .collect();
        problems.sort();
        problems
    }

    /// Whether `text` is one of the manifest's operations (so the allow-list lets it run).
    pub fn allows_text(&self, text: &str) -> bool {
        !self.allow_list || self.by_hash.contains_key(&sha256_hex(text))
    }

    fn lookup(&self, key: &str) -> Option<Arc<str>> {
        self.by_id
            .get(key)
            .or_else(|| self.by_hash.get(key))
            .cloned()
            .or_else(|| self.registered.lock().unwrap().get(key).cloned())
    }

    fn register(&self, hash: &str, text: &str) {
        let mut registered = self.registered.lock().unwrap();
        if registered.contains_key(hash) || self.by_hash.contains_key(hash) {
            return;
        }
        if registered.len() >= MAX_REGISTERED {
            registered.clear();
        }
        registered.insert(hash.to_string(), text.into());
    }
}

/// `(id, text)` of each operation of a manifest.
fn parse_manifest(text: &str) -> Result<Vec<(String, String)>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("not JSON: {err}"))?;
    let serde_json::Value::Object(map) = value else {
        return Err("expected an object".to_string());
    };
    if let Some(operations) = map.get("operations") {
        // Apollo's persisted query manifest
        let operations = operations
            .as_array()
            .ok_or("`operations` must be an array")?;
        return operations
            .iter()
            .enumerate()
            .map(|(i, op)| {
                let field = |name: &str| {
                    op.get(name)
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| format!("operations[{i}]: `{name}` must be a string"))
                };
                Ok((field("id")?, field("body")?))
            })
            .collect();
    }
    map.into_iter()
        .map(|(id, body)| match body {
            serde_json::Value::String(body) => Ok((id, body)),
            _ => Err(format!("`{id}`: the operation's text must be a string")),
        })
        .collect()
}

/// The operation text of a request: its `query`, or the persisted query it names.
pub fn resolve(
    persisted: Option<&PersistedQueries>,
    query: Option<&str>,
    extensions: Option<&serde_json::Value>,
    document_id: Option<&str>,
) -> Result<Arc<str>, Rejection> {
    let bad = |message: &str| Rejection::new(StatusCode::BAD_REQUEST, "BAD_REQUEST", message);
    let hash = match extensions.and_then(|e| e.get("persistedQuery")) {
        None => None,
        Some(pq) => {
            if pq.get("version").and_then(|v| v.as_i64()) != Some(1) {
                return Err(bad("unsupported persistedQuery version (1 is supported)"));
            }
            let hash = pq
                .get("sha256Hash")
                .and_then(|h| h.as_str())
                .ok_or_else(|| bad("persistedQuery needs a sha256Hash"))?;
            Some(hash.to_ascii_lowercase())
        }
    };
    if query.is_some() && document_id.is_some() {
        return Err(bad("send either `query` or `documentId`, not both"));
    }
    let Some(persisted) = persisted else {
        if hash.is_some() || document_id.is_some() {
            return Err(Rejection::new(
                StatusCode::OK,
                "PERSISTED_QUERY_NOT_SUPPORTED",
                "PersistedQueryNotSupported",
            ));
        }
        return query
            .map(Into::into)
            .ok_or_else(|| bad("the request has no `query`"));
    };
    let not_allowed = || {
        Rejection::new(
            StatusCode::FORBIDDEN,
            "OPERATION_NOT_ALLOWED",
            "the operation is not in the allow-list of persisted queries",
        )
    };
    match (query, hash, document_id) {
        (Some(text), Some(hash), _) => {
            if sha256_hex(text) != hash {
                return Err(Rejection::new(
                    StatusCode::BAD_REQUEST,
                    "PERSISTED_QUERY_HASH_MISMATCH",
                    "persistedQuery.sha256Hash is not the SHA-256 of the query",
                ));
            }
            if !persisted.allows_text(text) {
                return Err(not_allowed());
            }
            if persisted.automatic {
                persisted.register(&hash, text);
            }
            Ok(text.into())
        }
        (Some(text), None, _) => {
            if persisted.allows_text(text) {
                Ok(text.into())
            } else {
                Err(not_allowed())
            }
        }
        (None, Some(key), _) => persisted.lookup(&key).ok_or_else(Rejection::not_found),
        (None, None, Some(id)) => persisted.lookup(id).ok_or_else(Rejection::not_found),
        (None, None, None) => Err(bad(
            "the request has no `query`, `documentId` or persisted query hash",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const QUERY: &str = "{ users { id } }";

    fn config(automatic: bool, allow_list: bool) -> PersistedQueriesConfig {
        PersistedQueriesConfig {
            enabled: true,
            automatic,
            manifest: None,
            allow_list,
        }
    }

    fn apq(hash: &str) -> serde_json::Value {
        json!({ "persistedQuery": { "version": 1, "sha256Hash": hash } })
    }

    #[test]
    fn hash_is_the_apollo_one() {
        // `echo -n '{ users { id } }' | sha256sum`
        assert_eq!(
            sha256_hex(QUERY),
            "b2abc043a4d432b6ba17d37369fcf972de147c06d0b82390ffde0272fcefb33e"
        );
    }

    #[test]
    fn apq_registers_then_serves_by_hash() {
        let pq = PersistedQueries::with_manifest(&config(true, false), None).unwrap();
        let hash = sha256_hex(QUERY);
        let miss = resolve(Some(&pq), None, Some(&apq(&hash)), None).unwrap_err();
        assert_eq!(miss.code, "PERSISTED_QUERY_NOT_FOUND");
        assert_eq!(miss.message, "PersistedQueryNotFound");
        assert_eq!(
            &*resolve(Some(&pq), Some(QUERY), Some(&apq(&hash)), None).unwrap(),
            QUERY
        );
        assert_eq!(
            &*resolve(Some(&pq), None, Some(&apq(&hash.to_uppercase())), None).unwrap(),
            QUERY
        );
    }

    #[test]
    fn a_wrong_hash_registers_nothing() {
        let pq = PersistedQueries::with_manifest(&config(true, false), None).unwrap();
        let wrong = sha256_hex("{ other }");
        let err = resolve(Some(&pq), Some(QUERY), Some(&apq(&wrong)), None).unwrap_err();
        assert_eq!(err.code, "PERSISTED_QUERY_HASH_MISMATCH");
        assert!(resolve(Some(&pq), None, Some(&apq(&wrong)), None).is_err());
    }

    #[test]
    fn without_automatic_nothing_registers() {
        let pq = PersistedQueries::with_manifest(&config(false, false), None).unwrap();
        let hash = sha256_hex(QUERY);
        assert!(resolve(Some(&pq), Some(QUERY), Some(&apq(&hash)), None).is_ok());
        let err = resolve(Some(&pq), None, Some(&apq(&hash)), None).unwrap_err();
        assert_eq!(err.code, "PERSISTED_QUERY_NOT_FOUND");
    }

    #[test]
    fn off_refuses_references_and_runs_text() {
        let err = resolve(None, None, Some(&apq(&sha256_hex(QUERY))), None).unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            (
                "PERSISTED_QUERY_NOT_SUPPORTED",
                "PersistedQueryNotSupported"
            )
        );
        assert_eq!(
            resolve(None, None, None, Some("abc")).unwrap_err().code,
            "PERSISTED_QUERY_NOT_SUPPORTED"
        );
        assert_eq!(&*resolve(None, Some(QUERY), None, None).unwrap(), QUERY);
        assert_eq!(
            resolve(None, None, None, None).unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn manifests_of_both_shapes() {
        let apollo = json!({
            "format": "apollo-persisted-query-manifest",
            "version": 1,
            "operations": [{ "id": "op1", "name": "Users", "type": "query", "body": QUERY }]
        })
        .to_string();
        let flat = json!({ "op1": QUERY }).to_string();
        for manifest in [apollo, flat] {
            let pq =
                PersistedQueries::with_manifest(&config(false, true), Some(&manifest)).unwrap();
            assert_eq!(pq.manifest_len(), 1);
            assert_eq!(
                &*resolve(Some(&pq), None, None, Some("op1")).unwrap(),
                QUERY
            );
            assert_eq!(
                &*resolve(Some(&pq), None, Some(&apq(&sha256_hex(QUERY))), None).unwrap(),
                QUERY
            );
        }
        assert!(PersistedQueries::with_manifest(&config(false, false), Some("[]")).is_err());
        assert!(
            PersistedQueries::with_manifest(&config(false, false), Some(r#"{"a": 1}"#)).is_err()
        );
    }

    #[test]
    fn the_allow_list_runs_only_listed_operations() {
        let manifest = json!({ "op1": QUERY }).to_string();
        // `automatic` is ignored: clients cannot add to the list
        let pq = PersistedQueries::with_manifest(&config(true, true), Some(&manifest)).unwrap();
        assert_eq!(
            &*resolve(Some(&pq), Some(QUERY), None, None).unwrap(),
            QUERY
        );
        let other = "{ forums { id } }";
        let err = resolve(Some(&pq), Some(other), None, None).unwrap_err();
        assert_eq!(
            (err.status, err.code),
            (StatusCode::FORBIDDEN, "OPERATION_NOT_ALLOWED")
        );
        let hash = sha256_hex(other);
        assert_eq!(
            resolve(Some(&pq), Some(other), Some(&apq(&hash)), None)
                .unwrap_err()
                .code,
            "OPERATION_NOT_ALLOWED"
        );
        assert_eq!(
            resolve(Some(&pq), None, Some(&apq(&hash)), None)
                .unwrap_err()
                .code,
            "PERSISTED_QUERY_NOT_FOUND"
        );
    }

    #[test]
    fn malformed_references() {
        let pq = PersistedQueries::with_manifest(&config(true, false), None).unwrap();
        let v2 = json!({ "persistedQuery": { "version": 2, "sha256Hash": "x" } });
        assert_eq!(
            resolve(Some(&pq), None, Some(&v2), None)
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        assert!(resolve(Some(&pq), Some(QUERY), None, Some("op1")).is_err());
    }
}
