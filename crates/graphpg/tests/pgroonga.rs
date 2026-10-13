//! The `pgroonga` integration (docs/extensions.md): PGroonga full-text indexes give full-text
//! search fields, in any language, ranked by `pgroonga_score` and highlighted. Runs on the
//! test image; skipped without the extension.
mod common;
use axum::body::Body;
use axum::http::Request;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

async fn client() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// The tables in `schema` (one per test: a test dropping it would break another's queries).
async fn app(schema: &str, config: &str) -> Option<axum::Router> {
    if !common::with_extensions(&["pgroonga"]).await {
        return None;
    }
    let db = client().await;
    db.batch_execute("SELECT pg_advisory_lock(7461024)")
        .await
        .unwrap();
    db.batch_execute(
        &"DROP SCHEMA IF EXISTS groonga CASCADE; CREATE SCHEMA groonga;
         CREATE TABLE groonga.memos (id int PRIMARY KEY, title text NOT NULL, body text NOT NULL);
         INSERT INTO groonga.memos VALUES
           (1, 'PostgreSQL', 'PostgreSQLはリレーショナル・データベース管理システムです。'),
           (2, 'Groonga', 'Groongaは日本語対応の高速な全文検索エンジンです。全文検索なら全文検索エンジン。'),
           (3, 'PGroonga', 'PGroongaはインデックスとしてGroongaを使うためのPostgreSQLの拡張機能です。'),
           (4, 'groonga コマンド', 'groongaコマンドがあります。');
         INSERT INTO groonga.memos SELECT i, 'memo ' || i, 'その他のメモ ' || i FROM generate_series(5, 2000) i;
         CREATE INDEX memos_body ON groonga.memos USING pgroonga (body);
         CREATE INDEX memos_all ON groonga.memos USING pgroonga ((ARRAY[title, body]));
         ANALYZE groonga.memos;"
            .replace("groonga.", &format!("{schema}."))
            .replace("SCHEMA IF EXISTS groonga", &format!("SCHEMA IF EXISTS {schema}"))
            .replace("SCHEMA groonga", &format!("SCHEMA {schema}")),
    )
    .await
    .unwrap();
    let config = Config::from_yaml(config).unwrap();
    let catalog = CatalogInspector::new(&db)
        .inspect_schema_with_config(schema, &config)
        .await
        .unwrap();
    db.batch_execute("SELECT pg_advisory_unlock(7461024)")
        .await
        .unwrap();
    let sdl = SchemaBuilder::build(&catalog).unwrap();
    Some(create_router(Arc::new(
        ServerState::new(
            create_pool(common::db_url(), 2).unwrap(),
            sdl,
            catalog,
            schema.to_string(),
            DebugOptions {
                sql: false,
                explain: true,
            },
        )
        .with_config(config)
        .with_introspection(true),
    )))
}

async fn request(app: &axum::Router, uri: &str, body: Value) -> Value {
    let request = Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(request).await.unwrap();
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn graphql(app: &axum::Router, query: &str) -> Value {
    request(app, "/graphql", json!({ "query": query })).await
}

#[tokio::test]
async fn japanese_text_is_searched_ranked_and_highlighted() {
    let Some(app) = app("groonga_on", "{}").await else {
        return;
    };
    let reply = graphql(
        &app,
        "{ memosFullTextSearch(query: \"全文検索\", field: BODY) {
             hits { score highlight(startSel: \"[\", stopSel: \"]\") node { title } } } }",
    )
    .await;
    assert!(reply.get("errors").is_none(), "{reply}");
    let hits = reply["data"]["memosFullTextSearch"]["hits"]
        .as_array()
        .unwrap();
    // Only the memo about full-text search, its words marked
    assert_eq!(hits.len(), 1, "{reply}");
    assert_eq!(hits[0]["node"]["title"], "Groonga", "{reply}");
    assert!(hits[0]["score"].as_f64().unwrap() > 0.0, "{reply}");
    let highlight = hits[0]["highlight"].as_str().unwrap();
    assert!(highlight.contains("[全文検索]"), "{highlight}");

    // Groonga query syntax: OR, ranked by how often the words occur
    let reply = graphql(
        &app,
        "{ memosFullTextSearch(query: \"全文検索 OR PostgreSQL\", field: BODY) { hits { node { title } } } }",
    )
    .await;
    let titles: Vec<&str> = reply["data"]["memosFullTextSearch"]["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("{reply}"))
        .iter()
        .map(|h| h["node"]["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Groonga", "PostgreSQL", "PGroonga"], "{reply}");

    // The plan uses the index
    let plan = reply["extensions"]["explain"].to_string();
    assert!(plan.contains("memos_body"), "{plan}");
}

#[tokio::test]
async fn several_columns_rest_and_global_search() {
    let Some(app) = app("groonga_all", "{}").await else {
        return;
    };
    // The ARRAY[title, body] index: a word of a title
    let reply = request(
        &app,
        "/rest/memos/search/full-text",
        json!({ "query": "コマンド", "field": "TITLE_AND_BODY", "highlight": true }),
    )
    .await;
    assert_eq!(reply["hits"][0]["row"]["id"], 4, "{reply}");
    assert!(
        reply["hits"][0]["highlight"]
            .as_str()
            .unwrap()
            .contains("<b>コマンド</b>"),
        "{reply}"
    );

    let reply = graphql(
        &app,
        "{ globalSearch(query: \"拡張機能\") { hits { node { ... on Memo { title } } } } }",
    )
    .await;
    assert_eq!(
        reply["data"]["globalSearch"]["hits"][0]["node"]["title"], "PGroonga",
        "{reply}"
    );
}

#[tokio::test]
async fn off_no_search_fields_from_its_indexes() {
    let Some(app) = app(
        "groonga_off",
        "extensions:\n  integrations:\n    pgroonga: off\n",
    )
    .await
    else {
        return;
    };
    let reply = graphql(&app, "{ __type(name: \"Query\") { fields { name } } }").await;
    let fields = reply["data"]["__type"]["fields"].to_string();
    assert!(!fields.contains("memosFullTextSearch"), "{fields}");
}
