# The PostgreSQL the integration tests run against: PostgreSQL 18 with the extensions graphpg
# integrates with (docs/extensions.md), so their integrations are tested against the real
# thing. Published as ghcr.io/mihailtd/graphpg-test-pg:18 by .github/workflows/test-image.yml.
FROM docker.io/library/postgres:18-trixie

ARG PGMQ_VERSION=1.10.0
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends curl ca-certificates gnupg make \
        postgresql-server-dev-18 \
        postgresql-18-pgvector postgresql-18-postgis-3 postgresql-18-cron \
        postgresql-18-hypopg postgresql-18-partman; \
    # TimescaleDB from Timescale's repository: the full edition (continuous aggregates,
    # gap filling, compression); the distribution's is the Apache-only one
    curl -fsSL https://packagecloud.io/timescale/timescaledb/gpgkey | gpg --dearmor -o /usr/share/keyrings/timescaledb.gpg; \
    echo "deb [signed-by=/usr/share/keyrings/timescaledb.gpg] https://packagecloud.io/timescale/timescaledb/debian/ trixie main" > /etc/apt/sources.list.d/timescaledb.list; \
    apt-get update; \
    apt-get install -y --no-install-recommends timescaledb-2-postgresql-18; \
    # pgmq: SQL only
    curl -fsSL "https://github.com/pgmq/pgmq/archive/refs/tags/v${PGMQ_VERSION}.tar.gz" | tar -xz -C /tmp; \
    make -C /tmp/pgmq-${PGMQ_VERSION}/pgmq-extension install; \
    rm -rf /tmp/pgmq-*; \
    apt-get purge -y --auto-remove postgresql-server-dev-18 make gnupg; \
    rm -rf /var/lib/apt/lists/*

# Extensions published by their projects (amd64 builds for PostgreSQL 18)
ARG PG_JSONSCHEMA_VERSION=0.3.4
ARG PGVECTORSCALE_VERSION=0.9.1
ARG VCHORD_VERSION=1.1.1
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends curl ca-certificates unzip; \
    cd /tmp; \
    curl -fsSLO "https://github.com/supabase/pg_jsonschema/releases/download/v${PG_JSONSCHEMA_VERSION}/pg_jsonschema-v${PG_JSONSCHEMA_VERSION}-pg18-amd64-linux-gnu.deb"; \
    curl -fsSLO "https://github.com/tensorchord/VectorChord/releases/download/${VCHORD_VERSION}/postgresql-18-vchord_${VCHORD_VERSION}-1_amd64.deb"; \
    curl -fsSLO "https://github.com/timescale/pgvectorscale/releases/download/${PGVECTORSCALE_VERSION}/pgvectorscale-${PGVECTORSCALE_VERSION}-pg18-amd64.zip"; \
    unzip -o "pgvectorscale-${PGVECTORSCALE_VERSION}-pg18-amd64.zip"; \
    apt-get install -y --no-install-recommends ./*.deb; \
    # PGroonga from the Groonga repository
    curl -fsSLO https://packages.groonga.org/debian/groonga-apt-source-latest-trixie.deb; \
    apt-get install -y --no-install-recommends ./groonga-apt-source-latest-trixie.deb; \
    apt-get update; \
    apt-get install -y --no-install-recommends postgresql-18-pgdg-pgroonga; \
    rm -rf /tmp/*.deb /tmp/*.zip; \
    apt-get purge -y --auto-remove unzip; \
    rm -rf /var/lib/apt/lists/*

# Preloaded libraries some extensions need
CMD ["postgres", "-c", "shared_preload_libraries=timescaledb,pg_cron,pg_stat_statements,vchord", "-c", "cron.database_name=graphpg_test", "-c", "timescaledb.telemetry_level=off"]
