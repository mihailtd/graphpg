//! Integration tests about the database as a whole (the extensions in use, the schema watcher, the
//! statement statistics): schema changes of other tests would show. They run one at a time
//! (`common::alone`), and cargo runs this program after or before `tests/it`, never alongside it.
#[path = "../common/mod.rs"]
mod common;

mod dev_watcher;
mod extension_integrations;
mod pg_jsonschema;
mod pgroonga;
mod schema_reload;
mod statements_advice;
mod timescaledb;
mod vector_index_extensions;
