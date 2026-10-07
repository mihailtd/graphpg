//! Media types of the REST API beyond JSON: content negotiation, CSV in and out, multipart
//! bodies, the bytes of a `bytea` column, and `HEAD` / `OPTIONS`.
mod common;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

struct Env {
    app: axum::Router,
    client: tokio_postgres::Client,
}

async fn env() -> Env {
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
    let graphql = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        graphql,
        catalog,
        "rest_extras".to_string(),
        DebugOptions::default(),
    );
    Env {
        app: create_router(Arc::new(state)),
        client,
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("{}", self.text()))
    }
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

impl Env {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let res = self
            .app
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }
}

#[tokio::test]
async fn a_list_is_csv_when_asked_for() {
    let env = env().await;
    let csv = env
        .call(
            "GET",
            "/rest/typed?order=id&fields=id,big,doc,tags",
            &[("accept", "text/csv")],
            vec![],
        )
        .await;
    assert_eq!(csv.status, StatusCode::OK, "{}", csv.text());
    assert!(csv.header("content-type").starts_with("text/csv"));
    let text = csv.text();
    let mut lines = text.split("\r\n");
    assert_eq!(lines.next(), Some("id,big,doc,tags"));
    // 64-bit integers are text, objects and lists JSON, nulls empty
    let first = lines.next().unwrap();
    assert!(first.starts_with("1,9007199254740993,\"{"), "{first}");
    assert!(first.contains("\"\"a\"\""), "{first}");
    assert!(lines.any(|l| l == "2,,,"), "{text}");

    let json = env.call("GET", "/rest/typed?fields=id", &[], vec![]).await;
    assert!(json.header("content-type").starts_with("application/json"));
    let search = env
        .call(
            "POST",
            "/rest/typed/search",
            &[("accept", "text/csv"), ("content-type", "application/json")],
            br#"{"fields": ["id"], "order": "id"}"#.to_vec(),
        )
        .await;
    assert!(
        search.text().starts_with("id\r\n1\r\n"),
        "{}",
        search.text()
    );

    let unacceptable = env
        .call(
            "GET",
            "/rest/typed",
            &[("accept", "application/xml")],
            vec![],
        )
        .await;
    assert_eq!(unacceptable.status, StatusCode::NOT_ACCEPTABLE);
    assert!(unacceptable
        .header("content-type")
        .starts_with("application/problem+json"));
}

