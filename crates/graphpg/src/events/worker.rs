//! Delivery of events as webhooks (see `docs/events.md`).
//!
//! - **Claiming** is safe across instances: a batch of due events is claimed with a lease
//!   (`UPDATE … WHERE id IN (SELECT … FOR UPDATE SKIP LOCKED)`), committed at once, so no
//!   transaction stays open during requests. An instance that dies mid-delivery leaves the
//!   lease to expire, and the event is delivered again: delivery is at least once, and the
//!   event's id (`webhook-id`) is the same on every attempt.
//! - **Payloads** follow the documented contract (the OpenAPI `webhooks`): change events carry
//!   the rows `old` and `new` shaped as the REST API shapes rows (field names, omissions,
//!   values), converted in SQL by the same compiler; schedule events carry their `payload`.
//! - **Requests** are signed per Standard Webhooks (`webhook-id`, `webhook-timestamp`,
//!   `webhook-signature: v1,<HMAC-SHA256>`). A 2xx acknowledges; anything else is retried with
//!   exponential backoff and jitter, until `retry.max_attempts`, then the event is `dead`.
//! - The worker wakes on `NOTIFY <events.schema>` (sent by the capture triggers), at the next
//!   due event, and at least every `delivery.poll`. Old events are deleted after `retention`.
use crate::catalog::DatabaseCatalog;
use crate::compiler::{quote_ident, SqlCompiler};
use crate::config::models::{EventTriggersConfig, WebhookConfig};
use crate::planner::QueryPlanner;
use crate::server::ServerState;
use base64::Engine;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A claimed event.
#[derive(Debug, Clone)]
pub struct Event {
    pub id: uuid::Uuid,
    pub created_at: DateTime<Utc>,
    pub trigger: String,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub op: String,
    pub old: Option<serde_json::Value>,
    pub new: Option<serde_json::Value>,
    pub payload: Option<serde_json::Value>,
    pub scheduled_for: Option<DateTime<Utc>>,
    /// Attempts so far, this one included.
    pub attempts: i32,
}

/// What became of one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    Retry(String),
    Dead(String),
    /// The trigger is not in this instance's configuration (a deploy in progress): left for
    /// an instance that knows it.
    Unknown,
}

/// The REST row shape of a table, as SQL over a row read from JSON: `(sql, params)`, with
/// `$1` the row (`jsonb`).
type RowShape = (String, Vec<String>);
/// Row shapes by table, and the catalog (its address) they were compiled against.
type ShapeCache = (usize, HashMap<(String, String), Arc<RowShape>>);

pub struct Worker {
    state: Arc<ServerState>,
    config: EventTriggersConfig,
    http: reqwest::Client,
    /// Row shapes per table, for the catalog they were compiled against.
    shapes: Mutex<ShapeCache>,
}

