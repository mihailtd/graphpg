//! How subscriptions reach clients:
//!
//! - **WebSocket**, the `graphql-transport-ws` protocol (the `graphql-ws` library's, which
//!   Apollo Router uses with subgraphs): `GET /graphql` upgraded. The access token comes in
//!   `connection_init`'s payload (`{"Authorization": "Bearer …"}`), or the upgrade request's
//!   `Authorization` header. Queries and mutations can be sent over it too.
//! - **GraphQL over SSE** (the `graphql-sse` protocol's distinct connections mode): `POST
//!   /graphql` (or `GET`) with `Accept: text/event-stream`; `next` events, then `complete`.
//! - **Multipart HTTP** (Apollo's multipart subscriptions): `POST /graphql` with `Accept:
//!   multipart/mixed;subscriptionSpec="1.0"`; one JSON part per result (`{"payload": …}`).
//!
//! A subscription ends when its token expires: close code `4403` over WebSocket, an error
//! (`TOKEN_EXPIRED`) over HTTP. The client subscribes again with a new token.
use super::{client_key, compile, Hub, Rejection, Subscription};
use crate::server::http::{GraphQLError, GraphQLRequest, ServerState};
use crate::server::AuthSessionContext;
use apollo_compiler::ast::OperationType;
use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// The WebSocket subprotocol.
pub const PROTOCOL: &str = "graphql-transport-ws";

/// The streaming responses a request accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    Sse,
    Multipart,
}

/// The stream `headers` ask for (`Accept`): multipart when it names `subscriptionSpec`, else
/// SSE or multipart by media type; `None` without either.
pub fn stream_format(headers: &HeaderMap) -> Option<StreamFormat> {
    let accept = accept(headers);
    if accept.contains("multipart/mixed") && accept.contains("subscriptionspec") {
        Some(StreamFormat::Multipart)
    } else if accept.contains("text/event-stream") {
        Some(StreamFormat::Sse)
    } else if accept.contains("multipart/mixed") {
        Some(StreamFormat::Multipart)
    } else {
        None
    }
}

/// Whether `headers` accept only an event stream (not JSON): then a query's response is sent
/// as one SSE `next` event, as GraphQL over SSE has it.
pub fn wants_only_sse(headers: &HeaderMap) -> bool {
    let accept = accept(headers);
    accept.contains("text/event-stream")
        && !accept.contains("json")
        && !accept.contains("*/*")
        && !accept.contains("multipart/mixed")
}

fn accept(headers: &HeaderMap) -> String {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",")
        .to_ascii_lowercase()
}

/// A successful JSON response (a query's or a mutation's) as GraphQL over SSE sends it: one
/// `next` event, then `complete`. Anything else (a rejected request keeps its status and
/// JSON body) is left as it is.
pub async fn json_as_sse(response: Response) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.contains("json"));
    if !is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut text = sse_event("next", &String::from_utf8_lossy(&bytes));
    text.push_str(&sse_event("complete", ""));
    sse_headers(&mut parts.headers);
    Response::from_parts(parts, Body::from(text))
}

fn sse_event(event: &str, data: &str) -> String {
    // A newline would end the `data` line: JSON has none outside strings
    format!("event: {event}\ndata: {}\n\n", data.replace('\n', ""))
}

fn sse_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // Nginx would buffer the stream otherwise
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers.remove(header::CONTENT_LENGTH);
}

/// Seconds left before `expires_at` (seconds since the Unix epoch); zero once past.
fn until(expires_at: u64) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Duration::from_secs(expires_at.saturating_sub(now))
}

fn token_expired() -> GraphQLError {
    GraphQLError {
        message: "the access token expired: subscribe again with a new one".to_string(),
        path: None,
        extensions: Some(serde_json::json!({ "code": "TOKEN_EXPIRED" })),
    }
}

