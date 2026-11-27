#!/usr/bin/env bash
# Compares graphpg against PostgreSQL 18 and PostgreSQL 19 on the same host, with identical
# server sizing, planner settings, dataset and k6 scenarios (benchmarks/pg19/pg19.js).
#
#   benchmarks/pg19/compare.sh
#   ROUNDS=5 DURATION=15 benchmarks/pg19/compare.sh
#   ROUNDS=1 DURATION=3 SCENARIOS=simple,insert_fk benchmarks/pg19/compare.sh
#
# Rigour:
#   - `jit` is pinned to the same value on both servers (JIT=off by default). PostgreSQL 19
#     turned JIT off by default while PostgreSQL 18 has it on, so comparing server defaults
#     would credit PostgreSQL 19 with a setting PostgreSQL 18 users can change in one line.
#     JIT=default keeps each server's own default.
#   - ROUNDS runs per version (default 3), interleaved and in alternating order
#     (18,19 / 19,18 / ...), each on a fresh container and dataset, so drift on the host
#     hits both versions alike. The report shows the median and the range across rounds.
#   - Before each run the host load and the database's own commit rate (a 3 s single-client
#     pgbench INSERT probe, i.e. WAL flush speed) are recorded. Write scenarios are bound by
#     that flush: a busy or slow disk shows up there before it shows up in the results.
set -euo pipefail
cd "$(dirname "$0")/../.."

RUNTIME=${RUNTIME:-docker}
GRAPHPG_BIN=${GRAPHPG_BIN:-target/release/graphpg}
PG18_IMAGE=${PG18_IMAGE:-docker.io/library/postgres:18-trixie}
PG19_IMAGE=${PG19_IMAGE:-docker.io/library/postgres:19-trixie}
PG_PORT=${PG_PORT:-5439}
APP_PORT=${APP_PORT:-4009}
ROUNDS=${ROUNDS:-3}
JIT=${JIT:-off}
VUS=${VUS:-20}
DURATION=${DURATION:-10}
AUTHORS=${AUTHORS:-1000}
POSTS=${POSTS:-100000}
COMMENTS=${COMMENTS:-225000}
CATEGORIES=${CATEGORIES:-2000}
TAGS=${TAGS:-5000}
SCENARIOS=${SCENARIOS:-}
EXTRA_PG_ARGS=${EXTRA_PG_ARGS:-}
RESULTS_DIR=${RESULTS_DIR:-benchmarks/pg19/results}
RUNS_DIR="${RESULTS_DIR}/runs"
REPORT_OUT=${REPORT_OUT:-${RESULTS_DIR}/$(date +%Y-%m-%d)-pg18-vs-pg19.md}

mkdir -p "$RUNS_DIR"
rm -f "$RUNS_DIR"/*.json "$RUNS_DIR"/*.log

GPID=""
CNAME=""
TMP_CFG=$(mktemp /tmp/graphpg-pg19-XXXX.yaml)

cleanup() {
  [ -n "${GPID:-}" ] && kill "$GPID" 2>/dev/null || true
  [ -n "${CNAME:-}" ] && "$RUNTIME" rm -f "$CNAME" >/dev/null 2>&1 || true
  rm -f "$TMP_CFG"
}
trap cleanup EXIT

sed "s/^  port: .*/  port: ${APP_PORT}/" benchmarks/pg19/graphpg.yaml > "$TMP_CFG"

JIT_ARGS=()
[ "$JIT" != "default" ] && JIT_ARGS=(-c "jit=${JIT}")

psql_c() { "$RUNTIME" exec "$CNAME" psql -h 127.0.0.1 -U postgres -d bench -tAc "$1"; }

