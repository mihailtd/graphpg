use super::models::{
    table_to_type_name, ColumnMetadata, ConstraintKind, ConstraintMetadata, CtiChild, CtiKind,
    CtiMetadata, DatabaseCatalog, ForeignKeyMetadata, Operations, PgType, RelationKind,
    StiMetadata, StiVariant, TableMetadata, TypeLimits, UniqueKey,
};
use crate::compiler::quote_ident;
use crate::config::Config;
use std::collections::HashMap;
use thiserror::Error;
use tokio_postgres::Client;

/// A finding of [`CatalogInspector::advise`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advice {
    pub level: AdviceLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdviceLevel {
    /// Likely to slow common queries down.
    Warning,
    /// Worth knowing.
    Info,
}

#[derive(Error, Debug)]
pub enum CatalogError {
    #[error("PostgreSQL query error: {0}")]
    Postgres(#[from] tokio_postgres::Error),

    #[error("invalid engine config:\n  - {}", .0.join("\n  - "))]
    Config(Vec<String>),
}

/// SQL predicate: the object `oid` of system catalog `class` is not owned by an extension,
/// unless the extension is exposed (`$2`: expose all, `$3`: exposed extension names).
fn not_extension_owned(class: &str, oid: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM pg_depend d JOIN pg_extension e ON e.oid = d.refobjid
                     WHERE d.classid = '{class}'::regclass AND d.objid = {oid} AND d.deptype = 'e'
                       AND NOT ($2::bool OR e.extname = ANY($3::text[])))"
    )
}

/// Columns describing the type of a value whose `pg_type` row is aliased `t` (see
/// [`TYPE_JOINS`]): base type name, `typtype` and qualified name, then the same for the array
/// element type (NULL for non-arrays). Domains resolve to their base type. Read with
/// [`type_from_row`].
const TYPE_COLUMNS: &str = "bt.typname::text, bt.typtype::text, format('%I.%I', btn.nspname, bt.typname),
    et.typname::text, et.typtype::text, CASE WHEN et.oid IS NOT NULL THEN format('%I.%I', etn.nspname, et.typname) END";

const TYPE_JOINS: &str = "JOIN pg_type bt ON bt.oid = CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END
    JOIN pg_namespace btn ON btn.oid = bt.typnamespace
    LEFT JOIN pg_type et0 ON et0.oid = bt.typelem AND bt.typcategory = 'A'
    LEFT JOIN pg_type et ON et.oid = CASE WHEN et0.typtype = 'd' THEN et0.typbasetype ELSE et0.oid END
    LEFT JOIN pg_namespace etn ON etn.oid = et.typnamespace";

/// Type from the [`TYPE_COLUMNS`] starting at column `at`.
fn type_from_row(row: &tokio_postgres::Row, at: usize) -> PgType {
    match row.get::<_, Option<String>>(at + 3) {
        Some(elem) => PgType::Array(Box::new(column_type(
            &elem,
            row.get(at + 4),
            row.get(at + 5),
        ))),
        None => column_type(row.get(at), row.get(at + 1), row.get(at + 2)),
    }
}

/// Enum type key of a value of `pg_type` (or of its elements).
fn enum_key_of(pg_type: &PgType) -> Option<String> {
    match pg_type {
        PgType::Enum(key) => Some(key.clone()),
        PgType::Array(elem) => enum_key_of(elem),
        _ => None,
    }
}

/// Which extensions' objects are exposed (`extensions.expose` in the config).
struct ExtensionFilter {
    all: bool,
    names: Vec<String>,
}

/// `pg_relation_is_updatable` bits (`1 << CmdType`).
const UPDATABLE_UPDATE: i32 = 1 << 2;
const UPDATABLE_INSERT: i32 = 1 << 3;
const UPDATABLE_DELETE: i32 = 1 << 4;

pub struct CatalogInspector<'a> {
    client: &'a Client,
}

