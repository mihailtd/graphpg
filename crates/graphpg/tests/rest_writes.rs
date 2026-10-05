//! REST writes beyond a single row: nested writes in the shape of the GraphQL input (create,
//! connect, connectOrCreate, set, disconnect, update, delete, `MERGE`-based sync), their
//! atomicity, the optimistic version guard around them, and the row as it was
//! (`Prefer: previous`, `RETURNING OLD`).
mod common;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

struct Env {
    app: axum::Router,
    client: tokio_postgres::Client,
    /// Marks the rows of one test, so tests running side by side do not meet.
    tag: String,
    schema: &'static str,
}

/// The forum fixture (users, forums, messages).
async fn env(tag: &str) -> Env {
    common::ensure_forum_fixture().await;
    env_in("app_public", tag).await
}

/// The `rest_extras` fixture: crews and sailors, whose foreign key can be detached.
async fn crews(tag: &str) -> Env {
    common::ensure_rest_extras_fixture().await;
    env_in("rest_extras", tag).await
}

async fn env_in(schema: &'static str, tag: &str) -> Env {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema(schema)
        .await
        .unwrap();
    let graphql = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        graphql,
        catalog,
        schema.to_string(),
        DebugOptions::default(),
    );
    let env = Env {
        app: create_router(Arc::new(state)),
        client,
        tag: format!("rw_{tag}"),
        schema,
    };
    env.cleanup().await;
    env
}

impl Env {
    /// Removes what the test wrote (children first).
    async fn cleanup(&self) {
        let like = format!("{}%", self.tag);
        if self.schema == "rest_extras" {
            self.client
                .batch_execute(&format!(
                    "DELETE FROM rest_extras.sailors WHERE name LIKE '{like}' OR crew_id IN (SELECT id FROM rest_extras.crews WHERE name LIKE '{like}');
                     DELETE FROM rest_extras.crews WHERE name LIKE '{like}';"
                ))
                .await
                .unwrap();
            return;
        }
        self.client
            .batch_execute(&format!(
                "DELETE FROM app_public.messages WHERE body LIKE '{like}' OR forum_id IN (SELECT id FROM app_public.forums WHERE name LIKE '{like}') OR author_id IN (SELECT id FROM app_public.users WHERE username LIKE '{like}');
                 DELETE FROM app_public.forums WHERE name LIKE '{like}';
                 DELETE FROM app_public.users WHERE username LIKE '{like}';"
            ))
            .await
            .unwrap();
    }

