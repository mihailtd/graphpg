//! Serving the OpenAPI document and the Scalar reference that shows it: the page points at the
//! configured path, the document is there whenever the page is, and neither is served when the
//! REST API is off.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn app(config: &str) -> axum::Router {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("app_public")
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 2).unwrap(),
        schema,
        catalog,
        "app_public".to_string(),
        DebugOptions::default(),
    )
    .with_config(Config::from_yaml(config).unwrap());
    create_router(Arc::new(state))
}

async fn get(app: &axum::Router, uri: &str, accept: Option<&str>) -> (StatusCode, String, String) {
    let mut request = Request::builder().uri(uri);
    if let Some(accept) = accept {
        request = request.header(header::ACCEPT, accept);
    }
    let res = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body =
        String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).to_string();
    (status, content_type, body)
}

/// The URL the page asks Scalar to load, read from the page itself.
fn spec_url_of(page: &str) -> String {
    let marker = "&quot;url&quot;:&quot;";
    let start = page.find(marker).map(|i| i + marker.len()).or_else(|| {
        let plain = "\"url\":\"";
        page.find(plain).map(|i| i + plain.len())
    });
    let start = start.unwrap_or_else(|| panic!("the page names no document: {page}"));
    let rest = &page[start..];
    let end = rest.find("&quot;").or_else(|| rest.find('"')).unwrap();
    rest[..end].to_string()
}

#[tokio::test]
async fn the_reference_loads_the_document_it_is_pointed_at() {
    // the reference on and the document not asked for: the page still has a document to show
    let app = app("openapi:\n  ui: true\n  spec: false\n").await;

    let (status, content_type, page) = get(&app, "/scalar", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"));
    let url = spec_url_of(&page);
    assert_eq!(url, "/openapi.json");

    let (status, content_type, body) = get(&app, &url, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the page's document must exist: {body}"
    );
    assert!(content_type.starts_with("application/json"));
    let doc: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["openapi"], "3.1.0");
    assert!(doc["paths"]["/rest/forums"].is_object());

    // the script the page loads is served too, from the binary
    let (status, content_type, script) = get(&app, "/scalar/scalar.js", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("application/javascript"));
    assert!(
        script.len() > 100_000,
        "the bundled reference: {} bytes",
        script.len()
    );
}

#[tokio::test]
async fn the_reference_follows_a_custom_document_path() {
    let app = app("openapi:\n  ui: true\n  spec: true\n  path: docs/api.json\n").await;
    let (_, _, page) = get(&app, "/scalar", None).await;
    let url = spec_url_of(&page);
    assert_eq!(
        url, "/docs/api.json",
        "the page asks for the configured path"
    );
    let (status, _, body) = get(&app, &url, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["openapi"],
        "3.1.0"
    );
    // the default path is not served any more
    assert_eq!(
        get(&app, "/openapi.json", None).await.0,
        StatusCode::NOT_FOUND
    );

    // YAML on request
    let (status, content_type, yaml) = get(&app, &url, Some("application/yaml")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("application/yaml"));
    assert!(
        yaml.starts_with("openapi: 3.1.0") || yaml.contains("openapi: 3.1.0"),
        "{}",
        &yaml[..80]
    );
}

#[tokio::test]
async fn nothing_is_served_that_was_not_asked_for() {
    // both off (the prod default): neither the page nor the document
    let off = app("openapi:\n  ui: false\n  spec: false\n").await;
    assert_eq!(get(&off, "/scalar", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&off, "/openapi.json", None).await.0,
        StatusCode::NOT_FOUND
    );

    // the document without the page
    let spec_only = app("openapi:\n  ui: false\n  spec: true\n").await;
    assert_eq!(
        get(&spec_only, "/scalar", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&spec_only, "/openapi.json", None).await.0,
        StatusCode::OK
    );

    // the REST API off: there is nothing to document, whatever else is on
    let rest_off = app("openapi:\n  enabled: false\n  ui: true\n  spec: true\n").await;
    assert_eq!(
        get(&rest_off, "/scalar", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&rest_off, "/openapi.json", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&rest_off, "/rest/forums", None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_graphql_sdl_is_served_only_when_asked_for() {
    let off = app("").await;
    assert_eq!(
        get(&off, "/schema.graphql", None).await.0,
        StatusCode::NOT_FOUND
    );

    let on = app("graphql:\n  sdl: true\n  sdl_path: /sdl\n").await;
    let (status, content_type, sdl) = get(&on, "/sdl", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/plain"), "{content_type}");
    assert!(sdl.contains("type Forum"), "{sdl}");
    // the default path is not mounted, and the subgraph SDL needs federation
    assert_eq!(
        get(&on, "/schema.graphql", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&on, "/subgraph.graphql", None).await.0,
        StatusCode::NOT_FOUND
    );

    let federated = app("graphql:\n  sdl: true\n  federation: { enabled: true }\n").await;
    assert_eq!(
        get(&federated, "/schema.graphql", None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        get(&federated, "/subgraph.graphql", None).await.0,
        StatusCode::OK
    );

    // GraphQL off: nothing of it is served
    let disabled = app("graphql:\n  enabled: false\n  sdl: true\n").await;
    assert_eq!(
        get(&disabled, "/schema.graphql", None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_schema_reload_rebuilds_the_openapi_document_with_the_graphql_schema() {
    common::ensure_forum_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("app_public")
        .await
        .unwrap();
    let schema = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 2).unwrap(),
        schema,
        catalog.clone(),
        "app_public".to_string(),
        DebugOptions::default(),
    );
    let has = |state: &ServerState, text: &str| {
        let doc = state.current_openapi_document().to_string();
        let sdl = state.current_schema().to_string();
        (doc.contains(text), sdl.contains(text))
    };
    let note = "Described after a DDL change";
    assert_eq!(has(&state, note), (false, false));

    // DDL changed a table comment: the reload swaps in a schema and a document that show it
    let mut changed = catalog;
    changed.tables[0].description = Some(note.to_string());
    let schema = SchemaBuilder::build(&changed).unwrap();
    state.reload(schema, changed);
    assert_eq!(has(&state, note), (true, true));
}
