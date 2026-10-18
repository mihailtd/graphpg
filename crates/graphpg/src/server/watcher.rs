use crate::catalog::{finalize, CatalogError, CatalogInspector};
use crate::config::{Config, ConfigSchemaGenerator, Mode};
use crate::schema::SchemaBuilder;
use crate::server::ServerState;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_postgres::NoTls;

pub struct WatcherOptions {
    pub database_url: String,
    pub mode: Mode,
    /// Engine config re-read on every reload (and watched for changes), if any.
    pub config_path: Option<std::path::PathBuf>,
}

pub fn start_schema_watcher(options: WatcherOptions, state: Arc<ServerState>) {
    tokio::spawn(async move {
        if let Err(err) = run_watcher_loop(options, state).await {
            tracing::error!(error = %err, "dev mode: the schema watcher stopped");
        }
    });
}

/// DDL notifications (`NOTIFY graphpg_watch` from an event trigger) on a connection of their
/// own: `None` when it cannot connect. The channel closes when the connection fails.
async fn listen_for_ddl(options: &WatcherOptions) -> Option<tokio::sync::mpsc::Receiver<()>> {
    let (client, mut connection) = match tokio_postgres::connect(&options.database_url, NoTls).await
    {
        Ok(connected) => connected,
        Err(e) => {
            tracing::warn!(error = %e, "dev mode: the schema watcher cannot connect; changes are found by polling");
            return None;
        }
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<()>(100);
    tokio::spawn(async move {
        let mut stream = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
        use futures::StreamExt;
        while let Some(msg_result) = stream.next().await {
            match msg_result {
                Ok(tokio_postgres::AsyncMessage::Notification(n)) => {
                    if n.channel() == "graphpg_watch" {
                        let _ = tx.send(()).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "dev mode: the schema watcher's connection failed; changes are found by polling");
                    break;
                }
                _ => {}
            }
        }
    });

    // Install the PostgreSQL event trigger on DDL (ddl_command_end and sql_drop)
    let setup_sql = r#"
        CREATE OR REPLACE FUNCTION _graphpg_watch_notify() RETURNS event_trigger AS $$
        BEGIN
            PERFORM pg_notify('graphpg_watch', 'ddl');
        END;
        $$ LANGUAGE plpgsql;

        DO $$
        BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_event_trigger WHERE evtname = '_graphpg_watch_ddl') THEN
                CREATE EVENT TRIGGER _graphpg_watch_ddl ON ddl_command_end
                EXECUTE FUNCTION _graphpg_watch_notify();
            END IF;
            IF NOT EXISTS (SELECT 1 FROM pg_event_trigger WHERE evtname = '_graphpg_watch_drop') THEN
                CREATE EVENT TRIGGER _graphpg_watch_drop ON sql_drop
                EXECUTE FUNCTION _graphpg_watch_notify();
            END IF;
        END $$;

        LISTEN graphpg_watch;
    "#;

    // Event triggers need a superuser: without one, DDL is found by polling the catalog
    if let Err(e) = client.batch_execute(setup_sql).await {
        tracing::info!(
            reason = %e,
            "dev mode: no DDL event trigger (it needs a superuser); polling the catalog every 2 seconds"
        );
        let _ = client.batch_execute("LISTEN graphpg_watch;").await;
    } else {
        tracing::info!("dev mode: DDL event trigger installed (ddl_command_end, sql_drop)");
    }
    // The connection lives as long as its client: until the watcher drops the channel
    tokio::spawn(async move {
        let client = client;
        until_closed(&client).await;
    });
    Some(rx)
}

/// Waits until `client`'s connection is closed.
async fn until_closed(client: &tokio_postgres::Client) {
    while !client.is_closed() {
        sleep(Duration::from_secs(5)).await;
    }
}