impl<'a> CatalogInspector<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// The schemas to inspect: the config's `schemas` when it names any, else
    /// `schema_name`.
    pub fn effective_schemas<'s>(&self, schema_name: &'s str, config: &'s Config) -> Vec<&'s str> {
        if !config.database.schemas.is_empty() {
            config.database.schemas.iter().map(|s| s.as_str()).collect()
        } else if !schema_name.is_empty() {
            vec![schema_name]
        } else {
            vec![crate::config::DEFAULT_SCHEMA]
        }
    }

    /// Inspects `schema_name` without an engine config.
    pub async fn inspect_schema(&self, schema_name: &str) -> Result<DatabaseCatalog, CatalogError> {
        self.inspect_schema_with_config(schema_name, &Config::default())
            .await
    }

    /// Inspects `schema_names` without an engine config.
    pub async fn inspect_schemas(
        &self,
        schema_names: &[&str],
    ) -> Result<DatabaseCatalog, CatalogError> {
        self.inspect_schemas_with_config(schema_names, &Config::default())
            .await
    }

    /// Inspects `schema_names` and applies `config`: the catalog the GraphQL schema is built from.
    pub async fn inspect_schemas_with_config(
        &self,
        schema_names: &[&str],
        config: &Config,
    ) -> Result<DatabaseCatalog, CatalogError> {
        let base = self.inspect_base(schema_names, config).await?;
        finalize(base, config)
    }

    /// Inspects `schema_name` (or multiple schemas if defined in `config.database.schemas`) and applies `config`.
    pub async fn inspect_schema_with_config(
        &self,
        schema_name: &str,
        config: &Config,
    ) -> Result<DatabaseCatalog, CatalogError> {
        let schemas = self.effective_schemas(schema_name, config);
        self.inspect_schemas_with_config(&schemas, config).await
    }

    /// Everything detected from the database (plus the keys, relations and function mounts the
    /// config declares), before GraphQL names are assigned. This is the catalog the config's
    /// JSON Schema is generated from, since the config refers to database object names.
    pub async fn inspect_base(
        &self,
        schema_names: &[&str],
        config: &Config,
    ) -> Result<DatabaseCatalog, CatalogError> {
        let (extensions, installed) = self.extension_filter(config).await?;
        let mut catalog = DatabaseCatalog::default();
        catalog.extensions = installed;
        catalog.server_version = self
            .client
            .query_one("SELECT current_setting('server_version_num')::int", &[])
            .await?
            .get::<_, i32>(0) as u32;
        // Pruning inspects as the configured role. The role is reset whatever the outcome: the
        // client may be pooled, and a later borrower must not run as the restricted role.
        let pruning_role = config.rbac.role.as_deref().filter(|_| config.rbac.enabled);
        if let Some(role) = pruning_role {
            let safe_role = role.replace('"', "\"\"");
            self.client
                .batch_execute(&format!("SET ROLE \"{safe_role}\";"))
                .await?;
        }
        let detected = self
            .inspect_detected(schema_names, &extensions, config, &mut catalog)
            .await;
        if pruning_role.is_some() {
            self.client.batch_execute("RESET ROLE;").await?;
        }
        detected?;
        Ok(catalog)
    }

    /// The detection passes of [`inspect_base`](Self::inspect_base), run as the pruning role
    /// when one is configured.
    async fn inspect_detected(
        &self,
        schema_names: &[&str],
        extensions: &ExtensionFilter,
        config: &Config,
        catalog: &mut DatabaseCatalog,
    ) -> Result<(), CatalogError> {
        self.inspect_integrations(schema_names, config, catalog)
            .await?;
        catalog.tables = self
            .inspect_relations(schema_names, extensions, catalog.server_version, config)
            .await?;
        self.inspect_timescale(schema_names, catalog).await?;
        self.inspect_check_enums(schema_names, catalog).await?;
        self.inspect_functions(schema_names, extensions, catalog)
            .await?;
        self.inspect_value_types(catalog).await?;

        // Keys and relations the database cannot declare (views), and function mounts, feed
        // the detection passes below
        crate::config::apply_structure(catalog, config).map_err(CatalogError::Config)?;
        // The keys of remote tables (postgres_fdw), where graphpg.yaml declares none
        if catalog
            .integration(super::integrations::Integration::PostgresFdw)
            .is_some()
        {
            super::remote_keys::import(self.client, schema_names, catalog).await?;
        }

        self.inspect_exclusive_arcs(schema_names, catalog).await?;
        self.inspect_polymorphism(schema_names, catalog).await;
        detect_many_to_many(catalog);
        self.inspect_search_indexes(schema_names, catalog).await?;
        Ok(())
    }

    /// The integrations in use (see [`super::integrations`]): installed, not switched off, and
    /// used in `schema_names`.
    async fn inspect_integrations(
        &self,
        schema_names: &[&str],
        config: &Config,
        catalog: &mut DatabaseCatalog,
    ) -> Result<(), CatalogError> {
        use super::integrations::{ActiveIntegration, Integration};
        let installed: HashMap<String, (String, String)> = self
            .client
            .query(
                "SELECT e.extname::text, e.extversion::text, n.nspname::text
                 FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace",
                &[],
            )
            .await?
            .iter()
            .map(|r| (r.get(0), (r.get(1), r.get(2))))
            .collect();
        catalog.integrations.clear();
        for integration in Integration::ALL {
            let Some((version, ext_schema)) = installed.get(integration.extension()) else {
                continue;
            };
            if !config.extensions.integration_on(integration) {
                continue;
            }
            let detail = match integration.use_query() {
                None => "installed".to_string(),
                Some(sql) => {
                    // A failing probe (an incompatible version, no privilege) is no use
                    let probed = self.guarded_query(sql, schema_names).await?;
                    let uses: i64 = match probed.as_ref().map(|rows| rows.first()) {
                        Ok(Some(row)) => row.get(0),
                        Ok(None) => 0,
                        Err(err) => {
                            tracing::warn!(integration = integration.name(), error = %err, "could not detect the extension's use; integration off");
                            0
                        }
                    };
                    if uses == 0 {
                        continue;
                    }
                    let unit = integration.unit();
                    format!("{uses} {unit}{}", if uses == 1 { "" } else { "s" })
                }
            };
            catalog.integrations.push(ActiveIntegration {
                integration,
                version: version.clone(),
                schema: ext_schema.clone(),
                detail,
            });
        }
        Ok(())
    }

    /// Runs `sql` with the schemas as `$1`; an error is returned as a value, not raised. Inside
    /// a transaction (the pruning role's) a savepoint keeps a failure from aborting it.
    async fn guarded_query(
        &self,
        sql: &str,
        schema_names: &[&str],
    ) -> Result<Result<Vec<tokio_postgres::Row>, tokio_postgres::Error>, CatalogError> {
        let in_transaction = self
            .client
            .batch_execute("SAVEPOINT graphpg_probe")
            .await
            .is_ok();
        let result = self.client.query(sql, &[&schema_names]).await;
        if in_transaction {
            let end = if result.is_ok() {
                "RELEASE SAVEPOINT graphpg_probe"
            } else {
                "ROLLBACK TO SAVEPOINT graphpg_probe; RELEASE SAVEPOINT graphpg_probe"
            };
            self.client.batch_execute(end).await?;
        }
        Ok(result)
    }

    /// TimescaleDB's hypertables (and their time dimension) and continuous aggregates, when
    /// the integration is on.
    async fn inspect_timescale(
        &self,
        schema_names: &[&str],
        catalog: &mut DatabaseCatalog,
    ) -> Result<(), CatalogError> {
        use super::integrations::Integration;
        use super::models::Timescale;
        if catalog.integration(Integration::TimescaleDb).is_none() {
            return Ok(());
        }
        let hypertables = self
            .guarded_query(
                "SELECT hypertable_schema::text, hypertable_name::text, column_name::text, time_interval::text
                 FROM timescaledb_information.dimensions
                 WHERE dimension_number = 1 AND hypertable_schema = ANY($1)",
                schema_names,
            )
            .await?;
        let aggregates = self
            .guarded_query(
                "SELECT view_schema::text, view_name::text, materialized_only
                 FROM timescaledb_information.continuous_aggregates WHERE view_schema = ANY($1)",
                schema_names,
            )
            .await?;
        let (Ok(hypertables), Ok(aggregates)) = (hypertables, aggregates) else {
            tracing::warn!("could not read TimescaleDB's catalog; its integration does nothing");
            return Ok(());
        };
        let mut set = |schema: String, name: String, value: Timescale| {
            if let Some(t) = catalog
                .tables
                .iter_mut()
                .find(|t| t.schema == schema && t.name == name)
            {
                t.timescale = Some(value);
            }
        };
        for row in hypertables {
            set(
                row.get(0),
                row.get(1),
                Timescale::Hypertable {
                    time_column: row.get(2),
                    chunk_interval: row.get(3),
                },
            );
        }
        for row in aggregates {
            set(
                row.get(0),
                row.get(1),
                Timescale::ContinuousAggregate {
                    materialized_only: row.get(2),
                },
            );
        }
        Ok(())
    }

    /// Advice on indexes and keys the schema relies on, as warnings (not schema): foreign
    /// keys no index leads with (to-many relation fields and relation filters then scan the
    /// referencing table), tables without a primary key, and orderable columns no index
    /// leads with (ordering a connection by them sorts every matching row).
    pub async fn advise(
        &self,
        schema_name: &str,
        catalog: &DatabaseCatalog,
    ) -> Result<Vec<Advice>, CatalogError> {
        self.advise_schemas(&[schema_name], catalog).await
    }

    pub async fn advise_schemas(
        &self,
        schema_names: &[&str],
        catalog: &DatabaseCatalog,
    ) -> Result<Vec<Advice>, CatalogError> {
        // Types named by graphpg because the name derived from the database was taken
        let mut advice: Vec<Advice> = catalog
            .renamed_types
            .iter()
            .map(|r| Advice {
                level: AdviceLevel::Warning,
                message: r.message(),
            })
            .collect();
        let rows = self
            .client
            .query(
                "SELECT c.relname::text, con.conname::text,
                        ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(num, ord)
                              JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.num ORDER BY k.ord)
                 FROM pg_constraint con
                 JOIN pg_class c ON c.oid = con.conrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = ANY($1) AND con.contype = 'f' AND c.relkind IN ('r', 'p')
                   AND NOT EXISTS (
                       SELECT 1 FROM pg_index i
                       WHERE i.indrelid = con.conrelid AND i.indpred IS NULL
                         AND (i.indkey::int2[])[0:cardinality(con.conkey) - 1] @> con.conkey
                   )
                 ORDER BY 1, 2",
                &[&schema_names],
            )
            .await?;
        for row in rows {
            let (table, constraint, columns): (String, String, Vec<String>) =
                (row.get(0), row.get(1), row.get(2));
            let cols = columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            advice.push(Advice {
                level: AdviceLevel::Warning,
                message: format!(
                    "foreign key `{constraint}` of `{table}` ({}) has no index leading with its columns: relation fields and filters reaching `{table}` from the rows it references scan it. Consider: CREATE INDEX ON {} ({cols});",
                    columns.join(", "),
                    quote_ident(&table)
                ),
            });
        }
        for t in catalog.tables.iter().filter(|t| {
            t.primary_key.is_empty()
                && matches!(
                    t.kind,
                    super::models::RelationKind::Table
                        | super::models::RelationKind::PartitionedTable
                )
        }) {
            let message = match &t.timescale {
                // A hypertable's unique indexes must include its time column: few have a key
                Some(super::models::Timescale::Hypertable { time_column, .. }) => format!(
                    "hypertable `{}` has no primary key: it has no single-row fields, updates or deletes, and its cursors are not stable. A hypertable's key must include `{time_column}`: declare one in graphpg.yaml (`entities.{}.primary_key: [<id or device>, {time_column}]`), backed by a unique index on those columns",
                    t.name, t.name
                ),
                _ => format!(
                    "table `{}` has no primary key: it has no single-row fields, updates or deletes, and its cursors are not stable",
                    t.name
                ),
            };
            advice.push(Advice {
                level: AdviceLevel::Warning,
                message,
            });
        }
        // Columns an index leads with, per table
        let rows = self
            .client
            .query(
                "SELECT n.nspname::text, c.relname::text, a.attname::text
                 FROM pg_index i
                 JOIN pg_class c ON c.oid = i.indrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[0]
                 WHERE n.nspname = ANY($1)",
                &[&schema_names],
            )
            .await?;
        let mut leading: HashMap<(String, String), Vec<String>> = HashMap::new();
        for row in rows {
            leading
                .entry((row.get(0), row.get(1)))
                .or_default()
                .push(row.get(2));
        }
        for t in catalog.tables.iter().filter(|t| {
            matches!(
                t.kind,
                super::models::RelationKind::Table | super::models::RelationKind::PartitionedTable
            )
        }) {
            let indexed = leading.get(&(t.schema.clone(), t.name.clone()));
            let unindexed: Vec<&str> = t
                .visible_columns()
                .filter(|c| {
                    c.pg_type.is_orderable() && !indexed.is_some_and(|i| i.contains(&c.name))
                })
                .map(|c| c.name.as_str())
                .collect();
            if !unindexed.is_empty() && t.operations.connection {
                advice.push(Advice {
                    level: AdviceLevel::Info,
                    message: format!(
                        "`{}`: no index leads with {} (ordering its connections by them sorts every matching row)",
                        t.name,
                        unindexed.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ")
                    ),
                });
            }
        }
        Ok(advice)
    }

    /// Advice on foreign tables (see `docs/foreign-data.md`): what makes them slow or unsafe
    /// as they are set up. `auth`: the roles requests run as, and whether they are users at
    /// all (Row-Level Security cannot apply to a foreign table).
    pub async fn advise_foreign_tables(
        &self,
        schema_names: &[&str],
        catalog: &DatabaseCatalog,
        auth: &crate::config::models::AuthConfig,
    ) -> Result<Vec<Advice>, CatalogError> {
        let rows = self
            .client
            .query(
                "SELECT n.nspname::text, c.relname::text, s.srvname::text, w.fdwname::text,
                        coalesce(ft.ftoptions, '{}')::text[], coalesce(s.srvoptions, '{}')::text[],
                        c.reltuples::float8
                 FROM pg_foreign_table ft
                 JOIN pg_class c ON c.oid = ft.ftrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_foreign_server s ON s.oid = ft.ftserver
                 JOIN pg_foreign_data_wrapper w ON w.oid = s.srvfdw
                 WHERE n.nspname = ANY($1)
                 ORDER BY 1, 2",
                &[&schema_names],
            )
            .await?;
        // The roles requests run as: the configured ones, else the connecting user
        let mut roles: Vec<String> = [&auth.roles.authenticated, &auth.roles.anonymous]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        if roles.is_empty() || (auth.enabled && auth.roles.authenticated.is_none()) {
            roles.push(
                self.client
                    .query_one("SELECT current_user::text", &[])
                    .await?
                    .get(0),
            );
        }
        let mut advice = Vec::new();
        let mut warn = |level, message: String| advice.push(Advice { level, message });
        for row in rows {
            let (schema, name, server, wrapper): (String, String, String, String) =
                (row.get(0), row.get(1), row.get(2), row.get(3));
            let (table_options, server_options): (Vec<String>, Vec<String>) =
                (row.get(4), row.get(5));
            let reltuples: f64 = row.get(6);
            let Some(t) = catalog.find_table(&schema, &name) else {
                continue;
            };
            let at = format!("foreign table `{schema}.{name}` ({wrapper}, server `{server}`)");
            let exposed = t.operations.read || t.operations.list || t.operations.connection;
            let file = matches!(wrapper.as_str(), "file_fdw" | "parquet_fdw");

            if t.primary_key.is_empty() && exposed {
                warn(AdviceLevel::Warning, format!(
                    "{at} has no key: declare the remote side's (`primary_key` in graphpg.yaml, or `@primaryKey` in its comment) for lookups, Relay ids, updates and relations"
                ));
            }
            let remote_estimate = |options: &[String]| {
                options
                    .iter()
                    .find_map(|o| o.strip_prefix("use_remote_estimate="))
                    .map(|v| v == "true")
            };
            if wrapper == "postgres_fdw"
                && remote_estimate(&table_options).or_else(|| remote_estimate(&server_options))
                    != Some(true)
            {
                warn(AdviceLevel::Info, format!(
                    "{at}: `use_remote_estimate` is off, so PostgreSQL plans its joins with guesses. Consider: ALTER SERVER {} OPTIONS (ADD use_remote_estimate 'true');",
                    quote_ident(&server)
                ));
            }
            if reltuples < 0.0 {
                warn(AdviceLevel::Info, format!(
                    "{at} has never been analyzed, so its row estimates are guesses. Consider: ANALYZE {}.{};",
                    quote_ident(&schema), quote_ident(&name)
                ));
            }
            // To-many relations into it: one remote query (or file read) per parent row
            for parent in catalog
                .tables
                .iter()
                .filter(|p| p.kind != RelationKind::ForeignTable)
            {
                for fk in t.foreign_keys.iter().filter(|fk| {
                    fk.foreign_schema == parent.schema
                        && fk.foreign_table == parent.name
                        && !fk.omit_reverse
                        && !fk.omit
                }) {
                    let cost = if file {
                        "reads the whole file once per parent row"
                    } else {
                        "runs one remote query per parent row"
                    };
                    warn(AdviceLevel::Info, format!(
                        "the to-many relation from `{}.{}` to {at} (`{}`) {cost}: keep its parents' pages small, query from `{name}` instead, or serve a materialized view of it with an index on ({})",
                        parent.schema, parent.name, fk.constraint_name, fk.columns.join(", ")
                    ));
                }
            }
            if auth.enabled && exposed {
                let allowed = if file {
                    "its role's GRANTs allow"
                } else {
                    "its role's GRANTs and user mapping allow"
                };
                warn(AdviceLevel::Warning, format!(
                    "{at} has root fields while requests are authenticated, but Row-Level Security cannot apply to a foreign table: every request reads every row {allowed}. Reach it through its parents only (`omit_operations: [read, list, connection]`) or through a `security_barrier` view"
                ));
            }
            for role in &roles {
                let mapped: bool = self
                    .client
                    .query_one(
                        "SELECT EXISTS (SELECT 1 FROM pg_user_mappings
                                        WHERE srvname = $1 AND (usename = $2 OR usename = 'public'))",
                        &[&server, role],
                    )
                    .await?
                    .get(0);
                if !mapped && wrapper != "file_fdw" {
                    warn(AdviceLevel::Warning, format!(
                        "{at}: role `{role}` has no user mapping for server `{server}`, so its requests reading it fail. Consider: CREATE USER MAPPING FOR {} SERVER {} OPTIONS (user '...', password '...');",
                        quote_ident(role), quote_ident(&server)
                    ));
                }
            }
        }
        Ok(advice)
    }

    /// Validates `extensions.expose` against the installed extensions.
    async fn extension_filter(
        &self,
        config: &Config,
    ) -> Result<(ExtensionFilter, Vec<String>), CatalogError> {
        let installed: Vec<String> = self
            .client
            .query("SELECT extname::text FROM pg_extension ORDER BY 1", &[])
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let errors: Vec<String> = config
            .extensions
            .expose
            .iter()
            .filter(|e| *e != "*" && !installed.contains(e))
            .map(|e| format!("extensions.expose: no extension `{e}` is installed"))
            .collect();
        if !errors.is_empty() {
            return Err(CatalogError::Config(errors));
        }
        let filter = ExtensionFilter {
            all: config.extensions.exposes_all(),
            names: config.extensions.expose.clone(),
        };
        Ok((filter, installed))
    }

    /// Tables, partitioned tables, views, materialized views and foreign tables of the schema
    /// with their columns, primary keys and foreign keys, in three catalog-wide queries.
    ///
    /// Like `information_schema`, only relations and columns the current role has some
    /// privilege on are included. Partitions are left out (their partitioned table is the
    /// relation to query), and so are relations owned by extensions that are not exposed.
    async fn inspect_relations(
        &self,
        schema_names: &[&str],
        extensions: &ExtensionFilter,
        server_version: u32,
        config: &Config,
    ) -> Result<Vec<TableMetadata>, CatalogError> {
        let schemas_vec: Vec<String> = schema_names.iter().map(|s| s.to_string()).collect();
        let rel_query = format!(
            "SELECT c.oid, c.relname, c.relkind::text, obj_description(c.oid, 'pg_class'),
                    pg_relation_is_updatable(c.oid, true),
                    (has_table_privilege(c.oid, 'SELECT') OR has_any_column_privilege(c.oid, 'SELECT')) AS can_select,
                    (has_table_privilege(c.oid, 'INSERT') OR has_any_column_privilege(c.oid, 'INSERT')) AS can_insert,
                    (has_table_privilege(c.oid, 'UPDATE') OR has_any_column_privilege(c.oid, 'UPDATE')) AS can_update,
                    has_table_privilege(c.oid, 'DELETE') AS can_delete,
                    n.nspname::text
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = ANY($1)
               AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
               AND NOT c.relispartition
               AND (has_table_privilege(c.oid, 'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')
                    OR has_any_column_privilege(c.oid, 'SELECT, INSERT, UPDATE, REFERENCES'))
               AND {}
             ORDER BY n.nspname, c.relname",
            not_extension_owned("pg_class", "c.oid")
        );
        let rel_rows = self
            .client
            .query(
                &rel_query,
                &[&schemas_vec, &extensions.all, &extensions.names],
            )
            .await?;

        let mut tables: Vec<TableMetadata> = Vec::with_capacity(rel_rows.len());
        let mut by_oid: HashMap<u32, usize> = HashMap::new();
        for row in rel_rows {
            let oid: u32 = row.get(0);
            let Some(kind) = RelationKind::from_relkind(row.get(2)) else {
                continue;
            };
            let updatable: i32 = row.get(4);
            let can_select: bool = row.get(5);
            let can_insert: bool = row.get(6);
            let can_update: bool = row.get(7);
            let can_delete: bool = row.get(8);

            if config.rbac.enabled && !can_select && !can_insert && !can_update && !can_delete {
                continue;
            }

            let operations = if config.rbac.enabled {
                let create = (updatable & UPDATABLE_INSERT != 0) && can_insert;
                let update = (updatable & UPDATABLE_UPDATE != 0) && can_update;
                let delete = (updatable & UPDATABLE_DELETE != 0) && can_delete;
                Operations {
                    read: can_select,
                    list: can_select,
                    connection: can_select,
                    create,
                    update,
                    delete,
                    create_many: create,
                    update_many: update,
                    delete_many: delete,
                    upsert: create && update,
                }
            } else {
                Operations {
                    read: true,
                    list: true,
                    connection: true,
                    create: updatable & UPDATABLE_INSERT != 0,
                    update: updatable & UPDATABLE_UPDATE != 0,
                    delete: updatable & UPDATABLE_DELETE != 0,
                    create_many: updatable & UPDATABLE_INSERT != 0,
                    update_many: updatable & UPDATABLE_UPDATE != 0,
                    delete_many: updatable & UPDATABLE_DELETE != 0,
                    upsert: updatable & UPDATABLE_INSERT != 0 && updatable & UPDATABLE_UPDATE != 0,
                }
            };

            by_oid.insert(oid, tables.len());
            tables.push(TableMetadata {
                timescale: None,
                schema: row.get(9),
                name: row.get(1),
                kind,
                columns: Vec::new(),
                primary_key: Vec::new(),
                foreign_keys: Vec::new(),
                description: row.get(3),
                type_name: String::new(),
                list_field: String::new(),
                single_field: String::new(),
                connection_field: String::new(),
                all_connection_field: None,
                // `read`, `update` and `delete` also need a key; see `refresh_key_operations`
                operations,
                unique_keys: Vec::new(),
                constraints: Vec::new(),
                version_column: None,
                deprecation: None,
                federation_key: None,
                federation_shareable: false,
            });
        }

        // Columns. Domains are exposed as their base type; array element types are resolved
        // the same way. Composite types are recognised by `typtype = 'c'`.
        let col_query = format!(
                "SELECT a.attrelid, a.attname,
                        {TYPE_COLUMNS},
                        a.attnotnull OR t.typnotnull,
                        pg_get_expr(ad.adbin, ad.adrelid),
                        a.attgenerated::text, a.attidentity::text,
                        col_description(a.attrelid, a.attnum),
                        CASE WHEN t.typtype = 'd' THEN format('%I.%I', dn.nspname, t.typname) END,
                        CASE WHEN a.atttypmod < 0 AND t.typtype = 'd' THEN t.typtypmod ELSE a.atttypmod END,
                        has_column_privilege(a.attrelid, a.attnum, 'SELECT') AS col_can_select,
                        (has_column_privilege(a.attrelid, a.attnum, 'INSERT') OR has_column_privilege(a.attrelid, a.attnum, 'UPDATE')) AS col_can_write
                 FROM pg_attribute a
                 JOIN pg_class c ON c.oid = a.attrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_type t ON t.oid = a.atttypid
                 JOIN pg_namespace dn ON dn.oid = t.typnamespace
                 {TYPE_JOINS}
                 LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
                 WHERE n.nspname = ANY($1)
                   AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
                   AND NOT c.relispartition
                   AND a.attnum > 0
                   AND NOT a.attisdropped
                   AND (pg_has_role(c.relowner, 'USAGE')
                        OR has_column_privilege(a.attrelid, a.attnum, 'SELECT, INSERT, UPDATE, REFERENCES'))
                 ORDER BY a.attrelid, a.attnum");
        let col_rows = self.client.query(&col_query, &[&schemas_vec]).await?;
        for row in col_rows {
            let Some(&idx) = by_oid.get(&row.get::<_, u32>(0)) else {
                continue;
            };
            let pg_type = type_from_row(&row, 2);
            let generated: String = row.get(10);
            let identity: String = row.get(11);
            let is_generated = !generated.is_empty() || identity == "a";
            let default_value: Option<String> = match (is_generated, identity.as_str()) {
                (true, _) => None,
                (false, "d") => Some("generated by default as identity".to_string()),
                (false, _) => row.get(9),
            };
            let generation: Option<String> = if generated.is_empty() {
                None
            } else {
                row.get(9)
            };
            let col_can_select: bool = row.get(15);
            let col_can_write: bool = row.get(16);
            let omit = if config.rbac.enabled {
                !col_can_select
            } else {
                false
            };
            let readonly = if config.rbac.enabled {
                !col_can_write
            } else {
                false
            };
            tables[idx].columns.push(ColumnMetadata {
                name: row.get(1),
                is_nullable: !row.get::<_, bool>(8),
                is_primary_key: false,
                default_value,
                is_generated,
                description: row.get(12),
                limits: TypeLimits::from_typmod(&pg_type, row.get(14)),
                generation,
                deprecation: None,
                field_name: String::new(),
                omit,
                readonly,
                enum_key: enum_key_of(&pg_type),
                domain: row
                    .get::<_, Option<String>>(13)
                    .filter(|_| pg_type.is_scalar_base()),
                pg_type,
            });
        }

        // Constraints: primary and foreign keys (columns in key order), unique, check and
        // exclusion constraints
        let (period, enforced) = if server_version >= 180000 {
            ("con.conperiod", "con.conenforced")
        } else {
            ("false", "true")
        };
        let con_rows = self
            .client
            .query(
                &format!(
                    "SELECT con.conrelid, con.conname::text, con.contype::text,
                            ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(num, ord)
                                  JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.num ORDER BY k.ord),
                            fn.nspname::text, fc.relname::text,
                            ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(num, ord)
                                  JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.num ORDER BY k.ord),
                            pg_get_constraintdef(con.oid), {period}, {enforced}
                     FROM pg_constraint con
                     JOIN pg_class c ON c.oid = con.conrelid
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                     LEFT JOIN pg_class fc ON fc.oid = con.confrelid
                     LEFT JOIN pg_namespace fn ON fn.oid = fc.relnamespace
                     WHERE n.nspname = ANY($1) AND con.contype IN ('p', 'u', 'f', 'c', 'x')
                     ORDER BY con.conrelid, con.oid"
                ),
                &[&schemas_vec],
            )
            .await?;
        for row in con_rows {
            let Some(&idx) = by_oid.get(&row.get::<_, u32>(0)) else {
                continue;
            };
            let table = &mut tables[idx];
            let columns: Vec<String> = row.get(3);
            let kind = match row.get::<_, &str>(2) {
                "p" => ConstraintKind::PrimaryKey,
                "u" => ConstraintKind::Unique,
                "f" => ConstraintKind::ForeignKey,
                "c" => ConstraintKind::Check,
                _ => ConstraintKind::Exclusion,
            };
            let references = (kind == ConstraintKind::ForeignKey)
                .then(|| (row.get::<_, String>(4), row.get::<_, String>(5)));
            // PostgreSQL 18 `NOT ENFORCED`: a check promises nothing, so nothing is derived
            // from it; a foreign key still states a relationship, but no write fails on it
            let enforced: bool = row.get(9);
            if !enforced && kind == ConstraintKind::Check {
                continue;
            }
            if enforced {
                table.constraints.push(ConstraintMetadata {
                    name: row.get(1),
                    kind,
                    columns: columns.clone(),
                    definition: row.get(7),
                    references,
                    period: row.get(8),
                    code: String::new(),
                    referenced_code: String::new(),
                });
            }
            match kind {
                ConstraintKind::PrimaryKey => {
                    for c in &mut table.columns {
                        c.is_primary_key = columns.contains(&c.name);
                    }
                    table.primary_key = columns;
                }
                ConstraintKind::ForeignKey => table.foreign_keys.push(ForeignKeyMetadata {
                    batch: false,
                    constraint_name: row.get(1),
                    columns,
                    foreign_schema: row.get(4),
                    foreign_table: row.get(5),
                    foreign_columns: row.get(6),
                    is_virtual: false,
                    field_name: None,
                    reverse_field_name: None,
                    omit: false,
                    omit_reverse: false,
                    period: row.get(8),
                }),
                _ => {}
            }
        }

        // Unique indexes that back no constraint (partial ones and ones on expressions
        // included) reject rows too: their names are what violations report
        let index_rows = self
            .client
            .query(
                "SELECT i.indrelid, ic.relname::text, pg_get_indexdef(i.indexrelid)
                 FROM pg_index i
                 JOIN pg_class c ON c.oid = i.indrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_class ic ON ic.oid = i.indexrelid
                 WHERE n.nspname = ANY($1) AND i.indisunique
                   AND NOT EXISTS (SELECT 1 FROM pg_constraint con WHERE con.conindid = i.indexrelid AND con.conrelid = i.indrelid)
                 ORDER BY i.indrelid, ic.relname",
                &[&schemas_vec],
            )
            .await?;
        for row in index_rows {
            let Some(&idx) = by_oid.get(&row.get::<_, u32>(0)) else {
                continue;
            };
            let table = &mut tables[idx];
            let definition: String = row.get(2);
            let columns = index_columns(&definition, table);
            table.constraints.push(ConstraintMetadata {
                name: row.get(1),
                kind: ConstraintKind::Unique,
                columns,
                definition,
                references: None,
                period: false,
                code: String::new(),
                referenced_code: String::new(),
            });
        }

        // Unique indexes usable as row keys and `ON CONFLICT` arbiters: valid, immediate
        // (deferrable constraints cannot arbitrate), on plain columns (no expressions), without
        // a predicate; key columns only (`INCLUDE` columns are not part of the key)
        let unique_rows = self
            .client
            .query(
                "SELECT i.indrelid, ic.relname::text, i.indisprimary,
                        ARRAY(SELECT a.attname::text FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(num, ord)
                              JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.num
                              WHERE k.ord <= i.indnkeyatts ORDER BY k.ord)
                 FROM pg_index i
                 JOIN pg_class c ON c.oid = i.indrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_class ic ON ic.oid = i.indexrelid
                 WHERE n.nspname = ANY($1) AND i.indisunique AND i.indisvalid AND i.indimmediate
                   AND i.indpred IS NULL AND i.indexprs IS NULL AND NOT (0 = ANY (i.indkey::int2[]))
                 ORDER BY i.indrelid, i.indisprimary DESC, ic.relname",
                &[&schemas_vec],
            )
            .await?;
        for row in unique_rows {
            let Some(&idx) = by_oid.get(&row.get::<_, u32>(0)) else {
                continue;
            };
            let columns: Vec<String> = row.get(3);
            let table = &mut tables[idx];
            // An index repeating another key's columns (e.g. a unique index beside the constraint)
            if columns.is_empty()
                || table
                    .unique_keys
                    .iter()
                    .any(|k| same_columns(&k.columns, &columns))
            {
                continue;
            }
            table.unique_keys.push(UniqueKey {
                name: row.get(1),
                is_primary: row.get(2),
                columns,
            });
        }
        Ok(tables)
    }

    /// Text columns closed by a single-column `CHECK` value list (`status IN ('draft', 'active')`)
    /// become enums. The first such constraint of a column wins.
    async fn inspect_check_enums(
        &self,
        schema_names: &[&str],
        catalog: &mut DatabaseCatalog,
    ) -> Result<(), CatalogError> {
        let schemas_vec: Vec<String> = schema_names.iter().map(|s| s.to_string()).collect();
        // Only a list every row satisfies makes an enum: a `NOT VALID` constraint leaves
        // older rows unchecked, a `NOT ENFORCED` one (PostgreSQL 18) checks nothing
        let enforced = if catalog.server_version >= 180000 {
            " AND con.conenforced"
        } else {
            ""
        };
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT n.nspname::text, c.relname::text, a.attname::text, pg_get_constraintdef(con.oid)
                     FROM pg_constraint con
                     JOIN pg_class c ON c.oid = con.conrelid
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                     JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = con.conkey[1]
                     WHERE n.nspname = ANY($1) AND con.contype = 'c' AND cardinality(con.conkey) = 1
                       AND con.convalidated{enforced}
                     ORDER BY n.nspname, c.relname, con.conname"
                ),
                &[&schemas_vec],
            )
            .await?;
        for row in rows {
            let (schema, table, column, def): (String, String, String, String) =
                (row.get(0), row.get(1), row.get(2), row.get(3));
            let Some(values) = parse_check_value_list(&def) else {
                continue;
            };
            let Some(col) = catalog
                .tables
                .iter_mut()
                .find(|t| t.schema == schema && t.name == table)
                .and_then(|t| t.columns.iter_mut().find(|c| c.name == column))
            else {
                continue;
            };
            let text_like = matches!(
                col.pg_type,
                PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext
            );
            if !text_like || col.enum_key.is_some() {
                continue;
            }
            let key = format!("check:{table}.{column}");
            col.enum_key = Some(key.clone());
            catalog.enums.push(super::models::EnumTypeMetadata {
                key,
                config_name: format!("{table}.{column}"),
                type_name: String::new(),
                description: None,
                values: enum_values(values),
            });
        }
        Ok(())
    }

    /// Enum and composite types used by columns, directly or through composite attributes
    /// (in any schema), and the domains columns are declared with.
    async fn inspect_value_types(&self, catalog: &mut DatabaseCatalog) -> Result<(), CatalogError> {
        let mut domains: Vec<(String, PgType)> = Vec::new();
        for col in catalog.tables.iter().flat_map(|t| &t.columns) {
            if let Some(key) = &col.domain {
                if !domains.iter().any(|(k, _)| k == key) {
                    domains.push((key.clone(), col.pg_type.clone()));
                }
            }
        }
        if !domains.is_empty() {
            let keys: Vec<&str> = domains.iter().map(|(k, _)| k.as_str()).collect();
            let rows = self
                .client
                .query(
                    "SELECT format('%I.%I', n.nspname, t.typname), t.typname::text, obj_description(t.oid, 'pg_type')
                     FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                     WHERE t.typtype = 'd' AND format('%I.%I', n.nspname, t.typname) = ANY($1)
                     ORDER BY 1",
                    &[&keys],
                )
                .await?;
            for row in rows {
                let key: String = row.get(0);
                let base = domains
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, t)| t.clone())
                    .unwrap_or(PgType::Text);
                catalog.domains.push(super::models::DomainMetadata {
                    key,
                    config_name: row.get(1),
                    type_name: String::new(),
                    description: row.get(2),
                    base,
                });
            }
        }
        let mut composite_keys: Vec<String> = Vec::new();
        let mut enum_keys: Vec<String> = Vec::new();
        fn collect(pg_type: &PgType, composites: &mut Vec<String>, enums: &mut Vec<String>) {
            match pg_type {
                PgType::Composite(k) if !composites.contains(k) => composites.push(k.clone()),
                PgType::Enum(k) if !enums.contains(k) => enums.push(k.clone()),
                PgType::Array(elem) => collect(elem, composites, enums),
                _ => {}
            }
        }
        for col in catalog.tables.iter().flat_map(|t| &t.columns) {
            collect(&col.pg_type, &mut composite_keys, &mut enum_keys);
        }
        for f in &catalog.functions {
            let returned = match &f.returns {
                super::models::FunctionReturn::Value(t) => Some(t),
                _ => None,
            };
            let types = f
                .arguments
                .iter()
                .map(|a| &a.pg_type)
                .chain(f.return_columns.iter().map(|c| &c.pg_type))
                .chain(returned);
            for t in types {
                collect(t, &mut composite_keys, &mut enum_keys);
            }
        }

        if !composite_keys.is_empty() {
            let query = format!(
                "SELECT format('%I.%I', tn.nspname, ct.typname), ct.typname::text, obj_description(ct.oid, 'pg_type'),
                        a.attname::text, col_description(c.oid, a.attnum), {TYPE_COLUMNS},
                        CASE WHEN a.atttypmod < 0 AND t.typtype = 'd' THEN t.typtypmod ELSE a.atttypmod END
                 FROM pg_type ct
                 JOIN pg_namespace tn ON tn.oid = ct.typnamespace
                 JOIN pg_class c ON c.oid = ct.typrelid AND c.relkind = 'c'
                 JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
                 JOIN pg_type t ON t.oid = a.atttypid
                 {TYPE_JOINS}
                 WHERE ct.typtype = 'c' AND tn.nspname NOT IN ('pg_catalog', 'information_schema')
                 ORDER BY 1, a.attnum"
            );
            let mut all: Vec<super::models::CompositeTypeMetadata> = Vec::new();
            for row in self.client.query(&query, &[]).await? {
                let key: String = row.get(0);
                if all.last().is_none_or(|c| c.key != key) {
                    all.push(super::models::CompositeTypeMetadata {
                        key: key.clone(),
                        config_name: row.get(1),
                        type_name: String::new(),
                        description: row.get(2),
                        attributes: Vec::new(),
                    });
                }
                let pg_type = type_from_row(&row, 5);
                all.last_mut()
                    .unwrap()
                    .attributes
                    .push(super::models::CompositeAttribute {
                        name: row.get(3),
                        enum_key: enum_key_of(&pg_type),
                        limits: TypeLimits::from_typmod(&pg_type, row.get(11)),
                        pg_type,
                        field_name: String::new(),
                        description: row.get(4),
                    });
            }
            // Keep the types reachable from columns (composite attributes may nest others)
            let mut i = 0;
            while i < composite_keys.len() {
                if let Some(c) = all.iter().find(|c| c.key == composite_keys[i]) {
                    for a in &c.attributes {
                        collect(&a.pg_type, &mut composite_keys, &mut enum_keys);
                    }
                }
                i += 1;
            }
            catalog.composites = all
                .into_iter()
                .filter(|c| composite_keys.contains(&c.key))
                .collect();
        }

        if !enum_keys.is_empty() {
            let rows = self
                .client
                .query(
                    "SELECT format('%I.%I', n.nspname, t.typname), t.typname::text, obj_description(t.oid, 'pg_type'),
                            array_agg(e.enumlabel::text ORDER BY e.enumsortorder)
                     FROM pg_type t
                     JOIN pg_namespace n ON n.oid = t.typnamespace
                     JOIN pg_enum e ON e.enumtypid = t.oid
                     WHERE format('%I.%I', n.nspname, t.typname) = ANY($1)
                     GROUP BY t.oid, n.nspname, t.typname
                     ORDER BY 1",
                    &[&enum_keys],
                )
                .await?;
            for row in rows {
                catalog.enums.push(super::models::EnumTypeMetadata {
                    key: row.get(0),
                    config_name: row.get(1),
                    type_name: String::new(),
                    description: row.get(2),
                    values: enum_values(row.get(3)),
                });
            }
        }
        Ok(())
    }

    /// Functions, each a root field (see [`super::models::FunctionReturn`] for the shapes they
    /// return). A function whose first argument is a row of a table is a field of that
    /// table's type instead (a computed column), unless it is `VOLATILE`. Skipped:
    /// procedures, aggregates and window functions, variadic functions, functions the role
    /// cannot execute, and functions taking a pseudo-type or (otherwise) a table row.
    async fn inspect_functions(
        &self,
        schema_names: &[&str],
        extensions: &ExtensionFilter,
        catalog: &mut DatabaseCatalog,
    ) -> Result<(), CatalogError> {
        let schemas_vec: Vec<String> = schema_names.iter().map(|s| s.to_string()).collect();
        let filter = format!(
            "n.nspname = ANY($1) AND p.prokind = 'f' AND p.provariadic = 0
             AND has_function_privilege(p.oid, 'EXECUTE') AND {}",
            not_extension_owned("pg_proc", "p.oid")
        );
        let fn_query = format!(
            "SELECT p.oid, p.proname::text, p.pronargs, p.pronargdefaults, p.proretset, p.provolatile::text,
                    t.typtype::text, t.typname::text, {TYPE_COLUMNS},
                    rn.nspname::text, rc.relname::text, obj_description(p.oid, 'pg_proc'),
                    ern.nspname::text, erc.relname::text, n.nspname::text
             FROM pg_proc p
             JOIN pg_namespace n ON p.pronamespace = n.oid
             JOIN pg_type t ON t.oid = p.prorettype
             {TYPE_JOINS}
             LEFT JOIN pg_class rc ON rc.oid = t.typrelid AND rc.relkind IN ('r', 'v', 'm', 'p', 'f')
             LEFT JOIN pg_namespace rn ON rn.oid = rc.relnamespace
             LEFT JOIN pg_class erc ON erc.oid = et.typrelid AND erc.relkind IN ('r', 'v', 'm', 'p', 'f')
             LEFT JOIN pg_namespace ern ON ern.oid = erc.relnamespace
             WHERE {filter}
             ORDER BY p.proname, p.oid"
        );
        // Arguments in declaration order, `OUT`/`TABLE` columns included
        let arg_query = format!(
            "SELECT p.oid, coalesce(p.proargnames[a.ord], ''), coalesce(p.proargmodes[a.ord]::text, 'i'),
                    t.typtype::text, aec.oid IS NOT NULL OR ac.relnamespace <> n.oid,
                    {TYPE_COLUMNS}, CASE WHEN ac.relnamespace = n.oid THEN ac.relname::text END
             FROM pg_proc p
             JOIN pg_namespace n ON p.pronamespace = n.oid
             CROSS JOIN LATERAL unnest(coalesce(p.proallargtypes, p.proargtypes::oid[])) WITH ORDINALITY AS a(oid, ord)
             JOIN pg_type t ON t.oid = a.oid
             {TYPE_JOINS}
             LEFT JOIN pg_class ac ON ac.oid = t.typrelid AND ac.relkind IN ('r', 'v', 'm', 'p', 'f')
             LEFT JOIN pg_class aec ON aec.oid = et.typrelid AND aec.relkind IN ('r', 'v', 'm', 'p', 'f')
             WHERE {filter}
             ORDER BY p.oid, a.ord"
        );
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 3] =
            [&schemas_vec, &extensions.all, &extensions.names];
        let mut args_by_fn: HashMap<u32, Vec<tokio_postgres::Row>> = HashMap::new();
        for row in self.client.query(&arg_query, &params).await? {
            args_by_fn.entry(row.get(0)).or_default().push(row);
        }

        let mut seen = std::collections::HashSet::new();
        for r in self.client.query(&fn_query, &params).await? {
            let oid: u32 = r.get(0);
            let func_name: String = r.get(1);
            let fn_schema: String = r.get(19);
            let pronargs: i16 = r.get(2);
            let pronargdefaults: i16 = r.get(3);
            let mut returns_set: bool = r.get(4);
            let mut returns_array = false;
            let volatile = r.get::<_, String>(5) == "v";
            let (ret_typtype, ret_typname): (String, String) = (r.get(6), r.get(7));
            let fn_description: Option<String> = r.get(16);

            let mut in_args = Vec::new();
            let mut ret_cols = Vec::new();
            let mut representable = true;
            let mut row_table: Option<String> = None;
            let mut table_columns = false;
            for (i, arg) in args_by_fn.get(&oid).into_iter().flatten().enumerate() {
                let name = match arg.get::<_, String>(1) {
                    n if !n.is_empty() => n,
                    _ => format!("arg{}", i + 1),
                };
                let mode: String = arg.get(2);
                let pseudo = arg.get::<_, String>(3) == "p";
                // Rows as arrays, or of tables in other schemas, have no input type
                let foreign_rows: bool = arg.get::<_, Option<bool>>(4).unwrap_or(false);
                let pg_type = type_from_row(arg, 5);
                let row_of: Option<String> = arg.get(11);
                if matches!(mode.as_str(), "i" | "b") {
                    // The first argument may take the row of a table: a computed column
                    match row_of {
                        Some(table) if in_args.is_empty() && mode == "i" => row_table = Some(table),
                        Some(_) => representable = false,
                        None => representable &= !pseudo && !foreign_rows,
                    }
                    let in_idx = in_args.len() as i16;
                    let has_default = pronargdefaults > 0 && in_idx >= (pronargs - pronargdefaults);
                    in_args.push(super::models::FunctionArgument {
                        name: name.clone(),
                        pg_type: pg_type.clone(),
                        has_default,
                    });
                }
                table_columns |= mode == "t";
                if matches!(mode.as_str(), "o" | "b" | "t") {
                    ret_cols.push(super::models::FunctionColumn { name, pg_type });
                }
            }
            let returns = match (
                r.get::<_, Option<String>>(15),
                ret_typtype.as_str(),
                ret_typname.as_str(),
            ) {
                (Some(table), _, _) => super::models::FunctionReturn::Table {
                    schema: r.get(14),
                    table,
                },
                // An array of rows is its rows (a set of arrays of rows has no GraphQL shape)
                _ if r.get::<_, Option<String>>(18).is_some() => {
                    if returns_set {
                        continue;
                    }
                    (returns_set, returns_array) = (true, true);
                    super::models::FunctionReturn::Table {
                        schema: r.get(17),
                        table: r.get(18),
                    }
                }
                (None, "p", "void") => super::models::FunctionReturn::Void,
                (None, "p", "record") if !ret_cols.is_empty() => {
                    super::models::FunctionReturn::Record
                }
                // `RETURNS TABLE` names its columns, even just one
                (None, _, _) if table_columns => super::models::FunctionReturn::Record,
                // Triggers, handlers, polymorphic and anonymous record results have no type
                (None, "p", _) => continue,
                // One `OUT` parameter: the function returns its value
                (None, _, _) => super::models::FunctionReturn::Value(type_from_row(&r, 8)),
            };
            // A computed column must not change data, and its table must be exposed
            if row_table
                .as_ref()
                .is_some_and(|t| volatile || catalog.find_table(&fn_schema, t).is_none())
            {
                continue;
            }
            // Overloads share a name: the first is exposed
            if !representable || !seen.insert((fn_schema.clone(), func_name.clone())) {
                continue;
            }
            if returns != super::models::FunctionReturn::Record {
                ret_cols.clear();
            }

            // Mount a function returning its own columns on a table when its first argument
            // references that table's primary key: same type, named `{table}_id` /
            // `{singular}_id` (or a one-letter `{initial}_id`), and the table appears in the
            // function name.
            let mountable =
                returns == super::models::FunctionReturn::Record && returns_set && !volatile;
            let parent = in_args.first().filter(|_| mountable).and_then(|arg| {
                let arg_name = arg.name.to_lowercase();
                let func_lower = func_name.to_lowercase();
                catalog.tables.iter().find_map(|t| {
                    let pk = match t.primary_key.as_slice() {
                        [pk] => pk,
                        _ => return None,
                    };
                    if t.get_column(pk)?.pg_type != arg.pg_type {
                        return None;
                    }
                    let t_name = t.name.to_lowercase();
                    let t_singular = super::models::table_singular_snake(&t.name).to_lowercase();
                    if !(func_lower.contains(&t_singular) || func_lower.contains(&t_name)) {
                        return None;
                    }
                    let prefix = arg_name
                        .strip_suffix("_id")
                        .or_else(|| arg_name.strip_suffix("id"))?;
                    let prefix = prefix.trim_matches('_');
                    let matches_name = prefix == t_singular
                        || prefix == t_name
                        || prefix == t_singular.replace('_', "")
                        || (prefix.len() == 1 && t_singular.starts_with(prefix));
                    matches_name.then(|| {
                        (
                            (t.schema.clone(), t.name.clone()),
                            vec![(arg.name.clone(), pk.clone())],
                        )
                    })
                })
            });
            let (parent_schema, parent_table, parent_arguments) = match (parent, &row_table) {
                (_, Some(table)) => (None, Some(table.clone()), Vec::new()),
                (Some(((schema, table), args)), None) => {
                    ((schema != fn_schema).then_some(schema), Some(table), args)
                }
                (None, None) => (None, None, Vec::new()),
            };
            let mounted_field_name = match &row_table {
                Some(table) => extract_computed_field_name(&func_name, table),
                None => String::new(),
            };

            catalog.functions.push(super::models::FunctionMetadata {
                schema: fn_schema,
                name: func_name,
                arguments: in_args,
                return_columns: ret_cols,
                returns,
                returns_set,
                returns_array,
                volatile,
                parent_schema,
                parent_table,
                description: fn_description,
                field_name: String::new(),
                omit_root: row_table.is_some(),
                mounted_field_name,
                parent_arguments,
                row_argument: row_table.is_some(),
                deprecation: None,
            });
        }
        Ok(())
    }

    /// Exclusive arcs: a `CHECK (num_nonnulls(a_id, b_id, ...) = 1)` over single-column
    /// foreign keys, each pointing at a different table.
    async fn inspect_exclusive_arcs(
        &self,
        schema_names: &[&str],
        catalog: &mut DatabaseCatalog,
    ) -> Result<(), CatalogError> {
        let schemas_vec: Vec<String> = schema_names.iter().map(|s| s.to_string()).collect();
        let chk_query = "
            SELECT c.conname, cl.relname, pg_get_constraintdef(c.oid) AS def, n.nspname::text
            FROM pg_constraint c
            JOIN pg_class cl ON cl.oid = c.conrelid
            JOIN pg_namespace n ON cl.relnamespace = n.oid
            WHERE n.nspname = ANY($1) AND c.contype = 'c'
            ORDER BY cl.relname, c.conname;
        ";
        for r in self.client.query(chk_query, &[&schemas_vec]).await? {
            let conname: String = r.get(0);
            let table_name: String = r.get(1);
            let def: String = r.get(2);
            let table_schema: String = r.get(3);

            if !(def.contains("num_nonnulls(") && def.contains("= 1")) {
                continue;
            }
            let Some(start) = def.find("num_nonnulls(") else {
                continue;
            };
            let inner = &def[start + 13..];
            let Some(end) = inner.find(')') else { continue };
            let target_cols: Vec<String> = inner[..end]
                .split(',')
                .map(|s| {
                    s.trim()
                        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                        .to_string()
                })
                .collect();

            let Some(table_meta) = catalog.find_table(&table_schema, &table_name) else {
                continue;
            };
            let Some(parent_fk) = table_meta
                .foreign_keys
                .iter()
                .find(|fk| fk.columns.iter().all(|c| !target_cols.contains(c)))
            else {
                continue;
            };
            let mut arc_columns = Vec::new();
            let mut target_tables = Vec::new();
            let mut target_schemas = Vec::new();
            let mut target_key_columns = Vec::new();
            for tc in &target_cols {
                let fk = table_meta
                    .foreign_keys
                    .iter()
                    .find(|k| k.single().is_some_and(|(c, _)| c == tc));
                if let Some(fk) = fk {
                    arc_columns.push(tc.clone());
                    target_tables.push(fk.foreign_table.clone());
                    target_schemas.push(fk.foreign_schema.clone());
                    target_key_columns.push(fk.foreign_columns[0].clone());
                }
            }

            if !target_tables.is_empty() {
                let union_name = format!("{}Target", super::models::to_pascal_case(&table_name));
                let arc = super::models::ExclusiveArcMetadata {
                    constraint_name: conname,
                    table_name: table_name.clone(),
                    parent_column: parent_fk.columns[0].clone(),
                    parent_table: parent_fk.foreign_table.clone(),
                    target_columns: arc_columns,
                    target_tables,
                    target_schemas,
                    target_key_columns,
                    target_type_names: Vec::new(),
                    union_name,
                };
                catalog.exclusive_arcs.push(arc);
            }
        }
        Ok(())
    }

    /// Search indexes: `pg_trgm` (fuzzy), `pgvector` (semantic), `bm25` (lexical), `tsvector`
    /// documents (full text: GIN/GiST indexes on a `tsvector` column or on a `to_tsvector(...)`
    /// expression) and PostGIS geometries (GiST/SP-GiST: nearest first).
    async fn inspect_search_indexes(
        &self,
        schema_names: &[&str],
        catalog: &mut DatabaseCatalog,
    ) -> Result<(), CatalogError> {
        let schemas_vec: Vec<String> = schema_names.iter().map(|s| s.to_string()).collect();
        let search_idx_query = "
            SELECT
              c.relname AS table_name,
              i.relname AS index_name,
              coalesce(att.attname, '') AS column_name,
              am.amname AS access_method,
              op.opcname AS operator_class,
              pg_get_indexdef(x.indexrelid, 1, true) AS key_expression,
              n.nspname::text AS table_schema
            FROM pg_index x
            JOIN pg_class c ON c.oid = x.indrelid
            JOIN pg_class i ON i.oid = x.indexrelid
            JOIN pg_am am ON am.oid = i.relam
            JOIN pg_opclass op ON op.oid = x.indclass[0]
            LEFT JOIN pg_attribute att ON att.attrelid = c.oid AND att.attnum = x.indkey[0]
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = ANY($1) AND x.indisvalid
              AND (
                (am.amname IN ('gin', 'gist') AND op.opcname IN ('gin_trgm_ops', 'gist_trgm_ops'))
                OR (am.amname IN ('hnsw', 'ivfflat') AND op.opcname LIKE 'vector_%')
                -- pgvectorscale (diskann) and VectorChord (vchordrq, vchordg), when their
                -- integrations are on: the distance classes, read with pgvector's operators
                OR (am.amname = ANY($2) AND op.opcname IN ('vector_l2_ops', 'vector_cosine_ops', 'vector_ip_ops'))
                OR (am.amname = 'bm25')
                OR (am.amname IN ('gin', 'gist') AND op.opcname IN ('tsvector_ops', 'gist_tsvector_ops'))
                -- PGroonga, when its integration is on: full-text indexes on text
                OR ($3 AND am.amname = 'pgroonga' AND op.opcname IN (
                  'pgroonga_text_full_text_search_ops_v2',
                  'pgroonga_varchar_full_text_search_ops_v2',
                  'pgroonga_text_array_full_text_search_ops_v2'))
                OR (am.amname IN ('gist', 'spgist') AND (op.opcname LIKE '%geometry_ops%' OR op.opcname LIKE '%geography_ops%'))
              )
            ORDER BY c.relname, i.relname;
        ";
        use super::integrations::Integration;
        let mut vector_methods: Vec<&str> = Vec::new();
        if catalog.integration(Integration::Pgvectorscale).is_some() {
            vector_methods.push("diskann");
        }
        if catalog.integration(Integration::VectorChord).is_some() {
            vector_methods.extend(["vchordrq", "vchordg"]);
        }
        let pgroonga_schema = catalog
            .integration(Integration::PgRoonga)
            .map(|i| i.schema.clone());
        for r in self
            .client
            .query(
                search_idx_query,
                &[&schemas_vec, &vector_methods, &pgroonga_schema.is_some()],
            )
            .await?
        {
            let table_name: String = r.get(0);
            let index_name: String = r.get(1);
            let column_name: String = r.get(2);
            let access_method: String = r.get(3);
            let operator_class: String = r.get(4);
            let key_expression: String = r.get(5);
            let table_schema: String = r.get(6);
            let Some(table) = catalog.find_table(&table_schema, &table_name) else {
                continue;
            };

            let mut columns = Vec::new();
            let kind = if access_method == "pgroonga" {
                // The text column, or an expression over columns (`ARRAY[title, body]`)
                let document = if column_name.is_empty() {
                    columns = sql_identifiers(&key_expression)
                        .into_iter()
                        .filter(|i| table.get_column(i).is_some())
                        .collect();
                    columns.dedup();
                    key_expression.clone()
                } else {
                    quote_ident(&column_name)
                };
                let source = if operator_class.contains("array") {
                    format!("array_to_string({document}, ' ')")
                } else {
                    document.clone()
                };
                super::models::SearchIndexKind::FullText {
                    document,
                    config: None,
                    source: Some(source),
                    pgroonga: Some(super::models::Pgroonga {
                        schema: pgroonga_schema.clone().unwrap_or_default(),
                    }),
                }
            } else if access_method == "bm25" {
                super::models::SearchIndexKind::Bm25
            } else if access_method == "hnsw"
                || access_method == "ivfflat"
                || operator_class.starts_with("vector_")
            {
                super::models::SearchIndexKind::Vector {
                    opclass: operator_class.clone(),
                }
            } else if operator_class.contains("tsvector") {
                // A document column (its generation expression tells how it is built), or an
                // expression over columns
                let built_from = if column_name.is_empty() {
                    columns = sql_identifiers(&key_expression)
                        .into_iter()
                        .filter(|i| table.get_column(i).is_some())
                        .collect();
                    columns.dedup();
                    Some(key_expression.clone())
                } else {
                    table
                        .get_column(&column_name)
                        .and_then(|c| c.generation.clone())
                };
                let parsed = built_from.as_deref().and_then(parse_to_tsvector);
                let document = if column_name.is_empty() {
                    key_expression.clone()
                } else {
                    quote_ident(&column_name)
                };
                super::models::SearchIndexKind::FullText {
                    document,
                    config: parsed.as_ref().and_then(|(config, _)| config.clone()),
                    source: parsed.map(|(_, source)| source),
                    pgroonga: None,
                }
            } else if operator_class.contains("geometry") || operator_class.contains("geography") {
                if column_name.is_empty() {
                    continue;
                }
                super::models::SearchIndexKind::Spatial
            } else {
                super::models::SearchIndexKind::FuzzyTrgm
            };

            catalog
                .search_indexes
                .push(super::models::SearchIndexMetadata {
                    table_schema,
                    table_name,
                    index_name,
                    column_name,
                    access_method,
                    operator_class,
                    kind,
                    columns,
                });
        }
        Ok(())
    }

    /// Detects row polymorphism in `schema_name`:
    ///
    /// - **Single Table Inheritance**: a discriminator-named column (`type`, `kind`, ...) whose
    ///   values are closed by a single-column CHECK (`IN (...)` / `= ANY (ARRAY[...])`) or by an
    ///   enum type. Each value becomes a variant type.
    /// - **Table-per-type via `INHERITS`**: every non-partition descendant from `pg_inherits`,
    ///   joined back to the parent on the parent's primary key.
    /// - **Table-per-type via shared primary key**: tables whose single-column PK is also an FK to
    ///   another table's PK. Only treated as subtypes when the parent has at least two such
    ///   children or a discriminator column, so plain 1:1 extension tables stay untouched.
    ///
    /// Names are finalised later by `DatabaseCatalog::resolve_polymorphic_names`.
    async fn inspect_polymorphism(&self, schema_names: &[&str], catalog: &mut DatabaseCatalog) {
        let schemas_vec: Vec<String> = schema_names.iter().map(|s| s.to_string()).collect();
        const DISCRIMINATOR_NAMES: &[&str] = &[
            "type",
            "kind",
            "discriminator",
            "entity_type",
            "record_type",
            "resource_type",
        ];
        let discriminators: Vec<String> =
            DISCRIMINATOR_NAMES.iter().map(|s| s.to_string()).collect();

        let sti = |schema: &str, table: &str, column: &str, values: Vec<String>| StiMetadata {
            schema: schema.to_string(),
            table_name: table.to_string(),
            discriminator_column: column.to_string(),
            variants: values
                .into_iter()
                .map(|v| StiVariant {
                    type_name: super::models::graphql_type_name(&v),
                    discriminator_value: v,
                })
                .collect(),
            fallback_type_name: None,
            base_type_name: String::new(),
        };

        // Sub-pass A: single-column CHECK constraints on discriminator columns
        let chk_query = "
            SELECT c.relname, a.attname, pg_get_constraintdef(con.oid), n.nspname::text
            FROM pg_constraint con
            JOIN pg_class c ON c.oid = con.conrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = con.conkey[1]
            WHERE n.nspname = ANY($1)
              AND con.contype = 'c'
              AND cardinality(con.conkey) = 1
              AND c.relkind IN ('r', 'p')
              AND a.attname = ANY($2)
            ORDER BY c.relname, con.conname;
        ";
        if let Ok(rows) = self
            .client
            .query(chk_query, &[&schemas_vec, &discriminators])
            .await
        {
            for r in rows {
                let table: String = r.get(0);
                let column: String = r.get(1);
                let def: String = r.get(2);
                let schema: String = r.get(3);
                if let Some(values) = parse_check_value_list(&def) {
                    catalog.sti.push(sti(&schema, &table, &column, values));
                }
            }
        }

        // Sub-pass B: enum-typed discriminator columns
        let enum_query = "
            SELECT c.relname, a.attname, array_agg(e.enumlabel::text ORDER BY e.enumsortorder), n.nspname::text
            FROM pg_attribute a
            JOIN pg_class c ON c.oid = a.attrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_enum e ON e.enumtypid = a.atttypid
            WHERE n.nspname = ANY($1)
              AND c.relkind IN ('r', 'p')
              AND a.attnum > 0
              AND NOT a.attisdropped
              AND a.attname = ANY($2)
            GROUP BY n.nspname, c.relname, a.attname, a.attnum
            ORDER BY n.nspname, c.relname, a.attnum;
        ";
        if let Ok(rows) = self
            .client
            .query(enum_query, &[&schemas_vec, &discriminators])
            .await
        {
            for r in rows {
                let table: String = r.get(0);
                let column: String = r.get(1);
                let values: Vec<String> = r.get(2);
                let schema: String = r.get(3);
                // A CHECK on the column is narrower than its enum, so it takes precedence
                if !catalog
                    .sti
                    .iter()
                    .any(|s| s.schema == schema && s.table_name == table)
                {
                    catalog.sti.push(sti(&schema, &table, &column, values));
                }
            }
        }

        // Sub-pass C: native INHERITS hierarchies (all descendants, deepest first)
        let inherits_query = "
            WITH RECURSIVE tree AS (
                SELECT i.inhparent AS root, i.inhrelid AS child, 1 AS depth
                FROM pg_inherits i
                JOIN pg_class p ON p.oid = i.inhparent
                JOIN pg_class ch ON ch.oid = i.inhrelid
                WHERE p.relkind = 'r' AND NOT ch.relispartition
                UNION ALL
                SELECT t.root, i.inhrelid, t.depth + 1
                FROM tree t
                JOIN pg_inherits i ON i.inhparent = t.child
                JOIN pg_class ch ON ch.oid = i.inhrelid
                WHERE NOT ch.relispartition
            )
            SELECT p.relname, cn.nspname, c.relname, pn.nspname
            FROM tree
            JOIN pg_class p ON p.oid = tree.root
            JOIN pg_namespace pn ON pn.oid = p.relnamespace
            JOIN pg_class c ON c.oid = tree.child
            JOIN pg_namespace cn ON cn.oid = c.relnamespace
            WHERE pn.nspname = ANY($1)
            ORDER BY p.relname, tree.depth DESC, c.relname;
        ";
        if let Ok(rows) = self.client.query(inherits_query, &[&schemas_vec]).await {
            for r in rows {
                let parent: String = r.get(0);
                let child_schema: String = r.get(1);
                let child: String = r.get(2);
                let parent_schema: String = r.get(3);
                // Children inherit the parent's columns, so they join on the parent's key
                let Some(parent_meta) = catalog.find_table(&parent_schema, &parent) else {
                    continue;
                };
                if parent_meta.primary_key.is_empty() {
                    continue;
                }
                let join_columns = parent_meta
                    .primary_key
                    .iter()
                    .map(|pk| (pk.clone(), pk.clone()))
                    .collect();
                push_cti_child(
                    catalog,
                    &parent_schema,
                    &parent,
                    CtiKind::Inherits,
                    CtiChild {
                        type_name: table_to_type_name(&child),
                        schema: child_schema,
                        table: child,
                        join_columns,
                    },
                );
            }
        }

        // Sub-pass D: shared-primary-key subtype tables
        let mut candidates: Vec<(String, String, CtiChild)> = Vec::new();
        for child in &catalog.tables {
            let [pk] = child.primary_key.as_slice() else {
                continue;
            };
            for fk in &child.foreign_keys {
                let Some((column, foreign_column)) = fk.single() else {
                    continue;
                };
                if column != pk
                    || (fk.foreign_schema == child.schema && fk.foreign_table == child.name)
                {
                    continue;
                }
                let parent_pk_matches = catalog
                    .find_table(&fk.foreign_schema, &fk.foreign_table)
                    .is_some_and(|p| p.primary_key == [foreign_column]);
                if parent_pk_matches {
                    candidates.push((
                        fk.foreign_schema.clone(),
                        fk.foreign_table.clone(),
                        CtiChild {
                            schema: child.schema.clone(),
                            table: child.name.clone(),
                            type_name: table_to_type_name(&child.name),
                            join_columns: vec![(pk.clone(), foreign_column.to_string())],
                        },
                    ));
                }
            }
        }
        for (parent_schema, parent, child) in &candidates {
            if catalog
                .find_cti(parent_schema, parent)
                .is_some_and(|c| c.kind == CtiKind::Inherits)
            {
                continue;
            }
            let siblings = candidates
                .iter()
                .filter(|(ps, p, _)| ps == parent_schema && p == parent)
                .count();
            let has_discriminator = catalog.find_table(parent_schema, parent).is_some_and(|t| {
                t.columns
                    .iter()
                    .any(|c| DISCRIMINATOR_NAMES.contains(&c.name.as_str()))
            });
            if siblings >= 2 || has_discriminator {
                push_cti_child(
                    catalog,
                    parent_schema,
                    parent,
                    CtiKind::SharedPrimaryKey,
                    child.clone(),
                );
            }
        }
    }
}