run_one() {
  local label=$1 image=$2 round=$3
  local tag="r${round}-${label}"
  CNAME="graphpg-bench-${label}"
  "$RUNTIME" rm -f "$CNAME" >/dev/null 2>&1 || true

  echo "== [round ${round}] Starting ${label} (${image}) on port ${PG_PORT}"
  # shellcheck disable=SC2086
  "$RUNTIME" run -d --rm --name "$CNAME" \
    -p "${PG_PORT}:5432" \
    -e POSTGRES_PASSWORD=postgres \
    -e POSTGRES_DB=bench \
    "$image" \
    -c shared_buffers=512MB \
    -c work_mem=16MB \
    -c effective_cache_size=2GB \
    -c max_connections=100 \
    "${JIT_ARGS[@]}" \
    $EXTRA_PG_ARGS >/dev/null

  for _ in $(seq 1 60); do
    "$RUNTIME" exec "$CNAME" pg_isready -h 127.0.0.1 -U postgres -d bench >/dev/null 2>&1 && break
    sleep 0.5
  done
  # pg_isready can succeed against the entrypoint's temporary server; wait for the real one
  for _ in $(seq 1 60); do psql_c "SELECT 1" >/dev/null 2>&1 && break; sleep 0.5; done

  echo "== Loading benchmark schema (${POSTS} posts) into ${label}"
  "$RUNTIME" exec -i "$CNAME" psql -U postgres -d bench -q -v ON_ERROR_STOP=1 \
    -v authors="$AUTHORS" -v posts="$POSTS" -v comments="$COMMENTS" \
    -v categories="$CATEGORIES" -v tags="$TAGS" < benchmarks/pg19/schema.sql >/dev/null

  # Commit-rate probe: one client, autocommit INSERTs into a scratch table for 3 s
  psql_c "CREATE TABLE public.commit_probe (id bigint GENERATED ALWAYS AS IDENTITY, v int)" >/dev/null
  printf 'INSERT INTO public.commit_probe (v) VALUES (1);\n' |
    "$RUNTIME" exec -i "$CNAME" sh -c 'cat > /tmp/probe.sql'
  local commits_per_s
  commits_per_s=$("$RUNTIME" exec "$CNAME" pgbench -h 127.0.0.1 -U postgres -n -c 1 -T 3 -f /tmp/probe.sql bench 2>/dev/null |
    awk '/^tps/ {printf "%.0f", $3}')
  psql_c "DROP TABLE public.commit_probe; CHECKPOINT" >/dev/null

  local version jit toast load_before
  version=$(psql_c "SELECT current_setting('server_version')")
  jit=$(psql_c "SELECT current_setting('jit')")
  toast=$(psql_c "SELECT current_setting('default_toast_compression')")
  load_before=$(cut -d' ' -f1-3 /proc/loadavg)
  echo "   PostgreSQL ${version} | jit=${jit} | toast=${toast} | commits/s=${commits_per_s} | load=${load_before}"

  echo "== Starting graphpg (${GRAPHPG_BIN}) against ${label} on port ${APP_PORT}"
  DATABASE_URL="postgres://postgres:postgres@localhost:${PG_PORT}/bench" \
    "$GRAPHPG_BIN" --config "$TMP_CFG" --mode prod \
    >"${RUNS_DIR}/${tag}-graphpg.log" 2>&1 &
  GPID=$!

  for _ in $(seq 1 60); do
    curl -sf "http://localhost:${APP_PORT}/ready" >/dev/null && break
    sleep 0.5
  done
  curl -sf "http://localhost:${APP_PORT}/ready" >/dev/null

  # Smoke check key endpoints before timing
  curl -sf "http://localhost:${APP_PORT}/rest/posts?first=1" >/dev/null
  curl -sf "http://localhost:${APP_PORT}/rest/posts?anti=comments&first=1" >/dev/null
  curl -sf "http://localhost:${APP_PORT}/rest/categories?include=ancestors&first=1&offset=1600" >/dev/null
  curl -sf "http://localhost:${APP_PORT}/rest/posts/aggregate?group=authorId,status&sum=priority" >/dev/null
  curl -sf -X POST "http://localhost:${APP_PORT}/graphql" \
    -H "Content-Type: application/json" \
    -d '{"query":"mutation { createNote(input: { authorId: 1, body: \"smoke\", tag: { connectOrCreate: { where: { slug: \"tag-1\" }, create: { slug: \"tag-1\", name: \"Tag 1\" } } } }) { note { databaseId tag { slug } } } }"}' \
    | grep -q '"tag-1"'
  curl -sf -X POST "http://localhost:${APP_PORT}/export" \
    -H "Content-Type: application/json" \
    -d '{"query":"{ postsConnection(first: 2, orderBy: [PRIMARY_KEY_ASC]) { nodes { databaseId } } }"}' \
    | grep -q '"databaseId"'

  echo "== Running k6 (${label}, round ${round}, VUS=${VUS}, DURATION=${DURATION}s)"
  LABEL="$label" \
    URL="http://localhost:${APP_PORT}" \
    VUS="$VUS" \
    DURATION="$DURATION" \
    AUTHORS="$AUTHORS" \
    POSTS="$POSTS" \
    TAGS="$TAGS" \
    CATEGORIES="$CATEGORIES" \
    SCENARIOS="$SCENARIOS" \
    OUT="${RUNS_DIR}/${tag}.json" \
    k6 run --quiet benchmarks/pg19/pg19.js >/dev/null

  local load_after
  load_after=$(cut -d' ' -f1-3 /proc/loadavg)
  python3 - "${RUNS_DIR}/${tag}.json" <<PY
import json, sys
path = sys.argv[1]
with open(path) as f: d = json.load(f)
d["meta"] = {
    "round": ${round}, "label": "${label}", "image": "${image}", "version": "${version}",
    "jit": "${jit}", "toast": "${toast}", "commits_per_s": int("${commits_per_s:-0}" or 0),
    "load_before": "${load_before}", "load_after": "${load_after}",
}
with open(path, "w") as f: json.dump(d, f, indent=2)
PY

  kill "$GPID" 2>/dev/null || true
  wait "$GPID" 2>/dev/null || true
  GPID=""
  "$RUNTIME" rm -f "$CNAME" >/dev/null 2>&1 || true
  CNAME=""
}