/// A subscription's results as an HTTP stream (`format`), until the client goes away or its
/// token expires.
pub async fn http_stream(
    state: Arc<ServerState>,
    hub: Arc<Hub>,
    query: Arc<crate::compiler::CompiledQuery>,
    auth: Option<AuthSessionContext>,
    client: String,
    format: StreamFormat,
) -> Response {
    let subscription = match hub.subscribe(&state, query, auth, client).await {
        Ok(subscription) => subscription,
        Err(rejection) => return rejection_response(rejection),
    };
    let keep_alive = hub.config().keep_alive.0;
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::convert::Infallible>>(4);
    tokio::spawn(async move {
        let Subscription {
            mut updates,
            expires_at,
            _guard,
        } = subscription;
        let expiry = async {
            match expires_at {
                Some(at) => tokio::time::sleep(until(at)).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(expiry);
        let mut ticks =
            tokio::time::interval_at(tokio::time::Instant::now() + keep_alive, keep_alive);
        if format == StreamFormat::Multipart {
            // The preamble: the first part's delimiter must start a line
            if tx.send(Ok(Bytes::from_static(b"\r\n"))).await.is_err() {
                return;
            }
        }
        loop {
            let chunk = tokio::select! {
                update = updates.recv() => match update {
                    Some(response) => match format {
                        StreamFormat::Sse => sse_event("next", &response.to_string()),
                        StreamFormat::Multipart => multipart_part(&serde_json::json!({ "payload": response })),
                    },
                    None => break,
                },
                _ = ticks.tick() => match format {
                    StreamFormat::Sse => ":\n\n".to_string(),
                    StreamFormat::Multipart => multipart_part(&serde_json::json!({})),
                },
                // The client went away: the subscription ends (its guard is dropped)
                _ = tx.closed() => return,
                _ = &mut expiry => {
                    let last = match format {
                        StreamFormat::Sse => sse_event(
                            "next",
                            &serde_json::json!({ "errors": [token_expired()] }).to_string(),
                        ),
                        StreamFormat::Multipart => multipart_part(
                            &serde_json::json!({ "payload": null, "errors": [token_expired()] }),
                        ),
                    };
                    let _ = tx.send(Ok(Bytes::from(last))).await;
                    break;
                }
            };
            if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return;
            }
        }
        let end = match format {
            StreamFormat::Sse => sse_event("complete", ""),
            StreamFormat::Multipart => "--graphql--\r\n".to_string(),
        };
        let _ = tx.send(Ok(Bytes::from(end))).await;
    });
    let body = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    let mut response = Response::new(Body::from_stream(body));
    match format {
        StreamFormat::Sse => sse_headers(response.headers_mut()),
        StreamFormat::Multipart => {
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(
                    "multipart/mixed;boundary=\"graphql\";subscriptionSpec=\"1.0\"",
                ),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
        }
    }
    response
}

fn multipart_part(json: &serde_json::Value) -> String {
    format!("--graphql\r\ncontent-type: application/json; charset=utf-8\r\n\r\n{json}\r\n")
}

fn rejection_response(rejection: Rejection) -> Response {
    let status = if rejection.status == StatusCode::OK {
        StatusCode::BAD_REQUEST
    } else {
        rejection.status
    };
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/graphql-response+json"),
        )],
        serde_json::json!({ "errors": rejection.errors }).to_string(),
    )
        .into_response()
}

/// A request for a subscription, prepared: its compiled query, or not a subscription.
enum Prepared {
    Subscription(Arc<crate::compiler::CompiledQuery>),
    /// A query or mutation: answered as over HTTP.
    Other,
}

/// Resolves, parses and validates `req` (with the caches requests use), and compiles it
/// when it is a subscription.
fn prepare(state: &ServerState, req: &GraphQLRequest) -> Result<Prepared, Rejection> {
    let query = crate::server::persisted::resolve(
        state.persisted.as_deref(),
        req.query.as_deref(),
        req.extensions.as_ref(),
        req.document_id.as_deref(),
    )
    .map_err(|r| Rejection::new(r.status, r.code, r.message))?;
    let doc = match state.cache.document(&query) {
        Some(doc) => doc,
        None => {
            let schema = state.current_schema();
            let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
                &schema,
                &*query,
                "request.graphql",
            )
            .map_err(|err| {
                Rejection::new(
                    StatusCode::BAD_REQUEST,
                    "GRAPHQL_VALIDATION_FAILED",
                    err.to_string(),
                )
            })?;
            let doc = Arc::new(doc);
            state.cache.store_document(&query, doc.clone());
            doc
        }
    };
    let operation = doc
        .operations
        .get(req.operation_name.as_deref())
        .map_err(|err| {
            Rejection::new(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                err.to_graphql_error(&doc.sources).message,
            )
        })?;
    if operation.operation_type != OperationType::Subscription {
        return Ok(Prepared::Other);
    }
    compile(
        state,
        &query,
        &doc,
        operation,
        req.operation_name.as_deref(),
        req.variables.as_ref(),
    )
    .map(Prepared::Subscription)
}

