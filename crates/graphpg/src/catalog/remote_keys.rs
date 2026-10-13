//! The `postgres_fdw` integration (see `docs/foreign-data.md`): the keys of the remote tables
//! foreign tables read, from the remote catalog, so they need no declaration in
//! `graphpg.yaml`. Primary keys, unique constraints and foreign keys between remote tables that
//! are both reached through foreign tables.
//!
//! The remote catalog is read over the foreign server itself, through temporary foreign
//! tables on its `pg_catalog`, in a transaction rolled back afterwards: nothing is left behind,
//! and nothing beyond the user mapping graphpg already uses is needed (and the right to create
//! temporary tables, which every role has by default).
use super::models::{
    ConstraintKind, ConstraintMetadata, DatabaseCatalog, ForeignKeyMetadata, RelationKind,
    UniqueKey,
};
use crate::compiler::quote_ident;
use std::collections::HashMap;
use tokio_postgres::Client;

/// A foreign table over a remote table of a `postgres_fdw` server.
struct ForeignTable {
    schema: String,
    name: String,
    server: String,
    remote_schema: String,
    remote_table: String,
    /// Remote column name → local column name.
    columns: HashMap<String, String>,
}

/// A remote constraint, its columns by (remote) name.
struct RemoteConstraint {
    schema: String,
    table: String,
    name: String,
    kind: char,
    columns: Vec<String>,
    /// A foreign key's referenced table and columns.
    references: Option<(String, String, Vec<String>)>,
}

/// Imports the remote keys of the `postgres_fdw` foreign tables in `schema_names`. Keys the
/// tables already have (declared in `graphpg.yaml`) win. A server that cannot be read is
/// reported and skipped.
pub async fn import(
    client: &Client,
    schema_names: &[&str],
    catalog: &mut DatabaseCatalog,
) -> Result<(), tokio_postgres::Error> {
    let schemas: Vec<String> = schema_names.iter().map(|s| s.to_string()).collect();
    let rows = client
        .query(
            "SELECT n.nspname::text, c.relname::text, s.srvname::text,
                    coalesce(ft.ftoptions, '{}')::text[],
                    coalesce((SELECT array_agg(a.attname::text || '=' || coalesce(
                                (SELECT substr(o, 13) FROM unnest(a.attfdwoptions) o WHERE o LIKE 'column\\_name=%'),
                                a.attname::text))
                              FROM pg_attribute a
                              WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped), '{}')
             FROM pg_foreign_table ft
             JOIN pg_class c ON c.oid = ft.ftrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_foreign_server s ON s.oid = ft.ftserver
             JOIN pg_foreign_data_wrapper w ON w.oid = s.srvfdw
             WHERE w.fdwname = 'postgres_fdw' AND n.nspname = ANY($1)
             ORDER BY 1, 2",
            &[&schemas],
        )
        .await?;
    let tables: Vec<ForeignTable> = rows
        .iter()
        .map(|r| {
            let (schema, name): (String, String) = (r.get(0), r.get(1));
            let options: Vec<String> = r.get(3);
            let option = |key: &str| {
                options
                    .iter()
                    .find_map(|o| o.strip_prefix(&format!("{key}=")).map(str::to_string))
            };
            let columns = r
                .get::<_, Vec<String>>(4)
                .into_iter()
                .filter_map(|pair| {
                    let (local, remote) = pair.split_once('=')?;
                    Some((remote.to_string(), local.to_string()))
                })
                .collect();
            ForeignTable {
                remote_schema: option("schema_name").unwrap_or_else(|| schema.clone()),
                remote_table: option("table_name").unwrap_or_else(|| name.clone()),
                server: r.get(2),
                schema,
                name,
                columns,
            }
        })
        .collect();

    let mut servers: Vec<&str> = tables.iter().map(|t| t.server.as_str()).collect();
    servers.dedup();
    for server in servers {
        let on_server: Vec<&ForeignTable> = tables.iter().filter(|t| t.server == server).collect();
        let constraints = match remote_constraints(client, server, &on_server).await {
            Ok(constraints) => constraints,
            Err(err) => {
                tracing::warn!(server, error = %err, "could not read the remote keys of the foreign server; declare them in graphpg.yaml");
                continue;
            }
        };
        apply(catalog, &on_server, &constraints);
    }
    Ok(())
}

