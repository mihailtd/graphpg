//! The statements subscriptions run as: a compiled query batched over many subscriptions'
//! parameters, and the check of whether claims can change a query's result.
use regex::Regex;
use std::sync::LazyLock;

/// A parameter reference of a compiled query: `$1`, `$12`.
static PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(\d+)").expect("valid regex"));

/// A schema-qualified name as the compiler writes them: `"schema"."name"`.
static QUALIFIED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""((?:[^"]|"")+)"\."((?:[^"]|"")+)""#).expect("valid regex"));

/// `sql` (a compiled query returning one row with a `data` column, its parameters `$1` to
/// `$params`, all text) run once per element of the arrays it is given:
///
/// - `$1::int8[]`: the row numbers the results are returned with (`i`);
/// - `$2::text[]`: the hash of the result each row was last sent, or NULL;
/// - `$3::text[]` ... : the values of the query's parameters `$1` ... for each row.
///
/// Each result row is `(i, h, data)`: `h` the hash of the result now, `data` the result when
/// `h` differs from the one given, NULL otherwise (an unchanged result is not sent back).
///
/// The query's sqlcommenter tags (a comment ending it) end the batch instead, where
/// `pg_stat_statements` readers look for them.
pub fn batched_sql(sql: &str, params: usize) -> String {
    let trimmed = sql.trim_end();
    let (query, tags) = match trimmed.rfind("/*") {
        Some(start) if trimmed.ends_with("*/") => (&trimmed[..start], &trimmed[start..]),
        _ => (trimmed, ""),
    };
    let inner = PARAM.replace_all(query, "s.p$1");
    let tags = if tags.is_empty() {
        String::new()
    } else {
        format!(" {tags}")
    };
    let mut columns = String::new();
    let mut arrays = String::new();
    for k in 1..=params {
        columns.push_str(&format!(", p{k}"));
        arrays.push_str(&format!(", ${}::text[]", k + 2));
    }
    format!(
        "SELECT s.i, r.h, CASE WHEN r.h IS DISTINCT FROM s.h THEN r.data END AS data \
         FROM unnest($1::int8[], $2::text[]{arrays}) AS s(i, h{columns}) \
         CROSS JOIN LATERAL (\
         SELECT q.data, encode(sha256(convert_to(q.data::text, 'UTF8')), 'hex') AS h \
         FROM ({inner}\n) AS q) AS r{tags}"
    )
}

/// The schema-qualified names `sql` refers to: tables, views, functions, types.
pub fn qualified_names(sql: &str) -> Vec<(String, String)> {
    let mut names: Vec<(String, String)> = QUALIFIED
        .captures_iter(sql)
        .map(|c| (c[1].replace("\"\"", "\""), c[2].replace("\"\"", "\"")))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether claims cannot change what the names read: each one is a table without Row-Level
/// Security (or a materialized view or foreign table, which have none), or a type. A view, a
/// function or anything unknown could read the claims, and keeps them (`false`).
pub const CLAIMS_FREE_SQL: &str = "\
SELECT coalesce(bool_and(CASE
    WHEN c.oid IS NOT NULL THEN c.relkind IN ('r', 'p', 'm', 'f') AND NOT c.relrowsecurity
    WHEN EXISTS (SELECT 1 FROM pg_proc p WHERE p.pronamespace = ns.oid AND p.proname = n.o) THEN false
    WHEN EXISTS (SELECT 1 FROM pg_type t WHERE t.typnamespace = ns.oid AND t.typname = n.o) THEN true
    ELSE false
END), true)
FROM unnest($1::text[], $2::text[]) AS n(s, o)
LEFT JOIN pg_namespace ns ON ns.nspname = n.s
LEFT JOIN pg_class c ON c.relnamespace = ns.oid AND c.relname = n.o";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_become_columns_of_the_batch() {
        let sql = batched_sql(
            "SELECT json_build_object('a', x) AS data FROM t WHERE id = $1::text::int AND n < $12::text::int",
            12,
        );
        assert!(sql.contains("id = s.p1::text::int AND n < s.p12::text::int"));
        assert!(sql.contains("unnest($1::int8[], $2::text[], $3::text[]"));
        assert!(sql.contains(", $14::text[])"));
        assert!(sql.contains("AS s(i, h, p1, p2"));
        assert!(sql.ends_with(", p12) CROSS JOIN LATERAL (SELECT q.data, encode(sha256(convert_to(q.data::text, 'UTF8')), 'hex') AS h FROM (SELECT json_build_object('a', x) AS data FROM t WHERE id = s.p1::text::int AND n < s.p12::text::int\n) AS q) AS r"));
    }

    #[test]
    fn sqlcommenter_tags_end_the_batch() {
        let sql = batched_sql(
            "SELECT 1 AS data /*application='graphpg',graphql_type='subscription'*/",
            0,
        );
        assert!(
            sql.ends_with(") AS q) AS r /*application='graphpg',graphql_type='subscription'*/"),
            "{sql}"
        );
        assert_eq!(
            crate::catalog::statements::operation_label(&sql).as_deref(),
            Some("subscription anonymous")
        );
    }

    #[test]
    fn no_parameters() {
        let sql = batched_sql("SELECT 1 AS data", 0);
        assert!(sql.contains("unnest($1::int8[], $2::text[]) AS s(i, h)"));
    }

    #[test]
    fn names_are_unquoted() {
        let names = qualified_names(
            r#"SELECT t0."name" FROM "app"."users" t0 JOIN "app"."we""ird" t1 ON "app"."f"(t0) WHERE x::"app"."mood" = 'a' AND "app"."users" IS NOT NULL"#,
        );
        assert_eq!(
            names,
            vec![
                ("app".to_string(), "f".to_string()),
                ("app".to_string(), "mood".to_string()),
                ("app".to_string(), "users".to_string()),
                ("app".to_string(), "we\"ird".to_string()),
            ]
        );
    }
}
