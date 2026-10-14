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
                    "explain": Self::switch_described("Include PostgreSQL's EXPLAIN (ANALYZE) output of every statement"),
                    "statements": Self::switch_described("Serve GET /debug/statements: the most expensive operations so far (pg_stat_statements), with the indexes they lack. On in dev mode; dev only")
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
                    },
                    "persisted_queries": {
                        "type": "object",
                        "description": "Operations sent by hash (`extensions.persistedQuery.sha256Hash`) or manifest id (`documentId`), and the allow-list",
                        "properties": {
                            "enabled": Self::switch_described("Accept persisted queries; off, a request naming one gets PERSISTED_QUERY_NOT_SUPPORTED (default false)"),
                            "automatic": Self::switch_described("Automatic persisted queries: a hash sent with its text registers it in memory (default true; ignored with allow_list)"),
                            "manifest": { "type": "string", "description": "JSON file of known operations: an Apollo persisted query manifest or a map of ids to texts" },
                            "allow_list": Self::switch_described("Run only the manifest's operations (default false)")
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
                    "enabled": Self::switch_described("Serve the REST API at /rest (default true)"),
                    "ui": Self::switch_described("Serve the Scalar API reference at /scalar (default: on in dev mode, off in prod)"),
                    "spec": Self::switch_described("Serve the OpenAPI 3.1 document at `path` (default: on in dev mode, off in prod)"),
                    "path": json!({
                        "type": "string",
                        "description": "Path to serve the OpenAPI document at (default: /openapi.json)"
                    })
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "rest".to_string(),
            json!({
                "type": "object",
                "description": "Alias for openapi: the REST API and its OpenAPI description",
                "properties": {
                    "enabled": Self::switch_described("Serve the REST API at /rest (default true)"),
                    "ui": Self::switch_described("Serve the Scalar API reference at /scalar (default: on in dev mode, off in prod)"),
                    "spec": Self::switch_described("Serve the OpenAPI 3.1 document at `path` (default: on in dev mode, off in prod)"),
                    "path": json!({
                        "type": "string",
                        "description": "Path to serve the OpenAPI document at (default: /openapi.json)"
                    })
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
        let duration = |d: &str| json!({ "type": "string", "description": format!("{d}, with a unit: `500ms`, `10s`, `5m`, `1h`, `7d`") });
        let webhook = |extra: Value| {
            let mut props = json!({
                "url": { "type": "string", "pattern": "^https?://", "description": "The endpoint the event is POSTed to as JSON" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Headers added to every request (values may read ${NAME})" },
                "secret": { "type": "string", "description": "Standard Webhooks signing secret: whsec_<base64>, or any text" },
                "timeout": duration("How long a request may take (default 10s)"),
                "retry": {
                    "type": "object",
                    "properties": {
                        "max_attempts": { "type": ["integer", "string"], "minimum": 1, "description": "Deliveries attempted at most (default 10)" },
                        "backoff": duration("Wait before the second attempt, doubled for each further one (default 10s)"),
                        "max_backoff": duration("Longest wait between attempts (default 1h)")
                    },
                    "additionalProperties": false
                }
            });
            if let (Some(p), Value::Object(e)) = (props.as_object_mut(), extra) {
                p.extend(e);
            }
            json!({ "type": "object", "properties": props, "required": ["url"], "additionalProperties": false })
        };
        let mut trigger = webhook(json!({
            "table": { "type": "string", "description": "The table, `table` or `schema.table`" },
            "operations": { "type": "array", "items": { "enum": ["insert", "update", "delete"] }, "description": "The changes that raise an event (default: all)" },
            "columns": { "type": "array", "items": { "type": "string" }, "description": "Updates raise an event only when one of these columns changes" }
        }));
        trigger["required"] = json!(["table", "url"]);
        let schedule = webhook(json!({
            "cron": { "type": "string", "description": "5-field cron in UTC (`0 3 * * *`), or @hourly, @daily, @weekly, @monthly. Without it, only events scheduled from SQL are sent" },
            "payload": { "description": "The `payload` member of the event" },
            "catch_up": { "enum": ["latest", "none"], "description": "Slots missed while no instance ran: the most recent one, or none (default latest)" }
        }));
        let name_key = "^[a-z_][a-z0-9_]{0,39}$";
        settings.insert(
            "events".to_string(),
            json!({
                "type": "object",
                "description": "Event triggers: webhooks on row changes (captured in the change's transaction) and on schedules",
                "properties": {
                    "enabled": Self::switch_described("Capture and deliver events (default true; nothing happens without triggers or schedules)"),
                    "schema": { "type": "string", "description": "The schema of graphpg's event table and functions (default graphpg_events)" },
                    "install": { "enum": ["auto", "manual"], "description": "auto: install at startup; manual: only check (`graphpg events sql` prints the DDL)" },
                    "triggers": { "type": "object", "propertyNames": { "pattern": name_key }, "additionalProperties": trigger },
                    "schedules": { "type": "object", "propertyNames": { "pattern": name_key }, "additionalProperties": schedule },
                    "delivery": {
                        "type": "object",
                        "properties": {
                            "concurrency": { "type": ["integer", "string"], "minimum": 1, "description": "Requests in flight at once, per instance (default 4)" },
                            "batch": { "type": ["integer", "string"], "minimum": 1, "description": "Events claimed at a time (default 50)" },
                            "poll": duration("Longest wait for new events without a notification (default 5s)"),
                            "lease": duration("How long a claimed event is held before another instance may deliver it (default 60s)"),
                            "retention": duration("Delivered events are deleted after (default 7d)"),
                            "dead_retention": duration("Dead events are deleted after (default 30d)")
                        },
                        "additionalProperties": false
                    }
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "reload".to_string(),
            json!({
                "type": "object",
                "description": "Schema reload without a restart, in any mode: on NOTIFY <channel> and SIGHUP. The configuration is the one the server started with",
                "properties": {
                    "enabled": Self::switch_described("Listen for reload requests (default true)"),
                    "channel": { "type": "string", "pattern": "^[a-z_][a-z0-9_]{0,62}$", "description": "The NOTIFY channel (default graphpg_reload)" }
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
                    "config_schema": { "type": "string", "description": "Path to export YAML config JSON schema (default graphpg.schema.json)" },
                    "openapi": { "type": "string", "description": "Path to export the OpenAPI 3.1 document (default openapi.json; YAML for .yaml / .yml)" },
                    "pin": {
                        "type": ["string", "boolean"],
                        "enum": ["fail", "warn", "off", true, false],
                        "description": "At startup, compare the exported schema files with the schema the database generates now: `fail` (default in prod mode) refuses to start on drift, `warn` (default in dev mode) logs it, `off` does not check. Files that do not exist pin nothing."
                    }
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
                    },
                    "integrations": {
                        "type": "object",
                        "description": "What graphpg infers from the extensions the schema uses (docs/extensions.md): auto (default) or off, by integration",
                        "properties": crate::catalog::integrations::Integration::ALL
                            .iter()
                            .map(|i| (i.name().to_string(), json!({ "enum": ["auto", "off"] })))
                            .collect::<Map<String, Value>>(),
                        "additionalProperties": false
                    }
                },
                "additionalProperties": false
            }),
        );
        let limit = |description: &str| {
            json!({
                "type": ["integer", "string"],
                "minimum": 1,
                "description": format!("{description} A number, or `off` for no limit.")
            })
        };
        let rate = |description: &str| {
            json!({
                "type": "object",
                "description": description,
                "properties": {
                    "requests_per_minute": limit("Requests per minute."),
                    "burst": { "type": ["integer", "string"], "minimum": 1, "description": "Requests allowed at once (default: requests_per_minute)" }
                },
                "required": ["requests_per_minute"],
                "additionalProperties": false
            })
        };
        settings.insert(
            "limits".to_string(),
            json!({
                "type": "object",
                "description": "Safety limits shared by the GraphQL and REST APIs, enforced on the plan",
                "properties": {
                    "page_size": {
                        "type": "object",
                        "description": "Rows per page: `first` / `last`",
                        "properties": {
                            "default": { "type": ["integer", "string"], "minimum": 1, "description": "Rows of a connection asked for neither `first` nor `last` (default 100)" },
                            "max": limit("Largest `first` / `last`, and the most rows of a list that has no page size (default 1000).")
                        },
                        "additionalProperties": false
                    },
                    "depth": limit("Deepest nesting of rows (default 10)."),
                    "cost": limit("Largest estimated cost: the rows an operation reads (default 10000000)."),
                    "bulk_rows": limit("Most rows one write inserts or an update or delete by filter affects, nested writes included (default 1000)."),
                    "require_filter": { "type": "boolean", "description": "GraphQL `update<Types>(where:)` and `delete<Types>(where:)` with no condition (every row) need `unfiltered: true`; REST always asks for it (default false)" },
                    "groups": limit("Most groups an aggregate request may return (default 10000)."),
                    "request_body_bytes": limit("Largest request body in bytes (default 1048576)."),
                    "count_threshold": limit("`Prefer: count=estimated` counts exactly up to this many estimated rows, and uses the planner estimate above (default 10000)."),
                    "statement_timeout": {
                        "type": "string",
                        "description": "`statement_timeout` of every connection, with a unit: `500ms`, `30s`, `2m`, `1h`; `off` for none (default 30s)"
                    },
                    "rate_limit": {
                        "type": "object",
                        "description": "Requests per client and minute over GraphQL and REST, per instance (off by default)",
                        "properties": {
                            "enabled": Self::switch_described("Limit request rates (default false)"),
                            "anonymous": rate("Requests without a valid access token, by IP address (default 60 a minute)"),
                            "authenticated": rate("Requests with a valid access token, by `key_claim` (default 600 a minute)"),
                            "key_claim": { "type": "string", "description": "The claim naming a client (default sub)" },
                            "trusted_proxies": { "type": "array", "items": { "type": "string" }, "description": "Addresses or CIDR ranges of proxies whose X-Forwarded-For names the client" },
                            "max_clients": { "type": ["integer", "string"], "minimum": 1, "description": "Clients tracked at most per instance (default 100000)" }
                        },
                        "additionalProperties": false
                    }
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "errors".to_string(),
            json!({
                "type": "object",
                "description": "How database errors reach clients, over GraphQL and REST",
                "properties": {
                    "detail": {
                        "enum": ["masked", "full"],
                        "description": "`masked` (default in prod) hides the database's message; `full` (default in dev) shows it"
                    },
                    "map": {
                        "type": "object",
                        "description": "SQLSTATE codes or constraint names -> the problem they become. A constraint name wins over a SQLSTATE. In prod a mapped error shows only its title",
                        "additionalProperties": {
                            "type": "object",
                            "properties": {
                                "status": { "type": "integer", "minimum": 400, "maximum": 599, "description": "HTTP status of the REST problem (default 400)" },
                                "title": { "type": "string", "description": "Problem title (default: the status's reason phrase)" },
                                "code": { "type": "string", "description": "Machine-readable code, `errors[].extensions.code` in GraphQL (default: the SQLSTATE)" },
                                "type": { "type": "string", "description": "RFC 9457 problem type URI (default `about:blank`)" }
                            },
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            }),
        );
        settings.insert(
            "telemetry".to_string(),
            json!({
                "type": "object",
                "description": "Logs, Prometheus metrics, traces and OpenTelemetry (OTLP) export, for Prometheus, an OpenTelemetry collector or the Grafana LGTM stack. Read at startup",
                "properties": {
                    "service_name": { "type": "string", "description": "service.name of exported telemetry (default: OTEL_SERVICE_NAME, else graphpg)" },
                    "logs": {
                        "type": "object",
                        "description": "Log lines on standard output",
                        "properties": {
                            "level": { "type": "string", "description": "Lowest level logged (`error`, `warn`, `info`, `debug`, `trace`) or a filter like `info,graphpg=debug`; RUST_LOG wins when set (default info)" },
                            "format": { "enum": ["text", "json"], "description": "`text` (default) or `json`: one object per line, with the request's trace id" }
                        },
                        "additionalProperties": false
                    },
                    "metrics": {
                        "type": "object",
                        "description": "Prometheus metrics endpoint (text exposition format)",
                        "properties": {
                            "enabled": Self::switch_described("Serve metrics (default: on in dev mode, off in prod)"),
                            "path": { "type": "string", "description": "Path of the endpoint (default /metrics)" },
                            "listen": { "type": "string", "description": "Serve the endpoint on a listener of its own, like `0.0.0.0:9464`, instead of the API's port" }
                        },
                        "additionalProperties": false
                    },
                    "traces": {
                        "type": "object",
                        "description": "One span per request, with planning, compilation and execution inside it",
                        "properties": {
                            "sample_ratio": { "type": ["number", "string"], "minimum": 0, "maximum": 1, "description": "Share of new traces recorded (default 1); a request with a W3C traceparent follows the caller's decision" }
                        },
                        "additionalProperties": false
                    },
                    "otlp": {
                        "type": "object",
                        "description": "Push traces, metrics and logs over OTLP/HTTP (protobuf)",
                        "properties": {
                            "endpoint": { "type": "string", "description": "Base URL, like http://localhost:4318: signals go to /v1/traces, /v1/metrics and /v1/logs (default: OTEL_EXPORTER_OTLP_ENDPOINT, else no export)" },
                            "headers": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Headers of every export request, like Authorization (use ${NAME} for credentials)" },
                            "traces": Self::switch_described("Export traces (default true)"),
                            "metrics": Self::switch_described("Export metrics (default true)"),
                            "logs": Self::switch_described("Export logs (default true)"),
                            "interval": { "type": "string", "description": "How often metrics are pushed, with a unit: `10s`, `1m` (default 60s)" }
                        },
                        "additionalProperties": false
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
                "events": {
                    "type": "object",
                    "description": "Change events of the table (event triggers), described as OpenAPI webhooks",
                    "properties": {
                        "operations": {
                            "type": "array",
                            "items": { "type": "string", "enum": ["insert", "update", "delete"] },
                            "uniqueItems": true,
                            "description": "Row changes that raise an event (default: all)"
                        },
                        "description": { "type": "string", "description": "What the events of the table mean" }
                    },
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