/// Applies the GraphQL-facing parts of `config` (names, descriptions, omissions) to a catalog
/// from [`CatalogInspector::inspect_base`] and resolves the remaining names.
pub fn finalize(
    mut catalog: DatabaseCatalog,
    config: &Config,
) -> Result<DatabaseCatalog, CatalogError> {
    for t in &mut catalog.tables {
        refresh_key_operations(t);
    }
    catalog.apply_comment_hints();
    catalog.assign_default_names();
    crate::config::apply_presentation(&mut catalog, config).map_err(CatalogError::Config)?;
    // A table the config names was not renamed after all
    let names: std::collections::HashMap<String, String> = catalog
        .tables
        .iter()
        .map(|t| {
            (
                format!("table `{}.{}`", t.schema, t.name),
                t.type_name.clone(),
            )
        })
        .collect();
    catalog
        .renamed_types
        .retain(|r| names.get(&r.object).is_none_or(|name| *name == r.given));
    catalog.prune_hidden_search_indexes();
    catalog.resolve_value_type_names();
    catalog.resolve_polymorphic_names();
    crate::schema::SchemaBuilder::prune_incompatible_cti_children(&mut catalog);
    catalog.invalidate_index();
    catalog.assign_error_codes();
    Ok(catalog)
}

/// Row lookups, updates and deletes target one row by its key: without a primary key a
/// relation only gets list and connection fields (and `create`, when insertable).
fn refresh_key_operations(table: &mut TableMetadata) {
    // `INSERT ... ON CONFLICT DO UPDATE` is not supported on foreign tables
    if table.kind == RelationKind::ForeignTable {
        table.operations.upsert = false;
    }
    let has_key = !table.primary_key.is_empty();
    table.operations.read = has_key && table.operations.read;
    if !has_key {
        table.operations.update = false;
        table.operations.delete = false;
    }
}