impl Worker {
    pub fn new(state: Arc<ServerState>, config: EventTriggersConfig) -> Self {
        Self {
            state,
            config,
            http: reqwest::Client::builder()
                .user_agent(concat!("graphpg/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("an HTTP client"),
            shapes: Mutex::default(),
        }
    }

    fn table(&self) -> String {
        format!("{}.event", quote_ident(&self.config.schema))
    }

    /// Claims due events (at most `delivery.batch`) for `delivery.lease`.
    pub async fn claim(&self) -> Result<Vec<Event>, String> {
        let client = self.state.pool.get().await.map_err(|e| e.to_string())?;
        let sql = format!(
            "UPDATE {t} SET deliver_at = now() + make_interval(secs => $2), attempts = attempts + 1
             WHERE id IN (SELECT id FROM {t} WHERE status = 'pending' AND deliver_at <= now()
                          ORDER BY deliver_at, id LIMIT $1 FOR UPDATE SKIP LOCKED)
             RETURNING id, created_at, trigger_name, schema_name, table_name, op, old, new, payload, scheduled_for, attempts",
            t = self.table()
        );
        let rows = client
            .query(
                &sql,
                &[
                    &(self.config.delivery.batch as i64),
                    &self.config.delivery.lease.0.as_secs_f64(),
                ],
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .iter()
            .map(|r| Event {
                id: r.get(0),
                created_at: r.get(1),
                trigger: r.get(2),
                schema: r.get(3),
                table: r.get(4),
                op: r.get(5),
                old: r.get(6),
                new: r.get(7),
                payload: r.get(8),
                scheduled_for: r.get(9),
                attempts: r.get(10),
            })
            .collect())
    }

    /// The SQL shaping rows of `schema.table` as the REST API does.
    fn row_shape(
        &self,
        catalog: &DatabaseCatalog,
        schema: &str,
        table: &str,
    ) -> Result<Arc<RowShape>, String> {
        let generation = catalog as *const DatabaseCatalog as usize;
        let mut cache = self.shapes.lock().unwrap();
        if cache.0 != generation {
            *cache = (generation, HashMap::new());
        }
        let key = (schema.to_string(), table.to_string());
        if let Some(shape) = cache.1.get(&key) {
            return Ok(shape.clone());
        }
        let meta = catalog
            .find_table(schema, table)
            .ok_or_else(|| format!("the table {schema}.{table} is no longer served"))?;
        let selection = crate::server::rest::column_fields(catalog, meta);
        let (fields, polymorphism, plan) = QueryPlanner::new(catalog, &self.state.default_schema)
            .plan_row(&selection, meta, "r")
            .map_err(|e| e.to_string())?;
        // `$1` is the row
        let mut params = vec![String::new()];
        let (obj, joins) =
            SqlCompiler::row_json("r", &fields, polymorphism.as_ref(), &plan, &mut params);
        params.remove(0);
        let sql = format!(
            "SELECT ({obj})::text FROM jsonb_populate_record(NULL::{}.{}, $1::text::jsonb) r{joins}",
            quote_ident(schema),
            quote_ident(table)
        );
        let shape = Arc::new((sql, params));
        cache.1.insert(key, shape.clone());
        Ok(shape)
    }

    /// The request body of `event`.
    pub async fn body(&self, event: &Event) -> Result<String, String> {
        let created = event
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if event.op == "schedule" {
            return Ok(serde_json::json!({
                "id": event.id,
                "createdAt": created,
                "schedule": event.trigger,
                "scheduledFor": event.scheduled_for.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                "payload": event.payload.clone().unwrap_or(serde_json::json!({})),
            })
            .to_string());
        }
        let (schema, table) = match (&event.schema, &event.table) {
            (Some(s), Some(t)) => (s.as_str(), t.as_str()),
            _ => return Err("a change event without its table".to_string()),
        };
        let catalog = self.state.current_catalog();
        let shape = self.row_shape(&catalog, schema, table)?;
        let client = self.state.pool.get().await.map_err(|e| e.to_string())?;
        let mut shaped = Vec::new();
        for row in [&event.old, &event.new] {
            shaped.push(match row {
                None => serde_json::Value::Null,
                Some(row) => {
                    let mut params: Vec<String> = vec![row.to_string()];
                    params.extend(shape.1.iter().cloned());
                    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        params.iter().map(|p| p as _).collect();
                    let text: String = client
                        .query_one(&shape.0, &refs)
                        .await
                        .map_err(|e| format!("shaping the row: {e}"))?
                        .get(0);
                    serde_json::from_str(&text).map_err(|e| e.to_string())?
                }
            });
        }
        let new = shaped.pop();
        let old = shaped.pop();
        Ok(serde_json::json!({
            "id": event.id,
            "createdAt": created,
            "table": { "schema": schema, "name": table },
            "op": event.op,
            "old": old,
            "new": new,
        })
        .to_string())
    }

    /// Sends `event` to its webhook, and says what became of it.
    pub async fn deliver(&self, event: &Event) -> Outcome {
        let Some(hook) = self.config.webhook(&event.trigger) else {
            return Outcome::Unknown;
        };
        let failed = |error: String| {
            if event.attempts >= hook.retry.max_attempts as i32 {
                Outcome::Dead(error)
            } else {
                Outcome::Retry(error)
            }
        };
        let body = match self.body(event).await {
            Ok(body) => body,
            Err(error) => return failed(error),
        };
        match send(&self.http, &hook, &event.id.to_string(), body).await {
            Ok(()) => Outcome::Delivered,
            Err(error) => failed(error),
        }
    }

    /// Records the outcome of a delivery.
    pub async fn record(&self, event: &Event, outcome: &Outcome) -> Result<(), String> {
        let client = self.state.pool.get().await.map_err(|e| e.to_string())?;
        let t = self.table();
        let id = event.id;
        let result = match outcome {
            Outcome::Delivered => client
                .execute(
                    &format!("UPDATE {t} SET status = 'delivered', delivered_at = now(), last_error = NULL WHERE id = $1"),
                    &[&id],
                )
                .await,
            Outcome::Retry(error) => {
                let hook = self.config.webhook(&event.trigger).unwrap_or_default();
                let wait = backoff(&hook, event.attempts);
                client
                    .execute(
                        &format!("UPDATE {t} SET deliver_at = now() + make_interval(secs => $2), last_error = $3 WHERE id = $1"),
                        &[&id, &wait.as_secs_f64(), error],
                    )
                    .await
            }
            Outcome::Dead(error) => client
                .execute(
                    &format!("UPDATE {t} SET status = 'dead', last_error = $2 WHERE id = $1"),
                    &[&id, error],
                )
                .await,
            // Not counted as an attempt; looked at again in a minute
            Outcome::Unknown => client
                .execute(
                    &format!("UPDATE {t} SET attempts = attempts - 1, deliver_at = now() + interval '1 minute', last_error = $2 WHERE id = $1"),
                    &[&id, &format!("no trigger or schedule `{}` in this instance's configuration", event.trigger)],
                )
                .await,
        };
        result.map(|_| ()).map_err(|e| e.to_string())
    }

    /// Claims, delivers and records one batch; the number of events it held.
    pub async fn run_batch(&self) -> Result<usize, String> {
        let events = self.claim().await?;
        let n = events.len();
        futures::stream::iter(events)
            .for_each_concurrent(self.config.delivery.concurrency, |event| async move {
                let outcome = self.deliver(&event).await;
                match &outcome {
                    Outcome::Delivered => tracing::debug!(id = %event.id, trigger = %event.trigger, "event delivered"),
                    Outcome::Retry(e) => tracing::info!(id = %event.id, trigger = %event.trigger, attempt = event.attempts, error = %e, "event delivery failed; will retry"),
                    Outcome::Dead(e) => tracing::warn!(id = %event.id, trigger = %event.trigger, attempts = event.attempts, error = %e, "event delivery failed for the last time: the event is dead"),
                    Outcome::Unknown => tracing::warn!(id = %event.id, trigger = %event.trigger, "event of a trigger this instance does not know: left for another"),
                }
                crate::telemetry::metrics().event_delivery(&event.trigger, outcome_label(&outcome));
                if let Err(e) = self.record(&event, &outcome).await {
                    tracing::error!(id = %event.id, error = %e, "could not record an event's delivery: it will be delivered again");
                }
            })
            .await;
        Ok(n)
    }

    /// How long until the next pending event is due (`None`: none pending).
    pub async fn next_due(&self) -> Option<Duration> {
        let client = self.state.pool.get().await.ok()?;
        let secs: Option<f64> = client
            .query_one(
                &format!(
                    "SELECT greatest(0, extract(epoch FROM min(deliver_at) - now()))::float8 FROM {} WHERE status = 'pending'",
                    self.table()
                ),
                &[],
            )
            .await
            .ok()?
            .get(0);
        secs.map(Duration::from_secs_f64)
    }

    /// Deletes delivered and dead events past their retention, a bounded batch at a time.
    pub async fn clean_up(&self) -> Result<u64, String> {
        let client = self.state.pool.get().await.map_err(|e| e.to_string())?;
        let d = &self.config.delivery;
        let deleted = client
            .execute(
                &format!(
                    "DELETE FROM {t} WHERE id IN (SELECT id FROM {t} WHERE status <> 'pending' AND created_at < now() - make_interval(secs => CASE WHEN status = 'dead' THEN $2 ELSE $1 END) LIMIT 1000)",
                    t = self.table()
                ),
                &[&d.retention.0.as_secs_f64(), &d.dead_retention.0.as_secs_f64()],
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(deleted)
    }
}

fn outcome_label(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Delivered => "delivered",
        Outcome::Retry(_) => "retry",
        Outcome::Dead(_) => "dead",
        Outcome::Unknown => "unknown",
    }
}

/// Wait before the next attempt after `attempts` failed ones: `backoff` doubled each time, at
/// most `max_backoff`, between half of it and all of it (jitter spreads retries out).
pub fn backoff(hook: &WebhookConfig, attempts: i32) -> Duration {
    let exp = attempts.saturating_sub(1).clamp(0, 30) as u32;
    let full = hook
        .retry
        .backoff
        .0
        .saturating_mul(2u32.saturating_pow(exp))
        .min(hook.retry.max_backoff.0);
    let jitter = (uuid::Uuid::new_v4().as_u128() % 1000) as f64 / 1000.0;
    full.mul_f64(0.5 + 0.5 * jitter)
}

/// The Standard Webhooks signature of `body`: `v1,<base64 HMAC-SHA256 of "id.timestamp.body">`.
/// A `whsec_` secret is base64 of the key; any other text is the key itself.
pub fn sign(secret: &str, id: &str, timestamp: i64, body: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let b64 = base64::engine::general_purpose::STANDARD;
    let key = secret
        .strip_prefix("whsec_")
        .and_then(|k| b64.decode(k).ok())
        .unwrap_or_else(|| secret.as_bytes().to_vec());
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key).expect("HMAC takes any key");
    mac.update(format!("{id}.{timestamp}.{body}").as_bytes());
    format!("v1,{}", b64.encode(mac.finalize().into_bytes()))
}

/// POSTs `body`; `Err` with why it was not acknowledged.
async fn send(
    http: &reqwest::Client,
    hook: &WebhookConfig,
    id: &str,
    body: String,
) -> Result<(), String> {
    let timestamp = Utc::now().timestamp();
    let mut request = http
        .post(&hook.url)
        .timeout(hook.timeout.0)
        .header("content-type", "application/json")
        .header("webhook-id", id)
        .header("webhook-timestamp", timestamp.to_string());
    if let Some(secret) = &hook.secret {
        request = request.header("webhook-signature", sign(secret, id, timestamp, &body));
    }
    for (name, value) in &hook.headers {
        request = request.header(name, value);
    }
    let response = request
        .body(body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        let text = response.text().await.unwrap_or_default();
        let text: String = text.chars().take(200).collect();
        Err(format!("HTTP {status}: {text}"))
    }
}

/// Delivers events until the process ends: on notifications, when the next event is due, and
/// at least every `delivery.poll`.
pub fn start(state: Arc<ServerState>, config: EventTriggersConfig, database_url: String) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<&'static str>(16);
    tokio::spawn(crate::server::reload::listen(
        database_url,
        config.schema.clone(),
        "new events",
        tx,
    ));
    tokio::spawn(async move {
        let poll = config.delivery.poll.0;
        let worker = Worker::new(state, config);
        let mut cleaned = std::time::Instant::now();
        loop {
            loop {
                match worker.run_batch().await {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, "claiming events failed");
                        break;
                    }
                }
            }
            if cleaned.elapsed() > Duration::from_secs(60) {
                cleaned = std::time::Instant::now();
                if let Err(e) = worker.clean_up().await {
                    tracing::warn!(error = %e, "deleting old events failed");
                }
            }
            let wait = worker.next_due().await.map_or(poll, |d| d.min(poll));
            tokio::select! {
                _ = rx.recv() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_webhooks_signature() {
        // The example of the Standard Webhooks specification
        assert_eq!(
            sign(
                "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw",
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                1614265330,
                r#"{"test": 2432232314}"#
            ),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let hook = WebhookConfig::default();
        for (attempts, full) in [(1, 10), (2, 20), (3, 40), (10, 3600), (40, 3600)] {
            let wait = backoff(&hook, attempts).as_secs_f64();
            assert!(
                wait >= full as f64 / 2.0 && wait <= full as f64,
                "attempt {attempts}: {wait}"
            );
        }
    }
}
