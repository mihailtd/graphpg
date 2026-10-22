//! REST beyond tables: views, aggregates and grouped aggregates, functions of every return
//! shape under `/rest/rpc`, and the search methods (per table and global).
//!
//! Uses the `rest_extras` fixture schema (see `fixtures/rest_extras.sql`).
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

async fn state() -> ServerState {
    common::ensure_rest_extras_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to the test database");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("rest_extras")
        .await
        .expect("inspect rest_extras");
    let schema = SchemaBuilder::build(&catalog).expect("build schema");
    ServerState::new(
        create_pool(common::db_url(), 4).expect("pool"),
        schema,
        catalog,
        "rest_extras".to_string(),
        DebugOptions::default(),
    )
}

async fn app() -> axum::Router {
    create_router(Arc::new(state().await))
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
    fn ok(&self) -> &Value {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        &self.body
    }

    #[track_caller]
    fn assert_problem(&self, status: StatusCode, code: &str) -> &Value {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(
            self.header("content-type"),
            Some("application/problem+json")
        );
        assert_eq!(self.body["code"], code, "{}", self.body);
        &self.body
    }
}

async fn call(
    app: &axum::Router,
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
    let res = app
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

async fn get(app: &axum::Router, uri: &str) -> Reply {
    call(app, "GET", uri, &[], None).await
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> Reply {
    call(app, "POST", uri, &[], Some(body)).await
}

fn ids(reply: &Reply) -> Vec<i64> {
    reply
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect()
}

fn number(v: &Value) -> f64 {
    v.as_f64()
        .unwrap_or_else(|| v.as_str().unwrap().parse().unwrap())
}

// ---------------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn views_are_resources_that_follow_their_updatability() {
    let app = app().await;

    // a view lists, filters, orders and pages like a table
    let reply = get(&app, "/rest/paid_orders?order=-total").await;
    assert_eq!(ids(&reply), [6, 1, 4, 2]);
    assert!(
        reply.body[0].get("status").is_none(),
        "only the view's columns: {}",
        reply.body[0]
    );
    assert_eq!(
        ids(&get(&app, "/rest/paid_orders?filter[customer][eq]=ada").await),
        [1, 2]
    );
    let page = get(&app, "/rest/paid_orders?first=2&order=id").await;
    assert_eq!(ids(&page), [1, 2]);
    assert!(page.header("link").unwrap().contains("rel=\"next\""));
    let totals = get(&app, "/rest/customer_totals?order=customer").await;
    assert_eq!(totals.ok()[0]["customer"], "ada");
    assert_eq!(totals.ok()[0]["orders"], 2);
    assert_eq!(number(&totals.ok()[0]["total"]), 150.5);

    // a simple view has its table's key, so a single-row read; a grouped view has none
    assert_eq!(
        get(&app, "/rest/paid_orders/1").await.ok()["customer"],
        "ada"
    );
    get(&app, "/rest/customer_totals/ada")
        .await
        .assert_problem(StatusCode::BAD_REQUEST, "BAD_REQUEST");
    // materialized views are read like any other
    assert_eq!(
        get(&app, "/rest/customer_summary")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // a view is written only where PostgreSQL can: `customer_totals` (grouped) and the
    // materialized view are not updatable, whatever the grants
    for (method, uri, body) in [
        (
            "POST",
            "/rest/customer_totals",
            Some(json!({ "customer": "x" })),
        ),
        (
            "PATCH",
            "/rest/customer_totals/x",
            Some(json!({ "customer": "x" })),
        ),
        (
            "PUT",
            "/rest/customer_totals/x",
            Some(json!({ "customer": "x" })),
        ),
        ("DELETE", "/rest/customer_totals/x", None),
        (
            "POST",
            "/rest/customer_summary",
            Some(json!({ "customer": "x" })),
        ),
    ] {
        let reply = call(&app, method, uri, &[], body).await;
        reply.assert_problem(StatusCode::FORBIDDEN, "FORBIDDEN");
    }
    // a simple view has its table's key: its rows are updated by key
    for customer in ["x", "ada"] {
        let reply = call(
            &app,
            "PATCH",
            "/rest/paid_orders/1",
            &[],
            Some(json!({ "customer": customer })),
        )
        .await;
        assert_eq!(reply.ok()["customer"], customer);
    }
    // aggregates and search work on views
    let agg = get(&app, "/rest/paid_orders/aggregate?sum=total").await;
    assert_eq!(number(&agg.ok()["sum"]["total"]), 425.75);
    assert_eq!(agg.ok()["count"], 4);
    let search = post(
        &app,
        "/rest/paid_orders/search",
        json!({ "filter": { "customer": { "eq": "cyd" } } }),
    )
    .await;
    assert_eq!(ids(&search), [6]);
}

#[tokio::test]
async fn views_with_a_declared_key_are_read_by_key() {
    common::ensure_rest_extras_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let config =
        graphpg::config::Config::from_yaml("entities:\n  paid_orders:\n    primary_key: [id]\n")
            .unwrap();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config("rest_extras", &config)
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 2).unwrap(),
        schema,
        catalog,
        "rest_extras".to_string(),
        DebugOptions::default(),
    );
    let app = create_router(Arc::new(state));

    let row = get(&app, "/rest/paid_orders/4").await;
    assert_eq!(row.ok()["id"], 4);
    assert_eq!(row.ok()["customer"], "bob");
    // the view only has the paid ones
    get(&app, "/rest/paid_orders/3")
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
    assert_eq!(
        get(&app, "/rest/paid_orders/6?fields=customer").await.ok(),
        &json!({ "customer": "cyd" })
    );
}

// ---------------------------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn aggregates_summarise_the_filtered_rows() {
    let app = app().await;

    let all = get(&app, "/rest/orders/aggregate").await;
    assert_eq!(all.ok(), &json!({ "count": 6 }));

    let paid = get(
        &app,
        "/rest/orders/aggregate?filter[status][eq]=PAID&sum=total,quantity&avg=quantity&min=total&max=createdAt&distinctCount=customer",
    )
    .await;
    let body = paid.ok();
    assert_eq!(body["count"], 4);
    assert_eq!(number(&body["sum"]["total"]), 425.75);
    assert_eq!(number(&body["sum"]["quantity"]), 11.0);
    assert_eq!(number(&body["avg"]["quantity"]), 2.75);
    assert_eq!(number(&body["min"]["total"]), 50.5);
    assert_eq!(body["max"]["createdAt"], "2026-03-01T12:00:00+00:00");
    assert_eq!(body["distinctCount"]["customer"], 3);

    // the same, as a body
    let same = post(
        &app,
        "/rest/orders/aggregate",
        json!({ "filter": { "status": { "equalTo": "PAID" } }, "sum": ["total", "quantity"], "avg": ["quantity"] }),
    )
    .await;
    assert_eq!(number(&same.ok()["sum"]["total"]), 425.75);
    assert_eq!(same.ok()["count"], 4);

    // statistics
    let stats = post(
        &app,
        "/rest/orders/aggregate",
        json!({ "median": ["total"], "percentile": ["total"], "fraction": 0.5, "stddevPopulation": ["quantity"] }),
    )
    .await;
    assert_eq!(number(&stats.ok()["median"]["total"]), 62.875);
    assert_eq!(number(&stats.ok()["percentile"]["total"]), 62.875);
    assert!(
        stats.ok()["stddevPopulation"]["quantity"].is_number()
            || stats.ok()["stddevPopulation"]["quantity"].is_string()
    );

    // filters reach through the same grammar as lists
    let filtered = get(
        &app,
        "/rest/orders/aggregate?filter[customer][in]=ada,bob&filter[total][gt]=30&sum=total",
    )
    .await;
    assert_eq!(filtered.ok()["count"], 3);
    assert_eq!(number(&filtered.ok()["sum"]["total"]), 225.75);
}

