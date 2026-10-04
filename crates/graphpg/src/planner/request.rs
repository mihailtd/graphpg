//! Protocol-neutral request model for query planning and execution,
//! along with adapters for protocols such as GraphQL.

use apollo_compiler::ast::{Value, VariableDefinition};
use apollo_compiler::executable::{Directive, Field, Operation, Selection, SelectionSet};
use apollo_compiler::{ExecutableDocument, Node};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// The kind of operation being planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OperationKind {
    Query,
    Mutation,
}

/// A protocol-neutral operation containing root fields to plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestOperation {
    pub kind: OperationKind,
    pub root_fields: Vec<RequestField>,
}

impl RequestOperation {
    pub fn query(root_fields: Vec<RequestField>) -> Self {
        Self {
            kind: OperationKind::Query,
            root_fields,
        }
    }

    pub fn mutation(root_fields: Vec<RequestField>) -> Self {
        Self {
            kind: OperationKind::Mutation,
            root_fields,
        }
    }
}

/// A protocol-neutral requested field in a selection tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestField {
    /// Name of the field or resource (e.g. `users`, `id`, `createUser`).
    pub name: String,
    /// Optional response alias if different from the name.
    pub alias: Option<String>,
    /// Arguments passed to the field, keyed by argument name with resolved JSON values.
    pub arguments: HashMap<String, serde_json::Value>,
    /// Child fields selected on this field.
    pub children: Vec<RequestField>,
    /// Optional type condition (e.g. from GraphQL fragment spreads or polymorphism).
    pub type_condition: Option<String>,
}

impl RequestField {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            alias: None,
            arguments: HashMap::new(),
            children: Vec::new(),
            type_condition: None,
        }
    }

    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = Some(alias.into());
        self
    }

    pub fn with_argument(mut self, name: impl Into<String>, val: serde_json::Value) -> Self {
        self.arguments.insert(name.into(), val);
        self
    }

    pub fn with_arguments(mut self, args: HashMap<String, serde_json::Value>) -> Self {
        self.arguments = args;
        self
    }

    pub fn with_children(mut self, children: Vec<RequestField>) -> Self {
        self.children = children;
        self
    }

    pub fn with_type_condition(mut self, cond: impl Into<String>) -> Self {
        self.type_condition = Some(cond.into());
        self
    }

    /// The key this field produces in the response: the alias if present, else the name.
    pub fn response_key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }

    /// Looks up an argument value by name.
    pub fn argument(&self, name: &str) -> Option<&serde_json::Value> {
        self.arguments.get(name)
    }

    /// Checks if a type condition matches the target types.
    pub fn matches_type(&self, target_types: &[&str]) -> bool {
        match &self.type_condition {
            None => true,
            Some(cond) => {
                target_types.is_empty()
                    || cond == "Node"
                    || target_types.iter().any(|t| t == cond || *t == "Node")
            }
        }
    }

    /// Filters direct child fields that match any of the given `target_types`.
    pub fn children_for<'a>(&'a self, target_types: &[&str]) -> Vec<&'a RequestField> {
        self.children
            .iter()
            .filter(|f| f.matches_type(target_types))
            .collect()
    }
}

/// Helper that filters a slice of `RequestField`s by target types.
pub fn flatten_selections_for<'a>(
    fields: &'a [RequestField],
    target_types: &[&str],
) -> Vec<&'a RequestField> {
    fields
        .iter()
        .filter(|f| f.matches_type(target_types))
        .collect()
}

/// Converts Apollo GraphQL AST structures into the protocol-neutral request model.
pub struct GraphQLAdapter;

impl GraphQLAdapter {
    /// Adapts an Apollo executable [`Operation`] into a protocol-neutral [`RequestOperation`].
    pub fn adapt_operation(
        operation: &Operation,
        document: Option<&ExecutableDocument>,
        variables: Option<&serde_json::Value>,
    ) -> RequestOperation {
        let kind = match operation.operation_type {
            apollo_compiler::ast::OperationType::Mutation => OperationKind::Mutation,
            _ => OperationKind::Query,
        };

        let target_type = match kind {
            OperationKind::Query => Some("Query"),
            OperationKind::Mutation => Some("Mutation"),
        };

        let mut visited = HashSet::new();
        let root_fields = Self::adapt_selection_set(
            &operation.selection_set,
            document,
            variables,
            &operation.variables,
            target_type,
            &mut visited,
        );

        RequestOperation { kind, root_fields }
    }

