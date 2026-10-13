//! The event table, the capture functions and the triggers on the tables (see `docs/events.md`).
//!
//! Events are written by triggers in the transaction of the change: an event exists if and only
//! if its change committed. Inserts and deletes are captured per statement from transition
//! tables, one `INSERT … SELECT` for the whole statement; updates per row, only when the row
//! changed (`WHEN (OLD IS DISTINCT FROM NEW)`) and, with `columns`, when one of them changed.
//! The trigger functions are generic: a trigger passes the name of the event trigger it serves.
//! Rows are stored as they are (`to_jsonb`) and shaped at delivery.
use crate::catalog::{DatabaseCatalog, RelationKind, TableMetadata};
use crate::compiler::quote_ident;
use crate::config::models::{EventOperation, EventTriggersConfig};
use tokio_postgres::Client;

/// Prefix of the names of the triggers graphpg creates on tables.
pub const TRIGGER_PREFIX: &str = "graphpg_event_";

/// A trigger to create on a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableTrigger {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub sql: String,
}

/// The table an event trigger's `table` names: `schema.table`, or a bare name in the default
/// schema first.
pub fn resolve_table<'c>(
    catalog: &'c DatabaseCatalog,
    default_schema: &str,
    name: &str,
) -> Result<&'c TableMetadata, String> {
    let found = match name.split_once('.') {
        Some((schema, table)) => catalog.find_table(schema, table),
        None => catalog.find_table(default_schema, name).or_else(|| {
            let mut all = catalog.tables.iter().filter(|t| t.name == name);
            match (all.next(), all.next()) {
                (Some(t), None) => Some(t),
                _ => None,
            }
        }),
    };
    let table =
        found.ok_or_else(|| format!("no table `{name}`, or several in different schemas"))?;
    if !matches!(
        table.kind,
        RelationKind::Table | RelationKind::PartitionedTable
    ) {
        return Err(format!(
            "`{name}` is not a table: only tables (and partitioned tables) capture changes"
        ));
    }
    Ok(table)
}

/// The triggers the configuration needs, checked against the catalog.
pub fn table_triggers(
    config: &EventTriggersConfig,
    catalog: &DatabaseCatalog,
    default_schema: &str,
) -> Result<Vec<TableTrigger>, Vec<String>> {
    let s = quote_ident(&config.schema);
    let (mut triggers, mut problems) = (Vec::new(), Vec::new());
    for (name, t) in &config.triggers {
        let path = format!("events.triggers.{name}");
        let table = match resolve_table(catalog, default_schema, &t.table) {
            Ok(table) => table,
            Err(err) => {
                problems.push(format!("{path}.table: {err}"));
                continue;
            }
        };
        for c in &t.columns {
            if table.get_column(c).is_none() {
                problems.push(format!(
                    "{path}.columns: no column `{c}` in `{}`",
                    table.name
                ));
            }
        }
        let target = format!(
            "{}.{}",
            quote_ident(&table.schema),
            quote_ident(&table.name)
        );
        let mut operations = t.operations.clone();
        operations.sort();
        operations.dedup();
        for op in operations {
            let trigger = format!("{TRIGGER_PREFIX}{name}_{}", op.as_str());
            let quoted = quote_ident(&trigger);
            let arg = format!("'{name}'");
            let sql = match op {
                EventOperation::Insert => format!(
                    "CREATE OR REPLACE TRIGGER {quoted} AFTER INSERT ON {target} REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION {s}.capture_insert({arg})"
                ),
                EventOperation::Delete => format!(
                    "CREATE OR REPLACE TRIGGER {quoted} AFTER DELETE ON {target} REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION {s}.capture_delete({arg})"
                ),
                EventOperation::Update => {
                    let of = if t.columns.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " OF {}",
                            t.columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ")
                        )
                    };
                    format!(
                        "CREATE OR REPLACE TRIGGER {quoted} AFTER UPDATE{of} ON {target} FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION {s}.capture_update({arg})"
                    )
                }
            };
            triggers.push(TableTrigger {
                schema: table.schema.clone(),
                table: table.name.clone(),
                name: trigger,
                sql,
            });
        }
    }
    if problems.is_empty() {
        Ok(triggers)
    } else {
        Err(problems)
    }
}