/// The primary key, unique and foreign key constraints of the remote tables of `tables`.
async fn remote_constraints(
    client: &Client,
    server: &str,
    tables: &[&ForeignTable],
) -> Result<Vec<RemoteConstraint>, tokio_postgres::Error> {
    // Inside the inspection's transaction (the pruning role's), a savepoint
    let nested = client
        .batch_execute("SAVEPOINT graphpg_remote_keys")
        .await
        .is_ok();
    if !nested {
        client.batch_execute("BEGIN").await?;
    }
    let result = read_remote_constraints(client, server, tables).await;
    client
        .batch_execute(if nested {
            "ROLLBACK TO SAVEPOINT graphpg_remote_keys; RELEASE SAVEPOINT graphpg_remote_keys"
        } else {
            "ROLLBACK"
        })
        .await?;
    result
}

async fn read_remote_constraints(
    client: &Client,
    server: &str,
    tables: &[&ForeignTable],
) -> Result<Vec<RemoteConstraint>, tokio_postgres::Error> {
    let server = quote_ident(server);
    client
        .batch_execute(&format!(
            "CREATE FOREIGN TABLE pg_temp.graphpg_r_constraint (conname text, contype text, conrelid oid, confrelid oid, conkey int2[], confkey int2[])
               SERVER {server} OPTIONS (schema_name 'pg_catalog', table_name 'pg_constraint');
             CREATE FOREIGN TABLE pg_temp.graphpg_r_class (oid oid, relname text, relnamespace oid)
               SERVER {server} OPTIONS (schema_name 'pg_catalog', table_name 'pg_class');
             CREATE FOREIGN TABLE pg_temp.graphpg_r_namespace (oid oid, nspname text)
               SERVER {server} OPTIONS (schema_name 'pg_catalog', table_name 'pg_namespace');
             CREATE FOREIGN TABLE pg_temp.graphpg_r_attribute (attrelid oid, attnum int2, attname text)
               SERVER {server} OPTIONS (schema_name 'pg_catalog', table_name 'pg_attribute');"
        ))
        .await?;
    let names: Vec<&str> = tables.iter().map(|t| t.remote_table.as_str()).collect();
    let schemas: Vec<&str> = tables.iter().map(|t| t.remote_schema.as_str()).collect();
    // One remote query: postgres_fdw sends the join of tables on one server as one
    let rows = client
        .query(
            "SELECT ns.nspname, cl.relname, co.conname, co.contype, co.conrelid, co.confrelid,
                    co.conkey, co.confkey
             FROM pg_temp.graphpg_r_constraint co
             JOIN pg_temp.graphpg_r_class cl ON cl.oid = co.conrelid
             JOIN pg_temp.graphpg_r_namespace ns ON ns.oid = cl.relnamespace
             WHERE co.contype IN ('p', 'u', 'f') AND cl.relname = ANY($1) AND ns.nspname = ANY($2)",
            &[&names, &schemas],
        )
        .await?;
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| {
            let (schema, table): (&str, &str) = (r.get(0), r.get(1));
            tables
                .iter()
                .any(|t| t.remote_schema == schema && t.remote_table == table)
        })
        .collect();
    let mut relations: Vec<u32> = rows.iter().map(|r| r.get::<_, u32>(4)).collect();
    relations.extend(rows.iter().map(|r| r.get::<_, u32>(5)).filter(|o| *o != 0));
    relations.sort_unstable();
    relations.dedup();

    let mut relation_names: HashMap<u32, (String, String)> = HashMap::new();
    for r in client
        .query(
            "SELECT cl.oid, ns.nspname, cl.relname FROM pg_temp.graphpg_r_class cl
             JOIN pg_temp.graphpg_r_namespace ns ON ns.oid = cl.relnamespace
             WHERE cl.oid = ANY($1)",
            &[&relations],
        )
        .await?
    {
        relation_names.insert(r.get(0), (r.get(1), r.get(2)));
    }
    let mut attributes: HashMap<(u32, i16), String> = HashMap::new();
    for r in client
        .query(
            "SELECT attrelid, attnum, attname FROM pg_temp.graphpg_r_attribute
             WHERE attrelid = ANY($1) AND attnum > 0",
            &[&relations],
        )
        .await?
    {
        attributes.insert((r.get(0), r.get(1)), r.get(2));
    }
    let names_of = |relation: u32, numbers: Option<Vec<i16>>| -> Option<Vec<String>> {
        numbers?
            .into_iter()
            .map(|n| attributes.get(&(relation, n)).cloned())
            .collect()
    };

    let mut out = Vec::new();
    for r in rows {
        let kind = r.get::<_, String>(3).chars().next().unwrap_or(' ');
        let (relation, referenced): (u32, u32) = (r.get(4), r.get(5));
        let Some(columns) = names_of(relation, r.get(6)) else {
            continue;
        };
        let references = if kind == 'f' {
            let Some((schema, table)) = relation_names.get(&referenced).cloned() else {
                continue;
            };
            let Some(columns) = names_of(referenced, r.get(7)) else {
                continue;
            };
            Some((schema, table, columns))
        } else {
            None
        };
        out.push(RemoteConstraint {
            schema: r.get(0),
            table: r.get(1),
            name: r.get(2),
            kind,
            columns,
            references,
        });
    }
    Ok(out)
}

