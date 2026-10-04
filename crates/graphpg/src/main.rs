use clap::{Parser, Subcommand};
use graphpg::config::{resolve_mode, Config, Mode, Secrets, DEFAULT_CONFIG_FILE};
use graphpg::server::{create_pool, create_router, Authenticator, ServerState};
use graphpg::{CatalogInspector, SchemaBuilder};
use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "graphpg",
    version,
    about = "Instant GraphQL engine for PostgreSQL"
)]
pub struct Cli {
    /// Path to the YAML configuration file
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Runtime mode: dev or prod
    #[arg(long, value_name = "MODE", global = true)]
    pub mode: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Export the GraphQL SDL schema
    #[command(alias = "export-graphql-schema")]
    ExportGraphql {
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Export the Apollo Federation Subgraph SDL schema
    ExportSubgraph {
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Export the JSON Schema for graphpg.yaml
    ExportConfigSchema {
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Inspect schemas and print advice / optimization suggestions
    Advise,
    /// Schema subcommands
    Schema {
        #[command(subcommand)]
        command: SchemaCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum SchemaCommands {
    /// Export the GraphQL SDL schema
    #[command(alias = "export-schema")]
    ExportGraphql {
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Export the Apollo Federation Subgraph SDL schema
    ExportSubgraph {
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Export the JSON Schema for graphpg.yaml
    ExportConfigSchema {
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

/// Formats the single startup log summary line.
pub fn startup_log_summary(mode: Mode, config: &Config) -> String {
    let on_off = |b: bool| if b { "on" } else { "off" };
    format!(
        "mode={} graphql.ui={} openapi.ui={} watch={} debug.sql={}",
        mode,
        on_off(config.graphql.ui),
        on_off(config.openapi.ui),
        on_off(config.watch.enabled),
        on_off(config.debug.sql)
    )
}

/// Identifies dev features explicitly turned on in production mode to log warnings.
pub fn prod_warnings(mode: Mode, config: &Config) -> Vec<String> {
    let mut warnings = Vec::new();
    if mode == Mode::Prod {
        if config.graphql.ui {
            warnings.push("graphql.ui is enabled in production mode".to_string());
        }
        if config.openapi.ui {
            warnings.push("openapi.ui is enabled in production mode".to_string());
        }
        if config.openapi.spec {
            warnings.push("openapi.spec is enabled in production mode".to_string());
        }
        if config.watch.enabled {
            warnings.push("watch.enabled is enabled in production mode".to_string());
        }
    }
    warnings
}

/// Resolves the config path: `--config <path>` (must exist), else `./graphpg.yaml` if it exists.
fn resolve_config_path(explicit: Option<&Path>) -> Result<Option<PathBuf>, String> {
    match explicit {
        Some(path) => {
            if path.is_file() {
                Ok(Some(path.to_path_buf()))
            } else {
                Err(format!("config not found: {}", path.display()))
            }
        }
        None => {
            let default_path = Path::new(DEFAULT_CONFIG_FILE);
            if default_path.is_file() {
                Ok(Some(default_path.to_path_buf()))
            } else {
                Ok(None)
            }
        }
    }
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM (what container runtimes and orchestrators send).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _ = dotenvy::dotenv();
    let _telemetry = graphpg::telemetry::init();

    let cli = Cli::parse();

    let mode = resolve_mode(
        cli.mode.as_deref(),
        std::env::var("GRAPHPG_MODE").ok().as_deref(),
    )?;

    let secrets = Secrets::from_env()?;

    let config_path = resolve_config_path(cli.config.as_deref())?;
    let config = match &config_path {
        Some(path) => {
            tracing::info!(path = %path.display(), %mode, "loading config");
            Config::load_from_file(path, mode).map_err(|e| e.to_string())?
        }
        None => Config::defaults(mode),
    };

    let pool = create_pool(&secrets.database_url, config.database.pool_size)?;
    let schemas: Vec<&str> = config.database.schemas.iter().map(String::as_str).collect();
    let default_schema = schemas[0].to_string();

    // Check CLI subcommands
    match cli.command {
        Some(Commands::ExportGraphql { out })
        | Some(Commands::Schema {
            command: SchemaCommands::ExportGraphql { out },
        }) => {
            let out_path = out.unwrap_or_else(|| PathBuf::from(&config.export.graphql));
            let client = pool.get().await?;
            let catalog = CatalogInspector::new(&client)
                .inspect_schemas_with_config(&schemas, &config)
                .await?;
            let sdl = SchemaBuilder::generate_sdl(&catalog);

            if let Some(parent) = out_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&out_path, sdl)?;
            println!("✓ Exported GraphQL SDL schema to: {}", out_path.display());
            return Ok(());
        }
        Some(Commands::ExportSubgraph { out })
        | Some(Commands::Schema {
            command: SchemaCommands::ExportSubgraph { out },
        }) => {
            let out_path = out.unwrap_or_else(|| PathBuf::from(&config.export.subgraph));
            let client = pool.get().await?;
            let mut cfg = config.clone();
            cfg.graphql.federation.enabled = true;
            let catalog = CatalogInspector::new(&client)
                .inspect_schemas_with_config(&schemas, &cfg)
                .await?;
            let sdl = SchemaBuilder::generate_federation_subgraph_sdl(&catalog);

            if let Some(parent) = out_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&out_path, sdl)?;
            println!(
                "✓ Exported Apollo Federation Subgraph SDL to: {}",
                out_path.display()
            );
            return Ok(());
        }
        Some(Commands::ExportConfigSchema { out })
        | Some(Commands::Schema {
            command: SchemaCommands::ExportConfigSchema { out },
        }) => {
            let out_path = out.unwrap_or_else(|| PathBuf::from(&config.export.config_schema));
            let client = pool.get().await?;
            let inspector = CatalogInspector::new(&client);
            let base = inspector.inspect_base(&schemas, &Config::default()).await?;
            let schema_json = graphpg::ConfigSchemaGenerator::generate(&base);

            if let Some(parent) = out_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&out_path, serde_json::to_string_pretty(&schema_json)?)?;
            println!(
                "✓ Exported YAML config JSON schema to: {}",
                out_path.display()
            );
            return Ok(());
        }
        Some(Commands::Advise) => {
            let client = pool.get().await?;
            let inspector = CatalogInspector::new(&client);
            let catalog = inspector
                .inspect_schemas_with_config(&schemas, &config)
                .await?;
            let advice = inspector.advise_schemas(&schemas, &catalog).await?;
            for a in &advice {
                let level = match a.level {
                    graphpg::catalog::AdviceLevel::Warning => "warning",
                    graphpg::catalog::AdviceLevel::Info => "info",
                };
                println!("{level}: {}", a.message);
            }
            return Ok(());
        }
        None => {}
    }

    // Introspect once at startup on a pooled connection, then hand it back.
    let (catalog, schema, advice) = {
        let client = pool.get().await?;
        let inspector = CatalogInspector::new(&client);
        let catalog = inspector
            .inspect_schemas_with_config(&schemas, &config)
            .await?;
        let schema = SchemaBuilder::build(&catalog)?;
        let advice = inspector.advise_schemas(&schemas, &catalog).await?;
        (catalog, schema, advice)
    };

    for a in &advice {
        match a.level {
            graphpg::catalog::AdviceLevel::Warning => tracing::warn!("{}", a.message),
            graphpg::catalog::AdviceLevel::Info => tracing::debug!("{}", a.message),
        }
    }
    tracing::info!(
        schemas = %config.database.schemas.join(","),
        tables = catalog.tables.len(),
        pool_size = config.database.pool_size,
        "introspected database"
    );

    let replicas = secrets
        .replica_urls
        .iter()
        .map(|url| create_pool(url, config.database.pool_size))
        .collect::<Result<Vec<_>, _>>()?;

    let authenticator =
        match Authenticator::new(&config.auth, secrets.auth_jwt_secret.as_deref()).await {
            Ok(authenticator) => authenticator,
            Err(err) => {
                eprintln!("invalid auth configuration:\n{err}");
                std::process::exit(1);
            }
        };

    match &config.auth {
        auth if auth.enabled => {
            tracing::info!(
                issuer = auth.issuer.as_deref().unwrap_or_default(),
                "authentication: OAuth 2.0 bearer tokens"
            );
            if auth.roles.authenticated.is_none() {
                let bypasses = match pool.get().await {
                    Ok(client) => client
                        .query_one(
                            "SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user",
                            &[],
                        )
                        .await
                        .is_ok_and(|row| row.get::<_, bool>(0)),
                    Err(_) => false,
                };
                if bypasses {
                    tracing::warn!(
                        "auth.roles.authenticated is unset and the database user bypasses Row-Level Security: \
                         authenticated requests are not restricted by policies"
                    );
                }
            }
        }
        _ => tracing::warn!("authentication is off: every request runs as the database user"),
    }

    let state = Arc::new(
        ServerState::new(pool, schema, catalog, default_schema, config.debug.into())
            .with_sql_comments(config.database.sql_comments)
            .with_replicas(replicas)
            .with_auth(authenticator)
            .with_graphiql(config.graphql.ui)
            .with_scalar(config.openapi.ui)
            .with_graphql_enabled(config.graphql.enabled)
            .with_openapi_enabled(config.openapi.enabled)
            .with_config(config.clone()),
    );

    if !state.replicas.is_empty() {
        state.refresh_replicas().await;
        let standbys = state
            .replicas
            .iter()
            .filter(|r| r.standby.load(std::sync::atomic::Ordering::Relaxed))
            .count();
        tracing::info!(replicas = state.replicas.len(), standbys, "read replicas");
        let watched = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                watched.refresh_replicas().await;
            }
        });
    }

    if config.watch.enabled {
        let watcher_options = graphpg::server::WatcherOptions {
            database_url: secrets.database_url.clone(),
            mode,
            config_path: config_path.clone(),
        };
        graphpg::server::start_schema_watcher(watcher_options, state.clone());
        tracing::info!("watching PostgreSQL schema for DDL changes and auto-generating schemas");
    }

    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .map_err(|e| {
            format!(
                "invalid bind address '{}:{}': {e}",
                config.server.host, config.server.port
            )
        })?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("{}", startup_log_summary(mode, &config));
    for warning in prod_warnings(mode, &config) {
        tracing::warn!("{warning}");
    }

    if config.graphql.enabled {
        tracing::info!("graphpg listening on http://{}/graphql", addr);
    }
    if config.openapi.enabled {
        tracing::info!("REST API listening on http://{}/rest", addr);
    }
    if !config.graphql.enabled && !config.openapi.enabled {
        tracing::info!("graphpg listening on http://{}", addr);
    }
    if config.graphql.ui {
        tracing::info!(
            "GraphiQL available at http://localhost:{}/graphiql",
            config.server.port
        );
    }
    if config.openapi.ui {
        tracing::info!(
            "Scalar API reference available at http://localhost:{}/scalar",
            config.server.port
        );
    }
    if config.openapi.enabled && config.openapi.spec {
        let path = if config.openapi.path.starts_with('/') {
            config.openapi.path.clone()
        } else {
            format!("/{}", config.openapi.path)
        };
        tracing::info!(
            "OpenAPI document available at http://localhost:{}{}",
            config.server.port,
            path
        );
    }
    if config.debug.sql || config.debug.explain {
        tracing::warn!(
            explain = config.debug.explain,
            "debug mode: responses include generated SQL — do not enable in production"
        );
    }

    axum::serve(listener, create_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clap_unknown_or_removed_flags_error() {
        assert!(Cli::try_parse_from(["graphpg", "--dev"]).is_err());
        assert!(Cli::try_parse_from(["graphpg", "--watch"]).is_err());
        assert!(Cli::try_parse_from(["graphpg", "--unknown-flag"]).is_err());
        assert!(Cli::try_parse_from(["graphpg", "--debug"]).is_err());
    }

    #[test]
    fn test_clap_valid_flags() {
        let cli =
            Cli::try_parse_from(["graphpg", "--mode", "dev", "--config", "graphpg.yaml"]).unwrap();
        assert_eq!(cli.mode.as_deref(), Some("dev"));
        assert_eq!(cli.config, Some(PathBuf::from("graphpg.yaml")));
    }

    #[test]
    fn test_startup_log_summary() {
        let mut dev_cfg = Config::defaults(Mode::Dev);
        dev_cfg.debug.sql = true;
        assert_eq!(
            startup_log_summary(Mode::Dev, &dev_cfg),
            "mode=dev graphql.ui=on openapi.ui=on watch=on debug.sql=on"
        );

        let prod_cfg = Config::defaults(Mode::Prod);
        assert_eq!(
            startup_log_summary(Mode::Prod, &prod_cfg),
            "mode=prod graphql.ui=off openapi.ui=off watch=off debug.sql=off"
        );
    }

    #[test]
    fn test_prod_warnings() {
        let prod_cfg = Config::defaults(Mode::Prod);
        assert!(prod_warnings(Mode::Prod, &prod_cfg).is_empty());

        let mut custom_prod = Config::defaults(Mode::Prod);
        custom_prod.graphql.ui = true;
        custom_prod.openapi.ui = true;
        custom_prod.openapi.spec = true;
        custom_prod.watch.enabled = true;
        let warnings = prod_warnings(Mode::Prod, &custom_prod);
        assert_eq!(warnings.len(), 4);
        assert!(warnings[0].contains("graphql.ui"));
        assert!(warnings[1].contains("openapi.ui"));
        assert!(warnings[2].contains("openapi.spec"));
        assert!(warnings[3].contains("watch.enabled"));

        // In dev mode, no warnings even if features are on
        let dev_cfg = Config::defaults(Mode::Dev);
        assert!(prod_warnings(Mode::Dev, &dev_cfg).is_empty());
    }
}
