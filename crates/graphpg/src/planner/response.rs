//! Response shaping for protocol-neutral and REST callers.
//!
//! Provides utilities to construct neutral request selections for collection queries
//! and unwrap execution results (such as Relay connection envelopes) into flat,
//! REST-friendly responses.

use crate::planner::request::RequestField;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// Error encountered during response shaping.
#[derive(Debug, Error, PartialEq)]
pub enum ResponseShapingError {
    #[error("Missing expected root field '{0}' in execution result")]
    MissingRootField(String),
    #[error("Expected object at path '{0}', found: {1}")]
    ExpectedObject(String, String),
    #[error("Expected array at path '{0}', found: {1}")]
    ExpectedArray(String, String),
}

/// Pagination metadata for REST list responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestPageInfo {
    #[serde(rename = "hasNextPage", default)]
    pub has_next_page: bool,
    #[serde(rename = "hasPreviousPage", default)]
    pub has_previous_page: bool,
    #[serde(rename = "startCursor", skip_serializing_if = "Option::is_none")]
    pub start_cursor: Option<String>,
    #[serde(rename = "endCursor", skip_serializing_if = "Option::is_none")]
    pub end_cursor: Option<String>,
}

/// Standard envelope for REST list responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestListResponse {
    pub rows: Vec<serde_json::Value>,
    #[serde(rename = "pageInfo", skip_serializing_if = "Option::is_none")]
    pub page_info: Option<RestPageInfo>,
    #[serde(rename = "totalCount", skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
}

/// Utility for shaping requests and transforming responses for REST endpoints.
pub struct ResponseShaper;

impl ResponseShaper {
    /// Constructs a `RequestField` for a collection query requesting `nodes`, `pageInfo`, and `totalCount`.
    ///
    /// # Arguments
    /// * `root_field_name` - The root field name in the schema (e.g. `users` or `usersConnection`).
    /// * `arguments` - Query arguments (e.g. `first`, `after`, `where`, `orderBy`).
    /// * `row_fields` - The scalar or relation fields requested for each item in the collection.
    /// * `include_page_info` - Whether to select `pageInfo { hasNextPage, hasPreviousPage, startCursor, endCursor }`.
    /// * `include_total_count` - Whether to select `totalCount`.
    pub fn build_list_request(
        root_field_name: impl Into<String>,
        arguments: HashMap<String, serde_json::Value>,
        row_fields: Vec<RequestField>,
        include_page_info: bool,
        include_total_count: bool,
    ) -> RequestField {
        let mut children = vec![RequestField::new("nodes").with_children(row_fields)];

        if include_page_info {
            children.push(RequestField::new("pageInfo").with_children(vec![
                RequestField::new("hasNextPage"),
                RequestField::new("hasPreviousPage"),
                RequestField::new("startCursor"),
                RequestField::new("endCursor"),
            ]));
        }

        if include_total_count {
            children.push(RequestField::new("totalCount"));
        }

        RequestField::new(root_field_name)
            .with_arguments(arguments)
            .with_children(children)
    }

    /// Unwraps an execution result (with or without a `"data"` top-level key) into a [`RestListResponse`].
    ///
    /// Handles:
    /// - Relay connections with `nodes` and optional `pageInfo` / `totalCount`.
    /// - Relay connections with `edges { node }`.
    /// - Direct arrays returned under `root_field_name`.
    pub fn unwrap_list(
        result: &serde_json::Value,
        root_field_name: &str,
    ) -> Result<RestListResponse, ResponseShapingError> {
        let data = match result.get("data") {
            Some(d) => d,
            None => result,
        };

        let root = data
            .get(root_field_name)
            .ok_or_else(|| ResponseShapingError::MissingRootField(root_field_name.to_string()))?;

        // Case 1: Direct JSON array
        if let Some(arr) = root.as_array() {
            return Ok(RestListResponse {
                rows: arr.clone(),
                page_info: None,
                total_count: Some(arr.len() as i64),
            });
        }

        // Case 2: Relay connection object
        if let Some(obj) = root.as_object() {
            let rows = if let Some(nodes) = obj.get("nodes").and_then(|n| n.as_array()) {
                nodes.clone()
            } else if let Some(edges) = obj.get("edges").and_then(|e| e.as_array()) {
                edges
                    .iter()
                    .filter_map(|edge| edge.get("node").cloned())
                    .collect()
            } else {
                Vec::new()
            };

            let page_info: Option<RestPageInfo> = obj
                .get("pageInfo")
                .and_then(|pi| serde_json::from_value(pi.clone()).ok());

            let total_count = obj
                .get("totalCount")
                .and_then(|tc| tc.as_i64())
                .or(Some(rows.len() as i64));

            return Ok(RestListResponse {
                rows,
                page_info,
                total_count,
            });
        }

        Err(ResponseShapingError::ExpectedObject(
            root_field_name.to_string(),
            root.to_string(),
        ))
    }