    /// Recursively adapts an Apollo [`SelectionSet`], resolving fragment spreads,
    /// inline fragments, `@skip` and `@include` directives, and variable references.
    pub fn adapt_selection_set(
        selection_set: &SelectionSet,
        document: Option<&ExecutableDocument>,
        variables: Option<&serde_json::Value>,
        op_variables: &[Node<VariableDefinition>],
        current_type_condition: Option<&str>,
        visited: &mut HashSet<String>,
    ) -> Vec<RequestField> {
        let mut out = Vec::new();

        for sel in &selection_set.selections {
            match sel {
                Selection::Field(f) => {
                    if !should_include(&f.directives, op_variables, variables) {
                        continue;
                    }

                    let mut arguments = HashMap::new();
                    for arg in &f.arguments {
                        let arg_name = arg.name.as_str().to_string();
                        let arg_val = resolve_ast_value(&arg.value, op_variables, variables);
                        arguments.insert(arg_name, arg_val);
                    }

                    let mut child_visited = HashSet::new();
                    let children = Self::adapt_selection_set(
                        &f.selection_set,
                        document,
                        variables,
                        op_variables,
                        None,
                        &mut child_visited,
                    );

                    out.push(RequestField {
                        name: f.name.as_str().to_string(),
                        alias: f.alias.as_ref().map(|a| a.as_str().to_string()),
                        arguments,
                        children,
                        type_condition: current_type_condition.map(str::to_string),
                    });
                }
                Selection::FragmentSpread(spread) => {
                    if !should_include(&spread.directives, op_variables, variables) {
                        continue;
                    }

                    let frag_name = spread.fragment_name.as_str();
                    if visited.insert(frag_name.to_string()) {
                        if let Some(doc) = document {
                            if let Some(frag) = doc.fragments.get(frag_name) {
                                let cond = frag.type_condition().as_str();
                                let frag_fields = Self::adapt_selection_set(
                                    &frag.selection_set,
                                    document,
                                    variables,
                                    op_variables,
                                    Some(cond),
                                    visited,
                                );
                                out.extend(frag_fields);
                            }
                        }
                        visited.remove(frag_name);
                    }
                }
                Selection::InlineFragment(inline) => {
                    if !should_include(&inline.directives, op_variables, variables) {
                        continue;
                    }

                    let inline_cond = inline
                        .type_condition
                        .as_ref()
                        .map(|tc| tc.as_str())
                        .or(current_type_condition);

                    let inline_fields = Self::adapt_selection_set(
                        &inline.selection_set,
                        document,
                        variables,
                        op_variables,
                        inline_cond,
                        visited,
                    );
                    out.extend(inline_fields);
                }
            }
        }

        out
    }

    /// Adapts a single Apollo [`Field`] into a [`RequestField`].
    pub fn adapt_field(
        field: &Field,
        document: Option<&ExecutableDocument>,
        variables: Option<&serde_json::Value>,
        op_variables: &[Node<VariableDefinition>],
    ) -> RequestField {
        let mut arguments = HashMap::new();
        for arg in &field.arguments {
            let arg_name = arg.name.as_str().to_string();
            let arg_val = resolve_ast_value(&arg.value, op_variables, variables);
            arguments.insert(arg_name, arg_val);
        }

        let mut visited = HashSet::new();
        let children = Self::adapt_selection_set(
            &field.selection_set,
            document,
            variables,
            op_variables,
            None,
            &mut visited,
        );

        RequestField {
            name: field.name.as_str().to_string(),
            alias: field.alias.as_ref().map(|a| a.as_str().to_string()),
            arguments,
            children,
            type_condition: None,
        }
    }
}

/// Evaluates `@skip(if: ...)` and `@include(if: ...)` directives on a selection.
fn should_include(
    directives: &[Node<Directive>],
    op_variables: &[Node<VariableDefinition>],
    variables: Option<&serde_json::Value>,
) -> bool {
    for d in directives {
        let name = d.name.as_str();
        if name == "skip" {
            if let Some(arg) = d.arguments.iter().find(|a| a.name.as_str() == "if") {
                let val = resolve_ast_value(&arg.value, op_variables, variables);
                if val.as_bool() == Some(true) {
                    return false;
                }
            }
        } else if name == "include" {
            if let Some(arg) = d.arguments.iter().find(|a| a.name.as_str() == "if") {
                let val = resolve_ast_value(&arg.value, op_variables, variables);
                if val.as_bool() == Some(false) {
                    return false;
                }
            }
        }
    }
    true
}