/// The event table and functions (idempotent).
pub fn base_sql(config: &EventTriggersConfig, server_version: u32) -> String {
    let s = quote_ident(&config.schema);
    let channel = config.schema.replace('\'', "''");
    // uuidv7 (PostgreSQL 18) is time-ordered: events sort, and index, by creation
    let uuid = if server_version >= 180000 {
        "uuidv7()"
    } else {
        "gen_random_uuid()"
    };
    format!(
        r#"CREATE SCHEMA IF NOT EXISTS {s};
CREATE TABLE IF NOT EXISTS {s}.event (
    id uuid PRIMARY KEY DEFAULT {uuid},
    created_at timestamptz NOT NULL DEFAULT now(),
    trigger_name text NOT NULL,
    schema_name text,
    table_name text,
    op text NOT NULL,
    old jsonb,
    new jsonb,
    payload jsonb,
    scheduled_for timestamptz,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'dead')),
    attempts integer NOT NULL DEFAULT 0,
    deliver_at timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz,
    last_error text
);
COMMENT ON TABLE {s}.event IS 'Events of graphpg event triggers: written with the change that raised them, delivered as webhooks';
CREATE INDEX IF NOT EXISTS event_due ON {s}.event (deliver_at, id) WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS event_done ON {s}.event (created_at) WHERE status <> 'pending';
CREATE UNIQUE INDEX IF NOT EXISTS event_slot ON {s}.event (trigger_name, scheduled_for) WHERE scheduled_for IS NOT NULL;

CREATE OR REPLACE FUNCTION {s}.capture_insert() RETURNS trigger LANGUAGE plpgsql AS $graphpg$
BEGIN
    INSERT INTO {s}.event (trigger_name, schema_name, table_name, op, new)
    SELECT TG_ARGV[0], TG_TABLE_SCHEMA, TG_TABLE_NAME, 'insert', to_jsonb(r) FROM new_rows r;
    IF FOUND THEN PERFORM pg_notify('{channel}', ''); END IF;
    RETURN NULL;
END $graphpg$;

CREATE OR REPLACE FUNCTION {s}.capture_delete() RETURNS trigger LANGUAGE plpgsql AS $graphpg$
BEGIN
    INSERT INTO {s}.event (trigger_name, schema_name, table_name, op, old)
    SELECT TG_ARGV[0], TG_TABLE_SCHEMA, TG_TABLE_NAME, 'delete', to_jsonb(r) FROM old_rows r;
    IF FOUND THEN PERFORM pg_notify('{channel}', ''); END IF;
    RETURN NULL;
END $graphpg$;

CREATE OR REPLACE FUNCTION {s}.capture_update() RETURNS trigger LANGUAGE plpgsql AS $graphpg$
BEGIN
    INSERT INTO {s}.event (trigger_name, schema_name, table_name, op, old, new)
    VALUES (TG_ARGV[0], TG_TABLE_SCHEMA, TG_TABLE_NAME, 'update', to_jsonb(OLD), to_jsonb(NEW));
    PERFORM pg_notify('{channel}', '');
    RETURN NULL;
END $graphpg$;

-- A one-off event of a schedule, delivered at `at`: SELECT {s}.schedule('reminder', now() + interval '1 day', '{{"user": 42}}');
CREATE OR REPLACE FUNCTION {s}.schedule(name text, at timestamptz DEFAULT now(), payload jsonb DEFAULT '{{}}')
RETURNS uuid LANGUAGE plpgsql AS $graphpg$
DECLARE id uuid;
BEGIN
    INSERT INTO {s}.event (trigger_name, op, payload, deliver_at)
    VALUES (name, 'schedule', payload, at) RETURNING event.id INTO id;
    PERFORM pg_notify('{channel}', '');
    RETURN id;
END $graphpg$;
"#
    )
}

