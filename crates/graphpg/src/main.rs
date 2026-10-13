use clap::{Parser, Subcommand};
use graphpg::config::{resolve_mode, Config, Mode, Secrets, DEFAULT_CONFIG_FILE};
use graphpg::server::{
    create_pool_with, create_router, Authenticator, PersistedQueries, ServerState,
};
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
    /// Export the OpenAPI 3.1 document (YAML for a `.yaml` / `.yml` path, else JSON)
    ExportOpenapi {
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
    /// Event triggers subcommands
    Events {
        #[command(subcommand)]
        command: EventsCommands,
    },
    /// Schema subcommands
    Schema {
        #[command(subcommand)]
        command: SchemaCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum EventsCommands {
    /// Print the SQL that installs the event table, functions and triggers the configuration
    /// needs (`events.install: manual`: apply it with your migrations)
    Sql,
}

#[derive(Subcommand, Debug)]
pub enum SchemaCommands {
    /// Export every schema (GraphQL SDL, OpenAPI, and the Federation subgraph SDL when
    /// federation is on) to the paths of `export:`. With `--check`, write nothing and exit
    /// with an error if an exported file differs from what the database generates now: the
    /// CI step that pins the schema.
    Export {
        #[arg(long)]
        check: bool,
    },
    /// Export the GraphQL SDL schema
    #[command(alias = "export-schema")]
    ExportGraphql {
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Export the OpenAPI 3.1 document (YAML for a `.yaml` / `.yml` path, else JSON)
    ExportOpenapi {
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

fn main() -> Result<(), Box<dyn Error>> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    let mode = resolve_mode(
        cli.mode.as_deref(),
        std::env::var("GRAPHPG_MODE").ok().as_deref(),
    )?;

    let config_path = resolve_config_path(cli.config.as_deref())?;
    let config = match &config_path {
        Some(path) => Config::load_from_file(path, mode).map_err(|e| e.to_string())?,
        None => Config::defaults(mode),
    };

    // Before the runtime starts, and dropped after it stops: the OTLP exporters' blocking
    // HTTP client cannot live inside it. Subcommands only log.
    let _telemetry = graphpg::telemetry::init(&config.telemetry, cli.command.is_none());
    if let Some(path) = &config_path {
        tracing::info!(path = %path.display(), %mode, "loaded config");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(cli, mode, config_path, config))
}

async fn run(
    cli: Cli,
    mode: Mode,
    config_path: Option<PathBuf>,
    config: Config,
) -> Result<(), Box<dyn Error>> {
    let secrets = Secrets::from_env()?;

    let statement_timeout = config.limits.statement_timeout.map(|t| t.0);
    let pool = create_pool_with(
        &secrets.database_url,
        config.database.pool_size,
        statement_timeout,
    )?;
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
        Some(Commands::ExportOpenapi { out })
        | Some(Commands::Schema {
            command: SchemaCommands::ExportOpenapi { out },
        }) => {
            let out_path = out.unwrap_or_else(|| PathBuf::from(&config.export.openapi));
            let client = pool.get().await?;
            let catalog = CatalogInspector::new(&client)
                .inspect_schemas_with_config(&schemas, &config)
                .await?;
            let graphql = SchemaBuilder::build(&catalog)?;
            let artifacts =
                graphpg::schema_pin::Artifacts::build(&catalog, &config, &default_schema, &graphql);
            let path = out_path.to_string_lossy().to_string();
            graphpg::schema_pin::write_file(
                &path,
                &graphpg::schema_pin::render_openapi(&artifacts.openapi, &path),
            )?;
            println!("✓ Exported OpenAPI document to: {path}");
            return Ok(());
        }
        Some(Commands::Schema {
            command: SchemaCommands::Export { check },
        }) => {
            let client = pool.get().await?;
            let catalog = CatalogInspector::new(&client)
                .inspect_schemas_with_config(&schemas, &config)
                .await?;
            let graphql = SchemaBuilder::build(&catalog)?;
            let artifacts =
                graphpg::schema_pin::Artifacts::build(&catalog, &config, &default_schema, &graphql);
            if check {
                let results = graphpg::schema_pin::check(&artifacts, &config);
                for result in &results {
                    println!("{}", result.report());
                }
                // In CI a schema nobody pinned is as much a failure as one that moved
                let failed = results
                    .iter()
                    .any(|r| !matches!(r.status, graphpg::schema_pin::PinStatus::Match));
                if failed {
                    eprintln!(
                        "✗ the exported schemas are out of date: run `graphpg schema export`"
                    );
                    std::process::exit(1);
                }
                println!("✓ the exported schemas are up to date");
            } else {
                for path in artifacts.write(&config)? {
                    println!("✓ Exported schema to: {path}");
                }
            }
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
        Some(Commands::Events {
            command: EventsCommands::Sql,
        }) => {
            let client = pool.get().await?;
            let catalog = CatalogInspector::new(&client)
                .inspect_schemas_with_config(&schemas, &config)
                .await?;
            match graphpg::events::install::install_sql(&config.events, &catalog, schemas[0]) {
                Ok(sql) => println!("{sql}"),
                Err(problems) => {
                    eprintln!("{}", problems.join("\n"));
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        Some(Commands::Advise) => {
            let client = pool.get().await?;
            let inspector = CatalogInspector::new(&client);
            let catalog = inspector
                .inspect_schemas_with_config(&schemas, &config)
                .await?;
            let mut advice = inspector.advise_schemas(&schemas, &catalog).await?;
            advice.extend(
                inspector
                    .advise_foreign_tables(&schemas, &catalog, &config.auth)
                    .await?,
            );
            for active in &catalog.integrations {
                println!(
                    "integration: {} {} ({})",
                    active.integration.name(),
                    active.version,
                    active.detail
                );
            }
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
        let mut advice = inspector.advise_schemas(&schemas, &catalog).await?;
        advice.extend(
            inspector
                .advise_foreign_tables(&schemas, &catalog, &config.auth)
                .await?,
        );
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
    for active in &catalog.integrations {
        tracing::info!(
            integration = active.integration.name(),
            version = %active.version,
            "extension integration on: {}",
            active.detail
        );
    }

    // Pinned schemas: what the database generates now against the exported files
    {
        use graphpg::config::models::PinMode;
        use graphpg::schema_pin::{drifted, startup_check, Artifacts, PinStatus};
        let artifacts = Artifacts::build(&catalog, &config, &default_schema, &schema);
        let results = startup_check(&artifacts, &config);
        for result in &results {
            if result.status == PinStatus::Missing {
                tracing::debug!("{} ({}): no pinned file", result.artifact, result.path);
            }
        }
        let drift = drifted(&results);
        if !drift.is_empty() {
            let report = drift
                .iter()
                .map(|r| r.report())
                .collect::<Vec<_>>()
                .join("\n");
            if config.export.pin == PinMode::Fail {
                eprintln!(
                    "the exported schema does not match the database (export.pin: fail):\n{report}\n\
                     Export the schema again (`graphpg schema export`) and commit it, or set `export.pin: warn`."
                );
                std::process::exit(1);
            }
            tracing::warn!("the exported schema does not match the database:\n{report}");
        } else if !results.is_empty() {
            tracing::debug!("the exported schemas match the database");
        }
    }

    let replicas = secrets
        .replica_urls
        .iter()
        .map(|url| create_pool_with(url, config.database.pool_size, statement_timeout))
        .collect::<Result<Vec<_>, _>>()?;
    let metrics = graphpg::telemetry::metrics();
    metrics.watch_pool("primary", pool.clone());
    for (i, replica) in replicas.iter().enumerate() {
        metrics.watch_pool(format!("replica_{}", i + 1), replica.clone());
    }

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

    let persisted = match PersistedQueries::new(&config.graphql.persisted_queries) {
        Ok(persisted) => persisted,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };
    if let Some(persisted) = &persisted {
        let problems = persisted.check(&schema);
        if !problems.is_empty() {
            let report = problems.join("\n");
            if mode == Mode::Prod {
                eprintln!("persisted query manifest: operations that do not validate against the schema:\n{report}");
                std::process::exit(1);
            }
            tracing::warn!("persisted query manifest: operations that do not validate against the schema:\n{report}");
        }
        tracing::info!(
            manifest = persisted.manifest_len(),
            allow_list = persisted.allow_list(),
            automatic = config.graphql.persisted_queries.automatic && !persisted.allow_list(),
            "persisted queries"
        );
    }
    if config.limits.rate_limit.enabled {
        let rate = |r: &graphpg::config::models::RateConfig| match r.requests_per_minute {
            Some(n) => format!("{n}/min, burst {}", r.burst.unwrap_or(n)),
            None => "off".to_string(),
        };
        tracing::info!(
            anonymous = %rate(&config.limits.rate_limit.anonymous),
            authenticated = %rate(&config.limits.rate_limit.authenticated),
            "rate limits (per instance)"
        );
    }

    let state = Arc::new(
        ServerState::new(pool, schema, catalog, default_schema, config.debug.into())
            .with_mode(mode)
            .with_error_detail(config.errors.detail)
            .with_sql_comments(config.database.sql_comments)
            .with_replicas(replicas)
            .with_auth(authenticator)
            .with_graphiql(config.graphql.ui)
            .with_introspection(config.graphql.introspection)
            .with_scalar(config.openapi.ui)
            .with_graphql_enabled(config.graphql.enabled)
            .with_openapi_enabled(config.openapi.enabled)
            .with_persisted_queries(persisted)
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

    // Event triggers: install or check, then deliver and schedule
    if config.events.active() {
        use graphpg::config::models::EventInstall;
        let catalog = state.current_catalog();
        let (mut client, connection) =
            tokio_postgres::connect(&secrets.database_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        match config.events.install {
            EventInstall::Auto => {
                if let Err(err) = graphpg::events::install::install(
                    &mut client,
                    &config.events,
                    &catalog,
                    &config.database.schemas,
                    &state.default_schema,
                )
                .await
                {
                    eprintln!("{err}");
                    std::process::exit(1);
                }
                tracing::info!(
                    triggers = config.events.triggers.len(),
                    schedules = config.events.schedules.len(),
                    "event triggers installed"
                );
            }
            EventInstall::Manual => {
                let missing = graphpg::events::install::check(
                    &client,
                    &config.events,
                    &catalog,
                    &state.default_schema,
                )
                .await
                .unwrap_or_else(|e| vec![e]);
                if !missing.is_empty() {
                    let report = format!(
                        "event triggers are not installed (events.install: manual); missing: {}. Apply `graphpg events sql` with your migrations.",
                        missing.join(", ")
                    );
                    if mode == Mode::Prod {
                        eprintln!("{report}");
                        std::process::exit(1);
                    }
                    tracing::warn!("{report}");
                }
            }
        }
        graphpg::events::worker::start(
            state.clone(),
            config.events.clone(),
            secrets.database_url.clone(),
        );
        graphpg::events::scheduler::start(state.clone(), config.events.clone());
    }

    if config.reload.enabled {
        graphpg::server::reload::start_reload_listener(
            secrets.database_url.clone(),
            config.reload.channel.clone(),
            state.clone(),
        );
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
    if state.serves_openapi_document() {
        tracing::info!(
            "OpenAPI document available at http://localhost:{}{}",
            config.server.port,
            state.openapi_path
        );
    }
    if config.debug.sql || config.debug.explain {
        tracing::warn!(
            explain = config.debug.explain,
            "debug mode: responses include generated SQL — do not enable in production"
        );
    }

    let telemetry = &config.telemetry;
    if telemetry.metrics.enabled {
        match &telemetry.metrics.listen {
            // On a listener of its own: create_router leaves it off the API's port
            Some(listen) => {
                let metrics_addr: SocketAddr = listen.parse()?;
                let metrics_listener = tokio::net::TcpListener::bind(metrics_addr).await?;
                let router = axum::Router::new().route(
                    &telemetry.metrics.path,
                    axum::routing::get(graphpg::telemetry::http::metrics_handler),
                );
                tokio::spawn(async move {
                    if let Err(err) = axum::serve(metrics_listener, router).await {
                        tracing::error!(error = %err, "the metrics listener failed");
                    }
                });
                tracing::info!(
                    "Prometheus metrics available at http://{metrics_addr}{}",
                    telemetry.metrics.path
                );
            }
            None => tracing::info!(
                "Prometheus metrics available at http://localhost:{}{}",
                config.server.port,
                telemetry.metrics.path
            ),
        }
    }

    // The peer address names anonymous clients for the rate limits
    axum::serve(
        listener,
        create_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
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
