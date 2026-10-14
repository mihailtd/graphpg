//! Live queries (`subscriptions:`): a `Subscription` type with the `Query` type's root
//! fields, re-run every interval and sent when the result changes, over SSE, multipart HTTP
//! and WebSocket (`graphql-transport-ws`), under Row-Level Security.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use futures::{SinkExt, StreamExt};
use graphpg::catalog::CatalogInspector;
use graphpg::config::models::{AuthConfig, RolesConfig, SubscriptionsConfig, Timeout};
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::subscriptions::batch;
use graphpg::server::{create_pool, create_router, Authenticator, ServerState};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::NoTls;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

const SECRET: &str = "secret-for-live-query-tests-0123456789";
const ISSUER: &str = "https://issuer.test/";
const AUDIENCE: &str = "graphpg-live";
/// Re-run interval in these tests.
const INTERVAL: Duration = Duration::from_millis(100);
/// How long a test waits for a result it expects.
const WAIT: Duration = Duration::from_secs(5);

const FIXTURE: &str = r#"
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'live_user') THEN CREATE ROLE live_user; END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'live_anon') THEN CREATE ROLE live_anon; END IF;
END $$;
DROP SCHEMA IF EXISTS live_test CASCADE;
CREATE SCHEMA live_test;
GRANT USAGE ON SCHEMA live_test TO live_user, live_anon;

-- No Row-Level Security: every user's subscriptions share one statement
CREATE TABLE live_test.items (id int PRIMARY KEY, name text NOT NULL, qty int NOT NULL);
INSERT INTO live_test.items SELECT g, 'item ' || g, g FROM generate_series(1, 100) g;
GRANT SELECT, UPDATE ON live_test.items TO live_user, live_anon;

-- Each user sees their own notes
CREATE TABLE live_test.notes (id int PRIMARY KEY, owner text NOT NULL, body text NOT NULL);
ALTER TABLE live_test.notes ENABLE ROW LEVEL SECURITY;
CREATE POLICY own_notes ON live_test.notes TO live_user
    USING (owner = current_setting('request.jwt.claim.sub', true));
GRANT SELECT ON live_test.notes TO live_user;
INSERT INTO live_test.notes VALUES (1, 'alice', 'a1'), (2, 'bob', 'b1'), (3, 'alice', 'a2');

CREATE VIEW live_test.item_names AS SELECT id, name FROM live_test.items;
GRANT SELECT ON live_test.item_names TO live_user, live_anon;
CREATE TYPE live_test.mood AS ENUM ('happy', 'sad');

-- A query field failing when an item's quantity is negative
CREATE FUNCTION live_test.checked_total() RETURNS int LANGUAGE plpgsql STABLE AS $$
DECLARE total int;
BEGIN
    SELECT sum(qty) INTO total FROM live_test.items WHERE id BETWEEN 90 AND 92;
    IF EXISTS (SELECT 1 FROM live_test.items WHERE id BETWEEN 90 AND 92 AND qty < 0) THEN
        RAISE EXCEPTION 'negative quantity';
    END IF;
    RETURN total;
END $$;
GRANT EXECUTE ON FUNCTION live_test.checked_total() TO live_user, live_anon;
"#;

static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("connect to the test database");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn ensure_fixture() {
    INIT.get_or_init(|| async {
        let c = client().await;
        c.batch_execute("SELECT pg_advisory_lock(7461001)")
            .await
            .unwrap();
        c.batch_execute(FIXTURE).await.expect("live_test fixture");
        c.batch_execute("SELECT pg_advisory_unlock(7461001)")
            .await
            .unwrap();
    })
    .await;
}

async fn exec(sql: &str) {
    client().await.batch_execute(sql).await.expect(sql);
}

