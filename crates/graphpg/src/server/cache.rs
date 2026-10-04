//! Requests seen before skip work: a document is parsed and validated once (the dominant cost
//! with schemas of thousands of types), and an operation run again with the same variables
//! reuses its compiled SQL. Values are bound parameters, but the statements an operation
//! compiles to still depend on them (enum values, which fields an input sets, list lengths),
//! so compiled operations are keyed by their variables too. Both caches are bounded (cleared
//! when full) and emptied when the schema is reloaded.
use crate::compiler::CompiledOperation;
use apollo_compiler::validation::Valid;
use apollo_compiler::ExecutableDocument;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Documents kept at most.
pub const MAX_CACHED_DOCUMENTS: usize = 1024;
/// Compiled operations kept at most.
pub const MAX_CACHED_OPERATIONS: usize = 4096;

type OperationKey = (String, Option<String>, String);

#[derive(Default)]
pub struct RequestCache {
    documents: Mutex<HashMap<String, Arc<Valid<ExecutableDocument>>>>,
    operations: Mutex<HashMap<OperationKey, Arc<CompiledOperation>>>,
}

impl RequestCache {
    /// The validated document of `query`, if seen before.
    pub fn document(&self, query: &str) -> Option<Arc<Valid<ExecutableDocument>>> {
        self.documents.lock().unwrap().get(query).cloned()
    }

    pub fn store_document(&self, query: &str, doc: Arc<Valid<ExecutableDocument>>) {
        let mut documents = self.documents.lock().unwrap();
        if documents.len() >= MAX_CACHED_DOCUMENTS {
            documents.clear();
        }
        documents.insert(query.to_string(), doc);
    }

    /// The compiled operation `operation_name` of `query` with `variables`, if compiled before.
    pub fn operation(&self, query: &str, operation_name: Option<&str>, variables: Option<&serde_json::Value>) -> Option<Arc<CompiledOperation>> {
        self.operations.lock().unwrap().get(&Self::key(query, operation_name, variables)).cloned()
    }

    pub fn store_operation(
        &self,
        query: &str,
        operation_name: Option<&str>,
        variables: Option<&serde_json::Value>,
        compiled: Arc<CompiledOperation>,
    ) {
        let mut operations = self.operations.lock().unwrap();
        if operations.len() >= MAX_CACHED_OPERATIONS {
            operations.clear();
        }
        operations.insert(Self::key(query, operation_name, variables), compiled);
    }

    /// Empties both caches (the schema changed).
    pub fn clear(&self) {
        self.documents.lock().unwrap().clear();
        self.operations.lock().unwrap().clear();
    }

    fn key(query: &str, operation_name: Option<&str>, variables: Option<&serde_json::Value>) -> OperationKey {
        let variables = match variables {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(v) => v.to_string(),
        };
        (query.to_string(), operation_name.map(str::to_string), variables)
    }
}
