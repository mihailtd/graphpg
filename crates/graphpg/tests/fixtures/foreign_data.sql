-- Foreign data (tests/foreign_data.rs): a "remote" PostgreSQL table reached with postgres_fdw
-- (a loopback connection to this database, standing in for another server), and a CSV
-- file read with file_fdw (written by the test with COPY before each run).
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE EXTENSION IF NOT EXISTS file_fdw;

-- The remote side: not exposed by graphpg, only reached through the foreign server
CREATE SCHEMA fdw_remote;
CREATE TABLE fdw_remote.invoices (
    id integer PRIMARY KEY,
    customer_id integer NOT NULL,
    amount numeric(10, 2) NOT NULL,
    status text NOT NULL
);
INSERT INTO fdw_remote.invoices VALUES
    (1, 1, 100.00, 'PAID'), (2, 1, 250.00, 'OPEN'), (3, 2, 75.50, 'OPEN');

-- The API's schema
CREATE SCHEMA fdw_app;
CREATE TABLE fdw_app.customers (
    id integer PRIMARY KEY,
    name text NOT NULL,
    country_code text NOT NULL
);
INSERT INTO fdw_app.customers VALUES (1, 'Acme', 'RO'), (2, 'Globex', 'DE'), (3, 'Initech', 'RO');

-- A loopback server over the Unix socket (trusted in the postgres image, so no password)
DO $$
BEGIN
    EXECUTE format(
        'CREATE SERVER fdw_loopback FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host %L, port %L, dbname %L)',
        split_part(current_setting('unix_socket_directories'), ',', 1),
        current_setting('port'),
        current_database()
    );
END $$;
CREATE USER MAPPING FOR CURRENT_USER SERVER fdw_loopback;
-- What `IMPORT FOREIGN SCHEMA fdw_remote LIMIT TO (invoices) ...` creates; written out because
-- the import connects to the remote side, which cannot see this uncommitted fixture
CREATE FOREIGN TABLE fdw_app.invoices (
    id integer NOT NULL,
    customer_id integer NOT NULL,
    amount numeric(10, 2) NOT NULL,
    status text NOT NULL
) SERVER fdw_loopback OPTIONS (schema_name 'fdw_remote', table_name 'invoices');

-- A file on the database server
CREATE SERVER fdw_files FOREIGN DATA WRAPPER file_fdw;
CREATE FOREIGN TABLE fdw_app.countries (code text NOT NULL, name text NOT NULL)
    SERVER fdw_files
    OPTIONS (filename '/tmp/graphpg_fdw_countries.csv', format 'csv', header 'true');

-- The recommended shape for a file: a materialized view with real indexes
CREATE MATERIALIZED VIEW fdw_app.country_list AS SELECT code, name FROM fdw_app.countries
    WITH NO DATA;
CREATE UNIQUE INDEX ON fdw_app.country_list (code);