#[tokio::test]
async fn csv_rows_are_created_in_bulk_with_typed_cells() {
    let env = env().await;
    env.client
        .batch_execute("DELETE FROM rest_extras.people WHERE id IN (9101, 9102)")
        .await
        .unwrap();
    let body = "id,name\r\n9101,\"Ann, the \"\"first\"\"\"\r\n9102,Bob\r\n";
    let res = env
        .call(
            "POST",
            "/rest/people",
            &[("content-type", "text/csv")],
            body.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.text());
    let rows = res.json();
    assert_eq!(rows.as_array().unwrap().len(), 2, "{rows}");
    let names: Vec<String> = env
        .client
        .query(
            "SELECT name FROM rest_extras.people WHERE id IN (9101, 9102) ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(names, ["Ann, the \"first\"", "Bob"]);

    // A cell that is not what its column takes names the row and the column
    let bad = env
        .call(
            "POST",
            "/rest/people",
            &[("content-type", "text/csv")],
            b"id,name\r\nnope,Cy\r\n".to_vec(),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.text().contains("body[0].id"), "{}", bad.text());
    let short = env
        .call(
            "POST",
            "/rest/people",
            &[("content-type", "text/csv")],
            b"id,name\r\n1\r\n".to_vec(),
        )
        .await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST);

    let xml = env
        .call(
            "POST",
            "/rest/people",
            &[("content-type", "application/xml")],
            b"<a/>".to_vec(),
        )
        .await;
    assert_eq!(xml.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    env.client
        .batch_execute("DELETE FROM rest_extras.people WHERE id IN (9101, 9102)")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_multipart_body_writes_fields_and_a_file() {
    let env = env().await;
    env.client
        .batch_execute("DELETE FROM rest_extras.typed WHERE id = 9201")
        .await
        .unwrap();
    let boundary = "XBOUNDARYX";
    let mut body = Vec::new();
    for (name, content, file) in [
        ("id", &b"9201"[..], false),
        ("big", &b"42"[..], false),
        ("raw", &b"\x89PNG\r\n\x1a\nbytes"[..], true),
    ] {
        body.extend(format!("--{boundary}\r\n").as_bytes());
        if file {
            body.extend(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"
                )
                .as_bytes(),
            );
        } else {
            body.extend(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
        }
        body.extend(content);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").as_bytes());
    let res = env
        .call(
            "POST",
            "/rest/typed",
            &[(
                "content-type",
                &format!("multipart/form-data; boundary={boundary}"),
            )],
            body,
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.text());
    let stored: Vec<u8> = env
        .client
        .query_one("SELECT raw FROM rest_extras.typed WHERE id = 9201", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, b"\x89PNG\r\n\x1a\nbytes");

    // ... and the bytes come back as they went in, with the type their first bytes name
    let got = env.call("GET", "/rest/typed/9201/raw", &[], vec![]).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text());
    assert_eq!(got.body, stored);
    assert_eq!(got.header("content-type"), "image/png");
    let refused = env
        .call(
            "GET",
            "/rest/typed/9201/raw",
            &[("accept", "text/html")],
            vec![],
        )
        .await;
    assert_eq!(refused.status, StatusCode::NOT_ACCEPTABLE);

    // A PUT replaces the bytes, whatever its media type
    let put = env
        .call(
            "PUT",
            "/rest/typed/9201/raw",
            &[("content-type", "application/pdf")],
            b"%PDF-1.7 body".to_vec(),
        )
        .await;
    assert_eq!(put.status, StatusCode::NO_CONTENT, "{}", put.text());
    let pdf = env.call("GET", "/rest/typed/9201/raw", &[], vec![]).await;
    assert_eq!(pdf.header("content-type"), "application/pdf");
    assert_eq!(pdf.body, b"%PDF-1.7 body");

    // Not a binary column, an empty value, an unknown row
    let text_column = env.call("GET", "/rest/typed/9201/big", &[], vec![]).await;
    assert_eq!(text_column.status, StatusCode::NOT_FOUND);
    let empty = env.call("GET", "/rest/typed/2/raw", &[], vec![]).await;
    assert_eq!(empty.status, StatusCode::NOT_FOUND);
    env.client
        .batch_execute("DELETE FROM rest_extras.typed WHERE id = 9201")
        .await
        .unwrap();
}

#[tokio::test]
async fn head_answers_like_get_without_a_body_and_options_lists_the_methods() {
    let env = env().await;
    let get = env
        .call("GET", "/rest/typed?fields=id&order=id", &[], vec![])
        .await;
    let head = env
        .call("HEAD", "/rest/typed?fields=id&order=id", &[], vec![])
        .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-type"), get.header("content-type"));
    let item = env.call("HEAD", "/rest/typed/1", &[], vec![]).await;
    assert_eq!(item.status, StatusCode::OK);
    assert!(item.body.is_empty());
    let missing = env.call("HEAD", "/rest/typed/987654", &[], vec![]).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let options = env.call("OPTIONS", "/rest/typed", &[], vec![]).await;
    assert_eq!(options.status, StatusCode::NO_CONTENT);
    assert_eq!(
        options.header("allow"),
        "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"
    );
    assert!(options.header("accept-post").contains("text/csv"));
    let item = env.call("OPTIONS", "/rest/typed/1", &[], vec![]).await;
    assert_eq!(
        item.header("allow"),
        "GET, HEAD, PATCH, PUT, DELETE, OPTIONS"
    );
    assert!(item.header("accept-patch").contains("multipart/form-data"));
    let column = env.call("OPTIONS", "/rest/typed/1/raw", &[], vec![]).await;
    assert_eq!(column.header("allow"), "GET, HEAD, PUT, OPTIONS");
}

#[tokio::test]
async fn the_openapi_document_describes_the_media_types_and_methods() {
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
    let doc = graphpg::openapi::OpenApiGenerator::generate_for(
        &catalog,
        &graphpg::config::Config::default(),
        "rest_extras",
    );
    let typed = &doc["paths"]["/rest/typed"];
    assert!(typed["get"]["responses"]["200"]["content"]["text/csv"].is_object());
    assert!(typed["head"]["responses"]["200"].get("content").is_none());
    assert!(typed["options"]["responses"]["204"]["headers"]["Accept-Post"].is_object());
    let bodies = &typed["post"]["requestBody"]["content"];
    assert!(bodies["text/csv"].is_object());
    // a file is a binary part, not Base64 text
    assert_eq!(
        bodies["multipart/form-data"]["schema"]["properties"]["raw"]["contentMediaType"],
        "application/octet-stream"
    );
    let column = &doc["paths"]["/rest/typed/{key}/{column}"];
    assert_eq!(
        column["get"]["parameters"][1]["schema"]["enum"],
        serde_json::json!(["raw"])
    );
    assert!(column["put"]["requestBody"]["content"]["*/*"].is_object());
    // tables without a binary column have no such path
    assert!(doc["paths"].get("/rest/people/{key}/{column}").is_none());
}