/// The local names of remote columns, if every one is a column of the foreign table.
fn local_columns(table: &ForeignTable, remote: &[String]) -> Option<Vec<String>> {
    remote
        .iter()
        .map(|c| table.columns.get(c).cloned())
        .collect()
}

fn apply(
    catalog: &mut DatabaseCatalog,
    tables: &[&ForeignTable],
    constraints: &[RemoteConstraint],
) {
    let mut imported: Vec<String> = Vec::new();
    for constraint in constraints {
        let owners = tables
            .iter()
            .filter(|t| t.remote_schema == constraint.schema && t.remote_table == constraint.table);
        for owner in owners {
            let Some(columns) = local_columns(owner, &constraint.columns) else {
                continue;
            };
            // The referenced remote table, through a foreign table of its own (one in the
            // owner's schema first)
            let target = match &constraint.references {
                None => None,
                Some((schema, table, remote_columns)) => {
                    let mut targets: Vec<&&ForeignTable> = tables
                        .iter()
                        .filter(|t| &t.remote_schema == schema && &t.remote_table == table)
                        .collect();
                    targets.sort_by_key(|t| t.schema != owner.schema);
                    let Some(target) = targets.first() else {
                        continue;
                    };
                    let Some(foreign_columns) = local_columns(target, remote_columns) else {
                        continue;
                    };
                    Some((target.schema.clone(), target.name.clone(), foreign_columns))
                }
            };
            let Some(table) = catalog
                .tables
                .iter_mut()
                .find(|t| t.schema == owner.schema && t.name == owner.name)
            else {
                continue;
            };
            if table.kind != RelationKind::ForeignTable {
                continue;
            }
            let label = format!("{}.{}", table.schema, table.name);
            match (constraint.kind, target) {
                ('p', _) if table.primary_key.is_empty() => {
                    for c in &mut table.columns {
                        c.is_primary_key = columns.contains(&c.name);
                    }
                    table.primary_key = columns.clone();
                    // Remote violations arrive without a constraint name: one of its own
                    // gives them a typed code (as for a key declared in graphpg.yaml)
                    table.constraints.push(ConstraintMetadata {
                        name: format!("{}_pkey", table.name),
                        kind: ConstraintKind::PrimaryKey,
                        columns: columns.clone(),
                        definition: format!(
                            "PRIMARY KEY ({}), of the remote table",
                            columns.join(", ")
                        ),
                        references: None,
                        period: false,
                        code: String::new(),
                        referenced_code: String::new(),
                    });
                    imported.push(format!("{label}: primary key ({})", columns.join(", ")));
                }
                ('u', _)
                    if !table.unique_keys.iter().any(|k| k.columns == columns)
                        && table.primary_key != columns =>
                {
                    table.unique_keys.push(UniqueKey {
                        name: constraint.name.clone(),
                        columns: columns.clone(),
                        is_primary: false,
                    });
                    imported.push(format!("{label}: unique ({})", columns.join(", ")));
                }
                ('f', Some((foreign_schema, foreign_table, foreign_columns))) => {
                    let declared = table.foreign_keys.iter().any(|fk| {
                        fk.constraint_name == constraint.name
                            || (fk.columns == columns
                                && fk.foreign_schema == foreign_schema
                                && fk.foreign_table == foreign_table)
                    });
                    if declared {
                        continue;
                    }
                    imported.push(format!(
                        "{label}: foreign key ({}) -> {foreign_schema}.{foreign_table}",
                        columns.join(", ")
                    ));
                    table.foreign_keys.push(ForeignKeyMetadata {
                        batch: false,
                        constraint_name: constraint.name.clone(),
                        columns,
                        foreign_schema,
                        foreign_table,
                        foreign_columns,
                        is_virtual: true,
                        field_name: None,
                        reverse_field_name: None,
                        omit: false,
                        omit_reverse: false,
                        period: false,
                    });
                }
                _ => {}
            }
        }
    }
    for key in imported {
        tracing::info!(key = %key, "imported a key of a remote table (postgres_fdw)");
    }
}
