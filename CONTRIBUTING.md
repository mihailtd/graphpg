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

## Releasing

Releases are cut by tagging; the **Publish** workflow does the rest.

1. Bump `version` in `Cargo.toml` (and `Cargo.lock`: run `cargo build`), and add a section to `CHANGELOG.md`. Merge to `main` through a pull request.
2. Tag that commit and push the tag:
   ```bash
   git tag -a v1.0.2 -m "graphpg 1.0.2" && git push origin v1.0.2
   ```
   Use a pre-release suffix (`v1.1.0-rc.1`) for release candidates: they get a pre-release on GitHub and no `latest`, `X.Y` or `X` tags.
3. The workflow refuses a tag that is not on `main` or whose version differs from `Cargo.toml`, runs the tests, builds the image, scans it, pushes it, signs it and creates the GitHub Release with generated notes.

Every push to `main` also publishes `ghcr.io/mihailtd/graphpg:sha-<short hash>` and `:edge`.

Release notes are generated from merged pull requests and grouped by label (`breaking`, `feature`, `bug`, `security`, `documentation`, `dependencies`; `skip-changelog` hides one). Label pull requests before tagging.
