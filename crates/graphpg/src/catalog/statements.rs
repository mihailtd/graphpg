//! `graphpg advise --statements`: what graphpg's own statements cost, from
//! `pg_stat_statements`, with the indexes their plans lack (checked with hypopg when installed).
//!
//! graphpg tags every statement with sqlcommenter comments (`graphql_operation='GetUsers'`, the
//! REST route), and `pg_stat_statements` keeps them in its query text, so its numbers group by
//! operation. Each statement is planned with `EXPLAIN (GENERIC_PLAN)` (PostgreSQL 16+), which
//! plans the normalized text, `$n` parameters and all, without running it.
use super::integrations::Integration;
use super::models::DatabaseCatalog;
use crate::compiler::quote_ident;
use std::collections::HashMap;
use tokio_postgres::Client;

/// A sequential scan costing less is cheap enough to leave alone.
const MIN_SCAN_COST: f64 = 1000.0;
/// A hypothetical index must lower the statement's cost by at least this fraction.
const MIN_GAIN: f64 = 0.1;

/// graphpg's statements, the most expensive (total time) first.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementsReport {
    /// When `pg_stat_statements` was last reset: the numbers are since then.
    pub since: Option<String>,
    /// Whether hypopg checked the suggested indexes.
    pub hypopg: bool,
    pub operations: Vec<OperationStats>,
}

/// The statements of one operation (a GraphQL operation, a REST route), summed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationStats {
    /// `query GetUsers`, `mutation CreateUser`, `GET /rest/users`.
    pub operation: String,
    pub calls: i64,
    pub total_ms: f64,
    pub rows: i64,
    /// Distinct statements (shapes) of the operation.
    pub statements: usize,
    pub suggestions: Vec<IndexSuggestion>,
}

impl OperationStats {
    pub fn mean_ms(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.total_ms / self.calls as f64
        }
    }
}

/// An index a statement's plan lacks: a sequential scan filtering (or sorting) on its columns.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexSuggestion {
    /// `CREATE INDEX ON "app"."orders" ("status")`.
    pub sql: String,
    /// The statement's estimated cost, as planned.
    pub cost: f64,
    /// Its estimated cost with the index (hypopg), when checked.
    pub cost_with_index: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum StatementsError {
    #[error("pg_stat_statements is not installed in this database (or `extensions.integrations.pg_stat_statements` is off)")]
    NotInstalled,
    #[error("pg_stat_statements could not be read: {0} (it needs `shared_preload_libraries = 'pg_stat_statements'`, and the role pg_read_all_stats to see other roles' statements)")]
    Unreadable(tokio_postgres::Error),
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
}

/// The `top` most expensive operations of graphpg, by total execution time.
pub async fn top_statements(
    client: &Client,
    catalog: &DatabaseCatalog,
    top: usize,
) -> Result<StatementsReport, StatementsError> {
    let pgss = catalog
        .integration(Integration::PgStatStatements)
        .ok_or(StatementsError::NotInstalled)?;
    let ext = quote_ident(&pgss.schema);
    let rows = client
        .query(
            &format!(
                "SELECT query, calls, total_exec_time, rows FROM {ext}.pg_stat_statements
                 WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
                   AND query LIKE '%application=''graphpg''%'
                   -- not the plans of earlier runs of this report
                   AND query !~* '^\\s*EXPLAIN'
                 ORDER BY total_exec_time DESC"
            ),
            &[],
        )
        .await
        .map_err(StatementsError::Unreadable)?;
    let since: Option<String> = client
        .query_opt(
            &format!("SELECT stats_reset::text FROM {ext}.pg_stat_statements_info"),
            &[],
        )
        .await
        .ok()
        .flatten()
        .and_then(|r| r.get(0));

    // Summed by operation, the statements of each kept (heaviest first)
    let mut order: Vec<String> = Vec::new();
    let mut by_operation: HashMap<String, (OperationStats, Vec<String>)> = HashMap::new();
    for row in rows {
        let query: String = row.get(0);
        let Some(operation) = operation_label(&query) else {
            continue;
        };
        let entry = by_operation.entry(operation.clone()).or_insert_with(|| {
            order.push(operation.clone());
            (
                OperationStats {
                    operation,
                    calls: 0,
                    total_ms: 0.0,
                    rows: 0,
                    statements: 0,
                    suggestions: Vec::new(),
                },
                Vec::new(),
            )
        });
        entry.0.calls += row.get::<_, i64>(1);
        entry.0.total_ms += row.get::<_, f64>(2);
        entry.0.rows += row.get::<_, i64>(3);
        entry.0.statements += 1;
        entry.1.push(query);
    }
    let mut operations: Vec<(OperationStats, Vec<String>)> = order
        .into_iter()
        .filter_map(|o| by_operation.remove(&o))
        .collect();
    operations.sort_by(|a, b| b.0.total_ms.total_cmp(&a.0.total_ms));
    operations.truncate(top);

    let hypopg = client
        .query_opt(
            "SELECT n.nspname::text FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
             WHERE e.extname = 'hypopg'",
            &[],
        )
        .await?
        .map(|r| quote_ident(&r.get::<_, String>(0)));

    let mut report = StatementsReport {
        since,
        hypopg: hypopg.is_some(),
        operations: Vec::new(),
    };
    for (mut stats, queries) in operations {
        for query in &queries {
            for suggestion in suggest_indexes(client, query, hypopg.as_deref()).await {
                if !stats.suggestions.iter().any(|s| s.sql == suggestion.sql) {
                    stats.suggestions.push(suggestion);
                }
            }
        }
        report.operations.push(stats);
    }
    Ok(report)
}