/// `(configuration, text)` of a `to_tsvector([config,] text)` expression, as PostgreSQL
/// prints it: `to_tsvector('english'::regconfig, ((title || ' '::text) || body))`.
fn parse_to_tsvector(expr: &str) -> Option<(Option<String>, String)> {
    let inner = expr
        .trim()
        .strip_prefix("to_tsvector(")?
        .strip_suffix(')')?;
    // Split at the first comma outside parentheses and string literals
    let (mut depth, mut in_string) = (0i32, false);
    for (i, c) in inner.char_indices() {
        match c {
            '\'' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth -= 1,
            ',' if !in_string && depth == 0 => {
                return Some((
                    Some(inner[..i].trim().to_string()),
                    inner[i + 1..].trim().to_string(),
                ));
            }
            _ => {}
        }
    }
    (depth == 0).then(|| (None, inner.trim().to_string()))
}

/// Columns of `table` a unique index covers, from its definition (`pg_get_indexdef`): the
/// columns its key parts are, or its expressions read (`lower(email)` covers `email`).
/// `INCLUDE` columns and the predicate are not part of the key.
fn index_columns(definition: &str, table: &TableMetadata) -> Vec<String> {
    let Some(start) = definition.find(" USING ") else {
        return Vec::new();
    };
    let mut key = &definition[start..];
    for end in [" INCLUDE (", " WHERE ", " NULLS NOT DISTINCT"] {
        if let Some(at) = key.find(end) {
            key = &key[..at];
        }
    }
    let mut out: Vec<String> = Vec::new();
    for ident in sql_identifiers(key) {
        if table.get_column(&ident).is_some() && !out.contains(&ident) {
            out.push(ident);
        }
    }
    out
}

