//! JSON Schema (draft-07) for `engine.config.yaml`, generated from an inspected catalog so
//! editors can complete and validate the relation, column, constraint and function names
//! of the actual database.
use crate::catalog::{DatabaseCatalog, RelationKind, TableMetadata};
use crate::config::models::Operation;
use serde_json::{json, Map, Value};

pub struct ConfigSchemaGenerator;

/// Pattern of a GraphQL name.
const NAME_PATTERN: &str = "^[_A-Za-z][_0-9A-Za-z]*$";

fn graphql_name(description: &str) -> Value {
    json!({ "type": "string", "pattern": NAME_PATTERN, "description": description })
}

/// A schema from `$defs/<def>` with its own description (shown by editors).
fn described_ref(def: &str, description: String) -> Value {
    json!({ "description": description, "allOf": [{ "$ref": format!("#/$defs/{def}") }] })
}

fn describe(comment: Option<&str>, fallback: String) -> String {
    match comment {
        Some(d) if !d.trim().is_empty() => format!("{}\n\n{fallback}", d.trim()),
        _ => fallback,
    }
}

impl ConfigSchemaGenerator {
    /// Config of a function exposed as a root field (a mutation when `VOLATILE`); a function
    /// returning columns of its own can also be mounted on a table.
    fn function_schema(func: &crate::catalog::FunctionMetadata) -> Value {
        let (kind, field) = if func.volatile {
            ("Mutation", "Root Mutation field name")
        } else {
            ("Function", "Root Query field name")
        };
        let mut properties = json!({
            "field_name": graphql_name(field),
            "description": { "type": "string", "description": "Field description" },
            "deprecated": { "type": "string", "description": "Deprecate the function's fields, with this reason" },
            "omit": { "type": "boolean", "description": "Remove the root field" }
        });
        if func.returns_record() && func.returns_set && !func.volatile {
            let args: Map<String, Value> = func
                .arguments
                .iter()
                .map(|a| (a.name.clone(), json!({ "type": "string", "description": format!("Parent column passed as `{}`", a.name) })))
                .collect();
            properties["mount"] = json!({
                "type": "object",
                "description": "Expose the function as a field of a table or view type",
                "properties": {
                    "on": described_ref("relation", "Relation whose type gets the field".to_string()),
                    "field_name": graphql_name("Field name on that type"),
                    "arguments": {
                        "type": "object",
                        "description": "Function arguments filled from the parent row (argument: column); default: first argument ← primary key",
                        "properties": args,
                        "additionalProperties": false
                    },
                    "omit": { "type": "boolean", "description": "Do not mount (also disables a detected mount)" }
                },
                "additionalProperties": false
            });
        }
        json!({
            "type": "object",
            "description": describe(func.description.as_deref(), format!("{kind} `{}`", func.name)),
            "properties": properties,
            "additionalProperties": false
        })
    }