/// The whole installation: the base, then the triggers (what `graphpg events sql` prints).
pub fn install_sql(
    config: &EventTriggersConfig,
    catalog: &DatabaseCatalog,
    default_schema: &str,
) -> Result<String, Vec<String>> {
    let mut sql = base_sql(config, catalog.server_version);
    for t in table_triggers(config, catalog, default_schema)? {
        sql.push_str(&t.sql);
        sql.push_str(";\n");
    }
    Ok(sql)
}

/// graphpg's triggers on tables of `schemas` the configuration no longer needs:
/// `(schema, table, trigger)`.
async fn stale_triggers(
    client: &impl tokio_postgres::GenericClient,
    schemas: &[String],
    wanted: &[TableTrigger],
) -> Result<Vec<(String, String, String)>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT n.nspname::text, c.relname::text, t.tgname::text
             FROM pg_trigger t
             JOIN pg_class c ON c.oid = t.tgrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE NOT t.tgisinternal AND n.nspname = ANY($1) AND starts_with(t.tgname, $2)",
            &[&schemas, &TRIGGER_PREFIX],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                r.get::<_, String>(1),
                r.get::<_, String>(2),
            )
        })
        .filter(|(s, t, n)| {
            !wanted
                .iter()
                .any(|w| &w.schema == s && &w.table == t && &w.name == n)
        })
        .collect())
}

/// Creates or updates the installation, and drops graphpg's triggers the configuration no
/// longer names, in one transaction under an advisory lock (instances starting together).
pub async fn install(
    client: &mut Client,
    config: &EventTriggersConfig,
    catalog: &DatabaseCatalog,
    schemas: &[String],
    default_schema: &str,
) -> Result<(), String> {
    let triggers = table_triggers(config, catalog, default_schema).map_err(|p| p.join("\n"))?;
    let err = |e: tokio_postgres::Error| match e.as_db_error() {
        Some(db) => format!("installing event triggers: {}", db.message()),
        None => format!("installing event triggers: {e}"),
    };
    let tx = client.transaction().await.map_err(err)?;
    tx.batch_execute("SELECT pg_advisory_xact_lock(7461000)")
        .await
        .map_err(err)?;
    tx.batch_execute(&base_sql(config, catalog.server_version))
        .await
        .map_err(err)?;
    for t in &triggers {
        tx.batch_execute(&t.sql).await.map_err(err)?;
    }
    for (schema, table, name) in stale_triggers(&tx, schemas, &triggers).await.map_err(err)? {
        tx.batch_execute(&format!(
            "DROP TRIGGER {} ON {}.{}",
            quote_ident(&name),
            quote_ident(&schema),
            quote_ident(&table)
        ))
        .await
        .map_err(err)?;
        tracing::info!(%schema, %table, trigger = %name, "dropped an event trigger the configuration no longer names");
    }
    tx.commit().await.map_err(err)?;
    Ok(())
}

/// What is missing for `install: manual` (empty: installed).
pub async fn check(
    client: &Client,
    config: &EventTriggersConfig,
    catalog: &DatabaseCatalog,
    default_schema: &str,
) -> Result<Vec<String>, String> {
    let triggers = table_triggers(config, catalog, default_schema).map_err(|p| p.join("\n"))?;
    let mut missing = Vec::new();
    let table: Option<String> = client
        .query_one(
            "SELECT to_regclass($1)::text",
            &[&format!("{}.event", quote_ident(&config.schema))],
        )
        .await
        .map_err(|e| e.to_string())?
        .get(0);
    if table.is_none() {
        missing.push(format!("the table {}.event", config.schema));
    }
    for t in &triggers {
        let exists: bool = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_trigger tg JOIN pg_class c ON c.oid = tg.tgrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relname = $2 AND tg.tgname = $3)",
                &[&t.schema, &t.table, &t.name],
            )
            .await
            .map_err(|e| e.to_string())?
            .get(0);
        if !exists {
            missing.push(format!(
                "the trigger {} on {}.{}",
                t.name, t.schema, t.table
            ));
        }
    }
    Ok(missing)
}