/// Identifiers in an SQL fragment: bare words (folded to lower case) and quoted names;
/// string literals are skipped.
fn sql_identifiers(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                while let Some(n) = chars.next() {
                    if n == '\'' && chars.peek() != Some(&'\'') {
                        break;
                    }
                    if n == '\'' {
                        chars.next();
                    }
                }
            }
            '"' => {
                let mut name = String::new();
                while let Some(n) = chars.next() {
                    if n == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            name.push('"');
                            continue;
                        }
                        break;
                    }
                    name.push(n);
                }
                out.push(name);
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut name = c.to_lowercase().to_string();
                while let Some(&n) = chars.peek() {
                    if n.is_alphanumeric() || n == '_' || n == '$' {
                        name.extend(n.to_lowercase());
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(name);
            }
            _ => {}
        }
    }
    out
}

/// Whether two keys have the same columns, in any order (both enforce the same uniqueness).
fn same_columns(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|c| b.contains(c))
}

/// Column type from its (domain-resolved) `pg_type` name, `typtype` and qualified name.
fn column_type(type_name: &str, typtype: &str, qualified: &str) -> PgType {
    match typtype {
        "c" => PgType::Composite(qualified.to_string()),
        "e" => PgType::Enum(qualified.to_string()),
        _ => PgType::from_sql_type(type_name),
    }
}