for round in $(seq 1 "$ROUNDS"); do
  if (( round % 2 == 1 )); then
    run_one pg18 "$PG18_IMAGE" "$round"
    run_one pg19 "$PG19_IMAGE" "$round"
  else
    run_one pg19 "$PG19_IMAGE" "$round"
    run_one pg18 "$PG18_IMAGE" "$round"
  fi
done

python3 - "$RUNS_DIR" "$REPORT_OUT" "$VUS" "$DURATION" "$ROUNDS" "$JIT" <<'PY'
import glob, json, os, statistics, sys

runs_dir, out_path, vus, duration, rounds, jit = sys.argv[1:7]
runs = []
for path in sorted(glob.glob(os.path.join(runs_dir, "*.json"))):
    with open(path) as f:
        runs.append(json.load(f))

by = {"pg18": {}, "pg19": {}}
order = []
for run in runs:
    for s in run["scenarios"]:
        by[run["label"]].setdefault(s["scenario"], []).append(s)
        if s["scenario"] not in order:
            order.append(s["scenario"])

med = statistics.median
fail = lambda s: max(s["httpFailedPct"], s["checkFailPct"])

def cell_rps(rows):
    values = [r["rps"] for r in rows]
    return f"{med(values):.0f} ({min(values):.0f}–{max(values):.0f})"

lines = [
    "# PostgreSQL 18 vs PostgreSQL 19 under graphpg",
    "",
    f"{rounds} interleaved rounds per version, {vus} VUs, {duration} s per scenario, `jit` "
    + ("left at each server's default" if jit == "default" else f"pinned to `{jit}` on both servers")
    + ". Each cell is the median across rounds; req/s also shows the range (min–max). "
    "Δ compares medians. A difference smaller than the ranges is noise.",
    "",
    "| scenario | PG 18 req/s | PG 19 req/s | Δ req/s | PG 18 p50 / p95 ms | PG 19 p50 / p95 ms | PG 18 fail % | PG 19 fail % |",
    "| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
]
for name in order:
    a, b = by["pg18"].get(name), by["pg19"].get(name)
    if not a or not b:
        continue
    ra, rb = med([r["rps"] for r in a]), med([r["rps"] for r in b])
    delta = (rb - ra) / ra * 100.0 if ra > 0 else 0.0
    lines.append(
        f"| {name} | {cell_rps(a)} | {cell_rps(b)} | {delta:+.1f}% "
        f"| {med([r['p50'] for r in a]):.2f} / {med([r['p95'] for r in a]):.2f} "
        f"| {med([r['p50'] for r in b]):.2f} / {med([r['p95'] for r in b]):.2f} "
        f"| {med([fail(r) for r in a]):.1f}% (max {max(fail(r) for r in a):.1f}%) "
        f"| {med([fail(r) for r in b]):.1f}% (max {max(fail(r) for r in b):.1f}%) |"
    )

lines += [
    "",
    "## Run conditions",
    "",
    "`commits/s` is a 3 s single-client `INSERT` probe on the fresh database before the run (WAL flush speed); "
    "a run with a much lower value than its peers had a busy or slow disk.",
    "",
    "| round | server | version | jit | default TOAST | commits/s | load avg before → after |",
    "| ---: | :--- | :--- | :--- | :--- | ---: | :--- |",
]
for run in sorted(runs, key=lambda r: (r["meta"]["round"], r["label"])):
    m = run["meta"]
    lines.append(
        f"| {m['round']} | {m['label']} | {m['version']} | {m['jit']} | {m['toast']} "
        f"| {m['commits_per_s']} | {m['load_before']} → {m['load_after']} |"
    )

report = "\n".join(lines) + "\n"
with open(out_path, "w") as f:
    f.write(report)
print("\n" + report)
PY
