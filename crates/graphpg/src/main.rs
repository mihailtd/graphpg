use graphpg::config::{ConfigOverrides, EngineConfig};
use graphpg::server::Authenticator;
use graphpg::{create_pool, create_router, CatalogInspector, DebugOptions, SchemaBuilder, ServerState};
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;

const DEFAULT_DATABASE_URL: &str = "postgres://postgres:postgres@localhost:5432/graphpg";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> Result<T, String> {
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|_| format!("invalid value for {key}: {v:?}")),
        Err(_) => Ok(default),
    }
}

/// A boolean switch set by a CLI flag (`--debug`) or an env var (`DEBUG=1`/`true`).
fn switch(flag: &str, env_key: &str) -> bool {
    std::env::args().any(|a| a == flag)
        || std::env::var(env_key).is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// A setting turned on (`on` flag) or off (`off` flag), else by `env_key` (`true`/`false`),
/// over the config file; `None` leaves the file's setting.
fn flag_or_env(on: &str, off: &str, env_key: &str) -> Result<Option<bool>, String> {
    if std::env::args().any(|a| a == on) {
        return Ok(Some(true));
    }
    if std::env::args().any(|a| a == off) {
        return Ok(Some(false));
    }
    match std::env::var(env_key).map(|v| v.trim().to_ascii_lowercase()) {
        Err(_) => Ok(None),
        Ok(v) if v.is_empty() => Ok(None),
        Ok(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Ok(Some(true)),
        Ok(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Ok(Some(false)),
        Ok(v) => Err(format!("invalid value for {env_key}: {v:?} (true or false)")),
    }
}

/// Default config file, used when present.
const DEFAULT_CONFIG_PATH: &str = "engine.config.yaml";

/// Engine config path: `--config <path>`, else `ENGINE_CONFIG`, else `engine.config.yaml` when
/// that file exists. An explicitly named file must exist.
fn config_path() -> Result<Option<std::path::PathBuf>, String> {
    let args: Vec<String> = std::env::args().collect();
    let explicit = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("ENGINE_CONFIG").ok().filter(|v| !v.is_empty()));
    match explicit {
        Some(path) if std::path::Path::new(&path).is_file() => Ok(Some(path.into())),
        Some(path) => Err(format!("engine config not found: {path}")),
        None => Ok(std::path::Path::new(DEFAULT_CONFIG_PATH).is_file().then(|| DEFAULT_CONFIG_PATH.into())),
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

    let database_url = env_or("DATABASE_URL", DEFAULT_DATABASE_URL);
    let pool_size: usize = env_parse("DATABASE_POOL_SIZE", 10)?;
    let port: u16 = env_parse("PORT", 4000)?;
    let debug = DebugOptions {
        sql: switch("--debug", "DEBUG"),
        explain: switch("--debug-explain", "DEBUG_EXPLAIN"),
    };

    let pool = create_pool(&database_url, pool_size)?;
    let config_path = config_path()?;
    let mut config = match &config_path {
        Some(path) => {
            tracing::info!(path = %path.display(), "loading engine config");
            EngineConfig::load_from_file(path).map_err(|e| e.to_string())?
        }
        None => EngineConfig::default(),
    };
    // Environment and flags over the file; the watcher re-applies them on every reload
    let overrides = ConfigOverrides {
        database_schemas: std::env::var("DATABASE_SCHEMAS").ok(),
        database_schema: std::env::var("DATABASE_SCHEMA").ok(),
        federation: switch("--federation", "FEDERATION"),
        graphiql: flag_or_env("--graphiql", "--no-graphiql", "GRAPHIQL")?,
        auth_enabled: flag_or_env("--auth", "--no-auth", "AUTH_ENABLED")?,
        auth_issuer: std::env::var("AUTH_ISSUER").ok(),
        auth_audience: std::env::var("AUTH_AUDIENCE").ok(),
        auth_jwks_uri: std::env::var("AUTH_JWKS_URI").ok(),
        auth_secret: std::env::var("AUTH_JWT_SECRET").ok(),
    };
    overrides.apply(&mut config);
    let schemas: Vec<&str> = config.schemas.iter().map(String::as_str).collect();
    let default_schema = schemas[0].to_string();

    // Check CLI subcommands
    let args: Vec<String> = std::env::args().collect();
    let is_export_graphql = args.iter().any(|a| a == "export-graphql" || a == "export-graphql-schema")
        || (args.len() >= 3 && args[1] == "schema" && (args[2] == "export-graphql" || args[2] == "export-schema"));
    let is_export_subgraph = args.iter().any(|a| a == "export-subgraph")
        || (args.len() >= 3 && args[1] == "schema" && args[2] == "export-subgraph");
    let is_advise = args.iter().any(|a| a == "advise");
    let is_export_config_schema = args.iter().any(|a| a == "export-config-schema")
        || (args.len() >= 3 && args[1] == "schema" && args[2] == "export-config-schema");

    // Helper to find --out <path> argument
    let get_out_path = |flag: &str, env_var: &str, default_path: &str| -> String {
        if let Some(pos) = args.iter().position(|a| a == flag) {
            if let Some(val) = args.get(pos + 1) {
                return val.clone();
            }
        }
        std::env::var(env_var).unwrap_or_else(|_| default_path.to_string())
    };

    if is_export_graphql {
        let out_path = get_out_path("--out", "GRAPHQL_SCHEMA_OUT", "schema.graphql");
        let client = pool.get().await?;
        let catalog = CatalogInspector::new(&client).inspect_schemas_with_config(&schemas, &config).await?;
        let sdl = SchemaBuilder::generate_sdl(&catalog);

        if let Some(parent) = std::path::Path::new(&out_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&out_path, sdl)?;
        println!("✓ Exported GraphQL SDL schema to: {}", out_path);
        return Ok(());
    }

    if is_export_subgraph {
        let out_path = get_out_path("--out", "SUBGRAPH_SCHEMA_OUT", "subgraph.graphql");
        let client = pool.get().await?;
        let mut cfg = config.clone();
        cfg.federation.enabled = true;
        let catalog = CatalogInspector::new(&client).inspect_schemas_with_config(&schemas, &cfg).await?;
        let sdl = SchemaBuilder::generate_federation_subgraph_sdl(&catalog);

        if let Some(parent) = std::path::Path::new(&out_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&out_path, sdl)?;
        println!("✓ Exported Apollo Federation Subgraph SDL to: {}", out_path);
        return Ok(());
    }

    if is_export_config_schema {
        let out_path = get_out_path("--out", "CONFIG_SCHEMA_OUT", ".engine/schema.config.json");
        let client = pool.get().await?;
        let inspector = CatalogInspector::new(&client);
        // Database names only: the schema must not depend on the config it validates
        let base = inspector.inspect_base(&schemas, &EngineConfig::default()).await?;
        let schema_json = graphpg::ConfigSchemaGenerator::generate(&base);

        if let Some(parent) = std::path::Path::new(&out_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&out_path, serde_json::to_string_pretty(&schema_json)?)?;
        println!("✓ Exported YAML config JSON schema to: {}", out_path);
        return Ok(());
    }

    // Introspect once at startup on a pooled connection, then hand it back.
    let (catalog, schema, advice) = {
        let client = pool.get().await?;
        let inspector = CatalogInspector::new(&client);
        let catalog = inspector.inspect_schemas_with_config(&schemas, &config).await?;
        let schema = SchemaBuilder::build(&catalog)?;
        let advice = inspector.advise_schemas(&schemas, &catalog).await?;
        (catalog, schema, advice)
    };
    if is_advise {
        for a in &advice {
            let level = match a.level {
                graphpg::catalog::AdviceLevel::Warning => "warning",
                graphpg::catalog::AdviceLevel::Info => "info",
            };
            println!("{level}: {}", a.message);
        }
        return Ok(());
    }
    for a in &advice {
        match a.level {
            graphpg::catalog::AdviceLevel::Warning => tracing::warn!("{}", a.message),
            graphpg::catalog::AdviceLevel::Info => tracing::debug!("{}", a.message),
        }
    }
    tracing::info!(
        schemas = %config.schemas.join(","),
        tables = catalog.tables.len(),
        pool_size,
        "introspected database"
    );

    let is_dev_mode = switch("--dev", "DEV") || switch("--watch", "WATCH");

    let sql_comments = !std::env::var("SQL_COMMENTS").is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"));
    // Read replicas: comma-separated URLs; queries run on those that are standbys
    let replica_urls: Vec<String> = std::env::var("DATABASE_REPLICA_URLS")
        .unwrap_or_default()
        .split(',')
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect();
    let replicas = replica_urls.iter().map(|url| create_pool(url, pool_size)).collect::<Result<Vec<_>, _>>()?;
    // Requests are authenticated (OAuth 2.0 bearer tokens) when `auth.enabled`; a configuration
    // problem stops startup
    let authenticator = match Authenticator::new(&config.auth).await {
        Ok(authenticator) => authenticator,
        Err(err) => {
            eprintln!("invalid auth configuration:\n{err}");
            std::process::exit(1);
        }
    };
    match &config.auth {
        auth if auth.enabled => {
            tracing::info!(issuer = auth.issuer.as_deref().unwrap_or_default(), "authentication: OAuth 2.0 bearer tokens");
            // Without a role to switch to, policies only apply if the login role is subject to them
            if auth.roles.authenticated.is_none() {
                let bypasses = match pool.get().await {
                    Ok(client) => client
                        .query_one("SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user", &[])
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
        ServerState::new(pool, schema, catalog, default_schema, debug)
            .with_sql_comments(sql_comments)
            .with_replicas(replicas)
            .with_auth(authenticator)
            .with_graphiql(config.graphiql),
    );
    if !state.replicas.is_empty() {
        state.refresh_replicas().await;
        let standbys = state.replicas.iter().filter(|r| r.standby.load(std::sync::atomic::Ordering::Relaxed)).count();
        tracing::info!(replicas = state.replicas.len(), standbys, "read replicas");
        let watched = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                watched.refresh_replicas().await;
            }
        });
    }

    if is_dev_mode {
        let watcher_options = graphpg::server::WatcherOptions {
            database_url: database_url.clone(),
            overrides,
            graphql_out: std::env::var("GRAPHQL_SCHEMA_OUT").unwrap_or_else(|_| "schema.graphql".to_string()),
            config_schema_out: std::env::var("CONFIG_SCHEMA_OUT").unwrap_or_else(|_| ".engine/schema.config.json".to_string()),
            config_path: config_path.clone(),
        };
        graphpg::server::start_schema_watcher(watcher_options, state.clone());
        println!("🚀 [dev mode] Active: watching PostgreSQL schema for DDL changes and auto-generating schemas");
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("graphpg listening on http://{}/graphql", addr);
    if config.graphiql {
        tracing::info!("GraphiQL available at http://localhost:{}/graphiql", port);
    }
    if debug.sql || debug.explain {
        let explain = debug.explain;
        tracing::warn!(
            explain,
            "debug mode: responses include generated SQL — do not enable in production"
        );
    }

    axum::serve(listener, create_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}