/// The next DDL notification; never, without a listener.
async fn next_notification(rx: &mut Option<tokio::sync::mpsc::Receiver<()>>) -> Option<()> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn run_watcher_loop(
    options: WatcherOptions,
    state: Arc<ServerState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!("dev mode: watching the database schema and the config file for changes");
    let mut rx = listen_for_ddl(&options).await;

    // Watch loop: listens for notifications AND periodically polls catalog state and the
    // config file
    let fingerprint = |state: Arc<ServerState>,
                       schemas: Vec<String>,
                       config: Option<std::path::PathBuf>| async move {
        let modified = config
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
            .map(|t| format!("{t:?}"))
            .unwrap_or_default();
        format!(
            "{}|{modified}",
            query_catalog_fingerprint(&state, &schemas).await
        )
    };
    // The schemas the last load inspected (the config file may change them)
    let mut schemas = load_config(&options)
        .map(|c| c.database.resolved_schemas())
        .unwrap_or_default();
    let current = |schemas: &Vec<String>| {
        fingerprint(state.clone(), schemas.clone(), options.config_path.clone())
    };
    let mut last_class_fingerprint = current(&schemas).await;

    loop {
        tokio::select! {
            notified = next_notification(&mut rx) => {
                if notified.is_none() {
                    // The listener's connection failed: poll until it reconnects
                    rx = None;
                    continue;
                }
                // Debounce multi-statement DDL migrations
                sleep(Duration::from_millis(200)).await;
                // Drain any pending notifications in channel
                if let Some(rx) = rx.as_mut() {
                    while rx.try_recv().is_ok() {}
                }

                match reload_and_regenerate(&options, &state).await {
                    Ok(reloaded) => {
                        schemas = reloaded;
                        last_class_fingerprint = current(&schemas).await;
                    }
                    Err(e) => tracing::error!(error = %e, "dev mode: the schema could not be reloaded after DDL"),
                }
            }
            _ = sleep(Duration::from_secs(2)) => {
                if rx.is_none() {
                    rx = listen_for_ddl(&options).await;
                }
                // Secondary fallback: poll catalog fingerprint
                let current_fingerprint = current(&schemas).await;
                if current_fingerprint != last_class_fingerprint {
                    last_class_fingerprint = current_fingerprint;
                    tracing::debug!("dev mode: the catalog or the config file changed");
                    match reload_and_regenerate(&options, &state).await {
                        Ok(reloaded) if reloaded != schemas => {
                            schemas = reloaded;
                            last_class_fingerprint = current(&schemas).await;
                        }
                        Ok(_) => {}
                        Err(e) => tracing::error!(error = %e, "dev mode: the schema could not be reloaded"),
                    }
                }
            }
        }
    }
}

