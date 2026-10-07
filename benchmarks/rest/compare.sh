#!/usr/bin/env bash
# Runs benchmarks/rest/rest.js against graphpg and PostgREST on one PostgreSQL, same schema,
# same data, same pool size, one after the other. See README.md.
#
#   PGURL=postgres://postgres:postgres@localhost:5432 benchmarks/rest/compare.sh
#
# Needs: psql (or PSQL), k6, a graphpg binary (GRAPHPG_BIN, default target/release/graphpg) and a
# container runtime (RUNTIME, default docker) to run PostgREST.
set -euo pipefail
cd "$(dirname "$0")/../.."

PGURL=${PGURL:-postgres://postgres:postgres@localhost:5432}
DB=${DB:-bench}
GRAPHPG_BIN=${GRAPHPG_BIN:-target/release/graphpg}
RUNTIME=${RUNTIME:-docker}
# Where psql runs; `PSQL="docker exec -i <container> psql"` when it is not installed
PSQL=${PSQL:-psql}
POSTGREST_IMAGE=${POSTGREST_IMAGE:-docker.io/postgrest/postgrest:latest}
POOL=${POOL:-10}
VUS=${VUS:-20}
DURATION=${DURATION:-10}
POSTS=${POSTS:-100000}
AUTHORS=${AUTHORS:-1000}
COMMENTS=${COMMENTS:-300000}
mkdir -p benchmarks/rest/results

echo "== loading $POSTS posts into $DB"
$PSQL "$PGURL/postgres" -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" -c "CREATE DATABASE $DB"
$PSQL "$PGURL/$DB" -q -v authors="$AUTHORS" -v posts="$POSTS" -v comments="$COMMENTS" <benchmarks/rest/schema.sql

cleanup() {
  [ -n "${GPID:-}" ] && kill "$GPID" 2>/dev/null || true
  "$RUNTIME" rm -f graphpg-bench-postgrest >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== graphpg"
DATABASE_URL="$PGURL/$DB" "$GRAPHPG_BIN" --config benchmarks/rest/graphpg.yaml --mode prod \
  >benchmarks/rest/results/graphpg.log 2>&1 &
GPID=$!
echo "== PostgREST"
"$RUNTIME" run -d --rm --name graphpg-bench-postgrest --network host \
  -e PGRST_DB_URI="$PGURL/$DB" -e PGRST_DB_SCHEMAS=bench -e PGRST_DB_ANON_ROLE=bench_anon \
  -e PGRST_DB_POOL="$POOL" -e PGRST_SERVER_PORT=3000 -e PGRST_DB_MAX_ROWS=1000 \
  "$POSTGREST_IMAGE" >/dev/null

for _ in $(seq 1 60); do
  curl -sf "http://localhost:4000/rest/posts?first=1" >/dev/null &&
    curl -sf "http://localhost:3000/posts?limit=1" >/dev/null && break
  sleep 1
done

for target in graphpg postgrest; do
  TARGET=$target VUS=$VUS DURATION=$DURATION POSTS=$POSTS k6 run --quiet benchmarks/rest/rest.js
done