/// `GET /graphql` upgraded to a WebSocket speaking `graphql-transport-ws`.
pub fn websocket(
    state: Arc<ServerState>,
    hub: Arc<Hub>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    peer: Option<IpAddr>,
) -> Response {
    let ws = ws.protocols([PROTOCOL]);
    let speaks = ws.selected_protocol().is_some();
    ws.on_upgrade(move |socket| async move {
        if !speaks {
            close(
                socket,
                4406,
                "Subprotocol not acceptable: use graphql-transport-ws",
            )
            .await;
            return;
        }
        Connection {
            state,
            hub,
            headers,
            peer,
        }
        .run(socket)
        .await;
    })
}

async fn close(mut socket: WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// What the tasks of a connection send through it.
enum Outgoing {
    Message(serde_json::Value),
    /// Operation `id` finished: it is no longer running.
    Done(String),
}

struct Connection {
    state: Arc<ServerState>,
    hub: Arc<Hub>,
    /// The upgrade request's headers: its `Authorization` is used when `connection_init`
    /// carries none.
    headers: HeaderMap,
    peer: Option<IpAddr>,
}

impl Connection {
    async fn run(self, socket: WebSocket) {
        let (mut sink, mut stream) = socket.split();
        let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(64);
        let config = self.hub.config().clone();
        let mut auth: Option<Option<AuthSessionContext>> = None;
        let mut running: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        let init_deadline = tokio::time::sleep(config.init_timeout.0);
        tokio::pin!(init_deadline);
        let mut expiry: std::pin::Pin<Box<tokio::time::Sleep>> =
            Box::pin(tokio::time::sleep(Duration::from_secs(u32::MAX as u64)));
        let mut ticks = tokio::time::interval_at(
            tokio::time::Instant::now() + config.keep_alive.0,
            config.keep_alive.0,
        );

        let closing: Option<(u16, String)> = loop {
            tokio::select! {
                incoming = stream.next() => {
                    let text = match incoming {
                        Some(Ok(Message::Text(text))) => text.to_string(),
                        Some(Ok(Message::Binary(_))) => break Some((4400, "Binary messages are not supported".to_string())),
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break None,
                        Some(Ok(_)) => continue,
                    };
                    let Ok(message) = serde_json::from_str::<serde_json::Value>(&text) else {
                        break Some((4400, "Invalid message received".to_string()));
                    };
                    match message.get("type").and_then(|t| t.as_str()) {
                        Some("connection_init") => {
                            if auth.is_some() {
                                break Some((4429, "Too many initialisation requests".to_string()));
                            }
                            let headers = self.init_headers(message.get("payload"));
                            match self.state.authenticate(&headers).await {
                                Ok(ctx) => {
                                    if let Some(at) = ctx.as_ref().and_then(|c| c.expires_at) {
                                        expiry.as_mut().reset(tokio::time::Instant::now() + until(at));
                                    }
                                    auth = Some(ctx);
                                    if send(&mut sink, serde_json::json!({ "type": "connection_ack" })).await.is_err() {
                                        break None;
                                    }
                                }
                                Err(err) => break Some((4403, format!("Forbidden: {}", err.message()))),
                            }
                        }
                        Some("ping") => {
                            let mut pong = serde_json::json!({ "type": "pong" });
                            if let Some(payload) = message.get("payload") {
                                pong["payload"] = payload.clone();
                            }
                            if send(&mut sink, pong).await.is_err() {
                                break None;
                            }
                        }
                        Some("pong") => {}
                        Some("subscribe") => {
                            let Some(ctx) = &auth else {
                                break Some((4401, "Unauthorized".to_string()));
                            };
                            let (Some(id), Some(payload)) = (
                                message.get("id").and_then(|v| v.as_str()),
                                message.get("payload"),
                            ) else {
                                break Some((4400, "Invalid message received".to_string()));
                            };
                            if running.contains_key(id) {
                                break Some((4409, format!("Subscriber for {id} already exists")));
                            }
                            let Ok(req) = serde_json::from_value::<GraphQLRequest>(payload.clone()) else {
                                break Some((4400, "Invalid message received".to_string()));
                            };
                            let task = self.operation(id.to_string(), req, ctx.clone(), out_tx.clone());
                            running.insert(id.to_string(), tokio::spawn(task));
                        }
                        Some("complete") => {
                            if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
                                if let Some(task) = running.remove(id) {
                                    task.abort();
                                }
                            }
                        }
                        _ => break Some((4400, "Invalid message received".to_string())),
                    }
                }
                Some(outgoing) = out_rx.recv() => match outgoing {
                    Outgoing::Message(message) => {
                        if send(&mut sink, message).await.is_err() {
                            break None;
                        }
                    }
                    Outgoing::Done(id) => {
                        running.remove(&id);
                    }
                },
                _ = ticks.tick() => {
                    if send(&mut sink, serde_json::json!({ "type": "ping" })).await.is_err() {
                        break None;
                    }
                }
                _ = &mut init_deadline, if auth.is_none() => {
                    break Some((4408, "Connection initialisation timeout".to_string()));
                }
                _ = &mut expiry => {
                    break Some((4403, "Forbidden: the access token expired, connect again with a new one".to_string()));
                }
            }
        };
        // Ends every operation (a subscription ends with its task)
        for (_, task) in running {
            task.abort();
        }
        if let Some((code, reason)) = closing {
            let _ = sink
                .send(Message::Close(Some(CloseFrame {
                    code,
                    reason: reason.into(),
                })))
                .await;
        }
    }

    /// The headers a connection authenticates with: the access token of `connection_init`'s
    /// payload, else the upgrade request's `Authorization`. The payload's token is its
    /// `Authorization` or `token` member, at its top or under `headers` or `connectionParams`
    /// (Apollo Router's); without a scheme, it is a bearer token.
    fn init_headers(&self, payload: Option<&serde_json::Value>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let from_payload = payload.and_then(|p| {
            let find = |v: &serde_json::Value| {
                v.as_object().and_then(|o| {
                    o.iter()
                        .find(|(k, _)| {
                            k.eq_ignore_ascii_case("authorization")
                                || k.eq_ignore_ascii_case("token")
                        })
                        .and_then(|(_, v)| v.as_str().map(str::to_string))
                })
            };
            find(p)
                .or_else(|| p.get("headers").and_then(find))
                .or_else(|| p.get("connectionParams").and_then(find))
        });
        match from_payload {
            Some(value) => {
                let value = if value.contains(' ') {
                    value
                } else {
                    format!("Bearer {value}")
                };
                if let Ok(value) = HeaderValue::from_str(&value) {
                    headers.insert(header::AUTHORIZATION, value);
                }
            }
            None => {
                if let Some(value) = self.headers.get(header::AUTHORIZATION) {
                    headers.insert(header::AUTHORIZATION, value.clone());
                }
            }
        }
        headers
    }

    /// Runs operation `id`: a subscription's results as `next` messages until it is
    /// completed, or a query's or mutation's response, then `complete`.
    fn operation(
        &self,
        id: String,
        req: GraphQLRequest,
        auth: Option<AuthSessionContext>,
        out: mpsc::Sender<Outgoing>,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let state = self.state.clone();
        let hub = self.hub.clone();
        let peer = self.peer;
        async move {
            let message = |kind: &str, payload: Option<serde_json::Value>| {
                let mut m = serde_json::json!({ "id": id, "type": kind });
                if let Some(payload) = payload {
                    m["payload"] = payload;
                }
                Outgoing::Message(m)
            };
            match prepare(&state, &req) {
                Err(rejection) => {
                    let errors = serde_json::to_value(&rejection.errors).unwrap_or_default();
                    let _ = out.send(message("error", Some(errors))).await;
                }
                Ok(Prepared::Subscription(query)) => {
                    let client = client_key(auth.as_ref(), peer);
                    match hub.subscribe(&state, query, auth, client).await {
                        Err(rejection) => {
                            let errors =
                                serde_json::to_value(&rejection.errors).unwrap_or_default();
                            let _ = out.send(message("error", Some(errors))).await;
                        }
                        Ok(mut subscription) => {
                            while let Some(response) = subscription.updates.recv().await {
                                if out.send(message("next", Some(response))).await.is_err() {
                                    return;
                                }
                            }
                            let _ = out.send(message("complete", None)).await;
                        }
                    }
                }
                Ok(Prepared::Other) => {
                    let (status, response) =
                        crate::server::http::execute_json(state, auth, req, peer).await;
                    let failed = response.get("data").is_none();
                    if failed && !status.is_success() {
                        let errors = response.get("errors").cloned().unwrap_or_default();
                        let _ = out.send(message("error", Some(errors))).await;
                    } else {
                        let _ = out.send(message("next", Some(response))).await;
                        let _ = out.send(message("complete", None)).await;
                    }
                }
            }
            let _ = out.send(Outgoing::Done(id.clone())).await;
        }
    }
}

async fn send(
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
    message: serde_json::Value,
) -> Result<(), axum::Error> {
    sink.send(Message::Text(message.to_string().into())).await
}
