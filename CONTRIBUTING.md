# Contributing

Issues and pull requests are welcome.

## Setup

You need a recent stable Rust toolchain (see `rust-toolchain.toml`) and PostgreSQL 18 with the extensions in `docker/init.sql` (pgvector among them). The simplest way:

```bash
docker compose up -d        # PostgreSQL 18 + pgvector, database graphpg_test on localhost:5432
cargo test --workspace
```

To use another database, set `TEST_DATABASE_URL`. The tests create their own schemas (`*_fixture`, `app_public`, `interfaces_and_unions`) and leave them in place; use a throwaway database.

## Tests

- Each file in `crates/graphpg/tests/` is an integration test against a real database. A file creates the schema it needs, guarded by an advisory lock; when you change a fixture, bump its version string so it is rebuilt.
- Shared fixtures live in `crates/graphpg/tests/fixtures/` and `tests/common/mod.rs`.
- The generated-schema snapshot (`tests/snapshots/`) is reviewed with `cargo insta review`, or accepted with `INSTA_UPDATE=always cargo test --test forum_schema`.
- Tests in one file run in parallel: don't assert on counts of rows that other tests in the file create.

## Pull requests

- Keep changes focused, add or update tests, and run `cargo test --workspace` before pushing.
- Describe behaviour in `docs/` when you add or change a feature.
- By contributing you agree that your work is licensed under the project's licenses (MIT OR Apache-2.0).
