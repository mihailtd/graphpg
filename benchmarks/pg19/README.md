# PostgreSQL 18 vs PostgreSQL 19 under graphpg

Runs the same k6 scenarios through graphpg against `postgres:18-trixie` and `postgres:19-trixie` on one host, with the same container sizing (`shared_buffers=512MB`, `work_mem=16MB`, `effective_cache_size=2GB`), dataset (`schema.sql`, 100,000 posts) and `pool_size: 10`.

```bash
GRAPHPG_REQUIRE_UI=0 cargo build --release --locked --bin graphpg
benchmarks/pg19/compare.sh                          # 3 rounds, 10 s per scenario
RUNTIME=podman ROUNDS=1 DURATION=3 SCENARIOS=simple,insert_fk benchmarks/pg19/compare.sh
```

The report lands in `results/<date>-pg18-vs-pg19.md`, with the raw k6 output of each run in `results/runs/`.

## Keeping the comparison fair

- **`jit` is pinned** to the same value on both servers (`JIT=off` by default; `JIT=default` keeps each server's own). PostgreSQL 19 turned JIT off by default and PostgreSQL 18 has it on, so comparing defaults would credit 19 with a setting 18 can change in one line.
- **Rounds alternate** between the versions (18, 19, then 19, 18, ...), each on a fresh container and dataset. The report gives the median and the range across rounds: a difference smaller than the ranges is noise.
- **Run conditions are recorded** for every run: the host load average, and a 3 s single-client commit probe (WAL flush rate). Write scenarios wait for the flush at commit, and the grouped aggregate uses parallel workers, so they move the most when something else uses the disk or the CPUs. Run on a quiet host, and compare those scenarios only within one report.

## Scenarios

- **Reads** (the same SQL on both versions): `simple`, `embedded`, `not_in`, `anti_join`, `distinct_from`, `distinct_null`, `ancestors`, `aggregate_group`, `toast_read`.
- **Writes**: `insert_fk`, `connect_or_create_hit` (finds an existing tag), `connect_or_create_race` (20 clients creating the same new tag; its fail % counts `23505` errors surfaced to clients).
- **Export**: `export_ndjson` (`POST /export`, about 2,000 rows: a portal fetch on 18, `COPY ... FORMAT json` on 19).