#[tokio::test]
async fn grouped_aggregates_have_one_row_per_group() {
    let app = app().await;

    let by_status = get(&app, "/rest/orders/aggregate?group=status&sum=total").await;
    let groups = by_status.ok().as_array().unwrap();
    assert_eq!(groups.len(), 3);
    let paid = groups
        .iter()
        .find(|g| g["keys"]["status"] == "PAID")
        .expect("a PAID group");
    assert_eq!(paid["count"], 4);
    assert_eq!(number(&paid["sum"]["total"]), 425.75);
    // `keys` come first, as the group reads
    assert_eq!(paid.as_object().unwrap().keys().next().unwrap(), "keys");

    // several keys, in the order given
    let two = get(&app, "/rest/orders/aggregate?group=customer,status").await;
    let first = &two.ok()[0];
    assert_eq!(
        first["keys"],
        json!({ "customer": "ada", "status": "PAID" })
    );
    assert_eq!(first["count"], 2);

    // dates truncated to a unit, in a time zone
    let months = get(
        &app,
        "/rest/orders/aggregate?group=createdAt:month&sum=total",
    )
    .await;
    let counts: Vec<i64> = months
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["count"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, [2, 3, 1]);
    assert_eq!(
        months.ok()[0]["keys"]["createdAt:month"],
        "2026-01-01T00:00:00+00:00"
    );
    let zoned = post(
        &app,
        "/rest/orders/aggregate",
        json!({ "group": ["createdAt:day"], "timeZone": "Asia/Tokyo", "filter": { "id": { "equalTo": 1 } } }),
    )
    .await;
    // 10:00 UTC is 19:00 in Tokyo, the same day
    // the start of the day in Tokyo is 15:00 UTC the day before
    assert_eq!(
        zoned.ok()[0]["keys"]["createdAt:day"],
        "2026-01-04T15:00:00+00:00"
    );

    // having keeps the groups that match
    let busy = get(&app, "/rest/orders/aggregate?group=customer&sum=total&having[count][gte]=2&having[sum][total][gt]=100").await;
    let customers: Vec<&str> = busy
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["keys"]["customer"].as_str().unwrap())
        .collect();
    assert_eq!(customers, ["ada", "cyd"]);
    let body = post(
        &app,
        "/rest/orders/aggregate",
        json!({ "group": ["customer"], "having": { "sum": { "total": { "greaterThan": 200 } } }, "sum": ["total"] }),
    )
    .await;
    assert_eq!(body.ok().as_array().unwrap().len(), 1);
    assert_eq!(body.ok()[0]["keys"]["customer"], "cyd");

    // gaps between the first and last group are filled
    let gaps = post(
        &app,
        "/rest/orders/aggregate",
        json!({ "group": ["createdAt:month"], "fillGaps": true, "filter": { "id": { "in": [1, 6] } } }),
    )
    .await;
    let counts: Vec<i64> = gaps
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["count"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, [1, 0, 1], "{}", gaps.body);
}

