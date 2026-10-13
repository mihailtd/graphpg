//! Event triggers (`events:`): changes captured in their transaction, delivered as signed
//! webhooks, retried and dead-lettered; schedules enqueued once per slot.
mod common;
use axum::http::{HeaderMap, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::events::worker::{sign, Outcome, Worker};
use graphpg::events::{install, scheduler};
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, DebugOptions, ServerState};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_postgres::NoTls;

/// A webhook receiver: records requests, answers `fail_first` of them with 500.
#[derive(Clone, Default)]
struct Receiver {
    seen: Arc<Mutex<Vec<(HeaderMap, Value, String)>>>,
    fail_first: Arc<AtomicUsize>,
}

impl Receiver {
    async fn start(fail_first: usize) -> (Self, String) {
        let receiver = Self::default();
        receiver.fail_first.store(fail_first, Ordering::SeqCst);
        let r = receiver.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |headers: HeaderMap, body: String| {
                let r = r.clone();
                async move {
                    let json = serde_json::from_str(&body).unwrap_or(Value::Null);
                    r.seen.lock().unwrap().push((headers, json, body));
                    if r.fail_first
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok()
                    {
                        StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (receiver, url)
    }

    fn bodies(&self) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b, _)| b.clone())
            .collect()
    }
}

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// A schema of its own (`schema`, its events in `<schema>_events`), the `orders` table, and
/// a state configured with `events` (YAML under `events:`, `{url}` replaced).
async fn setup(schema: &str, events: &str, url: &str) -> (Arc<ServerState>, Config) {
    let db = client().await;
    db.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS {schema} CASCADE; DROP SCHEMA IF EXISTS {schema}_events CASCADE;
         CREATE SCHEMA {schema};
         CREATE TABLE {schema}.orders (id bigint PRIMARY KEY, status text NOT NULL, total numeric(10, 2) NOT NULL, note text);"
    ))
    .await
    .unwrap();
    let yaml = format!(
        "database:\n  schemas: [{schema}]\nentities:\n  orders:\n    columns:\n      note: {{ field_name: comment }}\nevents:\n  schema: {schema}_events\n{}",
        events.replace("{url}", url)
    );
    let config = Config::from_yaml(&yaml).unwrap_or_else(|e| panic!("{e}\n{yaml}"));
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config(schema, &config)
        .await
        .unwrap();
    let schema_built = SchemaBuilder::build(&catalog).unwrap();
    let state = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 4).unwrap(),
            schema_built,
            catalog,
            schema.to_string(),
            DebugOptions::default(),
        )
        .with_config(config.clone()),
    );
    let (mut install_client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    install::install(
        &mut install_client,
        &config.events,
        &state.current_catalog(),
        &config.database.schemas,
        schema,
    )
    .await
    .unwrap();
    (state, config)
}

/// Delivers until nothing is due.
async fn drain(worker: &Worker) {
    while worker.run_batch().await.unwrap() > 0 {}
}