fn auth_config() -> AuthConfig {
    AuthConfig {
        enabled: true,
        issuer: Some(ISSUER.to_string()),
        audience: vec![AUDIENCE.to_string()],
        roles: RolesConfig {
            authenticated: Some("live_user".to_string()),
            anonymous: Some("live_anon".to_string()),
        },
        ..AuthConfig::default()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn token(sub: &str, ttl: u64) -> String {
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &json!({ "iss": ISSUER, "aud": AUDIENCE, "exp": now() + ttl, "sub": sub }),
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

/// The app with subscriptions on (`tweak` changes their settings), re-runs started.
async fn app_with(
    tweak: impl FnOnce(&mut SubscriptionsConfig),
) -> (axum::Router, Arc<ServerState>) {
    ensure_fixture().await;
    let mut subscriptions = SubscriptionsConfig {
        enabled: true,
        interval: Timeout(INTERVAL),
        ..SubscriptionsConfig::default()
    };
    tweak(&mut subscriptions);
    let config = Config {
        database: graphpg::config::DatabaseConfig {
            schemas: vec!["live_test".to_string()],
            ..Default::default()
        },
        subscriptions,
        ..Config::default()
    };
    let c = client().await;
    let catalog = CatalogInspector::new(&c)
        .inspect_schema_with_config("live_test", &config)
        .await
        .expect("inspect live_test");
    let schema = SchemaBuilder::build(&catalog).expect("schema");
    let auth = Authenticator::new(&auth_config(), Some(SECRET))
        .await
        .expect("auth configuration");
    let state = Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 8).expect("pool"),
            schema,
            catalog,
            "live_test".to_string(),
            Default::default(),
        )
        .with_config(config)
        .with_auth(auth),
    );
    state.subscriptions.as_ref().expect("hub").start(&state);
    (create_router(state.clone()), state)
}

async fn app() -> (axum::Router, Arc<ServerState>) {
    app_with(|_| {}).await
}

fn request(query: &str, variables: Value, accept: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, accept);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder
        .body(Body::from(
            json!({ "query": query, "variables": variables }).to_string(),
        ))
        .unwrap()
}

/// The events of an SSE response, as they arrive.
struct Events {
    body: Body,
    buffer: String,
}

impl Events {
    fn new(body: Body) -> Self {
        Self {
            body,
            buffer: String::new(),
        }
    }

    /// The next event `(event, data)` within `wait` (keep-alive comments skipped); `None`
    /// when none came or the stream ended.
    async fn next_within(&mut self, wait: Duration) -> Option<(String, Value)> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                let mut event = String::new();
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event: ") {
                        event = v.to_string();
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data.push_str(v);
                    }
                }
                if event.is_empty() {
                    continue;
                }
                return Some((event, serde_json::from_str(&data).unwrap_or(Value::Null)));
            }
            let frame = tokio::time::timeout_at(deadline, self.body.frame())
                .await
                .ok()??;
            if let Ok(data) = frame.ok()?.into_data() {
                self.buffer.push_str(&String::from_utf8_lossy(&data));
            }
        }
    }

    async fn next(&mut self) -> (String, Value) {
        self.next_within(WAIT).await.expect("an event")
    }
}

