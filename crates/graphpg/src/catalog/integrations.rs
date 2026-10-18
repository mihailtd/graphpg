//! Integrations with PostgreSQL extensions (see `docs/extensions.md`): what graphpg infers from
//! an extension the database **uses**, not merely has installed. Each integration detects its
//! use in the exposed schemas (hypertables, JSON Schema checks, index access methods, foreign tables)
//! and can be switched off with `extensions.integrations.<name>: off`.
use serde::{Deserialize, Serialize};

/// An extension graphpg integrates with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Integration {
    TimescaleDb,
    PgJsonschema,
    Pgvectorscale,
    VectorChord,
    PgRoonga,
    PgStatStatements,
    PostgresFdw,
}

impl Integration {
    pub const ALL: [Self; 7] = [
        Self::TimescaleDb,
        Self::PgJsonschema,
        Self::Pgvectorscale,
        Self::VectorChord,
        Self::PgRoonga,
        Self::PgStatStatements,
        Self::PostgresFdw,
    ];

    /// The name under `extensions.integrations`.
    pub fn name(self) -> &'static str {
        match self {
            Self::TimescaleDb => "timescaledb",
            Self::PgJsonschema => "pg_jsonschema",
            Self::Pgvectorscale => "pgvectorscale",
            Self::VectorChord => "vectorchord",
            Self::PgRoonga => "pgroonga",
            Self::PgStatStatements => "pg_stat_statements",
            Self::PostgresFdw => "postgres_fdw",
        }
    }

    /// The extension's name in `pg_extension`.
    pub fn extension(self) -> &'static str {
        match self {
            Self::TimescaleDb => "timescaledb",
            Self::PgJsonschema => "pg_jsonschema",
            Self::Pgvectorscale => "vectorscale",
            Self::VectorChord => "vchord",
            Self::PgRoonga => "pgroonga",
            Self::PgStatStatements => "pg_stat_statements",
            Self::PostgresFdw => "postgres_fdw",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.name() == name)
    }

    /// The query counting the uses of the extension in the schemas `$1` (`text[]`): an
    /// integration is active when it counts more than none. `None`: installed is used (a tool
    /// for operators, not something the schema uses).
    pub(crate) fn use_query(self) -> Option<&'static str> {
        Some(match self {
            Self::TimescaleDb => {
                "SELECT count(*) FROM timescaledb_information.hypertables WHERE hypertable_schema = ANY($1)"
            }
            // Checks of the schemas' tables and domains, and of the domains their columns use
            Self::PgJsonschema => {
                "SELECT count(*) FROM pg_constraint con
                 WHERE con.contype = 'c'
                   AND pg_get_constraintdef(con.oid) ~ '\\mjsonb?_matches_schema\\('
                   AND (con.connamespace IN (SELECT oid FROM pg_namespace WHERE nspname = ANY($1))
                        OR con.contypid IN (
                            SELECT a.atttypid FROM pg_attribute a
                            JOIN pg_class c ON c.oid = a.attrelid
                            JOIN pg_namespace n ON n.oid = c.relnamespace
                            WHERE n.nspname = ANY($1) AND a.attnum > 0 AND NOT a.attisdropped))"
            }
            Self::Pgvectorscale => INDEXES_USING_DISKANN,
            Self::VectorChord => INDEXES_USING_VCHORD,
            Self::PgRoonga => INDEXES_USING_PGROONGA,
            Self::PostgresFdw => POSTGRES_FDW_TABLES,
            Self::PgStatStatements => return None,
        })
    }

    /// What one use is called, for the startup report.
    pub(crate) fn unit(self) -> &'static str {
        match self {
            Self::TimescaleDb => "hypertable",
            Self::PgJsonschema => "JSON Schema check",
            Self::Pgvectorscale | Self::VectorChord | Self::PgRoonga => "index",
            Self::PostgresFdw => "foreign table",
            Self::PgStatStatements => "",
        }
    }
}

const POSTGRES_FDW_TABLES: &str =
    "SELECT count(*) FROM pg_foreign_table ft JOIN pg_class c ON c.oid = ft.ftrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace JOIN pg_foreign_server s ON s.oid = ft.ftserver
    JOIN pg_foreign_data_wrapper w ON w.oid = s.srvfdw
    WHERE n.nspname = ANY($1) AND w.fdwname = 'postgres_fdw'";
const INDEXES_USING_DISKANN: &str =
    "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
    JOIN pg_am am ON am.oid = c.relam JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = ANY($1) AND am.amname = 'diskann'";
const INDEXES_USING_VCHORD: &str =
    "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
    JOIN pg_am am ON am.oid = c.relam JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = ANY($1) AND am.amname IN ('vchordrq', 'vchordg')";
const INDEXES_USING_PGROONGA: &str =
    "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
    JOIN pg_am am ON am.oid = c.relam JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = ANY($1) AND am.amname = 'pgroonga'";

/// An integration in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveIntegration {
    pub integration: Integration,
    /// The extension's version.
    pub version: String,
    /// The schema the extension's objects are in.
    #[serde(default)]
    pub schema: String,
    /// What made it active: `3 hypertables`, `installed`.
    pub detail: String,
}

/// Whether an integration may be used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntegrationMode {
    /// When the database uses the extension (the default).
    #[default]
    Auto,
    /// Never.
    Off,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for i in Integration::ALL {
            assert_eq!(Integration::from_name(i.name()), Some(i));
        }
        assert_eq!(Integration::from_name("timescale"), None);
    }
}