#[tokio::test]
async fn aggregates_reject_what_the_table_cannot_do() {
    let app = app().await;
    for uri in [
        "/rest/orders/aggregate?sum=customer", // text cannot be summed
        "/rest/orders/aggregate?sum=nope",     // no such column
        "/rest/orders/aggregate?bogus=1",      // no such parameter
        "/rest/orders/aggregate?group=nope",   // no such column
        "/rest/orders/aggregate?group=status:day", // a status is not a time
        "/rest/orders/aggregate?group=createdAt:fortnight", // no such unit
        "/rest/orders/aggregate?group=createdAt:binned", // needs a bin width
        "/rest/orders/aggregate?group=id,customer,status,quantity,total", // too many keys
        "/rest/orders/aggregate?having[count][gt]=1", // having needs groups
        "/rest/orders/aggregate?timeZone=UTC", // so does a time zone
        "/rest/orders/aggregate?group=status&having[count]=1", // having ends in an operator
        "/rest/orders/aggregate?group=status&having[bogus][gt]=1", // no such aggregate
        "/rest/orders/aggregate?percentile=total", // needs a fraction
        "/rest/orders/aggregate?fraction=0.5", // only for percentile
        "/rest/orders/aggregate?percentile=total&fraction=2",
        "/rest/orders/aggregate?filter[nope][eq]=1",
        "/rest/orders/aggregate?filter[status][eq]=NOPE",
        "/rest/orders/aggregate?fillGaps=maybe",
    ] {
        let reply = get(&app, uri).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            reply.body
        );
        assert_eq!(
            reply.header("content-type"),
            Some("application/problem+json"),
            "{uri}"
        );
    }
    for body in [
        json!({ "nope": 1 }),
        json!({ "sum": "total" }),
        json!({ "group": [] }),
        json!({ "group": ["status"], "having": [] }),
        json!({ "filter": [] }),
        json!({ "sum": ["customer"] }),
    ] {
        let reply = post(&app, "/rest/orders/aggregate", body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
    }
    assert_eq!(
        post(&app, "/rest/orders/aggregate?sum=total", json!({}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    // unknown resources are a 404, as everywhere
    get(&app, "/rest/nothing/aggregate")
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
}

// ---------------------------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn functions_returning_values() {
    let app = app().await;
    assert_eq!(
        get(&app, "/rest/rpc/add?a=1").await.ok(),
        &json!(11),
        "defaults apply"
    );
    assert_eq!(get(&app, "/rest/rpc/add?a=1&b=5").await.ok(), &json!(6));
    assert_eq!(
        post(&app, "/rest/rpc/add", json!({ "args": { "a": 2, "b": 3 } }))
            .await
            .ok(),
        &json!(5)
    );
    // arrays: a list, or JSON
    assert_eq!(
        get(&app, "/rest/rpc/sum_ids?ids=1,2,3").await.ok(),
        &json!(6)
    );
    assert_eq!(
        get(&app, "/rest/rpc/sum_ids?ids=%5B4,5%5D").await.ok(),
        &json!(9)
    );
    assert_eq!(
        post(&app, "/rest/rpc/sum_ids", json!({ "args": { "ids": [] } }))
            .await
            .ok(),
        &json!(0)
    );
    // single json/jsonb argument: body can be raw json or {"args": {"payload": ...}}
    assert_eq!(
        post(
            &app,
            "/rest/rpc/echo_json",
            json!({ "msg": "hello", "count": 42 })
        )
        .await
        .ok(),
        &json!({ "msg": "hello", "count": 42 })
    );
    assert_eq!(
        post(&app, "/rest/rpc/echo_json", json!([1, 2, "three"]))
            .await
            .ok(),
        &json!([1, 2, "three"])
    );
    assert_eq!(
        post(
            &app,
            "/rest/rpc/echo_json",
            json!({ "args": { "payload": { "nested": true } } })
        )
        .await
        .ok(),
        &json!({ "nested": true })
    );
    // enums, composites, arrays and sets of values
    assert_eq!(
        get(&app, "/rest/rpc/statuses").await.ok(),
        &json!(["PENDING", "PAID", "CANCELLED"])
    );
    assert_eq!(
        get(&app, "/rest/rpc/status_of?s=PAID").await.ok(),
        &json!("PAID")
    );
    assert_eq!(
        get(&app, "/rest/rpc/origin").await.ok(),
        &json!({ "x": 0, "y": 1 })
    );
    assert_eq!(get(&app, "/rest/rpc/tags").await.ok(), &json!(["a", "b"]));
    // columns of their own: one row, or a set
    assert_eq!(
        get(&app, "/rest/rpc/order_stats").await.ok(),
        &json!({ "total": 6, "top": "cyd" })
    );
    let stats = get(&app, "/rest/rpc/customer_stats").await;
    assert_eq!(stats.ok().as_array().unwrap().len(), 3);
    assert_eq!(stats.ok()[0]["customer"], "ada");
    assert_eq!(number(&stats.ok()[0]["orders"]), 2.0);
}

#[tokio::test]
async fn functions_returning_rows() {
    let app = app().await;

    // one row (with the raw key), or none
    let best = get(&app, "/rest/rpc/best_order").await;
    assert_eq!(best.ok()["id"], 6);
    assert_eq!(best.ok()["customer"], "cyd");
    let some = get(&app, "/rest/rpc/best_order?fields=id,customer").await;
    assert_eq!(some.ok(), &json!({ "id": 6, "customer": "cyd" }));
    assert_eq!(get(&app, "/rest/rpc/nobody").await.ok(), &Value::Null);
    // one row takes only fields and include
    get(&app, "/rest/rpc/best_order?first=1")
        .await
        .assert_problem(StatusCode::BAD_REQUEST, "BAD_REQUEST");
    let post_body = post(&app, "/rest/rpc/best_order", json!({ "fields": ["id"] })).await;
    assert_eq!(post_body.ok(), &json!({ "id": 6 }));
    post(&app, "/rest/rpc/best_order", json!({ "first": 1 }))
        .await
        .assert_problem(StatusCode::BAD_REQUEST, "INVALID_REQUEST");

    // a set is a list that filters, orders and pages like the table's own
    let found = get(&app, "/rest/rpc/search_orders?q=a&order=-total").await;
    assert_eq!(ids(&found), [1, 2]);
    let ordered = get(&app, "/rest/rpc/search_orders?q=b&order=total").await;
    assert_eq!(ids(&ordered), [3, 4]);
    let filtered = get(&app, "/rest/rpc/search_orders?q=a&filter[total][gt]=60").await;
    assert_eq!(ids(&filtered), [1]);
    let page1 = call(
        &app,
        "GET",
        "/rest/rpc/search_orders?q=a&order=id&first=1",
        &[("prefer", "count=exact")],
        None,
    )
    .await;
    assert_eq!(ids(&page1), [1]);
    assert_eq!(page1.header("x-total-count"), Some("2"));
    let next = page1
        .header("link")
        .expect("a next link")
        .split(';')
        .next()
        .unwrap()
        .trim_matches(|c| c == '<' || c == '>')
        .to_string();
    assert!(next.starts_with("/rest/rpc/search_orders?q=a"), "{next}");
    assert_eq!(
        ids(&get(&app, &next).await),
        [2],
        "the link keeps the arguments"
    );

    // the same by POST: arguments in `args`, options beside them; cursors in headers
    let posted = post(
        &app,
        "/rest/rpc/search_orders",
        json!({ "args": { "q": "a" }, "order": "id", "first": 1, "filter": { "total": { "gt": 10 } } }),
    )
    .await;
    assert_eq!(ids(&posted), [1]);
    assert!(posted.header("link").is_none());
    let cursor = posted
        .header("x-next-cursor")
        .expect("a cursor")
        .to_string();
    let second = post(
        &app,
        "/rest/rpc/search_orders",
        json!({ "args": { "q": "a" }, "order": "id", "first": 1, "after": cursor }),
    )
    .await;
    assert_eq!(ids(&second), [2]);
    // embedded and projected like any list
    let projected = get(&app, "/rest/rpc/search_orders?q=cyd&fields=id").await;
    assert_eq!(projected.ok(), &json!([{ "id": 5 }, { "id": 6 }]));
}

#[tokio::test]
async fn functions_reject_bad_calls() {
    let app = app().await;
    for uri in [
        "/rest/rpc/add",                       // a missing argument
        "/rest/rpc/add?a=x",                   // an argument of the wrong type
        "/rest/rpc/add?a=1&a=2",               // given twice
        "/rest/rpc/add?a=1&c=2",               // an unknown one
        "/rest/rpc/add?a=1&first=1",           // a value is not a list
        "/rest/rpc/status_of?s=NOPE",          // not an enum value
        "/rest/rpc/sum_ids?ids=1,x",           // a bad element
        "/rest/rpc/search_orders",             // required
        "/rest/rpc/search_orders?q=a&bogus=1", // not a list parameter
        "/rest/rpc/search_orders?q=a&first=0",
        "/rest/rpc/search_orders?q=a&order=bogus",
        "/rest/rpc/search_orders?q=a&filter[nope][eq]=1",
    ] {
        let reply = get(&app, uri).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            reply.body
        );
        assert_eq!(
            reply.header("content-type"),
            Some("application/problem+json"),
            "{uri}"
        );
    }
    for (uri, body) in [
        ("/rest/rpc/add", json!({ "args": { "a": 1, "c": 2 } })),
        ("/rest/rpc/add", json!({ "args": [1] })),
        ("/rest/rpc/add", json!({ "a": 1 })),
        ("/rest/rpc/add", json!({ "args": { "a": "x" } })),
        ("/rest/rpc/status_of", json!({ "args": { "s": "NOPE" } })),
        (
            "/rest/rpc/search_orders",
            json!({ "args": { "q": "a" }, "bogus": 1 }),
        ),
        ("/rest/rpc/search_orders", json!({})),
    ] {
        let reply = post(&app, uri, body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{uri} {body}: {}",
            reply.body
        );
    }
    assert_eq!(
        post(&app, "/rest/rpc/add?a=1", json!({})).await.status,
        StatusCode::BAD_REQUEST
    );

    // not functions
    get(&app, "/rest/rpc/nothing")
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
    get(&app, "/rest/rpc")
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
    // `rpc` is not a resource, and neither is `search`
    get(&app, "/rest/rpc/1/extra").await;
}

#[tokio::test]
async fn volatile_functions_are_post_only_and_run_in_a_write_transaction() {
    let app = app().await;

    let refused = get(&app, "/rest/rpc/bump_counter?counterId=1").await;
    refused.assert_problem(StatusCode::METHOD_NOT_ALLOWED, "METHOD_NOT_ALLOWED");
    assert_eq!(refused.header("allow"), Some("POST"));

    // a function that changes a row returns it; undo what it did
    let before = post(&app, "/rest/rpc/counter_total", json!({}))
        .await
        .ok()
        .as_i64()
        .unwrap();
    let up = post(
        &app,
        "/rest/rpc/bump_counter",
        json!({ "args": { "counterId": 1, "by": 5 } }),
    )
    .await;
    assert_eq!(up.ok()["id"], 1);
    let value = up.ok()["value"].as_i64().unwrap();
    let down = post(
        &app,
        "/rest/rpc/bump_counter",
        json!({ "args": { "counterId": 1, "by": -5 } }),
    )
    .await;
    assert_eq!(down.ok()["value"].as_i64().unwrap(), value - 5);
    assert_eq!(
        post(&app, "/rest/rpc/counter_total", json!({}))
            .await
            .ok()
            .as_i64()
            .unwrap(),
        before
    );

    // void: no content; a volatile function returning a value
    assert_eq!(
        post(&app, "/rest/rpc/touch_counters", json!({}))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    let empty = call(&app, "POST", "/rest/rpc/touch_counters", &[], None).await;
    assert_eq!(empty.status, StatusCode::NO_CONTENT);
    // a stable function can be POSTed too
    assert_eq!(
        post(&app, "/rest/rpc/add", json!({ "args": { "a": 1 } }))
            .await
            .ok(),
        &json!(11)
    );
}

// ---------------------------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------------------------

fn hit_ids(reply: &Reply) -> Vec<i64> {
    reply.ok()["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["row"]["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn each_search_method_has_its_endpoint() {
    let app = app().await;

    let fuzzy = post(
        &app,
        "/rest/articles/search/fuzzy",
        json!({ "query": "postgre" }),
    )
    .await;
    assert_eq!(hit_ids(&fuzzy), [1]);
    assert!(fuzzy.ok()["hits"][0]["score"].as_f64().unwrap() > 0.5);
    assert_eq!(fuzzy.ok()["hasMore"], false);
    assert_eq!(
        fuzzy.ok()["hits"][0]["row"]["title"],
        "PostgreSQL full text search"
    );

    let text = post(
        &app,
        "/rest/articles/search/full-text",
        json!({ "query": "ranking" }),
    )
    .await;
    assert_eq!(hit_ids(&text), [1]);

    // hybrid: the lexical and vector rankings fused
    let hybrid = post(
        &app,
        "/rest/articles/search/hybrid",
        json!({ "query": "postgre", "vector": [1, 0, 0] }),
    )
    .await;
    assert_eq!(hit_ids(&hybrid)[0], 1, "{}", hybrid.body);

    // the nearest vectors first
    let semantic = post(
        &app,
        "/rest/articles/search/semantic",
        json!({ "vector": [0, 1, 0] }),
    )
    .await;
    assert_eq!(hit_ids(&semantic)[0], 2);
    let scores: Vec<f64> = semantic.ok()["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["score"].as_f64().unwrap())
        .collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "best first: {scores:?}"
    );

    // `first`, `fields` and `filter` scope a search
    let one = post(
        &app,
        "/rest/articles/search/semantic",
        json!({ "vector": [0, 1, 0], "first": 1 }),
    )
    .await;
    assert_eq!(hit_ids(&one), [2]);
    assert_eq!(one.ok()["hasMore"], true);
    let projected = post(
        &app,
        "/rest/articles/search/fuzzy",
        json!({ "query": "postgre", "fields": ["id", "title"] }),
    )
    .await;
    assert_eq!(
        projected.ok()["hits"][0]["row"],
        json!({ "id": 1, "title": "PostgreSQL full text search" })
    );
    let scoped = post(
        &app,
        "/rest/articles/search/semantic",
        json!({ "vector": [0, 1, 0], "filter": { "id": { "in": [1, 3] } } }),
    )
    .await;
    assert!(!hit_ids(&scoped).contains(&2));
}

#[tokio::test]
async fn searches_reject_what_they_do_not_take() {
    let app = app().await;
    for (uri, body) in [
        ("/rest/articles/search/fuzzy", json!({})), // needs a query
        (
            "/rest/articles/search/fuzzy",
            json!({ "query": "x", "bogus": 1 }),
        ), // unknown member
        ("/rest/articles/search/fuzzy", json!({ "query": 5 })), // not a string
        (
            "/rest/articles/search/fuzzy",
            json!({ "query": "x", "first": 1000 }),
        ), // over the limit
        (
            "/rest/articles/search/fuzzy",
            json!({ "query": "x", "condition": {} }),
        ), // not offered
        (
            "/rest/articles/search/fuzzy",
            json!({ "query": "x", "filter": { "nope": 1 } }),
        ),
        (
            "/rest/articles/search/fuzzy",
            json!({ "query": "x", "fields": ["nope"] }),
        ),
        (
            "/rest/articles/search/fuzzy",
            json!({ "query": "x", "fields": "id" }),
        ),
        ("/rest/articles/search/fuzzy", json!([1])),
        (
            "/rest/articles/search/semantic",
            json!({ "vector": "1,0,0" }),
        ),
        (
            "/rest/articles/search/semantic",
            json!({ "vector": [1, 0, 0], "query": "x" }),
        ), // not its argument
    ] {
        let reply = post(&app, uri, body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{uri} {body}: {}",
            reply.body
        );
        assert_eq!(
            reply.header("content-type"),
            Some("application/problem+json")
        );
    }
    assert_eq!(
        post(&app, "/rest/articles/search/fuzzy?query=x", json!({}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    // a method the table's indexes do not support, and tables without any: 404, saying what is
    let missing = post(&app, "/rest/articles/search/bm25", json!({ "query": "x" })).await;
    let detail = missing.assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND")["detail"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        detail.contains("fuzzy") && detail.contains("full-text"),
        "{detail}"
    );
    post(&app, "/rest/orders/search/fuzzy", json!({ "query": "x" }))
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
    post(&app, "/rest/articles/search/nonsense", json!({}))
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
    post(&app, "/rest/nothing/search/fuzzy", json!({}))
        .await
        .assert_problem(StatusCode::NOT_FOUND, "NOT_FOUND");
}

#[tokio::test]
async fn global_search_spans_the_searchable_resources() {
    let app = app().await;
    for reply in [
        get(&app, "/rest/search?query=search").await,
        post(&app, "/rest/search", json!({ "query": "search" })).await,
    ] {
        let hits = reply.ok()["hits"].as_array().unwrap();
        let types: Vec<&str> = hits.iter().map(|h| h["type"].as_str().unwrap()).collect();
        assert!(
            types.contains(&"articles") && types.contains(&"people"),
            "{types:?}"
        );
        for hit in hits {
            let row = hit["row"].as_object().unwrap();
            assert!(!row.contains_key("__typename"), "{row:?}");
            match hit["type"].as_str().unwrap() {
                "articles" => assert!(row.contains_key("title") && !row.contains_key("name")),
                "people" => assert!(row.contains_key("name") && !row.contains_key("title")),
                other => panic!("unexpected type {other}"),
            }
        }
        let scores: Vec<f64> = hits.iter().map(|h| h["score"].as_f64().unwrap()).collect();
        assert!(
            scores.windows(2).all(|w| w[0] >= w[1]),
            "best first: {scores:?}"
        );
    }
    let limited = get(&app, "/rest/search?query=search&first=1").await;
    assert_eq!(limited.ok()["hits"].as_array().unwrap().len(), 1);
    assert_eq!(limited.ok()["hasMore"], true);

    for reply in [
        get(&app, "/rest/search").await,
        get(&app, "/rest/search?query=").await,
        get(&app, "/rest/search?query=x&bogus=1").await,
        get(&app, "/rest/search?query=x&first=x").await,
        post(&app, "/rest/search", json!({ "query": "x", "bogus": 1 })).await,
        post(&app, "/rest/search", json!({})).await,
        post(&app, "/rest/search?query=x", json!({})).await,
        get(&app, "/rest/search?query=x&first=1000").await,
    ] {
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    }
}

// ---------------------------------------------------------------------------------------------
// The OpenAPI document
// ---------------------------------------------------------------------------------------------

/// Every `$ref` of `value` names a schema the document has.
fn assert_refs_resolve(doc: &Value, value: &Value, trail: &str) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref") {
                let name = r
                    .strip_prefix("#/components/schemas/")
                    .unwrap_or_else(|| panic!("{trail}: {r}"));
                assert!(
                    doc["components"]["schemas"].get(name).is_some(),
                    "{trail}: `{r}` does not exist"
                );
            }
            for (k, v) in map {
                assert_refs_resolve(doc, v, &format!("{trail}/{k}"));
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                assert_refs_resolve(doc, v, &format!("{trail}/{i}"));
            }
        }
        _ => {}
    }
}

/// An example of what `schema` describes (enough for a call).
fn sample(schema: &Value) -> Value {
    if let Some(values) = schema["enum"].as_array() {
        return values[0].clone();
    }
    let kind = match &schema["type"] {
        Value::Array(kinds) => kinds[0].clone(),
        other => other.clone(),
    };
    match kind.as_str() {
        Some("integer") => json!(1),
        Some("number") => json!(1.5),
        Some("boolean") => json!(true),
        Some("array") => json!([sample(&schema["items"])]),
        _ => match schema["format"].as_str() {
            Some("uuid") => json!("ca700000-0000-0000-0000-000000000ca7"),
            Some("date-time") => json!("2026-01-01T00:00:00Z"),
            _ => json!("a"),
        },
    }
}

#[tokio::test]
async fn openapi_describes_views_aggregates_functions_and_search() {
    let state = state().await;
    let doc = (*state.current_openapi_document()).clone();
    let app = create_router(Arc::new(state));
    let paths = doc["paths"].as_object().unwrap();

    // the document is consistent: references resolve, operation ids are unique
    assert_refs_resolve(&doc, &doc["paths"], "paths");
    assert_refs_resolve(&doc, &doc["components"], "components");
    let mut operation_ids = std::collections::HashSet::new();
    for (path, item) in paths {
        for (method, op) in item.as_object().unwrap() {
            let id = op["operationId"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} {path} has no operationId"));
            assert!(
                operation_ids.insert(id.to_string()),
                "operationId `{id}` is used twice"
            );
        }
    }

    // views are resources; only the updatable ones can be written; a simple view has its
    // table's key, a grouped one none
    assert!(
        paths.contains_key("/rest/paid_orders") && paths.contains_key("/rest/customer_summary")
    );
    assert!(
        paths.contains_key("/rest/paid_orders/{key}"),
        "a simple view has its table's key"
    );
    assert!(
        !paths.contains_key("/rest/customer_totals/{key}"),
        "a grouped view has no key"
    );
    assert!(
        paths["/rest/customer_totals"].get("post").is_none(),
        "grouped views are read-only"
    );
    assert!(paths["/rest/customer_summary"].get("post").is_none());
    assert!(
        paths["/rest/paid_orders"].get("post").is_some(),
        "simple views are updatable"
    );

    // aggregates: typed members, in the query string and the body
    let aggregate = &paths["/rest/orders/aggregate"];
    let params = aggregate["get"]["parameters"].as_array().unwrap();
    let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
    for expected in [
        "filter",
        "group",
        "sum",
        "avg",
        "min",
        "max",
        "median",
        "percentile",
        "fraction",
        "having",
        "timeZone",
        "binWidth",
        "binOrigin",
        "fillGaps",
    ] {
        assert!(names.contains(&expected), "{expected} in {names:?}");
    }
    let param = |name: &str| params.iter().find(|p| p["name"] == name).unwrap();
    assert_eq!(
        param("sum")["schema"]["items"]["enum"],
        json!(["id", "total", "quantity"]),
        "numbers only"
    );
    assert!(param("min")["schema"]["items"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("createdAt")));
    assert!(param("group")["schema"]["items"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("createdAt:month")));
    assert!(param("group")["schema"]["items"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("createdAt:binned")));
    assert_eq!(param("group")["schema"]["maxItems"], 4);
    assert_eq!(param("having")["style"], "deepObject");
    assert_eq!(param("fraction")["schema"]["maximum"], 1);
    let request = &doc["components"]["schemas"]["ordersAggregateRequest"];
    assert_eq!(request["additionalProperties"], false);
    assert_eq!(request["properties"]["sum"], param("sum")["schema"]);
    assert_eq!(
        aggregate["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ordersAggregateRequest"
    );
    let result =
        &aggregate["get"]["responses"]["200"]["content"]["application/json"]["schema"]["oneOf"];
    assert_eq!(result[1]["maxItems"], 10_000);
    assert!(aggregate["get"]["responses"].get("422").is_some());
    // every documented aggregate parameter is accepted
    let sample_query = "/rest/orders/aggregate?filter[status][eq]=PAID&group=createdAt:month&sum=total&avg=quantity&min=createdAt&having[count][gt]=0&timeZone=UTC&fillGaps=false";
    get(&app, sample_query).await.ok();

    // search methods: only the ones the table's indexes support, typed from the GraphQL field
    assert!(
        paths.contains_key("/rest/articles/search/fuzzy")
            && paths.contains_key("/rest/articles/search/semantic")
    );
    assert!(paths.contains_key("/rest/articles/search/full-text"));
    assert!(
        !paths.contains_key("/rest/articles/search/bm25"),
        "no BM25 index"
    );
    assert!(
        !paths.keys().any(|p| p.starts_with("/rest/orders/search/")),
        "orders has no search index"
    );
    let fuzzy = &paths["/rest/articles/search/fuzzy"]["post"]["requestBody"];
    let body = &fuzzy["content"]["application/json"]["schema"];
    assert_eq!(body["required"], json!(["query"]));
    assert_eq!(body["properties"]["query"]["type"], "string");
    assert_eq!(
        body["properties"]["minSimilarity"]["type"],
        json!(["number", "null"])
    );
    assert_eq!(body["properties"]["first"]["maximum"], 50);
    assert_eq!(body["additionalProperties"], false);
    let semantic = &paths["/rest/articles/search/semantic"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"];
    assert_eq!(semantic["properties"]["vector"]["type"], "array");
    assert_eq!(semantic["required"], json!(["vector"]));
    // the global search names the resources it covers
    let global = &doc["components"]["schemas"]["GlobalSearchResults"]["properties"]["hits"]
        ["items"]["properties"]["type"]["enum"];
    assert_eq!(global, &json!(["articles", "people"]));
    assert!(paths["/rest/search"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "query" && p["required"] == true));
    assert!(paths["/rest/search"]["post"].is_object());

    // functions: GET only when read-only, typed arguments, results by return shape
    let add = &paths["/rest/rpc/add"];
    assert!(add["get"].is_object() && add["post"].is_object());
    let add_params = add["get"]["parameters"].as_array().unwrap();
    let a = add_params.iter().find(|p| p["name"] == "a").unwrap();
    assert_eq!(
        (a["required"].clone(), a["schema"]["type"].clone()),
        (json!(true), json!("integer"))
    );
    assert_eq!(
        add_params.iter().find(|p| p["name"] == "b").unwrap()["required"],
        false,
        "defaults are optional"
    );
    assert_eq!(
        add["get"]["responses"]["200"]["content"]["application/json"]["schema"]["type"],
        "integer"
    );
    let add_body = &add["post"]["requestBody"]["content"]["application/json"]["schema"];
    assert_eq!(add_body["required"], json!(["args"]));
    assert_eq!(add_body["properties"]["args"]["required"], json!(["a"]));
    assert_eq!(
        add_body["properties"]["args"]["properties"]["b"]["type"],
        json!(["integer", "null"])
    );

    let ok = |name: &str| {
        paths[&format!("/rest/rpc/{name}")]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"]
            .clone()
    };
    assert_eq!(
        ok("tags"),
        json!({ "type": "array", "items": { "type": "string" } })
    );
    // enums are described by the values the API speaks, not the database's labels
    assert_eq!(
        ok("statuses")["items"]["enum"],
        json!(["PENDING", "PAID", "CANCELLED"])
    );
    assert_eq!(ok("statuses")["type"], "array");
    assert_eq!(ok("origin")["properties"]["x"]["type"], "integer");
    assert_eq!(ok("order_stats")["properties"]["top"]["type"], "string");
    assert_eq!(ok("customer_stats")["type"], "array");
    assert_eq!(
        ok("customer_stats")["items"]["properties"]["customer"]["type"],
        "string"
    );
    assert_eq!(
        ok("best_order")["anyOf"][0]["$ref"],
        "#/components/schemas/orders"
    );
    assert_eq!(
        ok("search_orders")["items"]["$ref"],
        "#/components/schemas/orders"
    );
    let search_orders = paths["/rest/rpc/search_orders"]["get"]["parameters"]
        .as_array()
        .unwrap();
    let list_names: Vec<&str> = search_orders
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    for expected in ["q", "filter", "order", "first", "after", "fields"] {
        assert!(
            list_names.contains(&expected),
            "{expected} in {list_names:?}"
        );
    }
    let posted = &paths["/rest/rpc/search_orders"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"]["properties"];
    assert!(
        posted.get("args").is_some()
            && posted.get("filter").is_some()
            && posted.get("order").is_some()
    );
    // changing functions are POST only; void answers 204
    assert!(
        paths["/rest/rpc/bump_counter"].get("get").is_none()
            && paths["/rest/rpc/bump_counter"]["post"].is_object()
    );
    assert!(paths["/rest/rpc/bump_counter"]["post"]["responses"]
        .get("409")
        .is_some());
    assert!(paths["/rest/rpc/touch_counters"]["post"]["responses"]
        .get("204")
        .is_some());
    // functions of a row (computed columns) are fields, not endpoints
    assert!(!paths.keys().any(|p| p.contains("_greeting")));

    // every documented read-only function can be called with the arguments the document asks for
    for (path, item) in paths {
        let Some(name) = path.strip_prefix("/rest/rpc/") else {
            continue;
        };
        let Some(read) = item.get("get") else {
            continue;
        };
        let query: Vec<String> = read["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["in"] == "query" && p["required"] == true)
            .map(|p| {
                let value = sample(&p["schema"]);
                let text = match value {
                    Value::Array(items) => items
                        .iter()
                        .map(|v| v.to_string().trim_matches('"').to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                    other => other.to_string().trim_matches('"').to_string(),
                };
                format!("{}={}", p["name"].as_str().unwrap(), text)
            })
            .collect();
        let uri = format!("/rest/rpc/{name}?{}", query.join("&"));
        let reply = get(&app, &uri).await;
        assert!(
            reply.status == StatusCode::OK,
            "{uri}: {} {}",
            reply.status,
            reply.body
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Rows are what the document says they are
// ---------------------------------------------------------------------------------------------

/// Whether `value` is what `schema` (a JSON Schema of the document) describes: types, nullability,
/// enums, properties and items.
fn conforms(doc: &Value, schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    if let Some(r) = schema["$ref"].as_str() {
        let target =
            &doc["components"]["schemas"][r.strip_prefix("#/components/schemas/").unwrap()];
        return conforms(doc, target, value, at);
    }
    if let Some(options) = schema["anyOf"]
        .as_array()
        .or_else(|| schema["oneOf"].as_array())
    {
        return if options.iter().any(|o| conforms(doc, o, value, at).is_ok()) {
            Ok(())
        } else {
            Err(format!("{at}: {value} matches none of {schema}"))
        };
    }
    if schema.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(()); // any JSON value
    }
    let kinds: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().filter_map(|t| t.as_str()).collect(),
        _ => vec![],
    };
    let matches = |kind: &str| match kind {
        "null" => value.is_null(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    };
    if !kinds.is_empty() && !kinds.iter().any(|k| matches(k)) {
        return Err(format!("{at}: {value} is not {kinds:?} ({schema})"));
    }
    if let (Some(values), false) = (schema["enum"].as_array(), value.is_null()) {
        if !values.contains(value) {
            return Err(format!("{at}: {value} is not one of {values:?}"));
        }
    }
    if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
        for (i, v) in values.iter().enumerate() {
            conforms(doc, items, v, &format!("{at}[{i}]"))?;
        }
    }
    if let (Some(props), Some(obj)) = (schema["properties"].as_object(), value.as_object()) {
        for (k, v) in obj {
            if let Some(p) = props.get(k) {
                conforms(doc, p, v, &format!("{at}.{k}"))?;
            } else if schema["additionalProperties"] == false {
                return Err(format!("{at}.{k} is not in {schema}"));
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn every_column_is_read_in_the_shape_the_document_gives_it() {
    let state = state().await;
    let doc = (*state.current_openapi_document()).clone();
    let app = create_router(Arc::new(state));

    // composites and ranges used to come back as `{}`
    let typed = get(&app, "/rest/typed?order=id").await;
    let rows = typed.ok().as_array().unwrap();
    let one = &rows[0];
    assert_eq!(
        one["big"], "9007199254740993",
        "64-bit integers are strings"
    );
    assert_eq!(one["price"], "12.50", "decimals are exact strings");
    assert_eq!(one["doc"], json!({ "a": { "b": [1, 2] } }));
    assert_eq!(
        one["list"],
        json!([1, "two", null]),
        "json is any JSON, not only an object"
    );
    assert_eq!(one["raw"], "3q2+7w==", "bytea is base64");
    assert_eq!(
        one["point"],
        json!({ "x": 1, "y": 2 }),
        "a composite has its attributes"
    );
    assert_eq!(
        one["points"],
        json!([{ "x": 3, "y": 4 }, { "x": 5, "y": 6 }])
    );
    assert_eq!(
        one["span"],
        json!({ "start": { "value": 1, "inclusive": true }, "end": { "value": 5, "inclusive": false }, "isEmpty": false }),
        "a range has its bounds"
    );
    assert_eq!(one["during"]["start"]["value"], "2026-01-01T00:00:00+00:00");
    assert_eq!(one["spans"].as_array().unwrap().len(), 2);
    assert_eq!(one["spans"][1]["start"]["value"], 5);
    assert_eq!(one["mood"], "PAID");
    // an empty range and an unbounded one
    assert_eq!(rows[1]["span"]["isEmpty"], true);
    assert_eq!(rows[1]["during"]["start"], Value::Null);
    assert_eq!(rows[1]["big"], Value::Null);

    // ... and it all conforms to the schema of the document, for every table of the fixture
    for resource in [
        "typed",
        "orders",
        "articles",
        "crews",
        "sailors",
        "counters",
        "paid_orders",
        "customer_totals",
    ] {
        let reply = get(&app, &format!("/rest/{resource}")).await;
        let rows = reply
            .ok()
            .as_array()
            .unwrap_or_else(|| panic!("{resource}: {}", reply.body));
        assert!(
            !rows.is_empty() || resource == "crews" || resource == "sailors",
            "{resource} has rows"
        );
        let schema = &doc["components"]["schemas"][resource];
        for row in rows {
            conforms(&doc, schema, row, resource).unwrap_or_else(|e| panic!("{e}"));
        }
    }
}

#[tokio::test]
async fn test_custom_sql_problem_responses() {
    common::ensure_rest_extras_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    client
        .batch_execute(
            "CREATE OR REPLACE FUNCTION rest_extras.raise_custom(code text, msg text, hint text DEFAULT NULL)
             RETURNS void LANGUAGE plpgsql VOLATILE AS $$
             BEGIN
                 IF hint IS NOT NULL THEN
                     RAISE EXCEPTION '%', msg USING ERRCODE = code, HINT = hint;
                 ELSE
                     RAISE EXCEPTION '%', msg USING ERRCODE = code;
                 END IF;
             END $$;
             CREATE OR REPLACE FUNCTION rest_extras.raise_constraint(msg text)
             RETURNS void LANGUAGE plpgsql VOLATILE AS $$
             BEGIN
                 RAISE EXCEPTION '%', msg USING ERRCODE = '23514', CONSTRAINT = 'balance_check',
                     HINT = 'a database hint', DETAIL = 'Key (id)=(1) is secret';
             END $$;",
        )
        .await
        .unwrap();

    let catalog = CatalogInspector::new(&client)
        .inspect_schema("rest_extras")
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();

    let mut custom_map = std::collections::BTreeMap::new();
    custom_map.insert(
        "23514".to_string(),
        graphpg::config::models::CustomErrorConfig {
            status: Some(422),
            title: Some("Check Constraint Failed".to_string()),
            code: Some("CHECK_FAILED".to_string()),
            problem_type: Some("https://example.com/errors/check".to_string()),
        },
    );
    custom_map.insert(
        "balance_check".to_string(),
        graphpg::config::models::CustomErrorConfig {
            status: Some(409),
            title: Some("Balance".to_string()),
            code: Some("BALANCE".to_string()),
            problem_type: None,
        },
    );
    let errors_config = graphpg::config::models::ErrorsConfig {
        detail: graphpg::config::models::ErrorDetail::Masked,
        map: custom_map,
    };

    let state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        schema,
        catalog,
        "rest_extras".to_string(),
        DebugOptions::default(),
    )
    .with_mode(graphpg::config::Mode::Prod)
    .with_errors(errors_config.clone());

    let app = create_router(Arc::new(state));

    // 1. PostgREST PT402 error with plain text and hint (never masked in prod)
    let reply = post(
        &app,
        "/rest/rpc/raise_custom",
        json!({
            "args": {
                "code": "PT402",
                "msg": "Payment required for premium feature",
                "hint": "UPGRADE_PLAN"
            }
        }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(reply.body["code"], "UPGRADE_PLAN");
    assert_eq!(reply.body["detail"], "Payment required for premium feature");
    assert_eq!(reply.body["hint"], "UPGRADE_PLAN");

    // 2. PostgREST PT422 error with JSON body
    let json_msg = json!({
        "code": "INSUFFICIENT_FUNDS",
        "title": "Payment Failed",
        "detail": "Account balance is negative",
        "extra_field": 123
    })
    .to_string();
    let reply = post(
        &app,
        "/rest/rpc/raise_custom",
        json!({
            "args": {
                "code": "PT422",
                "msg": json_msg
            }
        }),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "body: {}",
        reply.body
    );
    assert_eq!(reply.body["code"], "INSUFFICIENT_FUNDS");
    assert_eq!(reply.body["title"], "Payment Failed");
    assert_eq!(reply.body["detail"], "Account balance is negative");
    assert_eq!(reply.body["extra_field"], 123);

    // 3. Custom errors.map mapping SQLSTATE 23514
    let reply = post(
        &app,
        "/rest/rpc/raise_custom",
        json!({
            "args": {
                "code": "23514",
                "msg": "value out of bounds"
            }
        }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["code"], "CHECK_FAILED");
    assert_eq!(reply.body["title"], "Check Constraint Failed");
    assert_eq!(reply.body["type"], "https://example.com/errors/check");
    // A mapped error is a built-in failure: in prod it says what its title says
    assert_eq!(reply.body["detail"], "Check Constraint Failed");

    // 3b. A mapped constraint name wins over a mapped SQLSTATE, and its hint and DETAIL line
    // stay out of a prod response
    let reply = post(
        &app,
        "/rest/rpc/raise_constraint",
        json!({ "args": { "msg": "balance below zero" } }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["code"], "BALANCE");
    assert!(reply.body.get("hint").is_none(), "body: {}", reply.body);
    assert!(reply.body.get("databaseDetail").is_none());

    // 3c. A PTxxx below 400 is a 400, not a success
    let reply = post(
        &app,
        "/rest/rpc/raise_custom",
        json!({ "args": { "code": "PT200", "msg": "fine, really" } }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["detail"], "fine, really");

    // 3d. In dev the mapped error carries the database's message, hint and DETAIL
    let dev_state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        SchemaBuilder::build(
            &CatalogInspector::new(&client)
                .inspect_schema("rest_extras")
                .await
                .unwrap(),
        )
        .unwrap(),
        CatalogInspector::new(&client)
            .inspect_schema("rest_extras")
            .await
            .unwrap(),
        "rest_extras".to_string(),
        DebugOptions::default(),
    )
    .with_mode(graphpg::config::Mode::Dev)
    .with_errors(errors_config);
    let dev = create_router(Arc::new(dev_state));
    let reply = post(
        &dev,
        "/rest/rpc/raise_constraint",
        json!({ "args": { "msg": "balance below zero" } }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["detail"], "balance below zero");
    assert_eq!(reply.body["hint"], "a database hint");
    assert_eq!(reply.body["databaseDetail"], "Key (id)=(1) is secret");

    // 4. GraphQL mutation execution exposes code in extensions.code and unmasked in prod
    let gql_req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "query": "mutation { raiseCustom(code: \"PT402\", msg: \"GraphQL payment required\") }"
            })
            .to_string(),
        ))
        .unwrap();
    let res = app.clone().oneshot(gql_req).await.unwrap();
    let gql_body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(gql_body["errors"][0]["extensions"]["code"], "PT402");
    assert_eq!(gql_body["errors"][0]["message"], "GraphQL payment required");

    // 5. A mapped error over GraphQL: the mapped code, the message masked in prod
    let gql_req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "query": "mutation { raiseCustom(code: \"23514\", msg: \"raw database words\") }"
            })
            .to_string(),
        ))
        .unwrap();
    let res = app.clone().oneshot(gql_req).await.unwrap();
    let gql_body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(gql_body["errors"][0]["extensions"]["code"], "CHECK_FAILED");
    assert_eq!(gql_body["errors"][0]["message"], "Check Constraint Failed");
}

#[tokio::test]
async fn full_text_search_can_mark_the_words_that_matched() {
    let app = app().await;
    let plain = post(
        &app,
        "/rest/articles/search/full-text",
        json!({ "query": "ranking" }),
    )
    .await;
    assert!(plain.ok()["hits"][0].get("highlight").is_none());

    let marked = post(
        &app,
        "/rest/articles/search/full-text",
        json!({ "query": "ranking", "highlight": true }),
    )
    .await;
    assert_eq!(marked.status, StatusCode::OK, "{}", marked.body);
    let highlight = marked.ok()["hits"][0]["highlight"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(highlight.contains("<b>ranking</b>"), "{highlight}");

    let custom = post(
        &app,
        "/rest/articles/search/full-text",
        json!({ "query": "ranking", "highlight": { "startSel": "[", "stopSel": "]" } }),
    )
    .await;
    assert!(custom.ok()["hits"][0]["highlight"]
        .as_str()
        .unwrap()
        .contains("[ranking]"));

    // Only full-text searches mark words; the options are checked
    for (uri, body) in [
        (
            "/rest/articles/search/fuzzy",
            json!({ "query": "x", "highlight": true }),
        ),
        (
            "/rest/articles/search/full-text",
            json!({ "query": "x", "highlight": "yes" }),
        ),
        (
            "/rest/articles/search/full-text",
            json!({ "query": "x", "highlight": { "nope": 1 } }),
        ),
    ] {
        let r = post(&app, uri, body).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}: {}", r.body);
    }
}

#[tokio::test]
async fn a_json_column_can_be_read_through_a_path() {
    let app = app().await;
    let whole = get(&app, "/rest/typed/1?fields=id,doc").await;
    assert_eq!(whole.ok()["doc"], json!({ "a": { "b": [1, 2] } }));
    let part = get(&app, "/rest/typed/1?fields=id,doc&path[doc]=$.a.b").await;
    assert_eq!(part.status, StatusCode::OK, "{}", part.body);
    assert_eq!(part.ok()["doc"], json!([1, 2]));
    let list = get(&app, "/rest/typed?fields=id,doc&path[doc]=$.a.b&order=id").await;
    assert_eq!(list.ok()[0]["doc"], json!([1, 2]));
    assert_eq!(list.ok()[1]["doc"], Value::Null);
    for uri in [
        "/rest/typed/1?path[id]=$.a",
        "/rest/typed/1?path[nope]=$.a",
        "/rest/typed/1?path[doc]=a.b",
        "/rest/typed/1?path[doc][x]=$.a",
    ] {
        assert_eq!(
            get(&app, uri).await.status,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn rows_can_carry_their_cursor_and_window_values() {
    let app = app().await;
    let r = get(
        &app,
        "/rest/orders?fields=id,customer&order=id&window=rowNumber,rank&cursors=true",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let rows = r.ok().as_array().unwrap();
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0]["_window"]["rowNumber"], 1);
    assert_eq!(rows[5]["_window"]["rowNumber"], 6);
    assert_eq!(rows[2]["_window"]["rank"], 3);
    let cursor = rows[1]["_cursor"].as_str().unwrap().to_string();
    assert!(!cursor.is_empty());

    // The cursor of a row is a page boundary: `after` it, the rows that follow
    let next = get(
        &app,
        &format!("/rest/orders?fields=id&order=id&first=2&after={cursor}"),
    )
    .await;
    assert_eq!(ids(&next), [3, 4]);

    // Within groups, and running sums (a ledger's balance)
    let r = get(
        &app,
        "/rest/orders?fields=id,customer&order=id&window=rowNumber&partition=customer&running=total",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let rows = r.ok().as_array().unwrap();
    let numbers: Vec<i64> = rows
        .iter()
        .map(|r| r["_window"]["rowNumber"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, [1, 2, 1, 2, 1, 2]);
    assert_eq!(number(&rows[1]["_window"]["runningSum"]["total"]), 150.5);
    assert_eq!(number(&rows[5]["_window"]["runningSum"]["total"]), 210.0);

    // Rows have neither unless asked
    let plain = get(&app, "/rest/orders?fields=id&order=id").await;
    assert!(plain.ok()[0].get("_window").is_none() && plain.ok()[0].get("_cursor").is_none());

    for uri in [
        "/rest/orders?window=median",
        "/rest/orders?running=customer",
        "/rest/orders?partition=customer",
        "/rest/orders?window=rank&partition=nope",
        "/rest/orders?cursors=sometimes",
        "/rest/orders/1?window=rank",
    ] {
        assert_eq!(
            get(&app, uri).await.status,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn aggregates_can_be_side_by_side_with_filters_of_their_own() {
    let app = app().await;
    let r = post(
        &app,
        "/rest/orders/aggregate",
        json!({
            "filter": { "id": { "lessThan": 100 } },
            "sets": {
                "paid": { "filter": { "status": { "equalTo": "PAID" } }, "sum": ["total"], "max": ["createdAt"] },
                "everything": { "sum": ["total"] },
                "byCustomer": { "group": ["customer"], "sum": ["total"], "having": { "count": { "greaterThan": 1 } } }
            }
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let out = r.ok();
    assert_eq!(number(&out["paid"]["sum"]["total"]), 425.75);
    assert_eq!(number(&out["everything"]["sum"]["total"]), 455.75);
    let groups = out["byCustomer"].as_array().unwrap();
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0]["keys"], json!({ "customer": "ada" }));
    assert_eq!(number(&groups[0]["sum"]["total"]), 150.5);

    for body in [
        // members belong in a set
        json!({ "sum": ["total"], "sets": { "a": { "sum": ["total"] } } }),
        json!({ "sets": {} }),
        json!({ "sets": { "a": {} } }),
        json!({ "sets": { "1bad name": { "sum": ["total"] } } }),
        json!({ "sets": { "a": { "sets": { "b": { "sum": ["total"] } }, "sum": ["total"] } } }),
        json!({ "sets": { "a": { "group": ["customer"], "filter": {}, "sum": ["total"] } } }),
        json!({ "sets": { "a": { "having": { "count": { "greaterThan": 1 } }, "sum": ["total"] } } }),
        json!({ "sets": { "a": { "sum": ["customer"] } } }),
    ] {
        let r = post(&app, "/rest/orders/aggregate", body.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}: {}", r.body);
    }
}

#[tokio::test]
async fn graphql_states_the_constraints_the_document_states() {
    common::ensure_rest_extras_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("rest_extras")
        .await
        .unwrap();
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    // The directive, and on the type and on both inputs, the same keywords as JSON Schema
    assert!(sdl.contains("directive @constraint("), "no directive");
    for field in [
        "score: Int @constraint(minimum: 0, maximum: 100)",
        "step: Int @constraint(multipleOf: 5)",
        "rating: Float @constraint(exclusiveMinimum: 0, exclusiveMaximum: 5)",
        "code: String @constraint(pattern: \"^[A-Z]{3}$\")",
        "title: String @constraint(minLength: 3, maxLength: 20)",
        "tags: [String] @constraint(maxItems: 5)",
    ] {
        let count = sdl.matches(field).count();
        assert!(count >= 3, "{field}: {count} times (type and two inputs)");
    }
    // A check that is not a simple bound says nothing
    assert!(sdl.contains("other: Int\n"));
    // The schema is valid with them
    SchemaBuilder::build(&catalog).unwrap();
}

#[tokio::test]
async fn an_estimated_total_can_be_exact_when_the_estimate_is_small() {
    let app = app().await;
    let count = |args: &'static str| {
        let app = app.clone();
        async move {
            let query = format!(
                "{{ ordersConnection(filter: {{ status: {{ equalTo: PAID }}, total: {{ greaterThan: 60 }} }}) {{ totalCount{args} }} }}"
            );
            let r = post(&app, "/graphql", json!({ "query": query })).await;
            assert!(r.body.get("errors").is_none(), "{}", r.body);
            r.body["data"]["ordersConnection"]["totalCount"]
                .as_i64()
                .unwrap()
        }
    };
    // Few rows: the count is exact; the estimate can be off, and may be anything
    assert_eq!(count("(estimate: true, exactBelow: 1000)").await, 3);
    assert_eq!(count("").await, 3);
    assert!((0..=6).contains(&count("(estimate: true)").await));
    // Above the threshold the estimate is the answer, as without `exactBelow`
    assert_eq!(
        count("(estimate: true, exactBelow: 0)").await,
        count("(estimate: true)").await
    );
    // Without a filter too, and beside rows
    let r = post(
        &app,
        "/graphql",
        json!({ "query": "{ ordersConnection(first: 2) { totalCount(estimate: true, exactBelow: 100) nodes { id } } }" }),
    )
    .await;
    assert_eq!(
        r.body["data"]["ordersConnection"]["totalCount"], 6,
        "{}",
        r.body
    );
}