    /// Unwraps a single entity query response, removing the `"data"` envelope and root field key.
    pub fn unwrap_single(
        result: &serde_json::Value,
        root_field_name: &str,
    ) -> Result<serde_json::Value, ResponseShapingError> {
        let data = match result.get("data") {
            Some(d) => d,
            None => result,
        };

        data.get(root_field_name)
            .cloned()
            .ok_or_else(|| ResponseShapingError::MissingRootField(root_field_name.to_string()))
    }

    /// Unwraps a mutation response, removing the `"data"` envelope, root field key,
    /// and optional nested entity field key (e.g. `createUser.user` -> `user`).
    pub fn unwrap_mutation(
        result: &serde_json::Value,
        root_field_name: &str,
        entity_field_name: Option<&str>,
    ) -> Result<serde_json::Value, ResponseShapingError> {
        let root = Self::unwrap_single(result, root_field_name)?;

        if let Some(nested_key) = entity_field_name {
            if let Some(obj) = root.as_object() {
                if let Some(entity) = obj.get(nested_key) {
                    return Ok(entity.clone());
                }
            }
        }

        Ok(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_build_list_request() {
        let req = ResponseShaper::build_list_request(
            "users",
            HashMap::from([("first".to_string(), json!(10))]),
            vec![RequestField::new("id"), RequestField::new("name")],
            true,
            true,
        );

        assert_eq!(req.name, "users");
        assert_eq!(req.arguments.get("first"), Some(&json!(10)));
        assert_eq!(req.children.len(), 3);

        let nodes_field = req.children.iter().find(|c| c.name == "nodes").unwrap();
        assert_eq!(nodes_field.children.len(), 2);
        assert_eq!(nodes_field.children[0].name, "id");
        assert_eq!(nodes_field.children[1].name, "name");

        assert!(req.children.iter().any(|c| c.name == "pageInfo"));
        assert!(req.children.iter().any(|c| c.name == "totalCount"));
    }

    #[test]
    fn test_unwrap_list_connection() {
        let result = json!({
            "data": {
                "users": {
                    "nodes": [
                        { "id": 1, "name": "Alice" },
                        { "id": 2, "name": "Bob" }
                    ],
                    "pageInfo": {
                        "hasNextPage": true,
                        "hasPreviousPage": false,
                        "startCursor": "cursor1",
                        "endCursor": "cursor2"
                    },
                    "totalCount": 42
                }
            }
        });

        let resp = ResponseShaper::unwrap_list(&result, "users").unwrap();
        assert_eq!(resp.rows.len(), 2);
        assert_eq!(resp.rows[0]["name"], "Alice");
        assert_eq!(resp.total_count, Some(42));

        let pi = resp.page_info.unwrap();
        assert!(pi.has_next_page);
        assert!(!pi.has_previous_page);
        assert_eq!(pi.start_cursor.as_deref(), Some("cursor1"));
        assert_eq!(pi.end_cursor.as_deref(), Some("cursor2"));
    }

    #[test]
    fn test_unwrap_list_direct_array() {
        let result = json!({
            "data": {
                "users": [
                    { "id": 1, "name": "Alice" }
                ]
            }
        });

        let resp = ResponseShaper::unwrap_list(&result, "users").unwrap();
        assert_eq!(resp.rows.len(), 1);
        assert_eq!(resp.rows[0]["name"], "Alice");
        assert_eq!(resp.total_count, Some(1));
        assert!(resp.page_info.is_none());
    }

    #[test]
    fn test_unwrap_single() {
        let result = json!({
            "data": {
                "user": { "id": 1, "name": "Alice" }
            }
        });

        let resp = ResponseShaper::unwrap_single(&result, "user").unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["name"], "Alice");
    }

    #[test]
    fn test_unwrap_mutation_with_nested_entity() {
        let result = json!({
            "data": {
                "createUser": {
                    "user": { "id": 10, "name": "Charlie" }
                }
            }
        });

        let resp = ResponseShaper::unwrap_mutation(&result, "createUser", Some("user")).unwrap();
        assert_eq!(resp["id"], 10);
        assert_eq!(resp["name"], "Charlie");
    }
}