/// GraphQL enum values for database labels, unique within the enum.
fn enum_values(labels: Vec<String>) -> Vec<super::models::EnumValue> {
    let mut taken = std::collections::HashSet::new();
    labels
        .into_iter()
        .map(|label| {
            let base = super::models::enum_value_name(&label);
            let mut name = base.clone();
            let mut n = 2;
            while !taken.insert(name.clone()) {
                name = format!("{base}_{n}");
                n += 1;
            }
            super::models::EnumValue { label, name }
        })
        .collect()
}

/// Junction tables: two or more foreign keys and either few columns, a junction-like name or a
/// composite primary key made of foreign key columns. Every pair of their foreign keys becomes
/// a many-to-many relation in both directions.
fn detect_many_to_many(catalog: &mut DatabaseCatalog) {
    let mut found = Vec::new();
    for table in &catalog.tables {
        let fks: Vec<&ForeignKeyMetadata> = table
            .foreign_keys
            .iter()
            .filter(|fk| {
                catalog
                    .find_table(&fk.foreign_schema, &fk.foreign_table)
                    .is_some()
            })
            .collect();
        if fks.len() < 2 {
            continue;
        }
        let name_indicates_junction = [
            "_link",
            "_member",
            "_role",
            "_result",
            "_endpoint",
            "_insurance",
            "_association",
            "_junction",
        ]
        .iter()
        .any(|s| table.name.contains(s));
        let is_lean = table.columns.len() <= 6;
        let composite_pk = table.primary_key.len() >= 2
            && fks
                .iter()
                .filter(|fk| fk.columns.iter().all(|c| table.primary_key.contains(c)))
                .count()
                >= 2;
        if !(is_lean || name_indicates_junction || composite_pk) {
            continue;
        }
        for i in 0..fks.len() {
            for j in (i + 1)..fks.len() {
                let (fk_a, fk_b) = (fks[i], fks[j]);
                if fk_a.foreign_table == fk_b.foreign_table && fk_a.columns == fk_b.columns {
                    continue;
                }
                for (from, to) in [(fk_a, fk_b), (fk_b, fk_a)] {
                    let field_name = super::models::to_camel_case(&to.foreign_table);
                    let via_field_name = format!(
                        "{}Via{}",
                        field_name,
                        super::models::to_pascal_case(&table.name)
                    );
                    found.push(super::models::ManyToManyMetadata {
                        source_schema: from.foreign_schema.clone(),
                        source_table: from.foreign_table.clone(),
                        target_schema: to.foreign_schema.clone(),
                        target_table: to.foreign_table.clone(),
                        junction_table: table.name.clone(),
                        junction_schema: table.schema.clone(),
                        junction_source: from
                            .columns
                            .iter()
                            .cloned()
                            .zip(from.foreign_columns.iter().cloned())
                            .collect(),
                        junction_target: to
                            .columns
                            .iter()
                            .cloned()
                            .zip(to.foreign_columns.iter().cloned())
                            .collect(),
                        field_name,
                        via_field_name,
                    });
                }
            }
        }
    }
    catalog.many_to_many = found;
}