async fn statuses(schema: &str) -> Vec<(String, String, i32)> {
    client()
        .await
        .query(
            &format!("SELECT trigger_name, status, attempts FROM {schema}_events.event ORDER BY created_at, id"),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

const ORDERS: &str = "  triggers:\n    order_changed:\n      table: orders\n      url: '{url}'\n      secret: whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw\n      headers: { x-team: billing }\n";

#[tokio::test]
async fn changes_are_captured_in_their_transaction_and_delivered_signed() {
    let (receiver, url) = Receiver::start(0).await;
    let (state, config) = setup("evt_deliver", ORDERS, &url).await;
    let db = client().await;
    db.batch_execute(
        "INSERT INTO evt_deliver.orders VALUES (1, 'new', 9.99, 'gift'), (2, 'new', 20, NULL);
         UPDATE evt_deliver.orders SET status = 'paid' WHERE id = 1;
         UPDATE evt_deliver.orders SET status = status WHERE id = 2;  -- no change: no event
         DELETE FROM evt_deliver.orders WHERE id = 2;",
    )
    .await
    .unwrap();
    // A change rolled back raises nothing (its own round trip: in one multi-statement query,
    // `BEGIN` would join the implicit transaction of the statements before it)
    db.batch_execute("BEGIN; INSERT INTO evt_deliver.orders VALUES (3, 'new', 1, NULL); ROLLBACK;")
        .await
        .unwrap();

    let worker = Worker::new(state, config.events.clone());
    drain(&worker).await;
    let bodies = receiver.bodies();
    let ops: Vec<&str> = bodies.iter().map(|b| b["op"].as_str().unwrap()).collect();
    assert_eq!(
        ops.iter().filter(|o| **o == "insert").count(),
        2,
        "{bodies:#?}"
    );
    assert_eq!(
        ops.iter().filter(|o| **o == "update").count(),
        1,
        "{bodies:#?}"
    );
    assert_eq!(
        ops.iter().filter(|o| **o == "delete").count(),
        1,
        "{bodies:#?}"
    );
    assert_eq!(
        bodies.len(),
        4,
        "a rolled-back insert and a no-op update raise nothing"
    );

    // The documented payload: rows shaped as the REST API shapes them
    let update = bodies.iter().find(|b| b["op"] == "update").unwrap();
    assert_eq!(
        update["table"],
        json!({ "schema": "evt_deliver", "name": "orders" })
    );
    assert_eq!(update["old"]["status"], "new");
    assert_eq!(update["new"]["status"], "paid");
    assert_eq!(update["new"]["comment"], "gift", "the config's field name");
    assert!(update["new"].get("note").is_none());
    assert_eq!(update["new"]["total"], "9.99", "numeric as in REST");
    assert_eq!(update["new"]["id"], "1", "bigint as in REST");
    let delete = bodies.iter().find(|b| b["op"] == "delete").unwrap();
    assert_eq!(delete["new"], Value::Null);
    assert_eq!(delete["old"]["id"], "2");

    // Standard Webhooks headers, verifiable with the secret
    for (headers, body, raw) in receiver.seen.lock().unwrap().iter() {
        let id = headers["webhook-id"].to_str().unwrap();
        assert_eq!(id, body["id"].as_str().unwrap());
        let ts: i64 = headers["webhook-timestamp"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            headers["webhook-signature"].to_str().unwrap(),
            sign("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw", id, ts, raw)
        );
        assert_eq!(headers["x-team"], "billing");
    }
    assert!(statuses("evt_deliver")
        .await
        .iter()
        .all(|(_, s, a)| s == "delivered" && *a == 1));
}

#[tokio::test]
async fn updates_filtered_by_columns() {
    let (receiver, url) = Receiver::start(0).await;
    let events = "  triggers:\n    status_changed:\n      table: orders\n      operations: [update]\n      columns: [status]\n      url: '{url}'\n";
    let (state, config) = setup("evt_columns", events, &url).await;
    let db = client().await;
    db.batch_execute(
        "INSERT INTO evt_columns.orders VALUES (1, 'new', 1, NULL);
         UPDATE evt_columns.orders SET note = 'x' WHERE id = 1;
         UPDATE evt_columns.orders SET status = 'paid' WHERE id = 1;",
    )
    .await
    .unwrap();
    drain(&Worker::new(state, config.events.clone())).await;
    let bodies = receiver.bodies();
    assert_eq!(bodies.len(), 1, "{bodies:#?}");
    assert_eq!(bodies[0]["new"]["status"], "paid");
}

#[tokio::test]
async fn failures_are_retried_then_dead_and_unknown_triggers_left_alone() {
    let (receiver, url) = Receiver::start(usize::MAX).await;
    let events = "  triggers:\n    order_changed:\n      table: orders\n      operations: [insert]\n      url: '{url}'\n      retry: { max_attempts: 2, backoff: 1ms, max_backoff: 1ms }\n";
    let (state, config) = setup("evt_retry", events, &url).await;
    let db = client().await;
    db.batch_execute(
        "INSERT INTO evt_retry.orders VALUES (1, 'new', 1, NULL);
         INSERT INTO evt_retry_events.event (trigger_name, op, payload) VALUES ('ghost', 'schedule', '{}');",
    )
    .await
    .unwrap();
    let worker = Worker::new(state, config.events.clone());
    for _ in 0..5 {
        drain(&worker).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(receiver.bodies().len(), 2, "two attempts, then none");
    let rows = statuses("evt_retry").await;
    assert!(
        rows.contains(&("order_changed".to_string(), "dead".to_string(), 2)),
        "{rows:?}"
    );
    // A trigger no configuration names is left pending, its attempt not counted
    assert!(
        rows.contains(&("ghost".to_string(), "pending".to_string(), 0)),
        "{rows:?}"
    );
    let error: String = db
        .query_one(
            "SELECT last_error FROM evt_retry_events.event WHERE status = 'dead'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(error.contains("HTTP 500"), "{error}");
}

#[tokio::test]
async fn concurrent_workers_deliver_each_event_once() {
    let (receiver, url) = Receiver::start(0).await;
    let events = "  triggers:\n    order_changed:\n      table: orders\n      operations: [insert]\n      url: '{url}'\n  delivery: { batch: 3 }\n";
    let (state, config) = setup("evt_concurrent", events, &url).await;
    client()
        .await
        .batch_execute("INSERT INTO evt_concurrent.orders SELECT i, 'new', i, NULL FROM generate_series(1, 30) i")
        .await
        .unwrap();
    let (a, b) = (
        Worker::new(state.clone(), config.events.clone()),
        Worker::new(state, config.events.clone()),
    );
    tokio::join!(drain(&a), drain(&b));
    let mut ids: Vec<String> = receiver
        .bodies()
        .iter()
        .map(|b| b["new"]["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    let before = ids.len();
    ids.dedup();
    assert_eq!((before, ids.len()), (30, 30));
}

#[tokio::test]
async fn schedule_slots_are_enqueued_once_and_one_offs_wait_for_their_time() {
    let (receiver, url) = Receiver::start(0).await;
    let events = "  schedules:\n    nightly:\n      cron: '0 3 * * *'\n      payload: { kind: report }\n      url: '{url}'\n    reminder:\n      url: '{url}'\n";
    let (state, config) = setup("evt_schedule", events, &url).await;
    let slot = chrono::Utc::now() - chrono::Duration::minutes(1);
    // Two instances reaching the same slot
    let (first, second) = tokio::join!(
        scheduler::enqueue(&state, &config.events, "nightly", slot),
        scheduler::enqueue(&state, &config.events, "nightly", slot),
    );
    assert_eq!(
        [first.unwrap(), second.unwrap()]
            .iter()
            .filter(|x| **x)
            .count(),
        1
    );

    let db = client().await;
    db.batch_execute(
        "SELECT evt_schedule_events.schedule('reminder', now() + interval '1 hour', '{\"user\": 42}');
         SELECT evt_schedule_events.schedule('reminder', now() - interval '1 second', '{\"user\": 7}');",
    )
    .await
    .unwrap();
    let worker = Worker::new(state, config.events.clone());
    drain(&worker).await;
    let bodies = receiver.bodies();
    assert_eq!(
        bodies.len(),
        2,
        "the due slot and the due one-off, not the later one: {bodies:#?}"
    );
    let nightly = bodies.iter().find(|b| b["schedule"] == "nightly").unwrap();
    assert_eq!(nightly["payload"], json!({ "kind": "report" }));
    assert!(nightly["scheduledFor"].is_string());
    let reminder = bodies.iter().find(|b| b["schedule"] == "reminder").unwrap();
    assert_eq!(reminder["payload"], json!({ "user": 7 }));
}

#[tokio::test]
async fn manual_install_is_checked_and_stale_triggers_dropped() {
    let (_, url) = Receiver::start(0).await;
    let (state, config) = setup("evt_install", ORDERS, &url).await;
    let db = client().await;
    assert_eq!(
        triggers(&db).await,
        [
            "graphpg_event_order_changed_delete",
            "graphpg_event_order_changed_insert",
            "graphpg_event_order_changed_update"
        ]
    );
    // Nothing missing
    let catalog = state.current_catalog();
    assert!(install::check(&db, &config.events, &catalog, "evt_install")
        .await
        .unwrap()
        .is_empty());

    // The configuration names another trigger: the old ones go, the new one is missing
    let mut events = config.events.clone();
    let trigger = events.triggers.remove("order_changed").unwrap();
    events.triggers.insert("order_paid".to_string(), trigger);
    let missing = install::check(&db, &events, &catalog, "evt_install")
        .await
        .unwrap();
    assert_eq!(missing.len(), 3, "{missing:?}");
    let sql = install::install_sql(&events, &catalog, "evt_install").unwrap();
    assert!(sql.contains("graphpg_event_order_paid_insert"), "{sql}");
    let (mut c, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    install::install(
        &mut c,
        &events,
        &catalog,
        &["evt_install".to_string()],
        "evt_install",
    )
    .await
    .unwrap();
    assert!(triggers(&db)
        .await
        .iter()
        .all(|t| t.starts_with("graphpg_event_order_paid_")));
}

async fn triggers(db: &tokio_postgres::Client) -> Vec<String> {
    db.query(
        "SELECT tgname::text FROM pg_trigger WHERE tgrelid = 'evt_install.orders'::regclass AND NOT tgisinternal ORDER BY 1",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<_, String>(0))
    .collect()
}

#[test]
fn bad_configuration_is_reported() {
    let err = Config::from_yaml(
        "events:\n  triggers:\n    Bad-Name:\n      table: t\n      url: ftp://x\n      operations: [insert]\n      columns: [a]\n  schedules:\n    s:\n      cron: '61 * * * *'\n      url: http://x\n",
    )
    .unwrap_err()
    .to_string();
    for needle in [
        "lowercase identifier",
        "http(s) URL",
        "only updates are filtered",
        "events.schedules.s.cron",
    ] {
        assert!(err.contains(needle), "{needle} in {err}");
    }
}

#[allow(dead_code)]
fn _outcome_is_public(o: Outcome) -> Outcome {
    o
}