async fn sse(app: &axum::Router, query: &str, variables: Value, token: Option<&str>) -> Events {
    let response = app
        .clone()
        .oneshot(request(query, variables, "text/event-stream", token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    Events::new(response.into_body())
}

#[tokio::test]
async fn the_subscription_type_mirrors_the_query_type_only_when_enabled() {
    let (_, state) = app().await;
    let sdl = SchemaBuilder::generate_sdl(&state.current_catalog());
    let section = |name: &str| {
        let start = sdl.find(&format!("type {name} {{")).expect(name);
        let end = start + sdl[start..].find("\n}").unwrap();
        sdl[start..end]
            .lines()
            .skip(1)
            .map(str::trim)
            .filter(|l| !l.starts_with('"'))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(section("Query"), section("Subscription"));

    let mut catalog = (*state.current_catalog()).clone();
    catalog.subscriptions_enabled = false;
    assert!(!SchemaBuilder::generate_sdl(&catalog).contains("type Subscription"));
}

#[tokio::test]
async fn sse_sends_the_result_then_each_change() {
    let (app, _) = app().await;
    let mut events = sse(
        &app,
        "subscription($id: Int!) { item(id: $id) { name qty } }",
        json!({ "id": 1 }),
        None,
    )
    .await;
    let (event, first) = events.next().await;
    assert_eq!(event, "next");
    assert_eq!(
        first,
        json!({ "data": { "item": { "name": "item 1", "qty": 1 } } })
    );

    // Nothing changed: nothing is sent
    assert!(events.next_within(INTERVAL * 5).await.is_none());

    exec("UPDATE live_test.items SET qty = 11 WHERE id = 1").await;
    let (_, changed) = events.next().await;
    assert_eq!(changed["data"]["item"]["qty"], 11);

    // An update leaving the result as it was sends nothing either
    exec("UPDATE live_test.items SET name = name WHERE id = 1").await;
    assert!(events.next_within(INTERVAL * 5).await.is_none());
}

#[tokio::test]
async fn subscriptions_of_one_query_with_different_variables_get_their_own_results() {
    let (app, _) = app().await;
    let mut streams = Vec::new();
    for id in 10..15 {
        let mut events = sse(
            &app,
            "subscription($id: Int!) { item(id: $id) { databaseId qty } }",
            json!({ "id": id }),
            None,
        )
        .await;
        let (_, first) = events.next().await;
        assert_eq!(first["data"]["item"]["databaseId"], id);
        streams.push(events);
    }
    exec("UPDATE live_test.items SET qty = 1200 WHERE id = 12").await;
    let (_, changed) = streams[2].next().await;
    assert_eq!(
        changed["data"]["item"],
        json!({ "databaseId": 12, "qty": 1200 })
    );
    for (i, events) in streams.iter_mut().enumerate() {
        if i != 2 {
            assert!(
                events.next_within(INTERVAL * 4).await.is_none(),
                "item {}",
                10 + i
            );
        }
    }
}

#[tokio::test]
async fn connections_filters_and_aggregates_are_batched_like_any_query() {
    let (app, _) = app().await;
    let query = "subscription($min: Int!, $max: Int!) { itemsConnection(first: 2, orderBy: [QTY_DESC], filter: { qty: { greaterThanOrEqualTo: $min, lessThanOrEqualTo: $max } }) { totalCount nodes { databaseId qty } aggregates { sum { qty } } } }";
    let mut low = sse(&app, query, json!({ "min": 70, "max": 74 }), None).await;
    let mut high = sse(&app, query, json!({ "min": 75, "max": 79 }), None).await;
    let (_, first) = low.next().await;
    let connection = &first["data"]["itemsConnection"];
    assert_eq!(connection["totalCount"], 5);
    assert_eq!(
        connection["nodes"],
        json!([{ "databaseId": 74, "qty": 74 }, { "databaseId": 73, "qty": 73 }])
    );
    assert_eq!(
        connection["aggregates"]["sum"]["qty"],
        json!(70 + 71 + 72 + 73 + 74)
    );
    high.next().await;

    // A row moving from one range to the other changes both results
    exec("UPDATE live_test.items SET qty = 79 WHERE id = 71").await;
    let (_, low_now) = low.next().await;
    assert_eq!(low_now["data"]["itemsConnection"]["totalCount"], 4);
    let (_, high_now) = high.next().await;
    assert_eq!(high_now["data"]["itemsConnection"]["totalCount"], 6);
    assert_eq!(high_now["data"]["itemsConnection"]["nodes"][0]["qty"], 79);
}

#[tokio::test]
async fn each_user_sees_their_own_rows_under_row_level_security() {
    let (app, _) = app().await;
    let query = "subscription { notesConnection(orderBy: [ID_ASC]) { nodes { databaseId body } } }";
    let mut alice = sse(&app, query, json!({}), Some(&token("alice", 300))).await;
    let mut bob = sse(&app, query, json!({}), Some(&token("bob", 300))).await;
    let (_, a) = alice.next().await;
    let (_, b) = bob.next().await;
    let ids = |v: &Value| -> Vec<i64> {
        v["data"]["notesConnection"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["databaseId"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(ids(&a), vec![1, 3]);
    assert_eq!(ids(&b), vec![2]);

    // A change to Alice's note reaches Alice only
    exec("UPDATE live_test.notes SET body = 'a1 edited' WHERE id = 1").await;
    let (_, a) = alice.next().await;
    assert_eq!(
        a["data"]["notesConnection"]["nodes"][0]["body"],
        "a1 edited"
    );
    assert!(bob.next_within(INTERVAL * 5).await.is_none());

    // Rows that become Bob's appear for him and leave Alice's result
    exec("UPDATE live_test.notes SET owner = 'bob' WHERE id = 3").await;
    let (_, b) = bob.next().await;
    assert_eq!(ids(&b), vec![2, 3]);
    let (_, a) = alice.next().await;
    assert_eq!(ids(&a), vec![1]);
}

#[tokio::test]
async fn claims_are_kept_unless_the_query_reads_only_tables_without_row_level_security() {
    ensure_fixture().await;
    let c = client().await;
    async fn free(c: &tokio_postgres::Client, names: &[(&str, &str)]) -> bool {
        let (schemas, names): (Vec<String>, Vec<String>) = names
            .iter()
            .map(|(s, n)| (s.to_string(), n.to_string()))
            .unzip();
        c.query_one(batch::CLAIMS_FREE_SQL, &[&schemas, &names])
            .await
            .unwrap()
            .get::<_, bool>(0)
    }
    assert!(free(&c, &[("live_test", "items")]).await);
    assert!(free(&c, &[("live_test", "items"), ("live_test", "mood")]).await);
    assert!(free(&c, &[]).await);
    assert!(
        !free(&c, &[("live_test", "notes")]).await,
        "a table with RLS"
    );
    assert!(!free(&c, &[("live_test", "items"), ("live_test", "notes")]).await);
    assert!(!free(&c, &[("live_test", "item_names")]).await, "a view");
    assert!(
        !free(&c, &[("live_test", "checked_total")]).await,
        "a function"
    );
    assert!(!free(&c, &[("live_test", "nothing")]).await, "unknown");
}

#[tokio::test]
async fn users_of_a_query_reading_no_row_level_security_share_results() {
    let (app, _) = app().await;
    let query = "subscription { item(id: 20) { qty } }";
    let mut alice = sse(&app, query, json!({}), Some(&token("alice", 300))).await;
    let mut bob = sse(&app, query, json!({}), Some(&token("bob", 300))).await;
    alice.next().await;
    bob.next().await;
    // A round to learn the query reads no table with RLS, then a change
    tokio::time::sleep(INTERVAL * 3).await;
    exec("UPDATE live_test.items SET qty = 2000 WHERE id = 20").await;
    assert_eq!(alice.next().await.1["data"]["item"]["qty"], 2000);
    assert_eq!(bob.next().await.1["data"]["item"]["qty"], 2000);
}

#[tokio::test]
async fn multipart_subscriptions_send_payload_parts() {
    let (app, _) = app().await;
    let response = app
        .clone()
        .oneshot(request(
            "subscription { item(id: 30) { qty } }",
            json!({}),
            "multipart/mixed;subscriptionSpec=\"1.0\",application/json",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "multipart/mixed;boundary=\"graphql\";subscriptionSpec=\"1.0\""
    );
    let mut body = response.into_body();
    let mut text = String::new();
    while !text.contains("\"qty\":30") {
        let frame = tokio::time::timeout(WAIT, body.frame())
            .await
            .expect("a part")
            .unwrap()
            .unwrap();
        text.push_str(&String::from_utf8_lossy(&frame.into_data().unwrap()));
    }
    assert!(
        text.starts_with("\r\n--graphql\r\ncontent-type: application/json; charset=utf-8\r\n\r\n")
    );
    assert!(text.contains(r#"{"payload":{"data":{"item":{"qty":30}}}}"#));
}

#[tokio::test]
async fn a_subscription_needs_a_streaming_response() {
    let (app, _) = app().await;
    let response = app
        .clone()
        .oneshot(request(
            "subscription { item(id: 1) { qty } }",
            json!({}),
            "application/json",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn a_query_asked_as_an_event_stream_is_one_next_event() {
    let (app, _) = app().await;
    let mut events = sse(&app, "{ item(id: 2) { qty } }", json!({}), None).await;
    assert_eq!(
        events.next().await,
        (
            "next".to_string(),
            json!({ "data": { "item": { "qty": 2 } } })
        )
    );
    assert_eq!(events.next().await.0, "complete");
}

#[tokio::test]
async fn subscriptions_per_client_are_limited() {
    let (app, _) = app_with(|s| s.max_per_subject = 2).await;
    let carol = token("carol", 300);
    let query = "subscription { item(id: 40) { qty } }";
    let _one = sse(&app, query, json!({}), Some(&carol)).await;
    let _two = sse(&app, query, json!({}), Some(&carol)).await;
    let response = app
        .clone()
        .oneshot(request(query, json!({}), "text/event-stream", Some(&carol)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["errors"][0]["extensions"]["code"],
        "TOO_MANY_SUBSCRIPTIONS"
    );

    // Another client is not counted with Carol
    let _dave = sse(&app, query, json!({}), Some(&token("dave", 300))).await;

    // A closed subscription frees its place
    drop(_one);
    tokio::time::sleep(INTERVAL * 3).await;
    let _three = sse(&app, query, json!({}), Some(&carol)).await;
}

#[tokio::test]
async fn a_subscription_ends_when_its_token_expires() {
    let (app, _) = app().await;
    let mut events = sse(
        &app,
        "subscription { item(id: 50) { qty } }",
        json!({}),
        Some(&token("erin", 2)),
    )
    .await;
    events.next().await;
    let (event, expired) = events.next().await;
    assert_eq!(event, "next");
    assert_eq!(expired["errors"][0]["extensions"]["code"], "TOKEN_EXPIRED");
    assert_eq!(events.next().await.0, "complete");
}

#[tokio::test]
async fn a_failing_query_sends_its_error_once_and_recovers() {
    let (app, _) = app().await;
    let mut events = sse(&app, "subscription { checkedTotal }", json!({}), None).await;
    assert_eq!(events.next().await.1["data"]["checkedTotal"], 90 + 91 + 92);

    exec("UPDATE live_test.items SET qty = -1 WHERE id = 91").await;
    let (_, failed) = events.next().await;
    assert!(failed["data"].is_null());
    assert!(failed["errors"][0]["message"].is_string());
    // The same failure is not sent again
    assert!(events.next_within(INTERVAL * 5).await.is_none());

    exec("UPDATE live_test.items SET qty = 91 WHERE id = 91").await;
    assert_eq!(events.next().await.1["data"]["checkedTotal"], 90 + 91 + 92);
}

/// A server on a local port, for WebSocket clients.
async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("ws://{addr}/graphql")
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(url: &str, protocol: &str) -> Socket {
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", protocol.parse().unwrap());
    tokio_tungstenite::connect_async(request)
        .await
        .expect("connect")
        .0
}

async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .unwrap();
}

/// The next message that is not a ping: JSON, or the close frame's code.
async fn receive(socket: &mut Socket) -> Result<Value, u16> {
    loop {
        let message = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("a message")
            .expect("open")
            .expect("a frame");
        match message {
            Message::Text(text) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] != "ping" {
                    return Ok(value);
                }
            }
            Message::Close(frame) => return Err(frame.map(|f| f.code.into()).unwrap_or(1005)),
            _ => {}
        }
    }
}

#[tokio::test]
async fn graphql_transport_ws_subscriptions() {
    let (app, _) = app().await;
    let url = serve(app).await;
    let mut socket = connect(&url, "graphql-transport-ws").await;
    send(
        &mut socket,
        json!({ "type": "connection_init", "payload": { "Authorization": format!("Bearer {}", token("alice", 300)) } }),
    )
    .await;
    assert_eq!(
        receive(&mut socket).await.unwrap()["type"],
        "connection_ack"
    );

    send(&mut socket, json!({ "type": "ping" })).await;
    assert_eq!(receive(&mut socket).await.unwrap()["type"], "pong");

    send(
        &mut socket,
        json!({ "id": "s1", "type": "subscribe", "payload": { "query": "subscription($id: Int!) { item(id: $id) { qty } }", "variables": { "id": 60 } } }),
    )
    .await;
    let next = receive(&mut socket).await.unwrap();
    assert_eq!(
        next,
        json!({ "id": "s1", "type": "next", "payload": { "data": { "item": { "qty": 60 } } } })
    );

    exec("UPDATE live_test.items SET qty = 600 WHERE id = 60").await;
    let next = receive(&mut socket).await.unwrap();
    assert_eq!(next["payload"]["data"]["item"]["qty"], 600);

    // Queries are answered over the socket too, with the connection's token
    send(
        &mut socket,
        json!({ "id": "q1", "type": "subscribe", "payload": { "query": "{ notesConnection(orderBy: [ID_ASC]) { nodes { databaseId } } }" } }),
    )
    .await;
    let answer = receive(&mut socket).await.unwrap();
    assert_eq!(answer["id"], "q1");
    assert_eq!(
        answer["payload"]["data"]["notesConnection"]["nodes"][0]["databaseId"],
        1
    );
    assert_eq!(
        receive(&mut socket).await.unwrap(),
        json!({ "id": "q1", "type": "complete" })
    );

    // Invalid operations are `error` messages
    send(
        &mut socket,
        json!({ "id": "e1", "type": "subscribe", "payload": { "query": "subscription { nope }" } }),
    )
    .await;
    let error = receive(&mut socket).await.unwrap();
    assert_eq!(error["type"], "error");
    assert!(error["payload"][0]["message"].is_string());

    // Completed by the client: no more results
    send(&mut socket, json!({ "id": "s1", "type": "complete" })).await;
    tokio::time::sleep(INTERVAL * 2).await;
    exec("UPDATE live_test.items SET qty = 6000 WHERE id = 60").await;
    send(&mut socket, json!({ "type": "ping" })).await;
    assert_eq!(receive(&mut socket).await.unwrap()["type"], "pong");
    assert!(
        tokio::time::timeout(INTERVAL * 5, socket.next())
            .await
            .is_err(),
        "nothing after complete"
    );
}

#[tokio::test]
async fn graphql_transport_ws_protocol_errors_close_the_socket() {
    let (app, _) = app().await;
    let url = serve(app).await;

    // Subscribing before connection_init
    let mut socket = connect(&url, "graphql-transport-ws").await;
    send(
        &mut socket,
        json!({ "id": "1", "type": "subscribe", "payload": { "query": "subscription { item(id: 1) { qty } }" } }),
    )
    .await;
    assert_eq!(receive(&mut socket).await, Err(4401));

    // A token that does not verify
    let mut socket = connect(&url, "graphql-transport-ws").await;
    send(
        &mut socket,
        json!({ "type": "connection_init", "payload": { "Authorization": "Bearer nope" } }),
    )
    .await;
    assert_eq!(receive(&mut socket).await, Err(4403));

    // The legacy protocol: the upgrade selects no subprotocol (the socket is then closed
    // with 4406)
    let mut request = url.as_str().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "graphql-ws".parse().unwrap());
    let refused = tokio_tungstenite::connect_async(request).await.unwrap_err();
    assert!(refused.to_string().contains("SubProtocol"), "{refused}");

    // Two operations with one id
    let mut socket = connect(&url, "graphql-transport-ws").await;
    send(&mut socket, json!({ "type": "connection_init" })).await;
    assert_eq!(
        receive(&mut socket).await.unwrap()["type"],
        "connection_ack"
    );
    let subscribe = json!({ "id": "x", "type": "subscribe", "payload": { "query": "subscription { item(id: 1) { qty } }" } });
    send(&mut socket, subscribe.clone()).await;
    send(&mut socket, subscribe).await;
    loop {
        match receive(&mut socket).await {
            Ok(_) => continue,
            Err(code) => {
                assert_eq!(code, 4409);
                break;
            }
        }
    }
}

#[tokio::test]
async fn graphql_transport_ws_closes_when_the_token_expires() {
    let (app, _) = app().await;
    let url = serve(app).await;
    let mut socket = connect(&url, "graphql-transport-ws").await;
    send(
        &mut socket,
        json!({ "type": "connection_init", "payload": { "Authorization": format!("Bearer {}", token("frank", 2)) } }),
    )
    .await;
    assert_eq!(
        receive(&mut socket).await.unwrap()["type"],
        "connection_ack"
    );
    let closed = loop {
        match receive(&mut socket).await {
            Ok(_) => continue,
            Err(code) => break code,
        }
    };
    assert_eq!(closed, 4403);
}

#[tokio::test]
async fn graphql_transport_ws_takes_apollo_routers_connection_params() {
    let (app, _) = app().await;
    let url = serve(app).await;
    let mut socket = connect(&url, "graphql-transport-ws").await;
    send(
        &mut socket,
        json!({ "type": "connection_init", "payload": { "connectionParams": { "token": token("alice", 300) } } }),
    )
    .await;
    assert_eq!(
        receive(&mut socket).await.unwrap()["type"],
        "connection_ack"
    );
    send(
        &mut socket,
        json!({ "id": "n", "type": "subscribe", "payload": { "query": "subscription { notesConnection(orderBy: [ID_ASC]) { nodes { owner } } }" } }),
    )
    .await;
    let next = receive(&mut socket).await.unwrap();
    let owners: Vec<&str> = next["payload"]["data"]["notesConnection"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["owner"].as_str().unwrap())
        .collect();
    assert!(
        !owners.is_empty() && owners.iter().all(|o| *o == "alice"),
        "{owners:?}"
    );
}