/// The operation a statement's sqlcommenter tags name: `query GetUsers`, `GET /rest/users`.
pub fn operation_label(query: &str) -> Option<String> {
    let start = query.rfind("/*")?;
    let comment = query[start + 2..].trim_end().strip_suffix("*/")?;
    let mut tags: HashMap<&str, String> = HashMap::new();
    for pair in comment.split(',') {
        let (key, value) = pair.split_once('=')?;
        let value = value.trim().trim_matches('\'').replace("\\'", "'");
        tags.insert(key.trim(), percent_decode(&value));
    }
    if tags.get("application").map(String::as_str) != Some("graphpg") {
        return None;
    }
    if let Some(resource) = tags.get("rest_resource") {
        let method = tags.get("rest_method").map(String::as_str).unwrap_or("");
        return Some(format!("{method} /rest/{resource}").trim().to_string());
    }
    let kind = tags
        .get("graphql_type")
        .map(String::as_str)
        .unwrap_or("query");
    let name = tags
        .get("graphql_operation")
        .map(String::as_str)
        .unwrap_or("anonymous");
    Some(format!("{kind} {name}"))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Indexes `query`'s generic plan lacks. Errors (a statement that cannot be planned
/// generically) mean no suggestions.
async fn suggest_indexes(
    client: &Client,
    query: &str,
    hypopg: Option<&str>,
) -> Vec<IndexSuggestion> {
    let Some(plan) = generic_plan(client, query).await else {
        return Vec::new();
    };
    let cost = plan["Plan"]["Total Cost"].as_f64().unwrap_or(0.0);
    let mut scans = Vec::new();
    let mut sort_keys = Vec::new();
    walk(&plan["Plan"], &mut scans, &mut sort_keys);

    let mut out: Vec<IndexSuggestion> = Vec::new();
    // An index scan filtering what it reads may be better served by another index; only hypopg
    // can tell (under a `LIMIT`, it may read little)
    for scan in scans
        .iter()
        .filter(|s| s.cost >= MIN_SCAN_COST && (s.sequential || hypopg.is_some()))
    {
        let sorted: Vec<String> = sort_keys
            .iter()
            .flat_map(|key| columns_of(key, &scan.alias))
            .filter(|c| !scan.filter_columns.contains(c))
            .take(1)
            .collect();
        // Candidates: the filter columns, then with the sort column (an index read in order)
        let mut candidates: Vec<Vec<String>> = Vec::new();
        if !scan.filter_columns.is_empty() {
            candidates.push(scan.filter_columns.clone());
            if scan.filter_columns.len() > 1 {
                candidates.extend(scan.filter_columns.iter().map(|c| vec![c.clone()]));
            }
        }
        if !sorted.is_empty() {
            candidates.push([scan.filter_columns.clone(), sorted].concat());
        }
        let table = format!(
            "{}.{}",
            quote_ident(&scan.schema),
            quote_ident(&scan.relation)
        );
        let index_sql = |columns: &[String]| {
            let columns: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
            format!("CREATE INDEX ON {table} ({})", columns.join(", "))
        };
        let best = match hypopg {
            // The candidate lowering the cost the most, if it lowers it enough
            Some(ext) => {
                let mut best: Option<IndexSuggestion> = None;
                for columns in &candidates {
                    let sql = index_sql(columns);
                    let Some(with_index) = hypothetical_cost(client, ext, &sql, query).await else {
                        continue;
                    };
                    if with_index <= cost * (1.0 - MIN_GAIN)
                        && best
                            .as_ref()
                            .is_none_or(|b| Some(with_index) < b.cost_with_index)
                    {
                        best = Some(IndexSuggestion {
                            sql,
                            cost,
                            cost_with_index: Some(with_index),
                        });
                    }
                }
                best
            }
            None => candidates.first().map(|columns| IndexSuggestion {
                sql: index_sql(columns),
                cost,
                cost_with_index: None,
            }),
        };
        if let Some(best) = best {
            if !out.iter().any(|s| s.sql == best.sql) {
                out.push(best);
            }
        }
    }
    out
}

/// `query`'s generic plan (verbose JSON), planned in a read-only transaction.
async fn generic_plan(client: &Client, query: &str) -> Option<serde_json::Value> {
    client
        .batch_execute("BEGIN READ ONLY; SET LOCAL statement_timeout = '10s'")
        .await
        .ok()?;
    // The simple protocol: the `$n` of the text are the generic plan's, not parameters to bind
    let plan = client
        .simple_query(&format!(
            "EXPLAIN (GENERIC_PLAN, VERBOSE, FORMAT JSON) {query}"
        ))
        .await;
    let _ = client.batch_execute("ROLLBACK").await;
    let text = plan.ok()?.into_iter().find_map(|m| match m {
        tokio_postgres::SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
        _ => None,
    })?;
    let plan: serde_json::Value = serde_json::from_str(&text).ok()?;
    plan.get(0).cloned()
}

/// `query`'s estimated cost with the hypothetical index `index_sql` (hypopg).
async fn hypothetical_cost(
    client: &Client,
    ext: &str,
    index_sql: &str,
    query: &str,
) -> Option<f64> {
    client
        .query(
            &format!("SELECT {ext}.hypopg_create_index($1)"),
            &[&index_sql],
        )
        .await
        .ok()?;
    let plan = generic_plan(client, query).await;
    let _ = client
        .batch_execute(&format!("SELECT {ext}.hypopg_reset()"))
        .await;
    plan?["Plan"]["Total Cost"].as_f64()
}

/// A scan of a plan filtering rows (sequential, or an index scan with a `Filter`), and the
/// columns its filter reads.
struct SeqScan {
    sequential: bool,
    schema: String,
    relation: String,
    alias: String,
    cost: f64,
    filter_columns: Vec<String>,
}

fn walk(node: &serde_json::Value, scans: &mut Vec<SeqScan>, sort_keys: &mut Vec<String>) {
    let text = |key: &str| node[key].as_str().unwrap_or_default().to_string();
    let node_type = node["Node Type"].as_str().unwrap_or_default();
    let sequential = node_type == "Seq Scan";
    let filtering_index_scan =
        matches!(node_type, "Index Scan" | "Index Only Scan") && node.get("Filter").is_some();
    if (sequential || filtering_index_scan) && node.get("Relation Name").is_some() {
        let alias = text("Alias");
        scans.push(SeqScan {
            sequential,
            schema: text("Schema"),
            relation: text("Relation Name"),
            filter_columns: columns_of(&text("Filter"), &alias),
            alias,
            cost: node["Total Cost"].as_f64().unwrap_or(0.0),
        });
    }
    if node["Node Type"] == "Sort" {
        sort_keys.extend(
            node["Sort Key"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|k| k.as_str().map(str::to_string)),
        );
    }
    for child in node["Plans"].as_array().into_iter().flatten() {
        walk(child, scans, sort_keys);
    }
}

/// The columns of `alias` an expression of a verbose plan reads (`s0.status`, `s0."Name"`),
/// in order, without system columns.
fn columns_of(expression: &str, alias: &str) -> Vec<String> {
    let prefix = format!("{alias}.");
    let mut out: Vec<String> = Vec::new();
    let mut rest = expression;
    while let Some(at) = rest.find(&prefix) {
        // A whole identifier: not the tail of another (`xs0.a`)
        let preceded = rest[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"');
        rest = &rest[at + prefix.len()..];
        if preceded {
            continue;
        }
        let column = if let Some(quoted) = rest.strip_prefix('"') {
            let mut name = String::new();
            let mut chars = quoted.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        name.push('"');
                        continue;
                    }
                    break;
                }
                name.push(c);
            }
            name
        } else {
            rest.chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect()
        };
        let system = matches!(
            column.as_str(),
            "ctid" | "tableoid" | "xmin" | "xmax" | "cmin" | "cmax"
        );
        if !column.is_empty() && !system && !out.contains(&column) {
            out.push(column);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_operations_by_their_tags() {
        assert_eq!(
            operation_label("SELECT 1 /*application='graphpg',graphql_operation='Get%20Users',graphql_type='query'*/").as_deref(),
            Some("query Get Users")
        );
        assert_eq!(
            operation_label(
                "SELECT 1 /*application='graphpg',rest_method='GET',rest_resource='users'*/"
            )
            .as_deref(),
            Some("GET /rest/users")
        );
        assert_eq!(operation_label("SELECT 1 /*application='other'*/"), None);
        assert_eq!(operation_label("SELECT 1"), None);
    }

    #[test]
    fn reads_the_columns_of_an_alias() {
        assert_eq!(
            columns_of("((s0.status = $1) AND (s0.\"Due \"\"At\" < now()) AND (xs0.a = 1) AND (s1.b = s0.id))", "s0"),
            ["status", "Due \"At", "id"]
        );
        assert_eq!(columns_of("s0.created_at DESC", "s0"), ["created_at"]);
        assert!(columns_of("(s0.ctid = $1)", "s0").is_empty());
    }
}