/// A fingerprint of what the API is built from in `schemas`, compared between polls: a change
/// to a relation, column, default, constraint, index, function (its argument defaults and
/// SQL-standard body too), type or comment changes it. Empty when
/// the database cannot be read.
async fn query_catalog_fingerprint(state: &ServerState, schemas: &[String]) -> String {
    let client = match state.pool.get().await {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    catalog_fingerprint(&client, schemas)
        .await
        .unwrap_or_default()
}

/// A hash of the catalog rows the API is built from in `schemas` (see
/// [`query_catalog_fingerprint`]).
pub async fn catalog_fingerprint(
    client: &tokio_postgres::Client,
    schemas: &[String],
) -> Result<String, tokio_postgres::Error> {
    const SQL: &str = "
WITH ns AS (SELECT oid FROM pg_namespace WHERE nspname = ANY($1)),
rel AS (SELECT oid FROM pg_class WHERE relnamespace IN (SELECT oid FROM ns))
SELECT md5(concat_ws('|',
    (SELECT string_agg(concat_ws(',', oid, relname, relkind, relrowsecurity), ';' ORDER BY oid)
       FROM pg_class WHERE oid IN (SELECT oid FROM rel)),
    (SELECT string_agg(concat_ws(',', attrelid, attnum, attname, atttypid, atttypmod, attnotnull,
                                 atthasdef, attgenerated, attisdropped), ';' ORDER BY attrelid, attnum)
       FROM pg_attribute WHERE attrelid IN (SELECT oid FROM rel) AND attnum > 0),
    (SELECT string_agg(concat_ws(',', oid, conname, contype, pg_get_constraintdef(oid)), ';' ORDER BY oid)
       FROM pg_constraint WHERE connamespace IN (SELECT oid FROM ns)),
    (SELECT string_agg(concat_ws(',', adrelid, adnum, md5(pg_get_expr(adbin, adrelid))), ';' ORDER BY adrelid, adnum)
       FROM pg_attrdef WHERE adrelid IN (SELECT oid FROM rel)),
    (SELECT string_agg(concat_ws(',', indexrelid, indisunique), ';' ORDER BY indexrelid)
       FROM pg_index WHERE indrelid IN (SELECT oid FROM rel)),
    (SELECT string_agg(concat_ws(',', oid, proname, md5(prosrc), provolatile,
                                 pg_get_function_identity_arguments(oid), prorettype, pronargdefaults,
                                 md5(pg_get_expr(proargdefaults, 0)), md5(prosqlbody::text)), ';' ORDER BY oid)
       FROM pg_proc WHERE pronamespace IN (SELECT oid FROM ns)),
    (SELECT string_agg(concat_ws(',', oid, typname, typtype), ';' ORDER BY oid)
       FROM pg_type WHERE typnamespace IN (SELECT oid FROM ns)),
    (SELECT string_agg(concat_ws(',', enumtypid, enumsortorder, enumlabel), ';' ORDER BY enumtypid, enumsortorder)
       FROM pg_enum WHERE enumtypid IN (SELECT oid FROM pg_type WHERE typnamespace IN (SELECT oid FROM ns))),
    (SELECT string_agg(concat_ws(',', objoid, objsubid, md5(description)), ';' ORDER BY objoid, objsubid)
       FROM pg_description WHERE objoid IN (SELECT oid FROM rel)
          OR objoid IN (SELECT oid FROM pg_proc WHERE pronamespace IN (SELECT oid FROM ns))
          OR objoid IN (SELECT oid FROM pg_type WHERE typnamespace IN (SELECT oid FROM ns))),
    (SELECT string_agg(concat_ws(',', oid, polname, polrelid), ';' ORDER BY oid)
       FROM pg_policy WHERE polrelid IN (SELECT oid FROM rel)),
    (SELECT string_agg(concat_ws(',', grantee, table_name, privilege_type), ';' ORDER BY grantee, table_name, privilege_type)
       FROM information_schema.role_table_grants WHERE table_schema = ANY($1))
))";
    let row = client.query_one(SQL, &[&schemas]).await?;
    Ok(row.get::<_, Option<String>>(0).unwrap_or_default())
}

/// The config file (if any) with the selected mode applied, as at startup.
pub fn load_config(
    options: &WatcherOptions,
) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let config = match &options.config_path {
        Some(path) => Config::load_from_file(path, options.mode)?,
        None => Config::defaults(options.mode),
    };
    Ok(config)
}