/// Resolves an Apollo AST [`Value`] into a `serde_json::Value`, interpolating variables.
pub fn resolve_ast_value(
    val: &Value,
    op_variables: &[Node<VariableDefinition>],
    variables: Option<&serde_json::Value>,
) -> serde_json::Value {
    match val {
        Value::Variable(var_name) => {
            let vname = var_name.as_str();
            if let Some(serde_json::Value::Object(map)) = variables {
                if let Some(v) = map.get(vname) {
                    return v.clone();
                }
            }
            if let Some(var_def) = op_variables.iter().find(|v| v.name.as_str() == vname) {
                if let Some(default_val) = &var_def.default_value {
                    return resolve_ast_value(default_val, op_variables, variables);
                }
            }
            serde_json::Value::Null
        }
        Value::List(l) => serde_json::Value::Array(
            l.iter()
                .map(|item| resolve_ast_value(item, op_variables, variables))
                .collect(),
        ),
        Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        resolve_ast_value(v, op_variables, variables),
                    )
                })
                .collect(),
        ),
        other => convert_ast_value_literal(other),
    }
}

/// Literal conversion of an Apollo AST [`Value`] without variable interpolation.
pub fn convert_ast_value_literal(val: &Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => {
            let i_str = i.as_str();
            match i_str.parse::<i64>() {
                Ok(num) => serde_json::json!(num),
                Err(_) => serde_json::json!(i_str),
            }
        }
        Value::Float(f) => {
            let f_str = f.as_str();
            match f_str.parse::<f64>() {
                Ok(num) => serde_json::json!(num),
                Err(_) => serde_json::json!(f_str),
            }
        }
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Enum(e) => serde_json::Value::String(e.as_str().to_string()),
        Value::List(l) => serde_json::Value::Array(
            l.iter()
                .map(|item| convert_ast_value_literal(item))
                .collect(),
        ),
        Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .map(|(k, v)| (k.as_str().to_string(), convert_ast_value_literal(v)))
                .collect(),
        ),
        Value::Variable(v) => serde_json::json!({ "$var": v.as_str() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apollo_compiler::Schema;

    #[test]
    fn test_neutral_request_model_and_graphql_adapter() {
        let schema_sdl = "
            type Query {
                user(id: Int!): User
                users: [User!]!
            }
            type User {
                id: Int!
                name: String!
                posts: [Post!]!
            }
            type Post {
                title: String!
            }
        ";
        let doc_sdl = "
            query GetUser($userId: Int!, $skipPosts: Boolean = false) {
                u: user(id: $userId) {
                    id
                    ...UserFields
                    posts @skip(if: $skipPosts) {
                        title
                    }
                }
            }
            fragment UserFields on User {
                name
            }
        ";

        let schema = Schema::parse_and_validate(schema_sdl, "schema.graphql").unwrap();
        let doc =
            ExecutableDocument::parse_and_validate(&schema, doc_sdl, "query.graphql").unwrap();
        let op = doc.operations.get(Some("GetUser")).unwrap();

        let variables = serde_json::json!({ "userId": 42 });
        let request = GraphQLAdapter::adapt_operation(op, Some(&doc), Some(&variables));

        assert_eq!(request.kind, OperationKind::Query);
        assert_eq!(request.root_fields.len(), 1);

        let root = &request.root_fields[0];
        assert_eq!(root.name, "user");
        assert_eq!(root.alias.as_deref(), Some("u"));
        assert_eq!(root.response_key(), "u");
        assert_eq!(root.argument("id"), Some(&serde_json::json!(42)));

        // Child fields: id, name (from fragment), and posts (since skipPosts is false by default)
        let child_names: Vec<&str> = root.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(child_names, vec!["id", "name", "posts"]);

        let posts_field = root.children.iter().find(|c| c.name == "posts").unwrap();
        assert_eq!(posts_field.children.len(), 1);
        assert_eq!(posts_field.children[0].name, "title");
    }

    #[test]
    fn test_skip_directive_evaluation() {
        let schema_sdl = "type Query { test: String }";
        let doc_sdl = "
            query Test($skip: Boolean!) {
                test @skip(if: $skip)
            }
        ";
        let schema = Schema::parse_and_validate(schema_sdl, "schema.graphql").unwrap();
        let doc =
            ExecutableDocument::parse_and_validate(&schema, doc_sdl, "query.graphql").unwrap();
        let op = doc.operations.iter().next().unwrap();

        let req_skipped = GraphQLAdapter::adapt_operation(
            op,
            Some(&doc),
            Some(&serde_json::json!({ "skip": true })),
        );
        assert_eq!(req_skipped.root_fields.len(), 0);

        let req_included = GraphQLAdapter::adapt_operation(
            op,
            Some(&doc),
            Some(&serde_json::json!({ "skip": false })),
        );
        assert_eq!(req_included.root_fields.len(), 1);
    }
}
