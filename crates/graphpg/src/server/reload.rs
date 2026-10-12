//! Schema reload without a restart, in any mode: on `NOTIFY <reload.channel>` (a migration ends
//! with `NOTIFY graphpg_reload`) and on `SIGHUP`.
//!
//! A reload reads the database again with the configuration the server started with, builds
//! the new schema, and runs the checks a startup runs: pinned schemas (`export.pin: fail`) and,
//! in prod, the persisted query manifest. Only when they pass is the new schema swapped in, at
//! once ([`ServerState::reload`]); otherwise the schema being served stays and the reason is
//! logged. Requests in flight finish on the schema they started with.
//!
//! The listener has a connection of its own and reconnects with a backoff; a connection that
//! cannot `LISTEN` (a pooler in transaction mode) is logged and retried, and never affects
//! serving. Development mode's DDL watcher ([`super::watcher`]) reloads by itself.
use super::ServerState;
use crate::catalog::{finalize, CatalogInspector};
use crate::config::models::PinMode;
use crate::config::Mode;
use crate::schema::SchemaBuilder;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::NoTls;

/// Pause before a notification is acted on, so a migration's several `NOTIFY`s reload once.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Longest wait between attempts to reconnect the listener.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Why a reload did not happen.
#[derive(Debug)]
pub struct ReloadError(pub String);

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReloadError {}

/// Reads the database again and swaps the new schema in, unless a startup check fails.
pub async fn reload_schema(state: &ServerState) -> Result<(), ReloadError> {
    let start = std::time::Instant::now();
    let err = |e: &dyn std::fmt::Display| ReloadError(e.to_string());
    let config = state.config.clone().unwrap_or_default();
    let client = state.pool.get().await.map_err(|e| err(&e))?;
    let schemas: Vec<&str> = config.database.schemas.iter().map(String::as_str).collect();
    let base = CatalogInspector::new(&client)
        .inspect_base(&schemas, &config)
        .await
        .map_err(|e| err(&e))?;
    let catalog = finalize(base, &config).map_err(|e| err(&e))?;
    let schema = SchemaBuilder::build(&catalog).map_err(|e| err(&e))?;

    if config.export.pin == PinMode::Fail {
        use crate::schema_pin::{drifted, startup_check, Artifacts};
        let artifacts = Artifacts::build(&catalog, &config, &state.default_schema, &schema);
        let results = startup_check(&artifacts, &config);
        let drift = drifted(&results);
        if !drift.is_empty() {
            let report: Vec<String> = drift.iter().map(|r| r.report()).collect();
            return Err(ReloadError(format!(
                "the new schema does not match the pinned files (export.pin: fail):\n{}",
                report.join("\n")
            )));
        }
    }
    if state.mode == Mode::Prod {
        if let Some(persisted) = &state.persisted {
            let problems = persisted.check(&schema);
            if !problems.is_empty() {
                return Err(ReloadError(format!(
                    "persisted query manifest operations would no longer validate:\n{}",
                    problems.join("\n")
                )));
            }
        }
    }

    let tables = catalog.tables.len();
    state.reload(schema, catalog);
    tracing::info!(tables, elapsed = ?start.elapsed(), "schema reloaded");
    Ok(())
}

/// Reloads on `NOTIFY <channel>` and on `SIGHUP`, until the process ends.
pub fn start_reload_listener(database_url: String, channel: String, state: Arc<ServerState>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<&'static str>(16);

    #[cfg(unix)]
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut hangup) = signal(SignalKind::hangup()) else {
                return;
            };
            while hangup.recv().await.is_some() {
                let _ = tx.send("SIGHUP").await;
            }
        });
    }

    tokio::spawn(listen(database_url, channel, tx));

    tokio::spawn(async move {
        while let Some(source) = rx.recv().await {
            tokio::time::sleep(DEBOUNCE).await;
            while rx.try_recv().is_ok() {}
            tracing::info!(source, "schema reload requested");
            if let Err(e) = reload_schema(&state).await {
                tracing::error!(error = %e, "schema reload refused: still serving the previous schema");
            }
        }
    });
}

/// Keeps a connection listening on `channel`, reconnecting after failures.
async fn listen(
    database_url: String,
    channel: String,
    tx: tokio::sync::mpsc::Sender<&'static str>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match tokio_postgres::connect(&database_url, NoTls).await {
            Ok((client, mut connection)) => {
                let (notified_tx, mut notified) = tokio::sync::mpsc::channel::<()>(16);
                let wanted = channel.clone();
                let driver = tokio::spawn(async move {
                    let mut messages =
                        futures::stream::poll_fn(move |cx| connection.poll_message(cx));
                    while let Some(message) = messages.next().await {
                        match message {
                            Ok(tokio_postgres::AsyncMessage::Notification(n))
                                if n.channel() == wanted =>
                            {
                                let _ = notified_tx.send(()).await;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "schema reload listener disconnected");
                                break;
                            }
                            _ => {}
                        }
                    }
                });
                match client.batch_execute(&format!("LISTEN {channel}")).await {
                    Ok(()) => {
                        tracing::info!(%channel, "listening for schema reload requests (NOTIFY {channel})");
                        backoff = Duration::from_secs(1);
                        while notified.recv().await.is_some() {
                            if tx.send("NOTIFY").await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "schema reload listener cannot LISTEN"),
                }
                driver.abort();
            }
            Err(e) => tracing::warn!(error = %e, "schema reload listener cannot connect"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
