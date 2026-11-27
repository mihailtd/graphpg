//! The integration tests: one program, so that the engine and its dependencies are linked once instead
//! of once per file. A file in this directory is a module of it; list new ones below.
//!
//! Every file's tests run at the same time as every other's: a file writes to a schema of its own,
//! and does not assert on the rest of the database. What cannot is in `tests/alone`.
#[path = "../common/mod.rs"]
mod common;

mod aggregate_types;
mod connection_safety;
mod event_triggers;
mod extension_search_path;
mod fdw_remote_keys;
mod federation;
mod foreign_data;
mod forum_schema;
mod incremental_delivery;
mod input_errors;
mod limits;
mod many_to_many;
mod multi_schema;
mod naming;
mod null_ordering;
mod openapi_inferred;
mod openapi_serving;
mod parquet;
mod partial_results;
mod persisted_queries;
mod pg18_constraints;
mod pg19;
mod pgmq_recipe;
mod phase10_env_schema;
mod phase11_config_views_keys_types;
mod phase12_writes;
mod phase13_functions;
mod phase14_scalars;
mod phase15_constraints;
mod phase16_temporal;
mod phase17_relational;
mod phase18_types;
mod phase19_operations;
mod phase2_single_table;
mod phase3_relations;
mod phase4_connections;
mod phase5_server_and_mutations;
mod phase6_lateral_joins;
mod phase7_variables_and_fragments;
mod phase8_relay_node;
mod phase9_polymorphism;
mod rate_limits;
mod rbac_pruning;
mod rest_api;
mod rest_collection;
mod rest_embed;
mod rest_extras;
mod rest_filters;
mod rest_graph;
mod rest_media;
mod rest_ranges;
mod rest_writes;
mod rls_auth;
mod schema_pinning;
mod subscriptions;
mod telemetry;