/// Reloads the schema; returns the schemas it inspected.
async fn reload_and_regenerate(
    options: &WatcherOptions,
    state: &ServerState,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let start = std::time::Instant::now();
    let client = state.pool.get().await?;

    // 1. Re-read the config and re-inspect the catalog
    let config = load_config(options)?;
    // `app_public` when none are configured
    let resolved = config.database.resolved_schemas();
    if resolved.first() != Some(&state.default_schema) {
        tracing::warn!(
            default_schema = %state.default_schema,
            schemas = %resolved.join(","),
            "the config's first schema changed: the default schema changes on restart"
        );
    }
    let inspector = CatalogInspector::new(&client);
    let schemas: Vec<&str> = resolved.iter().map(String::as_str).collect();
    let (base, config_json) = match inspector.inspect_base(&schemas, &config).await {
        Ok(base) => {
            let json = ConfigSchemaGenerator::generate(&base);
            (Ok(base), json)
        }
        // Keep the config's JSON Schema current so the config can be fixed in the editor
        Err(err @ CatalogError::Config(_)) => {
            let plain = inspector.inspect_base(&schemas, &Config::default()).await?;
            (Err(err), ConfigSchemaGenerator::generate(&plain))
        }
        Err(err) => return Err(err.into()),
    };
    if config.watch.export {
        write_file(
            &config.export.config_schema,
            &serde_json::to_string_pretty(&config_json)?,
        )?;
    }
    let new_catalog = finalize(base?, &config)?;

    // 2. Re-build GraphQL Schema
    let new_schema = SchemaBuilder::build(&new_catalog)?;
    let sdl = SchemaBuilder::generate_sdl(&new_catalog);

    // 3. Update in-memory ServerState atomically
    state.reload(new_schema, new_catalog.clone());

    // 4. Write export files to disk
    if config.watch.export {
        write_file(&config.export.graphql, &sdl)?;
        write_file(
            &config.export.openapi,
            &crate::schema_pin::render_openapi(
                &state.current_openapi_document(),
                &config.export.openapi,
            ),
        )?;
        if config.graphql.federation.enabled {
            let fed_sdl = SchemaBuilder::generate_federation_subgraph_sdl(&new_catalog);
            write_file(&config.export.subgraph, &fed_sdl)?;
        }
    }

    tracing::info!(
        elapsed = ?start.elapsed(),
        exported = config.watch.export,
        "dev mode: schema reloaded"
    );

    Ok(config.database.resolved_schemas())
}

fn write_file(path: &str, content: &str) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_config_uses_selected_mode() {
        let dev_options = WatcherOptions {
            database_url: "postgres://localhost/test".to_string(),
            mode: Mode::Dev,
            config_path: None,
        };
        let dev_cfg = load_config(&dev_options).expect("load dev config");
        assert!(dev_cfg.watch.enabled);
        assert!(dev_cfg.graphql.ui);
        assert!(dev_cfg.openapi.ui);

        let prod_options = WatcherOptions {
            database_url: "postgres://localhost/test".to_string(),
            mode: Mode::Prod,
            config_path: None,
        };
        let prod_cfg = load_config(&prod_options).expect("load prod config");
        assert!(!prod_cfg.watch.enabled);
        assert!(!prod_cfg.graphql.ui);
        assert!(!prod_cfg.openapi.ui);
    }

    #[test]
    fn test_load_config_from_file_uses_selected_mode_overlay() {
        use std::io::Write;
        let config_path = std::env::temp_dir().join(format!(
            "graphpg_test_watcher_{}.yaml",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let yaml = r#"
server:
  port: 5000
dev:
  server:
    port: 5001
prod:
  server:
    port: 5002
"#;
        let mut file = std::fs::File::create(&config_path).unwrap();
        file.write_all(yaml.as_bytes()).unwrap();

        let dev_options = WatcherOptions {
            database_url: "postgres://localhost/test".to_string(),
            mode: Mode::Dev,
            config_path: Some(config_path.clone()),
        };
        let dev_cfg = load_config(&dev_options).expect("load dev config");
        assert_eq!(dev_cfg.server.port, 5001);
        assert!(dev_cfg.watch.enabled);

        let prod_options = WatcherOptions {
            database_url: "postgres://localhost/test".to_string(),
            mode: Mode::Prod,
            config_path: Some(config_path.clone()),
        };
        let prod_cfg = load_config(&prod_options).expect("load prod config");
        assert_eq!(prod_cfg.server.port, 5002);
        assert!(!prod_cfg.watch.enabled);

        let _ = std::fs::remove_file(config_path);
    }
}