    fn name(&self, what: &str) -> String {
        format!("{}_{what}", self.tag)
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let body = match body {
            Some(v) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let res = self
            .app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn post(&self, uri: &str, body: Value) -> Reply {
        self.call("POST", uri, &[], Some(body)).await
    }

    async fn get(&self, uri: &str) -> Reply {
        self.call("GET", uri, &[], None).await
    }

    /// A user, returning its key.
    async fn user(&self, what: &str) -> String {
        let reply = self
            .post("/rest/users", json!({ "username": self.name(what) }))
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["id"].as_str().unwrap().to_string()
    }

    /// The bodies of a forum's messages, ordered.
    async fn bodies(&self, forum_id: &str) -> Vec<String> {
        let reply = self
            .get(&format!(
                "/rest/messages?filter[forumId][eq]={forum_id}&order=body"
            ))
            .await;
        reply
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap().to_string())
            .collect()
    }

    async fn forums_named(&self, what: &str) -> usize {
        let name = self.name(what);
        self.get(&format!("/rest/forums?filter[name][eq]={name}"))
            .await
            .body
            .as_array()
            .unwrap()
            .len()
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    #[track_caller]
    fn problem(&self, status: StatusCode, code: &str) -> &Value {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(
            self.header("content-type"),
            Some("application/problem+json")
        );
        assert_eq!(self.body["code"], code, "{}", self.body);
        &self.body
    }
}

// ---------------------------------------------------------------------------------------------
// Nested writes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn nested_creates_write_the_whole_graph_in_one_request() {
    let e = env("create").await;
    let author = e.user("author").await;

    // a forum with its messages (to-many create)
    let forum = e
        .post(
            "/rest/forums",
            json!({
                "name": e.name("forum"),
                "messages": { "create": [
                    { "body": e.name("one"), "authorId": author },
                    { "body": e.name("two"), "authorId": author }
                ] }
            }),
        )
        .await;
    assert_eq!(forum.status, StatusCode::CREATED, "{}", forum.body);
    let forum_id = forum.body["id"].as_str().unwrap();
    assert_eq!(e.bodies(forum_id).await, [e.name("one"), e.name("two")]);

    // a message with a new author and a connected forum (to-one create and connect), at once
    let message = e
        .post(
            "/rest/messages",
            json!({
                "body": e.name("three"),
                "author": { "create": { "username": e.name("newcomer") } },
                "forum": { "connect": { "id": forum_id } }
            }),
        )
        .await;
    assert_eq!(message.status, StatusCode::CREATED, "{}", message.body);
    assert_eq!(message.body["forumId"], forum_id);
    let new_author = message.body["authorId"].as_str().unwrap();
    assert_eq!(
        e.get(&format!("/rest/users/{new_author}")).await.body["username"],
        e.name("newcomer")
    );

    // connectOrCreate: found by key, or created
    let existing = e
        .post(
            "/rest/messages",
            json!({
                "body": e.name("four"),
                "author": { "connect": { "id": author } },
                "forum": { "connectOrCreate": { "where": { "id": forum_id }, "create": { "name": e.name("unused") } } }
            }),
        )
        .await;
    assert_eq!(existing.status, StatusCode::CREATED, "{}", existing.body);
    assert_eq!(existing.body["forumId"], forum_id);
    assert_eq!(
        e.forums_named("unused").await,
        0,
        "found, so nothing was created"
    );
    assert_eq!(e.bodies(forum_id).await.len(), 4);

    // a bulk create with nested rows, one statement
    let bulk = e
        .post(
            "/rest/forums",
            json!([
                { "name": e.name("bulk_a"), "messages": { "create": [{ "body": e.name("a1"), "authorId": author }] } },
                { "name": e.name("bulk_b"), "messages": { "create": [{ "body": e.name("b1"), "authorId": author }, { "body": e.name("b2"), "authorId": author }] } }
            ]),
        )
        .await;
    assert_eq!(bulk.status, StatusCode::CREATED, "{}", bulk.body);
    let ids: Vec<&str> = bulk
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        (e.bodies(ids[0]).await.len(), e.bodies(ids[1]).await.len()),
        (1, 2)
    );
    e.cleanup().await;
}

impl Env {
    /// The names of a crew's sailors, ordered.
    async fn sailors(&self, crew: i64) -> Vec<String> {
        let reply = self
            .get(&format!(
                "/rest/sailors?filter[crewId][eq]={crew}&order=name"
            ))
            .await;
        reply
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect()
    }

    async fn sailor(&self, id: i64) -> Reply {
        self.get(&format!("/rest/sailors/{id}")).await
    }
}