fn extract_computed_field_name(func_name: &str, table_name: &str) -> String {
    let table_prefix = format!("{}_", table_name);
    if let Some(rest) = func_name.strip_prefix(&table_prefix) {
        return super::models::to_camel_case(rest);
    }
    let singular = if let Some(stripped) = table_name.strip_suffix("ies") {
        format!("{}y", stripped)
    } else if let Some(stripped) = table_name.strip_suffix('s') {
        stripped.to_string()
    } else {
        table_name.to_string()
    };
    let singular_prefix = format!("{}_", singular);
    if let Some(rest) = func_name.strip_prefix(&singular_prefix) {
        return super::models::to_camel_case(rest);
    }
    super::models::to_camel_case(func_name)
}

/// Appends `child` to the CTI hierarchy of `parent`, creating the hierarchy if needed.
fn push_cti_child(
    catalog: &mut DatabaseCatalog,
    schema: &str,
    parent: &str,
    kind: CtiKind,
    child: CtiChild,
) {
    match catalog
        .cti
        .iter_mut()
        .find(|c| c.schema == schema && c.parent_table == parent)
    {
        Some(existing) => {
            if !existing
                .children
                .iter()
                .any(|c| c.schema == child.schema && c.table == child.table)
            {
                existing.children.push(child);
            }
        }
        None => catalog.cti.push(CtiMetadata {
            schema: schema.to_string(),
            parent_table: parent.to_string(),
            kind,
            fallback_type_name: String::new(),
            children: vec![child],
            base_type_name: String::new(),
        }),
    }
}