    /// Generates the schema from a catalog returned by `CatalogInspector::inspect_base`
    /// (database names, before the config renames or omits anything).
    pub fn generate(catalog: &DatabaseCatalog) -> Value {
        // Config keys name objects by bare name, qualified as `schema.name` when several
        // inspected schemas have one of that name
        let shared = |name: &str, names: &mut dyn Iterator<Item = &str>| {
            names.filter(|n| *n == name).count() > 1
        };
        let table_key = |schema: &str, name: &str| {
            if shared(name, &mut catalog.tables.iter().map(|t| t.name.as_str())) {
                format!("{schema}.{name}")
            } else {
                name.to_string()
            }
        };
        let function_key = |schema: &str, name: &str| {
            let mut names = catalog.functions.iter().map(|f| f.name.as_str());
            if shared(name, &mut names) {
                format!("{schema}.{name}")
            } else {
                name.to_string()
            }
        };
        let relation_names: Vec<String> = catalog
            .tables
            .iter()
            .map(|t| table_key(&t.schema, &t.name))
            .collect();
        let mut defs = Map::new();
        defs.insert(
            "relation".to_string(),
            json!({ "type": "string", "enum": relation_names, "description": "A table, view, materialized view or foreign table" }),
        );
        defs.insert(
            "column".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "field_name": graphql_name("GraphQL field name (also used in inputs, filters and orderBy values)"),
                    "description": { "type": "string", "description": "Field description" },
                    "deprecated": { "type": "string", "description": "Deprecate the field, with this reason" },
                    "omit": { "type": "boolean", "description": "Hide the column from every type, input, filter and ordering" }
                },
                "additionalProperties": false
            }),
        );
        defs.insert(
            "foreign_key".to_string(),
            json!({
                "type": "object",
                "properties": Self::relation_naming(),
                "additionalProperties": false
            }),
        );
        let mut entity_properties = Map::new();
        for table in &catalog.tables {
            let name = table_key(&table.schema, &table.name);
            let key = format!("entity_{name}");
            entity_properties.insert(name, json!({ "$ref": format!("#/$defs/{key}") }));
            defs.insert(key, Self::entity_schema(table));
        }

        let mut function_properties = Map::new();
        for func in &catalog.functions {
            let schema = if func.row_argument {
                json!({
                    "type": "object",
                    "description": describe(
                        func.description.as_deref(),
                        format!("Computed column `{}` of `{}`", func.name, func.parent_table.as_deref().unwrap_or("?"))
                    ),
                    "properties": {
                        "field_name": graphql_name("Field name on the table's type"),
                        "description": { "type": "string", "description": "Field description" },
                        "omit": { "type": "boolean", "description": "Remove the field" }
                    },
                    "additionalProperties": false
                })
            } else {
                Self::function_schema(func)
            };
            function_properties
                .entry(function_key(&func.schema, &func.name))
                .or_insert(schema);
        }

        let mut settings = Map::new();
        settings.insert("server".to_string(), Self::server_schema());
        settings.insert("database".to_string(), Self::database_schema());
        settings.insert("auth".to_string(), Self::auth_schema());
        settings.insert(
            "rbac".to_string(),
            json!({
                "type": "object",
                "description": "Prune the generated APIs to what a PostgreSQL role may access",
                "properties": {
                    "enabled": Self::switch(),
                    "role": { "type": "string", "description": "Role whose privileges are inspected (default: the connected user)" }
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "debug".to_string(),
            json!({
                "type": "object",
                "description": "Diagnostics added to responses. Never enable them in production",
                "properties": {
                    "sql": Self::switch_described("Include the generated SQL in the response `extensions`"),
                    "explain": Self::switch_described("Include PostgreSQL's EXPLAIN (ANALYZE) output of every statement")
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "graphql".to_string(),
            json!({
                "type": "object",
                "description": "The GraphQL API",
                "properties": {
                    "enabled": Self::switch_described("Serve the GraphQL API at /graphql (default true)"),
                    "ui": Self::switch_described("Serve the GraphiQL IDE at /graphiql (default: on in dev mode, off in prod)"),
                    "federation": {
                        "type": "object",
                        "description": "Apollo Federation subgraph mode",
                        "properties": {
                            "enabled": Self::switch(),
                            "version": { "type": "string", "description": "Federation spec version (default v2.3)" }
                        },
                        "additionalProperties": false
                    }
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "openapi".to_string(),
            json!({
                "type": "object",
                "description": "The REST API and its OpenAPI description",
                "properties": {
                    "enabled": Self::switch_described("Serve the REST API (default false)"),
                    "ui": Self::switch_described("Serve the Scalar API reference at /scalar (default: on in dev mode, off in prod)")
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "watch".to_string(),
            json!({
                "type": "object",
                "description": "Schema watching on DDL changes and auto-export",
                "properties": {
                    "enabled": Self::switch_described("Re-introspect on DDL changes (default: on in dev mode, off in prod)"),
                    "export": Self::switch_described("On each schema change, write the export files (default true)")
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "export".to_string(),
            json!({
                "type": "object",
                "description": "Export paths for schema and configuration files",
                "properties": {
                    "graphql": { "type": "string", "description": "Path to export GraphQL SDL schema (default schema.graphql)" },
                    "subgraph": { "type": "string", "description": "Path to export Apollo Federation Subgraph SDL (default subgraph.graphql)" },
                    "config_schema": { "type": "string", "description": "Path to export YAML config JSON schema (default graphpg.schema.json)" }
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "entities".to_string(),
            json!({
                "type": "object",
                "description": "Tables, views, materialized views and foreign tables, shared by every API",
                "properties": entity_properties,
                "additionalProperties": false
            }),
        );
        settings.insert(
            "functions".to_string(),
            json!({
                "type": "object",
                "description": "Table-valued and computed-column functions",
                "properties": function_properties,
                "additionalProperties": false
            }),
        );
        settings.insert(
            "types".to_string(),
            json!({
                "type": "object",
                "description": "Enum and composite types (CHECK-closed text columns as <table>.<column>)",
                "properties": Self::type_properties(catalog),
                "additionalProperties": false
            }),
        );
        settings.insert(
            "extensions".to_string(),
            json!({
                "type": "object",
                "description": "Objects owned by extensions are hidden unless their extension is exposed",
                "properties": {
                    "expose": {
                        "type": "array",
                        "items": { "type": "string", "enum": std::iter::once("*").chain(catalog.extensions.iter().map(String::as_str)).collect::<Vec<_>>() },
                        "uniqueItems": true,
                        "description": "Extensions whose tables, views and functions are exposed (\"*\": all)"
                    }
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "search".to_string(),
            json!({
                "type": "object",
                "description": "Search root fields return the best matches, without pagination",
                "properties": {
                    "default_limit": { "type": "integer", "minimum": 1, "description": "Hits returned without `first` (default 10)" },
                    "max_limit": { "type": "integer", "minimum": 1, "description": "Largest accepted `first` (default 50)" }
                },
                "additionalProperties": false
            }),
        );

        // Each setting is defined once and referenced from the root and from both mode
        // blocks, so the entity and column lists are not repeated
        let mut root_properties = Map::new();
        let mut block_properties = Map::new();
        for (name, schema) in settings {
            let key = format!("settings.{name}");
            let reference = json!({ "$ref": format!("#/$defs/{key}") });
            defs.insert(key, schema);
            root_properties.insert(name.clone(), reference.clone());
            block_properties.insert(name, reference);
        }
        defs.insert(
            "mode".to_string(),
            json!({
                "type": "object",
                "properties": block_properties,
                "additionalProperties": false
            }),
        );
        root_properties.insert(
            "dev".to_string(),
            json!({ "$ref": "#/$defs/mode", "description": "Settings that differ in dev mode (`--mode dev`), merged over the ones above" }),
        );
        root_properties.insert(
            "prod".to_string(),
            json!({ "$ref": "#/$defs/mode", "description": "Settings that differ in prod mode (the default), merged over the ones above" }),
        );

        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "graphpg configuration",
            "description": "One file for every environment: shared settings, then `dev:` and `prod:` blocks for what differs. Strings may read the environment: ${NAME} or ${NAME:-default} (quote them inside [] and {})",
            "type": "object",
            "properties": root_properties,
            "additionalProperties": false,
            "$defs": defs
        })
    }

    /// A boolean, or text that reads as one (`${NAME}`): the switches of the config.
    fn switch() -> Value {
        json!({ "type": ["boolean", "string"] })
    }

    fn switch_described(description: &str) -> Value {
        json!({ "type": ["boolean", "string"], "description": description })
    }

    /// `server`: the HTTP listener.
    fn server_schema() -> Value {
        json!({
            "type": "object",
            "description": "The HTTP listener",
            "properties": {
                "host": { "type": "string", "description": "Address to listen on (default 0.0.0.0)" },
                "port": { "type": ["integer", "string"], "description": "Port to listen on (default 4000)" }
            },
            "additionalProperties": false
        })
    }

    /// `database`: the connection, the schemas and the replicas.
    fn database_schema() -> Value {
        json!({
            "type": "object",
            "description": "The PostgreSQL connection, the schemas to expose and the read replicas",
            "properties": {
                "pool_size": { "type": ["integer", "string"], "description": "Connections in the pool (default 10)" },
                "schemas": {
                    "type": "array",
                    "items": { "type": "string" },
                    "uniqueItems": true,
                    "description": "Database schemas to expose, the first being the default (default app_public)"
                },
                "sql_comments": Self::switch_described("Append a sqlcommenter comment to every statement (default true)")
            },
            "additionalProperties": false
        })
    }

    /// `auth`: the engine as an OAuth 2.0 resource server.
    fn auth_schema() -> Value {
        let setting = json!({ "type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*(\\.[A-Za-z_][A-Za-z0-9_]*)+$" });
        json!({
            "type": "object",
            "description": "Authentication with OAuth 2.0 access tokens (`Authorization: Bearer <token>`), whose claims set the PostgreSQL role and the settings Row-Level Security policies read. Off by default",
            "properties": {
                "enabled": Self::switch_described("Whether requests are authenticated (default false)"),
                "issuer": { "type": "string", "format": "uri", "description": "Authorization server issuer: tokens' `iss` must equal it; signing keys are discovered from its metadata" },
                "audience": {
                    "description": "This API's identifier(s): tokens' `aud` must contain one",
                    "oneOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1 }]
                },
                "jwks_uri": { "type": "string", "format": "uri", "description": "JSON Web Key Set URL, when not discovered from the issuer" },
                "algorithms": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "EdDSA", "HS256", "HS384", "HS512"] },
                    "uniqueItems": true,
                    "description": "Accepted signature algorithms (default: the asymmetric ones, or HS256 with `secret`)"
                },
                "token_type": { "type": "string", "description": "Required `typ` header, e.g. `at+jwt` (RFC 9068); unchecked when unset" },
                "leeway_seconds": { "type": "integer", "minimum": 0, "description": "Clock skew tolerated for `exp` and `nbf` (default 30)" },
                "roles": {
                    "type": "object",
                    "description": "The PostgreSQL roles requests run as; tokens never choose one (their claims are data for RLS policies)",
                    "properties": {
                        "authenticated": { "type": "string", "description": "Role of requests with a valid token; unset, the engine's database user" },
                        "anonymous": { "type": "string", "description": "Role of requests without a token; unset, they are rejected (401)" }
                    },
                    "additionalProperties": false
                },
                "session_variables": {
                    "type": "object",
                    "description": "Settings a request's claims are written to",
                    "properties": {
                        "claims_setting": { "allOf": [setting.clone()], "description": "Setting holding every claim as JSON (default `request.jwt.claims`)" },
                        "prefix": { "type": "string", "description": "Prefix of a setting per top-level claim (default `request.jwt.claim.`; empty: none)" },
                        "mappings": { "type": "object", "additionalProperties": setting, "description": "Claim (name or dot-separated path) -> setting name" }
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        })
    }

    /// `types` entries: enums with their labels, composites with their attributes.
    fn type_properties(catalog: &DatabaseCatalog) -> Map<String, Value> {
        let mut props = Map::new();
        for e in &catalog.enums {
            let values: Map<String, Value> = e
                .values
                .iter()
                .map(|v| {
                    (
                        v.label.clone(),
                        graphql_name(&format!("GraphQL value for `{}`", v.label)),
                    )
                })
                .collect();
            let kind = if e.key.starts_with("check:") {
                "CHECK-closed column"
            } else {
                "Enum type"
            };
            props.insert(
                e.config_name.clone(),
                json!({
                    "type": "object",
                    "description": describe(e.description.as_deref(), format!("{kind} `{}`", e.config_name)),
                    "properties": {
                        "type_name": graphql_name("GraphQL enum type name"),
                        "description": { "type": "string" },
                        "values": { "type": "object", "properties": values, "additionalProperties": false }
                    },
                    "additionalProperties": false
                }),
            );
        }
        for c in &catalog.composites {
            let attributes: Map<String, Value> = c
                .attributes
                .iter()
                .map(|a| {
                    let schema = json!({
                        "type": "object",
                        "description": describe(a.description.as_deref(), format!("Attribute `{}`", a.name)),
                        "properties": {
                            "field_name": graphql_name("GraphQL field name"),
                            "description": { "type": "string" }
                        },
                        "additionalProperties": false
                    });
                    (a.name.clone(), schema)
                })
                .collect();
            props.insert(
                c.config_name.clone(),
                json!({
                    "type": "object",
                    "description": describe(c.description.as_deref(), format!("Composite type `{}`", c.config_name)),
                    "properties": {
                        "type_name": graphql_name("GraphQL object type name (the input type is <name>Input)"),
                        "description": { "type": "string" },
                        "attributes": { "type": "object", "properties": attributes, "additionalProperties": false }
                    },
                    "additionalProperties": false
                }),
            );
        }
        for d in &catalog.domains {
            props.insert(
                d.config_name.clone(),
                json!({
                    "type": "object",
                    "description": describe(d.description.as_deref(), format!("Domain `{}`, exposed as a scalar", d.config_name)),
                    "properties": {
                        "type_name": graphql_name("GraphQL scalar name"),
                        "description": { "type": "string" }
                    },
                    "additionalProperties": false
                }),
            );
        }
        props
    }

    /// Relation field options of a foreign key.
    fn relation_naming() -> Map<String, Value> {
        let naming = json!({
            "field_name": graphql_name("Name of the to-one relation field on this type"),
            "reverse_field_name": graphql_name("Name of the to-many relation field on the referenced type (its connection is <name>Connection)"),
            "omit": { "type": "boolean", "description": "Hide both relation fields" },
            "omit_reverse": { "type": "boolean", "description": "Hide the to-many relation fields on the referenced type" }
        });
        naming.as_object().cloned().unwrap_or_default()
    }

    fn entity_schema(table: &TableMetadata) -> Value {
        let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
        let column_properties: Map<String, Value> = table
            .columns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    described_ref(
                        "column",
                        describe(c.description.as_deref(), format!("Column `{}`", c.name)),
                    ),
                )
            })
            .collect();

        let existing_fk_properties: Map<String, Value> = table
            .foreign_keys
            .iter()
            .filter(|fk| !fk.is_virtual)
            .map(|fk| {
                let description = format!(
                    "Foreign key ({}) → {} ({})",
                    fk.columns.join(", "),
                    fk.foreign_table,
                    fk.foreign_columns.join(", ")
                );
                (
                    fk.constraint_name.clone(),
                    described_ref("foreign_key", description),
                )
            })
            .collect();
        let mut virtual_fk_properties = Self::relation_naming();
        virtual_fk_properties.insert(
            "columns".to_string(),
            json!({ "type": "array", "items": { "type": "string", "enum": columns }, "minItems": 1, "description": "Referencing columns" }),
        );
        virtual_fk_properties.insert(
            "references".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "table": described_ref("relation", "Referenced relation".to_string()),
                    "columns": { "type": "array", "items": { "type": "string" }, "description": "Referenced columns (default: its primary key)" }
                },
                "required": ["table"],
                "additionalProperties": false
            }),
        );

        let kind = match table.kind {
            RelationKind::Table => "Table",
            RelationKind::PartitionedTable => "Partitioned table",
            RelationKind::View => "View",
            RelationKind::MaterializedView => "Materialized view",
            RelationKind::ForeignTable => "Foreign table",
        };
        let operations: Vec<&str> = Operation::ALL.iter().map(|o| o.as_str()).collect();
        json!({
            "type": "object",
            "description": describe(table.description.as_deref(), format!("{kind} `{}`", table.name)),
            "properties": {
                "type_name": graphql_name("GraphQL type name (derived names such as <Type>Filter follow it)"),
                "description": { "type": "string", "description": "Type description" },
                "deprecated": { "type": "string", "description": "Deprecate the type's root fields and the relation fields leading to it, with this reason" },
                "omit": { "type": "boolean", "description": "Remove the type, its root fields and the relation fields pointing at it" },
                "omit_operations": {
                    "type": "array",
                    "items": { "type": "string", "enum": operations },
                    "uniqueItems": true,
                    "description": "Root fields not to generate"
                },
                "list_field": graphql_name("Name of the list root field"),
                "single_field": graphql_name("Name of the single-row root field"),
                "connection_field": graphql_name("Name of the connection root field"),
                "primary_key": {
                    "type": "array",
                    "items": { "type": "string", "enum": columns },
                    "minItems": 1,
                    "uniqueItems": true,
                    "description": "Key columns of a relation without a primary key (views, materialized views)"
                },
                "version_column": {
                    "type": "string",
                    "enum": columns,
                    "description": "Column a trigger maintains as the row version (`rowVersion`, `expectedVersion`); rows are versioned by `xmin` without one"
                },
                "columns": {
                    "type": "object",
                    "properties": column_properties,
                    "additionalProperties": false
                },
                "foreign_keys": {
                    "type": "object",
                    "description": "Relation fields by constraint name; other names declare virtual foreign keys",
                    "properties": existing_fk_properties,
                    "additionalProperties": {
                        "type": "object",
                        "properties": virtual_fk_properties,
                        "required": ["columns", "references"],
                        "additionalProperties": false
                    }
                }
            },
            "additionalProperties": false
        })
    }
}