#[tokio::test]
async fn nested_updates_relate_detach_replace_and_delete_children() {
    let e = crews("update").await;
    let crew = e
        .post(
            "/rest/crews",
            json!({ "id": 9101, "name": e.name("crew"), "sailors": { "create": [
        { "id": 9111, "name": e.name("keep") },
        { "id": 9112, "name": e.name("drop") }
    ] } }),
        )
        .await;
    assert_eq!(crew.status, StatusCode::CREATED, "{}", crew.body);
    let item = "/rest/crews/9101";
    let loose = e
        .post(
            "/rest/sailors",
            json!({ "id": 9113, "name": e.name("loose") }),
        )
        .await;
    assert_eq!(loose.status, StatusCode::CREATED, "{}", loose.body);
    assert_eq!(loose.body["crewId"], Value::Null);

    // connect moves an existing sailor here; update changes a child; delete removes one
    let moved = e
        .call(
            "PATCH",
            item,
            &[],
            Some(json!({ "sailors": {
            "connect": [{ "id": 9113 }],
            "update": [{ "where": { "id": 9111 }, "data": { "name": e.name("kept") } }],
            "delete": [{ "id": 9112 }]
        } })),
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.body);
    assert_eq!(e.sailors(9101).await, [e.name("kept"), e.name("loose")]);
    e.sailor(9112)
        .await
        .problem(StatusCode::NOT_FOUND, "NOT_FOUND");

    // disconnect detaches: the foreign key becomes NULL, the row stays
    let detached = e
        .call(
            "PATCH",
            item,
            &[],
            Some(json!({ "sailors": { "disconnect": [{ "id": 9113 }] } })),
        )
        .await;
    assert_eq!(detached.status, StatusCode::OK, "{}", detached.body);
    assert_eq!(e.sailors(9101).await, [e.name("kept")]);
    assert_eq!(e.sailor(9113).await.body["crewId"], Value::Null);

    // set makes the children exactly these: the others are detached, the listed ones related
    let set = e
        .call(
            "PATCH",
            item,
            &[],
            Some(json!({ "sailors": { "set": [{ "id": 9113 }] } })),
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert_eq!(e.sailors(9101).await, [e.name("loose")]);
    assert_eq!(
        e.sailor(9111).await.body["crewId"],
        Value::Null,
        "the one left out is detached, not deleted"
    );

    // create adds new ones; connectOrCreate finds one or makes one
    let created = e
        .call(
            "PATCH",
            item,
            &[],
            Some(json!({ "sailors": {
            "create": [{ "id": 9114, "name": e.name("fresh") }],
            "connectOrCreate": [
                { "where": { "id": 9111 }, "create": { "id": 9111, "name": e.name("unused") } },
                { "where": { "id": 9115 }, "create": { "id": 9115, "name": e.name("made") } }
            ]
        } })),
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert_eq!(
        e.sailors(9101).await,
        [
            e.name("fresh"),
            e.name("kept"),
            e.name("loose"),
            e.name("made")
        ]
    );
    e.cleanup().await;
}

#[tokio::test]
async fn sync_makes_the_children_exactly_the_rows_listed() {
    let e = crews("sync").await;
    let crew = e
        .post(
            "/rest/crews",
            json!({ "id": 9201, "name": e.name("crew"), "sailors": { "create": [
        { "id": 9211, "name": e.name("stay") },
        { "id": 9212, "name": e.name("change") },
        { "id": 9213, "name": e.name("go") }
    ] } }),
        )
        .await;
    assert_eq!(crew.status, StatusCode::CREATED, "{}", crew.body);
    // a sailor of another crew is none of its business
    e.post("/rest/crews", json!({ "id": 9202, "name": e.name("other"), "sailors": { "create": [{ "id": 9221, "name": e.name("other_one") }] } })).await;

    // one MERGE: a row whose key matches is updated, one with a new key is inserted, and the
    // child no row matches is deleted
    let reply = e
        .call(
            "PATCH",
            "/rest/crews/9201",
            &[],
            Some(json!({ "sailors": { "sync": [
            { "id": 9211, "name": e.name("stay") },
            { "id": 9212, "name": e.name("changed") },
            { "id": 9214, "name": e.name("added") }
        ] } })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        e.sailors(9201).await,
        [e.name("added"), e.name("changed"), e.name("stay")]
    );
    e.sailor(9213)
        .await
        .problem(StatusCode::NOT_FOUND, "NOT_FOUND");
    assert_eq!(e.sailor(9212).await.body["name"], e.name("changed"));
    assert_eq!(
        e.sailors(9202).await,
        [e.name("other_one")],
        "other crews are untouched"
    );

    // syncing with nothing deletes every child of this crew only
    let empty = e
        .call(
            "PATCH",
            "/rest/crews/9201",
            &[],
            Some(json!({ "sailors": { "sync": [] } })),
        )
        .await;
    assert_eq!(empty.status, StatusCode::OK, "{}", empty.body);
    assert!(e.sailors(9201).await.is_empty());
    assert_eq!(e.sailors(9202).await.len(), 1);

    // a failing sync changes nothing
    let bad = e
        .call(
            "PATCH",
            "/rest/crews/9202",
            &[],
            Some(json!({ "sailors": { "sync": [{ "id": 9231 }] } })),
        )
        .await;
    assert_eq!(
        bad.problem(StatusCode::UNPROCESSABLE_ENTITY, "NAME_REQUIRED")["fields"],
        json!(["input.sailors.sync[0].name"])
    );
    assert_eq!(e.sailors(9202).await, [e.name("other_one")]);
    e.cleanup().await;
}

#[tokio::test]
async fn a_failing_nested_write_rolls_back_everything() {
    let e = env("atomic").await;
    let author = e.user("author").await;

    // the second child has no body: the forum and the first child are not written either
    let reply = e
        .post(
            "/rest/forums",
            json!({ "name": e.name("forum"), "messages": { "create": [
                { "body": e.name("ok"), "authorId": author },
                { "authorId": author }
            ] } }),
        )
        .await;
    let body = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "BODY_REQUIRED");
    assert_eq!(body["fields"], json!(["input.messages.create[1].body"]));
    assert_eq!(e.forums_named("forum").await, 0);
    let count: i64 = e
        .client
        .query_one(
            "SELECT count(*) FROM app_public.messages WHERE body = $1",
            &[&e.name("ok")],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);

    // a constraint of a nested row keeps its typed code: a new author with a taken username
    let taken = e
        .post(
            "/rest/messages",
            json!({
                "body": e.name("m"),
                "author": { "create": { "username": e.name("author") } },
                "forum": { "create": { "name": e.name("tf") } }
            }),
        )
        .await;
    taken.problem(StatusCode::CONFLICT, "USERNAME_TAKEN");

    // a nested connect to a row that does not exist
    let missing = e
        .post(
            "/rest/messages",
            json!({ "body": e.name("m2"), "authorId": author, "forum": { "connect": { "id": "aaaaaaaa-0000-0000-0000-00000000dead" } } }),
        )
        .await;
    assert!(missing.status.is_client_error(), "{}", missing.body);
    assert_eq!(
        missing.header("content-type"),
        Some("application/problem+json")
    );
    let count: i64 = e
        .client
        .query_one(
            "SELECT count(*) FROM app_public.messages WHERE body = $1",
            &[&e.name("m2")],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "nothing was written");

    // nested members are validated like the rest of the body
    for body in [
        json!({ "name": e.name("f"), "messages": { "creat": [] } }),
        json!({ "name": e.name("f"), "messages": { "create": [{ "body": "x", "bogus": 1 }] } }),
        json!({ "name": e.name("f"), "messages": [] }),
    ] {
        let reply = e.post("/rest/forums", body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
    }
    assert_eq!(e.forums_named("f").await, 0);
    e.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// Optimistic concurrency around nested writes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_version_guard_covers_the_whole_nested_update() {
    let e = env("version").await;
    let author = e.user("author").await;
    let forum = e
        .post("/rest/forums", json!({ "name": e.name("forum") }))
        .await;
    let id = forum.body["id"].as_str().unwrap().to_string();
    let item = format!("/rest/forums/{id}");
    let etag = forum.header("etag").unwrap().to_string();

    // a rename moves the version on
    let renamed = e
        .call(
            "PATCH",
            &item,
            &[("if-match", &etag)],
            Some(json!({ "name": e.name("renamed") })),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    let current = renamed.header("etag").unwrap().to_string();
    assert_ne!(etag, current);

    // with the stale version, the nested write is refused whole: no message is created, the
    // forum keeps its name
    let stale = e
        .call(
            "PATCH",
            &item,
            &[("if-match", &etag)],
            Some(json!({ "name": e.name("lost"), "messages": { "create": [{ "body": e.name("never"), "authorId": author }] } })),
        )
        .await;
    stale.problem(StatusCode::PRECONDITION_FAILED, "STALE_VERSION");
    assert!(e.bodies(&id).await.is_empty());
    assert_eq!(e.get(&item).await.body["name"], e.name("renamed"));

    // with the current one it applies, nested rows and all
    let fresh = e
        .call(
            "PATCH",
            &item,
            &[("if-match", &current)],
            Some(json!({ "name": e.name("final"), "messages": { "create": [{ "body": e.name("kept"), "authorId": author }] } })),
        )
        .await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
    assert_eq!(e.bodies(&id).await, [e.name("kept")]);
    // a PUT with the stale tag is refused too
    let stale_put = e
        .call(
            "PUT",
            &item,
            &[("if-match", &current)],
            Some(json!({ "name": "x" })),
        )
        .await;
    stale_put.problem(StatusCode::PRECONDITION_FAILED, "STALE_VERSION");
    e.cleanup().await;
}

// ---------------------------------------------------------------------------------------------
// RETURNING OLD
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn previous_returns_the_row_as_it_was() {
    let e = env("previous").await;
    let created = e
        .post("/rest/forums", json!({ "name": e.name("before") }))
        .await;
    let id = created.body["id"].as_str().unwrap().to_string();
    let item = format!("/rest/forums/{id}");

    // an update answers with both states
    let reply = e
        .call(
            "PATCH",
            &item,
            &[("prefer", "previous")],
            Some(json!({ "name": e.name("after") })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.header("preference-applied"), Some("previous"));
    assert_eq!(reply.body["row"]["name"], e.name("after"));
    assert_eq!(reply.body["previous"]["name"], e.name("before"));
    assert_eq!(reply.body["previous"]["id"], id);
    assert!(
        reply.body["previous"].get("rowVersion").is_none(),
        "a past version is not stored"
    );
    assert!(
        reply.header("etag").is_some(),
        "the etag is of the row as it is now"
    );

    // without the preference the body is the row, as before
    let plain = e
        .call(
            "PATCH",
            &item,
            &[],
            Some(json!({ "name": e.name("plain") })),
        )
        .await;
    assert_eq!(plain.body["name"], e.name("plain"));
    assert!(plain.header("preference-applied").is_none());

    // minimal wins: no body, and `previous` is not applied
    let minimal = e
        .call(
            "PATCH",
            &item,
            &[("prefer", "return=minimal, previous")],
            Some(json!({ "name": e.name("min") })),
        )
        .await;
    assert_eq!(minimal.status, StatusCode::NO_CONTENT);
    assert_eq!(minimal.header("preference-applied"), Some("return=minimal"));

    // an upsert has no previous row when it inserts, and has one when it updates
    let new_id = "aaaaaaaa-0000-0000-0000-0000000000b1";
    e.client
        .execute(
            "DELETE FROM app_public.forums WHERE id = $1::text::uuid",
            &[&new_id],
        )
        .await
        .unwrap();
    let put = format!("/rest/forums/{new_id}");
    let inserted = e
        .call(
            "PUT",
            &put,
            &[("prefer", "previous")],
            Some(json!({ "name": e.name("put1") })),
        )
        .await;
    assert_eq!(inserted.status, StatusCode::OK, "{}", inserted.body);
    assert_eq!(inserted.body["previous"], Value::Null);
    assert_eq!(inserted.body["row"]["name"], e.name("put1"));
    let updated = e
        .call(
            "PUT",
            &put,
            &[("prefer", "previous")],
            Some(json!({ "name": e.name("put2") })),
        )
        .await;
    assert_eq!(updated.body["previous"]["name"], e.name("put1"));
    assert_eq!(updated.body["row"]["name"], e.name("put2"));

    // a create and a delete have no previous row: they answer as they always did
    let post = e
        .call(
            "POST",
            "/rest/forums",
            &[("prefer", "previous")],
            Some(json!({ "name": e.name("p3") })),
        )
        .await;
    assert_eq!(post.status, StatusCode::CREATED);
    assert_eq!(post.body["name"], e.name("p3"));
    let delete = e
        .call("DELETE", &put, &[("prefer", "previous")], None)
        .await;
    assert_eq!(delete.body["name"], e.name("put2"));
    e.cleanup().await;
}
