//! The GraphQL input types of writes as JSON Schema components.
//!
//! A create or update body has the shape of the GraphQL input, nested writes included: a
//! relation member takes `{ create: [...], connect: [<key>], connectOrCreate: [...] }`, and on
//! update also `disconnect`, `set`, `update`, `delete` and `sync`. Those shapes are described
//! by the input types of the served schema, so the document reads them from there: each input
//! type becomes a component named `Input.<Type>`, referencing the others (relations nest and
//! may refer back to each other).

use apollo_compiler::ast::Type;
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::Schema;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

/// Prefix of the components made from GraphQL input types.
const PREFIX: &str = "Input.";

/// The schemas of the column fields of input types, by input type and field: the catalog's,
/// with what the column's `CHECK` constraints state (GraphQL types cannot say it).
pub(super) type ColumnSchemas = HashMap<String, HashMap<String, Value>>;

/// Collects the components the input types a document refers to need.
pub(super) struct InputTypes<'a> {
    schema: &'a Schema,
    columns: ColumnSchemas,
    components: BTreeMap<String, Value>,
}

impl<'a> InputTypes<'a> {
    pub(super) fn new(schema: &'a Schema, columns: ColumnSchemas) -> Self {
        Self {
            schema,
            columns,
            components: BTreeMap::new(),
        }
    }

    /// A `$ref` to the component of the input type `name`, making it (and what it refers to).
    pub(super) fn reference(&mut self, name: &str) -> Value {
        let key = format!("{PREFIX}{name}");
        if !self.components.contains_key(&key) {
            // A placeholder first: relations refer back to the type being made
            self.components.insert(key.clone(), Value::Null);
            let component = self.input_object(name);
            self.components.insert(key.clone(), component);
        }
        json!({ "$ref": format!("#/components/schemas/{key}") })
    }

    /// Name of the type of argument `arg` of mutation field `field`.
    pub(super) fn argument_type(&self, field: &str, arg: &str) -> Option<String> {
        let root = self.schema.schema_definition.mutation.as_ref()?;
        let ExtendedType::Object(object) = self.schema.types.get(root.as_str())? else {
            return None;
        };
        let definition = object.fields.get(field)?;
        let argument = definition.argument_by_name(arg)?;
        Some(argument.ty.inner_named_type().to_string())
    }

    /// The components made so far.
    pub(super) fn into_components(self) -> BTreeMap<String, Value> {
        self.components
    }

    fn input_object(&mut self, name: &str) -> Value {
        let Some(ExtendedType::InputObject(input)) = self.schema.types.get(name) else {
            return json!({});
        };
        let mut properties = Map::new();
        let mut required = Vec::new();
        for (field, definition) in &input.fields {
            let column = self
                .columns
                .get(name)
                .and_then(|fields| fields.get(field.as_str()))
                .cloned();
            let mut schema = column.unwrap_or_else(|| self.type_schema(&definition.ty));
            if let (None, Some(description)) = (schema.get("description"), &definition.description)
            {
                // a `$ref` has no siblings in OpenAPI 3.0, but in 3.1 it may
                schema["description"] = json!(description.as_str());
            }
            properties.insert(field.to_string(), schema);
            if definition.ty.is_non_null() && definition.default_value.is_none() {
                required.push(field.to_string());
            }
        }
        let mut object = json!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false
        });
        if let Some(description) = &input.description {
            object["description"] = json!(description.as_str());
        }
        if !required.is_empty() {
            object["required"] = json!(required);
        }
        object
    }

    fn type_schema(&mut self, ty: &Type) -> Value {
        let schema = match ty {
            Type::List(inner) | Type::NonNullList(inner) => {
                json!({ "type": "array", "items": self.type_schema(inner) })
            }
            Type::Named(name) | Type::NonNullNamed(name) => self.named(name.as_str()),
        };
        if ty.is_non_null() {
            schema
        } else {
            super::generator::nullable(schema)
        }
    }

    fn named(&mut self, name: &str) -> Value {
        match name {
            "Int" => return json!({ "type": "integer" }),
            "Float" => return json!({ "type": "number" }),
            "String" => return json!({ "type": "string" }),
            "Boolean" => return json!({ "type": "boolean" }),
            // A key: the Relay ID, or the value of the key column
            "ID" => return json!({ "type": ["string", "integer"] }),
            _ => {}
        }
        match self.schema.types.get(name) {
            Some(ExtendedType::InputObject(_)) => self.reference(name),
            Some(ExtendedType::Enum(e)) => {
                let values: Vec<&str> = e.values.keys().map(|k| k.as_str()).collect();
                json!({ "type": "string", "enum": values })
            }
            Some(ExtendedType::Scalar(_)) => match name {
                "UUID" => json!({ "type": "string", "format": "uuid" }),
                "Datetime" => json!({ "type": "string", "format": "date-time" }),
                "Date" => json!({ "type": "string", "format": "date" }),
                "JSON" | "JSONObject" => json!({}),
                _ => json!({ "type": "string" }),
            },
            _ => json!({}),
        }
    }
}
