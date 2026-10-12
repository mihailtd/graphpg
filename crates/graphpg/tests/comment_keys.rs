//! Keys declared in SQL comments (`@primaryKey`, `@foreignKey`), for relations PostgreSQL
//! cannot constrain (views, materialized views, foreign tables): versioned with the migration
//! that creates them. `graphpg.yaml` wins over them.
mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::config::Config;
use graphpg::schema::SchemaBuilder;
use tokio_postgres::NoTls;

const FIXTURE: &str = r#"
DROP SCHEMA IF EXISTS comment_keys CASCADE;
CREATE SCHEMA comment_keys;
CREATE TABLE comment_keys.authors (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE comment_keys.books (id int PRIMARY KEY, author_id int NOT NULL, title text NOT NULL);
CREATE VIEW comment_keys.book_view AS SELECT id, author_id, title FROM comment_keys.books;
COMMENT ON VIEW comment_keys.book_view IS E'Books, as published\n@primaryKey id\n@foreignKey (author_id) references comment_keys.authors';
CREATE VIEW comment_keys.broken_view AS SELECT id FROM comment_keys.books;
"#;

async fn catalog(
    comment_on_broken: Option<&str>,
    config: &str,
) -> Result<graphpg::DatabaseCatalog, String> {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute("SELECT pg_advisory_lock(7461020)")
        .await
        .unwrap();
    client.batch_execute(FIXTURE).await.unwrap();
    if let Some(comment) = comment_on_broken {
        client
            .batch_execute(&format!(
                "COMMENT ON VIEW comment_keys.broken_view IS '{comment}'"
            ))
            .await
            .unwrap();
    }
    let config = Config::from_yaml(config).unwrap();
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config("comment_keys", &config)
        .await
        .map_err(|e| e.to_string());
    client
        .batch_execute("SELECT pg_advisory_unlock(7461020)")
        .await
        .unwrap();
    catalog
}

fn block<'s>(sdl: &'s str, header: &str) -> &'s str {
    let start = sdl.find(header).unwrap_or_else(|| panic!("no {header}"));
    let end = sdl[start..].find("\n}").map_or(sdl.len(), |e| start + e);
    &sdl[start..end]
}

#[tokio::test]
async fn a_view_takes_its_keys_from_its_comment() {
    let catalog = catalog(None, "{}").await.unwrap();
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    // The key: a lookup and a Relay id
    assert!(sdl.contains("bookViewById(id: Int!): BookView"), "{sdl}");
    let view = block(&sdl, "type BookView ");
    assert!(view.contains("implements Node"), "{view}");
    // The relation, both ways
    assert!(view.contains("author: Author"), "{view}");
    let author = block(&sdl, "type Author ");
    assert!(author.contains("BookView"), "{author}");
    // The hint lines are not part of the description
    assert!(sdl.contains("Books, as published"));
    assert!(
        !sdl.contains("@primaryKey") && !sdl.contains("@foreignKey"),
        "{sdl}"
    );
}

#[tokio::test]
async fn the_config_wins_over_a_comment() {
    let catalog = catalog(None, "entities:\n  book_view:\n    primary_key: [title]\n")
        .await
        .unwrap();
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    assert!(
        sdl.contains("bookViewById(title: String!): BookView"),
        "{sdl}"
    );
    // The comment's foreign key still applies: the config declares none
    assert!(block(&sdl, "type BookView ").contains("author: Author"));
}

#[tokio::test]
async fn bad_hints_are_reported_against_the_comment() {
    let err = catalog(Some("@foreignKey author_id references authors"), "{}")
        .await
        .unwrap_err();
    assert!(
        err.contains("the comment on `comment_keys.broken_view`") && err.contains("does not parse"),
        "{err}"
    );
    let err = catalog(Some("@primaryKey nope"), "{}").await.unwrap_err();
    assert!(
        err.contains("the comment on `comment_keys.broken_view`") && err.contains("nope"),
        "{err}"
    );
}