/// Extracts the closed value list of a discriminator CHECK constraint, as printed by
/// `pg_get_constraintdef`. PostgreSQL normalises `col IN ('a', 'b')` to
/// `(col = ANY (ARRAY['a'::text, 'b'::text]))`; OR-chains of equalities are also accepted.
/// Returns `None` for anything that is not a plain positive value list.
fn parse_check_value_list(def: &str) -> Option<Vec<String>> {
    let upper = def.to_uppercase();
    let other_operators = [
        "<", ">", "!=", "~", "NOT ", " AND ", " LIKE ", "SIMILAR", "LENGTH(", "LOWER(", "UPPER(",
    ];
    if other_operators.iter().any(|op| upper.contains(op)) {
        return None;
    }
    if !(upper.contains("= ANY") || upper.contains(" IN (") || upper.contains(" = '")) {
        return None;
    }
    let mut values = Vec::new();
    let mut chars = def.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut literal = String::new();
        loop {
            match chars.next() {
                Some('\'') if chars.peek() == Some(&'\'') => {
                    chars.next();
                    literal.push('\'');
                }
                Some('\'') | None => break,
                Some(other) => literal.push(other),
            }
        }
        if !values.contains(&literal) {
            values.push(literal);
        }
    }
    (values.len() >= 2).then_some(values)
}

#[cfg(test)]
mod tests {
    use super::parse_check_value_list;

    #[test]
    fn parses_to_tsvector_expressions() {
        use super::parse_to_tsvector;
        assert_eq!(
            parse_to_tsvector("to_tsvector('english'::regconfig, ((title || ' '::text) || body))"),
            Some((
                Some("'english'::regconfig".to_string()),
                "((title || ' '::text) || body)".to_string()
            ))
        );
        assert_eq!(
            parse_to_tsvector("to_tsvector(title)"),
            Some((None, "title".to_string()))
        );
        assert_eq!(
            parse_to_tsvector("to_tsvector('simple'::regconfig, 'a,b'::text)").map(|p| p.1),
            Some("'a,b'::text".to_string())
        );
        assert_eq!(
            parse_to_tsvector("setweight(to_tsvector(title), 'A'::\"char\")"),
            None
        );
    }

    #[test]
    fn parses_normalised_in_list() {
        let def = "CHECK ((type = ANY (ARRAY['dog'::text, 'cat'::text])))";
        assert_eq!(
            parse_check_value_list(def),
            Some(vec!["dog".into(), "cat".into()])
        );
    }

    #[test]
    fn parses_varchar_in_list_and_escaped_quotes() {
        let def = "CHECK (((kind)::text = ANY ((ARRAY['it''s'::character varying, 'b'::character varying])::text[])))";
        assert_eq!(
            parse_check_value_list(def),
            Some(vec!["it's".into(), "b".into()])
        );
    }

    #[test]
    fn parses_or_chain() {
        let def = "CHECK (((type = 'a'::text) OR (type = 'b'::text)))";
        assert_eq!(
            parse_check_value_list(def),
            Some(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn rejects_non_list_checks() {
        assert_eq!(parse_check_value_list("CHECK ((type <> 'x'::text))"), None);
        assert_eq!(
            parse_check_value_list("CHECK (((code ~ '^[a-z]+$'::text) OR (code = 'b'::text)))"),
            None
        );
        assert_eq!(
            parse_check_value_list("CHECK (((code ~~ 'a%'::text) OR (code = 'b'::text)))"),
            None
        );
        assert_eq!(parse_check_value_list("CHECK ((length(type) > 2))"), None);
    }
}
