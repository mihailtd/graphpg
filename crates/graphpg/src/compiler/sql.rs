use crate::catalog::TreeLink;
use crate::catalog::{pg_array_literal, ExclusiveArcMetadata};
use crate::plan::{
    AggregatesSelections, ArgValue, BoundArg, BoundPart, ColumnValue, CompositeSource,
    ConnectionSelections, FilterExpr, FunctionResult, FunctionSource, GlobalSearchBranch, GroupKey,
    GroupedAggregates, HavingCondition, HighlightOptions, JoinCondition, MutationKind, NodeRef,
    OnConflict, OperationPlan, PayloadField, RangePart, RelatedRows, RowPolymorphism, RowTarget,
    SearchOperation, SearchSelections, SelectedField, SortCompare, SortExpr, SortKey, Step, StepId,
    TreeSource, TypedFieldSet, ValueOutput, VersionGuard, WhereCondition, WriteNode, WriteNodeKind,
};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Quotes an SQL identifier (`"name"`), doubling embedded double quotes.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quoted schema-qualified name: `"schema"."name"`.
pub fn quote_qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Column `name` of the row aliased `alias` (aliases are generated, so never need quoting).
fn col(alias: &str, name: &str) -> String {
    format!("{alias}.{}", quote_ident(name))
}

/// Escapes `%`, `_` and the escape character itself so `s` matches literally inside a `LIKE`
/// pattern (PostgreSQL's default escape character is `\`).
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Joins attached to one FROM scope (the row source of a SELECT). Every field list compiled
/// against that row — including all branches of a polymorphic row — shares the scope, so a
/// relation reached several times is joined once:
///
/// - to-one relations onto a primary key become plain `LEFT JOIN`s, keyed by table alias
///   (the planner gives equal joins equal aliases);
/// - other relations become `LEFT JOIN LATERAL` subqueries, keyed by plan step.
#[derive(Default)]
struct JoinScope {
    sql: String,
    joined: HashSet<String>,
    laterals: HashMap<StepId, String>,
}

impl JoinScope {
    /// Appends `LEFT JOIN <table> <alias> ON <on>` unless `alias` is already joined.
    fn join(&mut self, table: String, alias: &str, on: String) {
        if self.joined.insert(alias.to_string()) {
            self.sql
                .push_str(&format!(" LEFT JOIN {table} {alias} ON {on}"));
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CompiledQuery {
    pub sql: String,
    pub params: Vec<String>,
    /// Row count estimates (`totalCount(estimate: true)` of filtered connections): each
    /// statement's `EXPLAIN` row estimate is bound to its parameter before the query runs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub estimates: Vec<Estimate>,
}

impl CompiledQuery {
    pub fn new(sql: String, params: Vec<String>) -> Self {
        Self {
            sql,
            params,
            estimates: Vec::new(),
        }
    }
}

/// A row count the planner estimates: the rows `query` returns, per its `EXPLAIN`, bound to
/// parameter `param` (0-based) as text.
#[derive(Debug, Clone, Serialize)]
pub struct Estimate {
    pub param: usize,
    pub query: CompiledQuery,
}

thread_local! {
    /// Estimates requested while compiling a query (compilation runs on one thread).
    static ESTIMATES: std::cell::RefCell<Vec<Estimate>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Whether the statement being compiled can take estimates: a query's (a mutation's
    /// payloads count exactly).
    static ESTIMATING: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
}

/// One root mutation field, executed as statements of its own in operation order.
#[derive(Debug, Clone, Serialize)]
pub struct CompiledMutation {
    /// Response key of the root field.
    pub field_alias: String,
    /// Statements run first, in order: lookups of connected rows, inserts (batched per table
    /// and set of columns), updates. Empty when `payload` is itself the data-modifying
    /// statement.
    pub writes: Vec<CompiledWrite>,
    /// Statement returning the payload as its `data` column. Run after `writes`, it reads the
    /// affected rows back, so they (and their relations) reflect the writes.
    pub payload: CompiledStatement,
    /// Payload parameter (0-based) the executor fills with the number of top-level rows the
    /// writes affected.
    pub count_param: Option<usize>,
    /// Input path of each write node (`inputs[0].author.create`), for error messages.
    pub node_paths: Vec<String>,
}

/// A statement some of whose parameters take values that earlier statements of the same
/// mutation field returned (generated keys of rows created through nested relations).
#[derive(Debug, Clone, Serialize)]
pub struct CompiledStatement {
    pub query: CompiledQuery,
    /// Parameters (0-based) filled when it runs.
    pub deferred: Vec<(usize, ParamSource)>,
    /// A value of a row that was not written (e.g. skipped by a trigger) is an error rather
    /// than NULL.
    pub strict: bool,
    /// What the statement writes, when it modifies data: a constraint violation is traced
    /// back to the input values that caused it.
    pub context: Option<WriteContext>,
}

impl CompiledStatement {
    fn new(query: CompiledQuery) -> Self {
        Self {
            query,
            deferred: Vec::new(),
            strict: true,
            context: None,
        }
    }

    fn with_context(mut self, context: Option<WriteContext>) -> Self {
        self.context = context;
        self
    }
}

/// The rows a data-modifying statement writes, as given in the operation's input.
#[derive(Debug, Clone, Serialize)]
pub struct WriteContext {
    pub schema: String,
    pub table: String,
    pub verb: WriteVerb,
    /// The input rows written, in order (none for deletes).
    pub rows: Vec<InputRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum WriteVerb {
    Insert,
    Update,
    Delete,
}

/// One input row of a write: where it is in the arguments (`inputs[1]`,
/// `input.players.create[0]`), and the columns it sets with their values as bound (NULL,
/// or known only when the statement runs: `None`).
#[derive(Debug, Clone, Serialize)]
pub struct InputRow {
    pub path: String,
    pub values: Vec<(String, Option<String>)>,
}

impl InputRow {
    fn of_node(node: &WriteNode) -> Self {
        let values = node
            .values
            .iter()
            .map(|cv| {
                (
                    cv.column.clone(),
                    (!cv.value.is_null()).then(|| SqlCompiler::param_text(&cv.value)),
                )
            })
            .chain(node.refs.iter().map(|r| (r.column.clone(), None)))
            .collect();
        Self {
            path: node.path.clone(),
            values,
        }
    }
}

/// Value of a deferred parameter.
#[derive(Debug, Clone, Serialize)]
pub enum ParamSource {
    Value(ValueRef),
    /// A text array of the values.
    Array(Vec<ValueRef>),
    /// A text array of column `column` of the rows `collect` writes returned, in order.
    Returned {
        column: usize,
    },
}

#[derive(Debug, Clone, Serialize)]
pub enum ValueRef {
    Literal(Option<String>),
    /// Column `column` of the row written or read for write node `node`.
    Node {
        node: usize,
        column: String,
    },
}

/// A statement of a mutation field run before its payload.
#[derive(Debug, Clone, Serialize)]
pub struct CompiledWrite {
    pub statement: CompiledStatement,
    /// Columns it returns (as text) ...
    pub returning: Vec<String>,
    /// ... for these write nodes, one row each, in order.
    pub row_nodes: Vec<usize>,
    /// Its returned rows are kept for `ParamSource::Returned` (the keys of updated rows).
    pub collect: bool,
    /// Must affect or return one row per node (a connected row must exist); the error
    /// otherwise.
    pub expect_rows: Option<String>,
    /// Its rows count towards `affectedCount` (top-level rows).
    pub counts: bool,
    /// A version check (`expectedVersion`): it returns whether the row still has the
    /// expected version, and the write fails with `STALE_VERSION` if not.
    pub guard: bool,
}

/// Returned column holding the row as it was before the write (`RETURNING OLD`), as JSON.
const OLD_ROW: &str = "__old";

/// Where a payload reads the rows as they were before the write: a `FROM` list ending in a
/// row-returning item (aliased by the caller), whether a row existed, and its order.
struct PreviousRows {
    source: String,
    present: String,
    order: String,
}

/// How a search ranks the rows of a table (see `SqlCompiler::ranking`).
struct Ranking {
    cte: String,
    from: String,
    rank: String,
    direction: &'static str,
    score: String,
    extra: Vec<String>,
}

/// A compiled GraphQL operation.
#[derive(Debug, Clone)]
pub enum CompiledOperation {
    /// A single statement producing the whole `data` object.
    Query(CompiledQuery),
    /// Mutation fields, executed one after another in one transaction.
    Mutation(Vec<CompiledMutation>),
}

impl CompiledOperation {
    /// Tags every statement with an [sqlcommenter](https://google.github.io/sqlcommenter/)
    /// comment (`/*graphql_operation='GetUsers'*/`), so `pg_stat_activity`, logs and
    /// `auto_explain` attribute statements to the GraphQL operation that ran them.
    pub fn tag(&mut self, tags: &[(&str, &str)]) {
        let mut tags: Vec<&(&str, &str)> = tags.iter().collect();
        tags.sort();
        let encoded: Vec<String> = tags
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}='{}'",
                    percent_encode(k),
                    percent_encode(v).replace('\'', "\\'")
                )
            })
            .collect();
        let comment = format!(" /*{}*/", encoded.join(","));
        let append = |q: &mut CompiledQuery| {
            q.sql = format!("{}{comment}", q.sql.trim_end().trim_end_matches(';'))
        };
        match self {
            CompiledOperation::Query(q) => append(q),
            CompiledOperation::Mutation(mutations) => {
                for m in mutations {
                    for w in &mut m.writes {
                        append(&mut w.statement.query);
                    }
                    append(&mut m.payload.query);
                }
            }
        }
    }
}

/// URL-encodes everything but unreserved characters (RFC 3986), as sqlcommenter requires:
/// a value can then never end the comment.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub struct SqlCompiler;

impl SqlCompiler {
    /// Compiles an operation plan: queries into one statement, mutations into statements per
    /// root field (see [`CompiledMutation`]).
    pub fn compile_operation(plan: &OperationPlan) -> CompiledOperation {
        let is_mutation = plan.root_fields().iter().any(|(_, id)| {
            matches!(
                plan.get_step(*id),
                Some(Step::PgMutation { .. } | Step::PgFunction { mutation: true, .. })
            )
        });
        if !is_mutation {
            return CompiledOperation::Query(Self::compile(plan));
        }
        let mutations = plan
            .root_fields()
            .iter()
            .filter_map(|(alias, id)| match plan.get_step(*id) {
                Some(step @ Step::PgMutation { .. }) => {
                    Some(Self::compile_mutation(alias, step, plan))
                }
                Some(step @ Step::PgFunction { mutation: true, .. }) => {
                    Some(Self::compile_function_mutation(alias, step, plan))
                }
                _ => None,
            })
            .collect();
        CompiledOperation::Mutation(mutations)
    }

    /// The rows of a plan's single root connection as a statement returning one `row` (the
    /// node's JSON) per matching row, in the connection's order: exports stream them
    /// instead of building one response. `first`, `after` and `before` apply; without
    /// `first` every row is returned. `None` when the plan is not one connection.
    pub fn compile_rows(plan: &OperationPlan) -> Option<CompiledQuery> {
        let [(_, step_id)] = plan.root_fields() else {
            return None;
        };
        let Step::PgConnection {
            schema,
            table,
            table_alias,
            selections,
            where_conditions,
            filter_expr,
            first,
            after,
            before,
            sort,
            source,
            id_cast,
            cursor_columns,
            ..
        } = plan.get_step(*step_id)?
        else {
            return None;
        };
        let mut params = Vec::new();
        let mut where_clauses = Self::connection_where(
            table_alias,
            where_conditions,
            filter_expr.as_ref(),
            &None,
            &mut params,
        );
        let legacy_key = [("id".to_string(), id_cast.clone())];
        let key = if cursor_columns.is_empty() {
            &legacy_key[..]
        } else {
            &cursor_columns[..]
        };
        let key_sort: Vec<SortKey> = key
            .iter()
            .map(|(column, cast)| SortKey {
                column: column.clone(),
                descending: false,
                nullable: false,
                compare: cast.clone().map_or(SortCompare::Text, SortCompare::Cast),
                expr: None,
            })
            .collect();
        let keyset: &[SortKey] = if sort.is_empty() { &key_sort } else { sort };
        let key_exprs: Vec<String> = keyset
            .iter()
            .map(|k| match k.compare {
                SortCompare::Text => format!("({})::text", Self::sort_expression(table_alias, k)),
                _ => Self::sort_expression(table_alias, k),
            })
            .collect();
        for (cursor, after) in [(after, true), (before, false)] {
            let Some(cursor) = cursor else { continue };
            let values = match keyset {
                [_] => vec![serde_json::Value::String(Self::decode_cursor(cursor))],
                _ => Self::decode_composite_cursor(cursor),
            };
            where_clauses.push(Self::keyset_condition(
                &key_exprs,
                keyset,
                &values,
                after,
                &mut params,
            ));
        }
        let order: Vec<String> = keyset
            .iter()
            .zip(&key_exprs)
            .map(|(k, e)| format!("{e} {}", if k.descending { "DESC" } else { "ASC" }))
            .collect();
        let from = match source {
            Some(source) => Self::function_from(source, table_alias, &mut params),
            None => Self::from_item(schema, table, table_alias, None),
        };
        let limit = first
            .map(|n| format!(" LIMIT {}", Self::bind_count(&mut params, n)))
            .unwrap_or_default();
        let mut scope = JoinScope::default();
        let node = Self::row_object(
            table_alias,
            &selections.node_fields,
            selections.node_polymorphism.as_ref(),
            "Node",
            &mut scope,
            plan,
            &mut params,
        );
        let order = order.join(", ");
        let sql = format!(
            "SELECT {node} AS row FROM (SELECT {table_alias}.* FROM {from}{} ORDER BY {order}{limit}) {table_alias}{} ORDER BY {order}",
            Self::where_sql(&where_clauses),
            scope.sql
        );
        Some(CompiledQuery::new(sql, params))
    }

    /// `INSERT INTO <table> (<columns>) SELECT ... FROM <staging>`: rows staged as text
    /// (columns `c0`, `c1`, ... of `staging`, in `columns` order), converted to the columns'
    /// types as writes are.
    pub fn import_statement(
        schema: &str,
        table: &str,
        columns: &[ColumnValue],
        staging: &str,
    ) -> String {
        let names: Vec<String> = columns.iter().map(|c| quote_ident(&c.column)).collect();
        let values: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(j, c)| Self::assign_expr(&format!("_s.c{j}"), c, true))
            .collect();
        format!(
            "INSERT INTO {} ({}) SELECT {} FROM {staging} _s",
            quote_qualified(schema, table),
            names.join(", "),
            values.join(", ")
        )
    }

    /// Text a value is bound (or staged) as: strings as they are, other JSON as its text.
    pub fn value_text(value: &serde_json::Value) -> Option<String> {
        (!value.is_null()).then(|| Self::param_text(value))
    }

    /// Compiles a query plan into a single unified SQL query that outputs the complete
    /// GraphQL `data` object natively from PostgreSQL.
    pub fn compile(plan: &OperationPlan) -> CompiledQuery {
        ESTIMATES.with(|e| e.borrow_mut().clear());
        ESTIMATING.with(|e| *e.borrow_mut() = true);
        let mut params = Vec::new();
        let mut root_field_pairs = Vec::new();

        for (root_field_alias, step_id) in plan.root_fields() {
            if let Some(step) = plan.get_step(*step_id) {
                let subquery = Self::compile_step(step, plan, &mut params);
                root_field_pairs.push(Self::json_pair(root_field_alias, &subquery));
            }
        }
        ESTIMATING.with(|e| *e.borrow_mut() = false);

        let root_obj = Self::json_object(&root_field_pairs);
        let mut query = CompiledQuery::new(format!("SELECT {} AS data;", root_obj), params);
        query.estimates = ESTIMATES.with(|e| std::mem::take(&mut *e.borrow_mut()));
        query
    }

    /// Recursively compiles a Step into a PostgreSQL scalar subquery.
    fn compile_step(step: &Step, plan: &OperationPlan, params: &mut Vec<String>) -> String {
        match step {
            Step::PgSelect { .. } => format!("({})", Self::compile_select_body(step, plan, params)),
            Step::PgConnection {
                schema,
                table,
                table_alias,
                selections,
                where_conditions,
                filter_expr,
                join_condition,
                first,
                last,
                offset,
                after,
                before,
                sort,
                source,
                id_cast,
                cursor_columns,
            } => Self::compile_connection(
                schema,
                table,
                table_alias,
                selections,
                where_conditions,
                filter_expr.as_ref(),
                join_condition,
                *first,
                *last,
                *offset,
                after,
                before,
                sort,
                source.as_ref(),
                id_cast.as_deref(),
                cursor_columns,
                plan,
                params,
            ),
            Step::PgSearch {
                schema,
                table,
                table_alias,
                search,
                where_conditions,
                filter_expr,
                limit,
                selections,
            } => Self::compile_search(
                schema,
                table,
                table_alias,
                search,
                where_conditions,
                filter_expr.as_ref(),
                *limit,
                selections,
                plan,
                params,
            ),
            Step::PgGlobalSearch {
                branches,
                limit,
                include_hits,
                include_score,
                include_has_more,
            } => Self::compile_global_search(
                branches,
                *limit,
                *include_hits,
                *include_score,
                *include_has_more,
                plan,
                params,
            ),
            // Mutations run as statements of their own (`compile_operation`)
            Step::PgMutation { .. } => "NULL".to_string(),
            Step::PgFunction {
                schema,
                func_name,
                arguments,
                returns_set,
                unnest,
                result,
                ..
            } => {
                let call = Self::unnested(
                    Self::function_call_sql(schema, func_name, None, arguments, params),
                    *unnest,
                );
                Self::compile_function_result(&call, *returns_set, result, plan, params)
            }
            Step::Constant { value } => match value {
                serde_json::Value::Null => "NULL".to_string(),
                other => format!("{}::json", Self::sql_literal(&other.to_string())),
            },
            Step::FieldAccess { field_name, .. } => Self::sql_literal(field_name),
            Step::Entities {
                count,
                groups,
                singles,
            } => {
                // Each group joins its single-row lookup to its keys, bound as one JSON array
                // of `{ord, k0, k1, ...}` records; the positions order the result and leave
                // nulls for representations nothing resolved
                let mut parts: Vec<String> = Vec::new();
                for group in groups {
                    let records: Vec<serde_json::Value> = group
                        .rows
                        .iter()
                        .map(|(ord, values)| {
                            let mut record = serde_json::Map::new();
                            record.insert("ord".to_string(), serde_json::json!(ord));
                            for (i, v) in values.iter().enumerate() {
                                record.insert(format!("k{i}"), v.clone());
                            }
                            serde_json::Value::Object(record)
                        })
                        .collect();
                    let keys =
                        Self::bind_str(params, serde_json::Value::Array(records).to_string());
                    let columns: Vec<String> = std::iter::once("\"ord\" int4".to_string())
                        .chain(
                            group
                                .key_casts
                                .iter()
                                .enumerate()
                                .map(|(i, cast)| format!("\"k{i}\" {cast}")),
                        )
                        .collect();
                    let step = plan
                        .get_step(group.step)
                        .expect("valid step for entity group");
                    let lookup = Self::compile_step(step, plan, params);
                    let alias = &group.keys_alias;
                    parts.push(format!(
                        "SELECT {alias}.\"ord\", ({lookup})::json FROM jsonb_to_recordset(${keys}::text::jsonb) AS {alias}({})",
                        columns.join(", ")
                    ));
                }
                for (ord, step_id) in singles {
                    let step = plan.get_step(*step_id).expect("valid step for entity");
                    parts.push(format!(
                        "SELECT {ord}, ({})::json",
                        Self::compile_step(step, plan, params)
                    ));
                }
                if parts.is_empty() {
                    parts.push("SELECT NULL::int4, NULL::json WHERE false".to_string());
                }
                let count = Self::bind_str(params, count.to_string());
                format!(
                    "(SELECT coalesce(json_agg(e.data ORDER BY s.ord), '[]'::json) \
                     FROM generate_series(0, ${count}::text::int4 - 1) AS s(ord) \
                     LEFT JOIN ({}) AS e(ord, data) ON e.ord = s.ord)",
                    parts.join(" UNION ALL ")
                )
            }
        }
    }

    /// Compiles a child step as the body of a `LEFT JOIN LATERAL (...)` subquery,
    /// producing a single `data` column.
    fn compile_lateral_body(step: &Step, plan: &OperationPlan, params: &mut Vec<String>) -> String {
        match step {
            Step::PgSelect { .. } => Self::compile_select_body(step, plan, params),
            // Connections (and anything else) already compile to a scalar subquery
            other => format!("SELECT {} AS data", Self::compile_step(other, plan, params)),
        }
    }

    /// `FROM` item of a relation step: the target table, joined to its junction table for
    /// many-to-many relations.
    fn from_item(
        schema: &str,
        table: &str,
        table_alias: &str,
        join: Option<&JoinCondition>,
    ) -> String {
        let mut from = format!("{} {}", quote_qualified(schema, table), table_alias);
        if let Some(junc) = join.and_then(|j| j.junction.as_ref()) {
            let on = junc
                .target
                .iter()
                .map(|(jc, tc)| format!("{} = {}", col("_junc", jc), col(table_alias, tc)))
                .collect::<Vec<_>>()
                .join(" AND ");
            from.push_str(&format!(
                " JOIN {} _junc ON {on}",
                quote_qualified(&junc.junction_schema, &junc.junction_table)
            ));
        }
        from
    }

    /// Compiles a PgSelect step to `SELECT <json> AS data FROM ...`.
    ///
    /// Used both as a root-level scalar subquery (wrapped in parens) and as the body of a
    /// `LEFT JOIN LATERAL`. Nested relations become `LEFT JOIN LATERAL (...) _latN ON true`
    /// and are referenced as `_latN.data`, which gives PostgreSQL:
    /// - Index-driven loops per parent row (rather than re-evaluating subqueries)
    /// - Zero in-memory stitching (JSON bytes arrive pre-aggregated)
    /// - No row duplication (unlike flat LEFT JOINs)
    ///
    /// Single rows (`is_single`) select directly with `LIMIT`. Lists apply
    /// `ORDER BY`/`LIMIT`/`OFFSET` to the child rows in an inner subquery, then aggregate
    /// on the outer level where the lateral joins are attached — so laterals only run for
    /// rows that survive the limit, and the limit never applies to the aggregate row.
    fn compile_select_body(step: &Step, plan: &OperationPlan, params: &mut Vec<String>) -> String {
        let Step::PgSelect {
            schema,
            table,
            table_alias,
            is_single,
            fields,
            where_conditions,
            filter_expr,
            join_condition,
            limit,
            offset,
            order_by,
            polymorphism,
            tree,
        } = step
        else {
            unreachable!("compile_select_body called on non-PgSelect step");
        };

        let mut where_clauses = Vec::new();
        if let Some(join) = join_condition {
            where_clauses.push(Self::join_clause(table_alias, join));
        }
        for cond in where_conditions {
            where_clauses.push(Self::where_clause(table_alias, cond, params));
        }
        if let Some(f) = filter_expr {
            let f_sql = Self::compile_filter_expr(f, params);
            if !f_sql.is_empty() {
                where_clauses.push(f_sql);
            }
        }

        let mut scope = JoinScope::default();
        let obj = Self::row_object(
            table_alias,
            fields,
            polymorphism.as_ref(),
            "Empty",
            &mut scope,
            plan,
            params,
        );
        let laterals_sql = scope.sql;

        let where_sql = Self::where_sql(&where_clauses);
        let order_sql = order_by
            .as_ref()
            .map(|o| format!(" ORDER BY {}", o))
            .unwrap_or_default();
        let offset_sql = offset
            .map(|o| format!(" OFFSET {}", Self::bind_count(params, o)))
            .unwrap_or_default();
        let from = match tree {
            Some(tree) => Self::tree_from(schema, table, table_alias, tree, params),
            None => Self::from_item(schema, table, table_alias, join_condition.as_ref()),
        };

        if *is_single {
            // A single row's limit is a constant (1), so it stays inline
            let limit_sql = format!(" LIMIT {}", limit.unwrap_or(1));
            format!(
                "SELECT {obj} AS data FROM {from}{laterals_sql}{where_sql}{order_sql}{limit_sql}{offset_sql}"
            )
        } else {
            let limit_sql = limit
                .map(|l| format!(" LIMIT {}", Self::bind_count(params, l)))
                .unwrap_or_default();
            // Aggregation order is only guaranteed with an explicit ORDER BY inside the
            // aggregate, so carry the row position out of the inner query.
            let (rn_col, agg_order) = match order_by {
                Some(o) => (
                    format!(", row_number() OVER (ORDER BY {}) AS \"__rn\"", o),
                    format!(" ORDER BY {}.\"__rn\"", table_alias),
                ),
                None => (String::new(), String::new()),
            };
            let inner = format!(
                "SELECT {table_alias}.*{rn_col} FROM {from}{where_sql}{order_sql}{limit_sql}{offset_sql}"
            );
            format!(
                "SELECT {} AS data FROM ({inner}) {table_alias}{laterals_sql}",
                Self::json_array_agg(&obj, &agg_order, "")
            )
        }
    }

    /// The rows above or below the row read from `tree.parent_alias`, aliased `table_alias`,
    /// each with its distance from it (`__depth`). A foreign key referencing the table is
    /// followed by a recursive query that stops at rows met again (`CYCLE`); an `ltree`
    /// path compares paths.
    fn tree_from(
        schema: &str,
        table: &str,
        table_alias: &str,
        tree: &TreeSource,
        params: &mut Vec<String>,
    ) -> String {
        let target = quote_qualified(schema, table);
        let parent = &tree.parent_alias;
        let max = tree.max_depth.map(|d| Self::bind_count(params, d));
        match &tree.link {
            TreeLink::ForeignKey(pairs) => {
                // Descendants: rows whose foreign key holds the parent's referenced columns;
                // ancestors: rows whose referenced columns the parent's foreign key holds
                let step = |to: &str| -> String {
                    pairs
                        .iter()
                        .map(|(fk, referenced)| {
                            if tree.ancestors {
                                format!("{} = {}", col("_c", referenced), col(to, fk))
                            } else {
                                format!("{} = {}", col("_c", fk), col(to, referenced))
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ")
                };
                let (anchor, recurse) = (step(parent), step("_tree"));
                let limit = max
                    .map(|m| format!(" WHERE _tree.\"__depth\" < {m}"))
                    .unwrap_or_default();
                let key = tree
                    .key
                    .iter()
                    .map(|k| quote_ident(k))
                    .collect::<Vec<_>>()
                    .join(", ");
                // A cycle leads back to the row itself, which is neither above nor below it
                let origin: Vec<String> = tree
                    .key
                    .iter()
                    .map(|k| format!("{} IS DISTINCT FROM {}", col("_tree", k), col(parent, k)))
                    .collect();
                let not_origin = if origin.is_empty() {
                    String::new()
                } else {
                    format!(" AND ({})", origin.join(" OR "))
                };
                format!(
                    "(WITH RECURSIVE _tree AS (\
                        SELECT _c.*, 1 AS \"__depth\" FROM {target} _c WHERE {anchor} \
                        UNION ALL \
                        SELECT _c.*, _tree.\"__depth\" + 1 FROM {target} _c JOIN _tree ON {recurse}{limit}\
                    ) CYCLE {key} SET \"__cycle\" USING \"__path\" \
                    SELECT _tree.* FROM _tree WHERE NOT _tree.\"__cycle\"{not_origin}) {table_alias}"
                )
            }
            TreeLink::Path(column) => {
                let (c, p) = (col("_c", column), col(parent, column));
                let (relation, depth) = if tree.ancestors {
                    (format!("{c} @> {p}"), format!("nlevel({p}) - nlevel({c})"))
                } else {
                    (format!("{c} <@ {p}"), format!("nlevel({c}) - nlevel({p})"))
                };
                let limit = max
                    .map(|m| format!(" AND {depth} <= {m}"))
                    .unwrap_or_default();
                format!("(SELECT _c.*, {depth} AS \"__depth\" FROM {target} _c WHERE {relation} AND {c} <> {p}{limit}) {table_alias}")
            }
        }
    }

    /// JSON of a row read from `table_alias`: its planned fields, or its concrete type when
    /// the row is polymorphic. Joins it needs are added to `scope`.
    fn row_object(
        table_alias: &str,
        fields: &[SelectedField],
        polymorphism: Option<&RowPolymorphism>,
        empty_typename: &str,
        scope: &mut JoinScope,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        match polymorphism {
            Some(poly) => Self::polymorphic_object(table_alias, poly, scope, plan, params),
            None => Self::json_object(&Self::object_fields(
                table_alias,
                fields,
                empty_typename,
                scope,
                plan,
                params,
            )),
        }
    }

    /// Response value of the stored value `expr`. `depth` keeps the aliases of nested array
    /// subqueries apart.
    ///
    /// - enums: `CASE expr::text WHEN 'in-progress' THEN 'IN_PROGRESS' ... END`
    /// - composites: `JSON_OBJECT` of the selected attributes `(expr)."attr"`, NULL for a NULL
    ///   value (tested through its text form: a composite whose attributes are all NULL
    ///   `IS NULL` too, but is still a value)
    /// - arrays of either: the element conversion aggregated in element order
    fn value_output(expr: &str, output: &ValueOutput, depth: usize) -> String {
        let list = |element: &dyn Fn(&str) -> String| {
            let (e, i) = (format!("_e{depth}"), format!("_i{depth}"));
            format!(
                "CASE WHEN {expr} IS NULL THEN NULL ELSE (SELECT coalesce(JSON_ARRAYAGG({} ORDER BY {i} NULL ON NULL RETURNING json), '[]'::json) FROM unnest({expr}) WITH ORDINALITY AS _u{depth}({e}, {i})) END",
                element(&e)
            )
        };
        match output {
            ValueOutput::Plain => expr.to_string(),
            ValueOutput::Cast(cast) => format!("{expr}::{cast}"),
            ValueOutput::Enum {
                values,
                list: false,
            } => {
                let whens: String = values
                    .iter()
                    .map(|(label, value)| {
                        format!(
                            " WHEN {} THEN {}",
                            Self::sql_literal(label),
                            Self::sql_literal(value)
                        )
                    })
                    .collect();
                format!("CASE {expr}::text{whens} END")
            }
            ValueOutput::Enum { values, list: true } => {
                let element = ValueOutput::Enum {
                    values: values.clone(),
                    list: false,
                };
                list(&|e| Self::value_output(e, &element, depth + 1))
            }
            ValueOutput::Composite {
                fields,
                list: false,
            } => {
                let pairs: Vec<String> = fields
                    .iter()
                    .map(|f| match &f.source {
                        CompositeSource::Attribute { name, output } => Self::json_pair(
                            &f.field_alias,
                            &Self::value_output(
                                &format!("({expr}).{}", quote_ident(name)),
                                output,
                                depth + 1,
                            ),
                        ),
                        CompositeSource::Typename(type_name) => {
                            Self::json_pair(&f.field_alias, &Self::sql_literal(type_name))
                        }
                    })
                    .collect();
                format!(
                    "CASE WHEN ({expr})::text IS NULL THEN NULL ELSE {} END",
                    Self::json_object(&pairs)
                )
            }
            ValueOutput::Template { sql, list: false } => sql.replace("{v}", &format!("({expr})")),
            ValueOutput::Template { sql, list: true } => {
                let element = ValueOutput::Template {
                    sql: sql.clone(),
                    list: false,
                };
                list(&|e| Self::value_output(e, &element, depth + 1))
            }
            ValueOutput::Range {
                fields,
                bound,
                list: false,
            } => {
                let r = format!("({expr})");
                let pairs: Vec<String> = fields
                    .iter()
                    .map(|f| match &f.part {
                        RangePart::Typename(name) => Self::json_pair(&f.field_alias, &Self::sql_literal(name)),
                        RangePart::IsEmpty => Self::json_pair(&f.field_alias, &format!("isempty({r})")),
                        RangePart::Bound { upper, fields } => {
                            let (value, inclusive, infinite) =
                                if *upper { ("upper", "upper_inc", "upper_inf") } else { ("lower", "lower_inc", "lower_inf") };
                            let parts: Vec<String> = fields
                                .iter()
                                .map(|(alias, part)| match part {
                                    BoundPart::Value => Self::json_pair(alias, &Self::value_output(&format!("{value}({r})"), bound, depth + 1)),
                                    BoundPart::Inclusive => Self::json_pair(alias, &format!("{inclusive}({r})")),
                                    BoundPart::Typename(name) => Self::json_pair(alias, &Self::sql_literal(name)),
                                })
                                .collect();
                            let obj = if parts.is_empty() { "'{}'::json".to_string() } else { Self::json_object(&parts) };
                            Self::json_pair(&f.field_alias, &format!("CASE WHEN isempty({r}) OR {infinite}({r}) THEN NULL ELSE {obj} END"))
                        }
                    })
                    .collect();
                let obj = if pairs.is_empty() {
                    "'{}'::json".to_string()
                } else {
                    Self::json_object(&pairs)
                };
                format!("CASE WHEN {r} IS NULL THEN NULL ELSE {obj} END")
            }
            ValueOutput::Range {
                fields,
                bound,
                list: true,
            } => {
                let element = ValueOutput::Range {
                    fields: fields.clone(),
                    bound: bound.clone(),
                    list: false,
                };
                list(&|e| Self::value_output(e, &element, depth + 1))
            }
            ValueOutput::Composite { fields, list: true } => {
                // Elements are read by subscript: `unnest` would expand a composite into columns
                let element = ValueOutput::Composite {
                    fields: fields.clone(),
                    list: false,
                };
                let i = format!("_i{depth}");
                format!(
                    "CASE WHEN {expr} IS NULL THEN NULL ELSE (SELECT coalesce(JSON_ARRAYAGG({} ORDER BY {i} NULL ON NULL RETURNING json), '[]'::json) FROM generate_subscripts({expr}, 1) AS {i}) END",
                    Self::value_output(&format!("({expr})[{i}]"), &element, depth + 1)
                )
            }
        }
    }

    /// Builds the key/value pairs of a row's JSON object. Scalar columns reference
    /// `table_alias`; relations add joins to `scope`.
    fn object_fields(
        table_alias: &str,
        fields: &[SelectedField],
        empty_typename: &str,
        scope: &mut JoinScope,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> Vec<String> {
        let mut pairs = Vec::new();

        for field in fields {
            match field {
                SelectedField::Column {
                    field_alias,
                    column_name,
                    output,
                } => {
                    pairs.push(Self::json_pair(
                        field_alias,
                        &Self::value_output(&col(table_alias, column_name), output, 0),
                    ));
                }
                SelectedField::GlobalId {
                    field_alias,
                    type_name,
                    pk_column,
                } => {
                    // Relay Global ID: base64("<TypeName>:<raw_pk>")
                    // Strip newlines inserted by PostgreSQL encode(..., 'base64') for >76 char strings
                    let expr = format!(
                        "replace(encode(concat({}, {}::text)::bytea, 'base64'), E'\\n', '')",
                        Self::sql_literal(&format!("{type_name}:")),
                        col(table_alias, pk_column)
                    );
                    pairs.push(Self::json_pair(field_alias, &expr));
                }
                SelectedField::Typename {
                    field_alias,
                    type_name,
                } => {
                    pairs.push(Self::json_pair(field_alias, &Self::sql_literal(type_name)));
                }
                SelectedField::Relation {
                    field_alias,
                    step_id,
                } => {
                    if let Some(child_step) = plan.get_step(*step_id) {
                        let expr = Self::relation_expr(*step_id, child_step, scope, plan, params);
                        pairs.push(Self::json_pair(field_alias, &expr));
                    }
                }
                SelectedField::ExclusiveArcTarget {
                    field_alias,
                    arc,
                    type_fields,
                } => {
                    let expr =
                        Self::exclusive_arc_expr(table_alias, arc, type_fields, plan, params);
                    pairs.push(Self::json_pair(field_alias, &expr));
                }
                SelectedField::OnAlias {
                    table_alias: other_alias,
                    field,
                } => {
                    let other_pairs = Self::object_fields(
                        other_alias,
                        std::slice::from_ref(field.as_ref()),
                        "",
                        scope,
                        plan,
                        params,
                    );
                    pairs.extend(
                        other_pairs
                            .into_iter()
                            .filter(|p| !p.starts_with("'__typename': ''")),
                    );
                }
                SelectedField::JoinRow {
                    schema,
                    table,
                    table_alias: joined,
                    join_columns,
                } => {
                    let on = join_columns
                        .iter()
                        .map(|(j, f)| format!("{} = {}", col(joined, j), col(table_alias, f)))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    scope.join(quote_qualified(schema, table), joined, on);
                }
                SelectedField::FunctionCall {
                    field_alias,
                    func,
                    arguments,
                    fields: sel_fields,
                } => {
                    let expr = Self::compile_function_call(
                        &func.schema,
                        &func.name,
                        Some(table_alias),
                        arguments,
                        sel_fields,
                        params,
                    );
                    pairs.push(Self::json_pair(field_alias, &expr));
                }
                SelectedField::RowVersion {
                    field_alias,
                    schema,
                    table,
                    key_columns,
                } => {
                    let on: Vec<String> = key_columns
                        .iter()
                        .map(|c| format!("{} = {}", col("_rv", c), col(table_alias, c)))
                        .collect();
                    let expr = format!(
                        "(SELECT _rv.xmin::text FROM {} _rv WHERE {})",
                        quote_qualified(schema, table),
                        on.join(" AND ")
                    );
                    pairs.push(Self::json_pair(field_alias, &expr));
                }
            }
        }

        if pairs.is_empty() {
            pairs.push(Self::json_pair(
                "__typename",
                &Self::sql_literal(empty_typename),
            ));
        }

        pairs
    }

    /// Value of a relation field. A to-one relation onto a primary key reads a plain
    /// `LEFT JOIN` of the target row (shared by every field following the same foreign key);
    /// anything else is a `LEFT JOIN LATERAL` subquery, emitted once per plan step.
    fn relation_expr(
        step_id: StepId,
        step: &Step,
        scope: &mut JoinScope,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        if let Step::PgSelect {
            schema,
            table,
            table_alias,
            is_single: true,
            fields,
            where_conditions,
            filter_expr: None,
            join_condition: Some(join),
            offset: None,
            order_by: None,
            polymorphism,
            ..
        } = step
        {
            if join.unique_target
                && join.junction.is_none()
                && where_conditions.is_empty()
                && !join.columns.is_empty()
            {
                scope.join(
                    quote_qualified(schema, table),
                    table_alias,
                    Self::join_clause(table_alias, join),
                );
                let obj = Self::row_object(
                    table_alias,
                    fields,
                    polymorphism.as_ref(),
                    "Empty",
                    scope,
                    plan,
                    params,
                );
                return format!(
                    "CASE WHEN {} IS NULL THEN NULL ELSE {obj} END",
                    col(table_alias, &join.columns[0].0)
                );
            }
        }
        let alias = match scope.laterals.get(&step_id) {
            Some(alias) => alias.clone(),
            None => {
                let alias = format!("_lat{}", scope.laterals.len());
                let body = Self::compile_lateral_body(step, plan, params);
                scope
                    .sql
                    .push_str(&format!(" LEFT JOIN LATERAL ({body}) {alias} ON true"));
                scope.laterals.insert(step_id, alias.clone());
                alias
            }
        };
        format!("{alias}.data")
    }

    /// Compiles a root mutation field.
    ///
    /// Creates run as writes: lookups of connected rows, inserts batched by table and set of
    /// columns (children after the rows whose keys they take), updates of re-parented rows.
    /// The payload then reads the top-level rows back by the keys the writes returned, in a
    /// statement of its own that sees the writes (unlike any query in a modifying
    /// statement); upserts read them back by their conflict key, so rows `DO NOTHING` left
    /// alone are included. Updates return the keys they touched the same way. Deletes and
    /// keyless tables build the payload from the rows the statement itself returns.
    fn compile_mutation(root_alias: &str, step: &Step, plan: &OperationPlan) -> CompiledMutation {
        let Step::PgMutation {
            schema,
            table,
            table_alias,
            kind,
            many,
            payload,
            key_columns,
            version_guard,
        } = step
        else {
            unreachable!("compile_mutation called on non-mutation step");
        };
        let target = quote_qualified(schema, table);
        // `expectedVersion` locks the row and checks its version before anything is written
        let guard = version_guard
            .as_ref()
            .map(|g| Self::compile_version_guard(schema, table, g));
        let finish = |mut writes: Vec<CompiledWrite>, payload, count_param, node_paths| {
            if let Some(guard) = guard.clone() {
                writes.insert(0, guard);
            }
            CompiledMutation {
                field_alias: root_alias.to_string(),
                writes,
                payload,
                count_param,
                node_paths,
            }
        };
        // The payload's `previous` rows come from `RETURNING OLD`
        let previous = payload
            .iter()
            .any(|f| matches!(f, PayloadField::Previous { .. }));

        let MutationKind::Insert {
            nodes,
            roots,
            on_conflict,
            ..
        } = kind
        else {
            // Update or delete
            let context = Some(match kind {
                MutationKind::Update { column_values, .. } => WriteContext {
                    schema: schema.clone(),
                    table: table.clone(),
                    verb: WriteVerb::Update,
                    rows: vec![InputRow {
                        path: if *many { "set" } else { "input" }.to_string(),
                        values: column_values
                            .iter()
                            .map(|cv| {
                                (
                                    cv.column.clone(),
                                    (!cv.value.is_null()).then(|| Self::param_text(&cv.value)),
                                )
                            })
                            .collect(),
                    }],
                },
                _ => WriteContext {
                    schema: schema.clone(),
                    table: table.clone(),
                    verb: WriteVerb::Delete,
                    rows: Vec::new(),
                },
            });
            if key_columns.is_empty() || matches!(kind, MutationKind::Delete { .. }) {
                let mut params = Vec::new();
                let dml = Self::compile_dml(&target, table_alias, kind, key_columns, &mut params);
                let payload = Self::returned_payload(
                    &target,
                    &[dml],
                    payload,
                    *many,
                    table_alias,
                    previous,
                    plan,
                    params,
                );
                return finish(
                    Vec::new(),
                    CompiledStatement::new(payload).with_context(context),
                    None,
                    Vec::new(),
                );
            }
            let mut params = Vec::new();
            let dml = Self::compile_dml(&target, table_alias, kind, key_columns, &mut params);
            let mut returning: Vec<String> = key_columns.iter().map(|(c, _)| c.clone()).collect();
            if previous {
                returning.push(OLD_ROW.to_string());
            }
            let write = CompiledWrite {
                statement: CompiledStatement::new(CompiledQuery::new(
                    format!("{dml}{}", Self::returning_sql(&returning)),
                    params,
                ))
                .with_context(context),
                returning,
                row_nodes: Vec::new(),
                collect: true,
                expect_rows: None,
                counts: true,
                guard: false,
            };
            let keys = (0..key_columns.len())
                .map(|column| ParamSource::Returned { column })
                .collect();
            let old = previous.then_some(ParamSource::Returned {
                column: key_columns.len(),
            });
            let (payload, count_param) = Self::located_payload(
                &target,
                table_alias,
                key_columns,
                keys,
                Vec::new(),
                old,
                payload,
                *many,
                plan,
            );
            return finish(vec![write], payload, count_param, Vec::new());
        };

        let node_paths: Vec<String> = nodes.iter().map(|n| n.path.clone()).collect();
        // A keyless table's rows cannot be read back by key: its payload statement inserts them
        let inline_roots = on_conflict.is_none() && key_columns.is_empty();
        let payload_keys: &[(String, Option<String>)] = if on_conflict.is_none() {
            key_columns
        } else {
            &[]
        };
        let mut needed = Self::needed_columns(nodes, roots, payload_keys);
        if previous {
            for &root in roots {
                needed[root].insert(OLD_ROW.to_string());
            }
        }
        let mut writes = Vec::new();
        let mut inline = Vec::new();
        let mut inline_params = Vec::new();
        let mut inline_deferred = Vec::new();
        for batch in Self::write_batches(nodes, roots) {
            let is_root = roots.contains(&batch[0]);
            let conflict = on_conflict.as_ref().filter(|_| is_root);
            if inline_roots && is_root {
                inline.push(Self::compile_insert_batch(
                    nodes,
                    &batch,
                    None,
                    key_columns,
                    &mut inline_params,
                    &mut inline_deferred,
                ));
                continue;
            }
            writes.push(Self::compile_node_write(
                nodes,
                &batch,
                is_root,
                conflict,
                key_columns,
                &needed,
            ));
        }

        if inline_roots {
            let payload = Self::returned_payload(
                &target,
                &inline,
                payload,
                *many,
                table_alias,
                false,
                plan,
                inline_params,
            );
            let context = WriteContext {
                schema: schema.clone(),
                table: table.clone(),
                verb: WriteVerb::Insert,
                rows: roots
                    .iter()
                    .map(|&r| InputRow::of_node(&nodes[r]))
                    .collect(),
            };
            let statement = CompiledStatement {
                query: payload,
                deferred: inline_deferred,
                strict: true,
                context: Some(context),
            };
            return finish(writes, statement, None, node_paths);
        }

        // Rows are read back by the keys the writes return, or by the conflict key values of
        // the input rows
        let (locate, keys, literals) = match on_conflict {
            Some(oc) => {
                let arrays = oc
                    .columns
                    .iter()
                    .map(|(column, _)| {
                        let values = roots
                            .iter()
                            .map(|&r| {
                                match nodes[r].values.iter().find(|cv| &cv.column == column) {
                                    Some(cv) if !cv.value.is_null() => {
                                        serde_json::Value::String(Self::param_text(&cv.value))
                                    }
                                    _ => serde_json::Value::Null,
                                }
                            })
                            .collect();
                        Self::param_text(&pg_array_literal(&serde_json::Value::Array(values)))
                    })
                    .collect();
                (oc.columns.as_slice(), Vec::new(), arrays)
            }
            None => {
                let keys = key_columns
                    .iter()
                    .map(|(column, _)| {
                        ParamSource::Array(
                            roots
                                .iter()
                                .map(|&node| ValueRef::Node {
                                    node,
                                    column: column.clone(),
                                })
                                .collect(),
                        )
                    })
                    .collect();
                (key_columns.as_slice(), keys, Vec::new())
            }
        };
        let old = previous.then(|| {
            ParamSource::Array(
                roots
                    .iter()
                    .map(|&node| ValueRef::Node {
                        node,
                        column: OLD_ROW.to_string(),
                    })
                    .collect(),
            )
        });
        let (payload, count_param) = Self::located_payload(
            &target,
            table_alias,
            locate,
            keys,
            literals,
            old,
            payload,
            *many,
            plan,
        );
        finish(writes, payload, count_param, node_paths)
    }

    /// `SELECT (<version> = $v)::text FROM <table> WHERE <key> FOR UPDATE`: locks the row a
    /// guarded write is about to change, so its version cannot change before the write.
    fn compile_version_guard(schema: &str, table: &str, guard: &VersionGuard) -> CompiledWrite {
        let mut params = Vec::new();
        let expected = Self::bind_str(&mut params, guard.expected.clone());
        let where_sql = Self::row_target_sql(&RowTarget::Key(guard.key.clone()), &mut params);
        let sql = format!(
            "SELECT ({}::text = ${expected}::text)::text FROM {}{where_sql} FOR UPDATE",
            quote_ident(&guard.column),
            quote_qualified(schema, table)
        );
        let context = WriteContext {
            schema: schema.to_string(),
            table: table.to_string(),
            verb: WriteVerb::Update,
            rows: Vec::new(),
        };
        CompiledWrite {
            statement: CompiledStatement::new(CompiledQuery::new(sql, params))
                .with_context(Some(context)),
            returning: vec!["__current".to_string()],
            row_nodes: Vec::new(),
            collect: false,
            expect_rows: None,
            counts: false,
            guard: true,
        }
    }

    /// ` RETURNING "a"::text, ...`, or nothing without columns. [`OLD_ROW`] is the row as it
    /// was before the write, as JSON (`RETURNING OLD`, PostgreSQL 18).
    fn returning_sql(columns: &[String]) -> String {
        if columns.is_empty() {
            return String::new();
        }
        let cols: Vec<String> = columns
            .iter()
            .map(|c| {
                if c == OLD_ROW {
                    "to_json(old)::text".to_string()
                } else {
                    format!("{}::text", quote_ident(c))
                }
            })
            .collect();
        format!(" RETURNING {}", cols.join(", "))
    }

    /// Payload built from the rows the data-modifying statements `dmls` return, in the same
    /// statement.
    #[allow(clippy::too_many_arguments)]
    fn returned_payload(
        target: &str,
        dmls: &[String],
        payload: &[PayloadField],
        many: bool,
        table_alias: &str,
        previous: bool,
        plan: &OperationPlan,
        mut params: Vec<String>,
    ) -> CompiledQuery {
        let old = if previous {
            format!(", to_json(old) AS \"{OLD_ROW}\"")
        } else {
            String::new()
        };
        let mutated = match dmls {
            [] => format!("_mutated AS (SELECT * FROM {target} WHERE false)"),
            [dml] => format!("_mutated AS ({dml} RETURNING *{old})"),
            dmls => {
                let mut ctes: Vec<String> = dmls
                    .iter()
                    .enumerate()
                    .map(|(i, dml)| format!("_m{i} AS ({dml} RETURNING *)"))
                    .collect();
                let union = (0..dmls.len())
                    .map(|i| format!("SELECT * FROM _m{i}"))
                    .collect::<Vec<_>>()
                    .join(" UNION ALL ");
                ctes.push(format!("_mutated AS ({union})"));
                ctes.join(", ")
            }
        };
        let previous = previous.then(|| PreviousRows {
            source: format!("_mutated _pm CROSS JOIN LATERAL json_populate_record(NULL::{target}, _pm.\"{OLD_ROW}\")"),
            present: format!("_pm.\"{OLD_ROW}\" IS NOT NULL"),
            order: String::new(),
        });
        let data = Self::payload_object(
            payload,
            many,
            table_alias,
            &format!("_mutated {table_alias}"),
            "",
            "(SELECT count(*)::int FROM _mutated)",
            previous.as_ref(),
            plan,
            &mut params,
        );
        CompiledQuery::new(format!("WITH {mutated} SELECT {data} AS data"), params)
    }

    /// Payload reading the affected rows of `target` back by `locate` columns, whose values
    /// are text arrays: `keys` filled when the payload runs, or `literals`. Returns the
    /// statement and the parameter taking the affected row count.
    #[allow(clippy::too_many_arguments)]
    fn located_payload(
        target: &str,
        table_alias: &str,
        locate: &[(String, Option<String>)],
        keys: Vec<ParamSource>,
        literals: Vec<String>,
        old: Option<ParamSource>,
        payload: &[PayloadField],
        many: bool,
        plan: &OperationPlan,
    ) -> (CompiledStatement, Option<usize>) {
        let mut params: Vec<String> = Vec::new();
        let mut deferred = Vec::new();
        for key in keys {
            params.push(String::new());
            deferred.push((params.len() - 1, key));
        }
        params.extend(literals);
        // The rows before the write, as JSON texts in the order of the keys
        let previous = old.map(|old| {
            params.push(String::new());
            deferred.push((params.len() - 1, old));
            let i = params.len();
            PreviousRows {
                source: format!(
                    "unnest(${i}::text::text[]) WITH ORDINALITY AS _o(j, _ord) CROSS JOIN LATERAL json_populate_record(NULL::{target}, _o.j::json)"
                ),
                present: "_o.j IS NOT NULL".to_string(),
                order: " ORDER BY _o._ord".to_string(),
            }
        });
        let count_param = payload
            .iter()
            .any(|f| matches!(f, PayloadField::AffectedCount { .. }))
            .then(|| {
                params.push(String::new());
                params.len() - 1
            });
        let count_expr = count_param
            .map(|i| format!("${}::text::int", i + 1))
            .unwrap_or_else(|| "NULL".to_string());
        let source = Self::located_source(target, table_alias, locate);
        let data = Self::payload_object(
            payload,
            many,
            table_alias,
            &source,
            " ORDER BY _k._ord",
            &count_expr,
            previous.as_ref(),
            plan,
            &mut params,
        );
        // Keys of rows that were not written (e.g. skipped by a trigger) read as NULL: the
        // payload leaves them out
        let statement = CompiledStatement {
            query: CompiledQuery::new(format!("SELECT {data} AS data"), params),
            deferred,
            strict: false,
            context: None,
        };
        (statement, count_param)
    }

    /// Rows of `target` located by `locate` columns, whose values are the text arrays of the
    /// first parameters, numbered in array order (`_k._ord`).
    fn located_source(
        target: &str,
        table_alias: &str,
        locate: &[(String, Option<String>)],
    ) -> String {
        let arrays: Vec<String> = (1..=locate.len())
            .map(|i| format!("${i}::text::text[]"))
            .collect();
        let names: Vec<String> = (0..locate.len()).map(|i| format!("k{i}")).collect();
        let on: Vec<String> = locate
            .iter()
            .enumerate()
            .map(|(i, (c, cast))| match cast {
                Some(ty) => format!("{} = _k.k{i}::{ty}", col(table_alias, c)),
                None => format!("{}::text = _k.k{i}", col(table_alias, c)),
            })
            .collect();
        format!(
            "unnest({}) WITH ORDINALITY AS _k({}, _ord) JOIN {target} {table_alias} ON {}",
            arrays.join(", "),
            names.join(", "),
            on.join(" AND ")
        )
    }

    /// The payload object of a mutation, reading affected rows from `source` (a `FROM` list
    /// that binds `table_alias`), in `order_sql` order.
    #[allow(clippy::too_many_arguments)]
    fn payload_object(
        payload: &[PayloadField],
        many: bool,
        table_alias: &str,
        source: &str,
        order_sql: &str,
        count_expr: &str,
        previous: Option<&PreviousRows>,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        let pairs: Vec<String> = payload
            .iter()
            .map(|field| match field {
                PayloadField::Previous {
                    field_alias,
                    fields,
                    polymorphism,
                    table_alias: alias,
                } => {
                    let Some(prev) = previous else {
                        return Self::json_pair(field_alias, "NULL");
                    };
                    let mut scope = JoinScope::default();
                    let obj = Self::row_object(
                        alias,
                        fields,
                        polymorphism.as_ref(),
                        "Payload",
                        &mut scope,
                        plan,
                        params,
                    );
                    // A row an upsert inserted had no previous version
                    let obj = format!("CASE WHEN {} THEN {obj} END", prev.present);
                    let from = format!("{} AS {alias}{}", prev.source, scope.sql);
                    let expr = if many {
                        format!(
                            "(SELECT {} FROM {from})",
                            Self::json_array_agg(&obj, &prev.order, "")
                        )
                    } else {
                        format!("(SELECT {obj} FROM {from}{} LIMIT 1)", prev.order)
                    };
                    Self::json_pair(field_alias, &expr)
                }
                PayloadField::Typename {
                    field_alias,
                    type_name,
                } => Self::json_pair(field_alias, &Self::sql_literal(type_name)),
                PayloadField::AffectedCount { field_alias } => {
                    Self::json_pair(field_alias, count_expr)
                }
                PayloadField::Rows {
                    field_alias,
                    fields,
                    polymorphism,
                } => {
                    let mut scope = JoinScope::default();
                    let obj = Self::row_object(
                        table_alias,
                        fields,
                        polymorphism.as_ref(),
                        "Payload",
                        &mut scope,
                        plan,
                        params,
                    );
                    let from = format!("{source}{}", scope.sql);
                    let expr = if many {
                        format!(
                            "(SELECT {} FROM {from})",
                            Self::json_array_agg(&obj, order_sql, "")
                        )
                    } else {
                        format!("(SELECT {obj} FROM {from}{order_sql} LIMIT 1)")
                    };
                    Self::json_pair(field_alias, &expr)
                }
            })
            .collect();
        Self::json_object(&pairs)
    }

    /// Columns of each write node's row that other statements read: the columns nodes
    /// referencing it take, and the payload's key for the top-level rows.
    fn needed_columns(
        nodes: &[WriteNode],
        roots: &[usize],
        payload_keys: &[(String, Option<String>)],
    ) -> Vec<BTreeSet<String>> {
        let mut needed = vec![BTreeSet::new(); nodes.len()];
        for node in nodes {
            for r in &node.refs {
                needed[r.node].insert(r.source_column.clone());
            }
        }
        for &root in roots {
            needed[root].extend(payload_keys.iter().map(|(c, _)| c.clone()));
        }
        needed
    }

    /// Write nodes grouped into statements, in execution order. A node runs after the nodes
    /// it takes values from (a greater level); inserts of one level into the same table
    /// setting the same columns share a statement (top-level rows apart, since they carry
    /// the upsert clause and count as affected). Lookups and updates run one per node.
    fn write_batches(nodes: &[WriteNode], roots: &[usize]) -> Vec<Vec<usize>> {
        // A node's level is one past the levels of the nodes it depends on (the graph is
        // acyclic, so this settles within as many passes as there are nodes)
        let mut level = vec![0usize; nodes.len()];
        let mut changed = true;
        while changed {
            changed = false;
            for (i, node) in nodes.iter().enumerate() {
                let deps = node
                    .refs
                    .iter()
                    .map(|r| r.node)
                    .chain(node.after.iter().copied());
                let l = deps.map(|d| level[d] + 1).max().unwrap_or(0);
                if l != level[i] {
                    level[i] = l;
                    changed = true;
                }
            }
        }
        let columns = |node: &WriteNode| -> Vec<String> {
            let mut cols: Vec<String> = node
                .values
                .iter()
                .map(|v| v.column.clone())
                .chain(node.refs.iter().map(|r| r.column.clone()))
                .collect();
            cols.sort_unstable();
            cols
        };
        let mut batches: Vec<Vec<usize>> = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            let joins = |batch: &Vec<usize>| {
                let other = &nodes[batch[0]];
                matches!(node.kind, WriteNodeKind::Insert)
                    && matches!(other.kind, WriteNodeKind::Insert)
                    && level[batch[0]] == level[i]
                    && other.schema == node.schema
                    && other.table == node.table
                    && roots.contains(&batch[0]) == roots.contains(&i)
                    && columns(other) == columns(node)
            };
            match batches.iter_mut().find(|b| joins(b)) {
                Some(batch) => batch.push(i),
                None => batches.push(vec![i]),
            }
        }
        batches.sort_by_key(|b| level[b[0]]);
        batches
    }

    /// The statement of a batch of write nodes (see [`write_batches`](Self::write_batches)).
    fn compile_node_write(
        nodes: &[WriteNode],
        batch: &[usize],
        is_root: bool,
        on_conflict: Option<&OnConflict>,
        primary_key: &[(String, Option<String>)],
        needed: &[BTreeSet<String>],
    ) -> CompiledWrite {
        let node = &nodes[batch[0]];
        let target = quote_qualified(&node.schema, &node.table);
        let returning: Vec<String> = batch
            .iter()
            .flat_map(|&n| needed[n].iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut params = Vec::new();
        let mut deferred = Vec::new();
        let missing = if node.path.contains("connect") {
            format!("`{}` matches no row to connect", node.path)
        } else {
            format!("`{}` matches no `{}` row", node.path, node.table)
        };
        let (sql, expect_rows) = match &node.kind {
            WriteNodeKind::Insert => {
                let insert = Self::compile_insert_batch(
                    nodes,
                    batch,
                    on_conflict,
                    primary_key,
                    &mut params,
                    &mut deferred,
                );
                (format!("{insert}{}", Self::returning_sql(&returning)), None)
            }
            WriteNodeKind::Lookup { key } => {
                // A row only read is its own previous version
                let cols: Vec<String> = returning
                    .iter()
                    .map(|c| {
                        if c == OLD_ROW {
                            "to_json(_l)::text".to_string()
                        } else {
                            format!("{}::text", quote_ident(c))
                        }
                    })
                    .collect();
                let where_sql = Self::row_target_sql(&RowTarget::Key(key.clone()), &mut params);
                (
                    format!("SELECT {} FROM {target} AS _l{where_sql}", cols.join(", ")),
                    Some(missing),
                )
            }
            WriteNodeKind::Update { key } => {
                let mut set: Vec<String> = node
                    .values
                    .iter()
                    .map(|cv| {
                        format!(
                            "{} = {}",
                            quote_ident(&cv.column),
                            Self::bind_value(&mut params, cv)
                        )
                    })
                    .collect();
                set.extend(node.refs.iter().map(|r| {
                    let spec = Self::ref_spec(r);
                    let expr = Self::deferred_value(
                        ValueRef::Node {
                            node: r.node,
                            column: r.source_column.clone(),
                        },
                        &spec,
                        &mut params,
                        &mut deferred,
                    );
                    format!("{} = {expr}", quote_ident(&r.column))
                }));
                let where_sql = Self::row_target_sql(&RowTarget::Key(key.clone()), &mut params);
                (
                    format!(
                        "UPDATE {target} SET {}{where_sql}{}",
                        set.join(", "),
                        Self::returning_sql(&returning)
                    ),
                    Some(missing),
                )
            }
            WriteNodeKind::Link => {
                let insert = Self::compile_insert_batch(
                    nodes,
                    batch,
                    None,
                    primary_key,
                    &mut params,
                    &mut deferred,
                );
                (format!("{insert} ON CONFLICT DO NOTHING"), None)
            }
            WriteNodeKind::Detach {
                key,
                except,
                through,
            } => {
                let mut conditions: Vec<String> = node
                    .refs
                    .iter()
                    .map(|r| {
                        let spec = Self::ref_spec(r);
                        let expr = Self::deferred_value(
                            ValueRef::Node {
                                node: r.node,
                                column: r.source_column.clone(),
                            },
                            &spec,
                            &mut params,
                            &mut deferred,
                        );
                        format!("{} = {expr}", col("_d", &r.column))
                    })
                    .collect();
                // The rows `keys` identify: children directly, junction rows by their target
                let matching = |keys: &[&Vec<WhereCondition>],
                                params: &mut Vec<String>|
                 -> String {
                    let alias = if through.is_some() { "_t" } else { "_d" };
                    let any: Vec<String> = keys
                        .iter()
                        .map(|key| {
                            let all: Vec<String> = key
                                .iter()
                                .map(|k| {
                                    Self::compare(
                                        &col(alias, &k.column),
                                        &k.operator,
                                        Self::bind(params, &k.value),
                                        k.pg_cast.as_deref(),
                                    )
                                })
                                .collect();
                            format!("({})", all.join(" AND "))
                        })
                        .collect();
                    let any = any.join(" OR ");
                    match through {
                        None => format!("({any})"),
                        Some(t) => {
                            let on: Vec<String> = t
                                .columns
                                .iter()
                                .map(|(jc, tc)| format!("{} = {}", col("_t", tc), col("_d", jc)))
                                .collect();
                            format!(
                                "EXISTS (SELECT 1 FROM {} _t WHERE {} AND ({any}))",
                                quote_qualified(&t.schema, &t.table),
                                on.join(" AND ")
                            )
                        }
                    }
                };
                match key {
                    Some(key) => conditions.push(matching(&[key], &mut params)),
                    None if !except.is_empty() => {
                        let keep: Vec<&Vec<WhereCondition>> = except.iter().collect();
                        conditions.push(format!("NOT {}", matching(&keep, &mut params)));
                    }
                    None => {}
                }
                let where_sql = conditions.join(" AND ");
                let sql = match through {
                    Some(_) => format!("DELETE FROM {target} AS _d WHERE {where_sql}"),
                    None => {
                        let set: Vec<String> = node
                            .refs
                            .iter()
                            .map(|r| format!("{} = NULL", quote_ident(&r.column)))
                            .collect();
                        format!(
                            "UPDATE {target} AS _d SET {} WHERE {where_sql}",
                            set.join(", ")
                        )
                    }
                };
                (sql, None)
            }
            WriteNodeKind::LookupOrInsert { key } => {
                // The row with the key, or else a new one: one statement, one row
                let where_sql = Self::key_conditions("_l", key, &mut params).join(" AND ");
                let (cols, exprs) = Self::insert_columns(node, &mut params, &mut deferred);
                let sql = format!(
                    "WITH _found AS (SELECT {} FROM {target} AS _l WHERE {where_sql} LIMIT 1), \
                     _new AS (INSERT INTO {target} AS _n{cols} SELECT {exprs} WHERE NOT EXISTS (SELECT 1 FROM _found) RETURNING {}) \
                     SELECT * FROM _found UNION ALL SELECT * FROM _new",
                    Self::returned_columns(&returning, "_l"),
                    Self::returned_columns(&returning, "_n")
                );
                (
                    sql,
                    Some(format!(
                        "`{}` neither found nor created a `{}` row",
                        node.path, node.table
                    )),
                )
            }
            WriteNodeKind::UpdateOrInsert { key } => {
                // A child with the key is moved to the parent, or else created with it
                let refs: Vec<(String, String)> = node
                    .refs
                    .iter()
                    .map(|r| {
                        let spec = Self::ref_spec(r);
                        let value = ValueRef::Node {
                            node: r.node,
                            column: r.source_column.clone(),
                        };
                        (
                            r.column.clone(),
                            Self::deferred_value(value, &spec, &mut params, &mut deferred),
                        )
                    })
                    .collect();
                let set: Vec<String> = refs
                    .iter()
                    .map(|(c, e)| format!("{} = {e}", quote_ident(c)))
                    .collect();
                let where_sql = Self::key_conditions("_u", key, &mut params).join(" AND ");
                let mut cols: Vec<String> = node
                    .values
                    .iter()
                    .map(|cv| quote_ident(&cv.column))
                    .collect();
                let mut exprs: Vec<String> = node
                    .values
                    .iter()
                    .map(|cv| Self::bind_value(&mut params, cv))
                    .collect();
                cols.extend(refs.iter().map(|(c, _)| quote_ident(c)));
                exprs.extend(refs.iter().map(|(_, e)| e.clone()));
                let sql = format!(
                    "WITH _upd AS (UPDATE {target} AS _u SET {} WHERE {where_sql} RETURNING {}), \
                     _new AS (INSERT INTO {target} AS _n ({}) SELECT {} WHERE NOT EXISTS (SELECT 1 FROM _upd) RETURNING {}) \
                     SELECT * FROM _upd UNION ALL SELECT * FROM _new",
                    set.join(", "),
                    Self::returned_columns(&returning, "_u"),
                    cols.join(", "),
                    exprs.join(", "),
                    Self::returned_columns(&returning, "_n")
                );
                (
                    sql,
                    Some(format!(
                        "`{}` neither found nor created a `{}` row",
                        node.path, node.table
                    )),
                )
            }
            WriteNodeKind::UpdateRelated { key } | WriteNodeKind::DeleteRelated { key } => {
                // Only a row related to the parent: the key, and the parent's key in its refs
                let mut conditions = Self::key_conditions("_u", key, &mut params);
                conditions.extend(node.refs.iter().map(|r| {
                    let spec = Self::ref_spec(r);
                    let value = ValueRef::Node {
                        node: r.node,
                        column: r.source_column.clone(),
                    };
                    format!(
                        "{} = {}",
                        col("_u", &r.column),
                        Self::deferred_value(value, &spec, &mut params, &mut deferred)
                    )
                }));
                let where_sql = conditions.join(" AND ");
                let sql = if matches!(node.kind, WriteNodeKind::DeleteRelated { .. }) {
                    format!("DELETE FROM {target} AS _u WHERE {where_sql}")
                } else {
                    let set = Self::set_list(node, key, &mut params);
                    format!(
                        "UPDATE {target} AS _u SET {set} WHERE {where_sql}{}",
                        Self::returning_sql(&returning)
                    )
                };
                (
                    sql,
                    Some(format!(
                        "`{}` matches no `{}` row related to it",
                        node.path, node.table
                    )),
                )
            }
            WriteNodeKind::UpdateReferenced {
                from_schema,
                from_table,
                from_key,
                columns,
            } => {
                let mut conditions = Self::key_conditions("_f", from_key, &mut params);
                conditions.extend(
                    columns
                        .iter()
                        .map(|(c, f)| format!("{} = {}", col("_u", c), col("_f", f))),
                );
                let set = Self::set_list(node, &[], &mut params);
                let sql = format!(
                    "UPDATE {target} AS _u SET {set} FROM {} AS _f WHERE {}{}",
                    quote_qualified(from_schema, from_table),
                    conditions.join(" AND "),
                    Self::returning_sql(&returning)
                );
                (
                    sql,
                    Some(format!(
                        "`{}`: the row refers to no `{}` row",
                        node.path, node.table
                    )),
                )
            }
            WriteNodeKind::Sync { rows, key } => (
                Self::compile_sync(node, rows, key, &target, &mut params, &mut deferred),
                None,
            ),
        };
        let verb = match &node.kind {
            WriteNodeKind::Insert
            | WriteNodeKind::Link
            | WriteNodeKind::LookupOrInsert { .. }
            | WriteNodeKind::UpdateOrInsert { .. } => Some(WriteVerb::Insert),
            WriteNodeKind::Update { .. }
            | WriteNodeKind::UpdateRelated { .. }
            | WriteNodeKind::UpdateReferenced { .. }
            | WriteNodeKind::Sync { .. } => Some(WriteVerb::Update),
            WriteNodeKind::Detach {
                through: Some(_), ..
            }
            | WriteNodeKind::DeleteRelated { .. } => Some(WriteVerb::Delete),
            WriteNodeKind::Detach { through: None, .. } => Some(WriteVerb::Update),
            WriteNodeKind::Lookup { .. } => None,
        };
        let rows = match &node.kind {
            WriteNodeKind::Sync { rows, .. } => rows
                .iter()
                .enumerate()
                .map(|(i, values)| InputRow {
                    path: format!("{}[{i}]", node.path),
                    values: values
                        .iter()
                        .map(|cv| {
                            (
                                cv.column.clone(),
                                (!cv.value.is_null()).then(|| Self::param_text(&cv.value)),
                            )
                        })
                        .collect(),
                })
                .collect(),
            _ => batch
                .iter()
                .map(|&n| InputRow::of_node(&nodes[n]))
                .collect(),
        };
        let context = verb.map(|verb| WriteContext {
            schema: node.schema.clone(),
            table: node.table.clone(),
            verb,
            rows,
        });
        CompiledWrite {
            statement: CompiledStatement {
                query: CompiledQuery::new(sql, params),
                deferred,
                strict: true,
                context,
            },
            returning,
            row_nodes: batch.to_vec(),
            collect: false,
            expect_rows,
            counts: is_root,
            guard: false,
        }
    }

    /// `alias."column" = $n::type` conditions of a key.
    fn key_conditions(
        alias: &str,
        key: &[WhereCondition],
        params: &mut Vec<String>,
    ) -> Vec<String> {
        key.iter()
            .map(|k| {
                Self::compare(
                    &col(alias, &k.column),
                    &k.operator,
                    Self::bind(params, &k.value),
                    k.pg_cast.as_deref(),
                )
            })
            .collect()
    }

    /// `"a"::text AS "a", ...` of `columns` read from `alias` (the row before the write for
    /// [`OLD_ROW`]); a constant when there are none (a select list cannot be empty).
    fn returned_columns(columns: &[String], alias: &str) -> String {
        if columns.is_empty() {
            return "1 AS \"__row\"".to_string();
        }
        columns
            .iter()
            .map(|c| {
                if c == OLD_ROW {
                    format!("to_json({alias})::text AS \"{OLD_ROW}\"")
                } else {
                    format!("{}::text AS {}", col(alias, c), quote_ident(c))
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// ` ("a", "b")` and `$1::text::int4, ...` of the values and references of an inserted node.
    fn insert_columns(
        node: &WriteNode,
        params: &mut Vec<String>,
        deferred: &mut Vec<(usize, ParamSource)>,
    ) -> (String, String) {
        let mut cols: Vec<String> = node
            .values
            .iter()
            .map(|cv| quote_ident(&cv.column))
            .collect();
        let mut exprs: Vec<String> = node
            .values
            .iter()
            .map(|cv| Self::bind_value(params, cv))
            .collect();
        for r in &node.refs {
            cols.push(quote_ident(&r.column));
            let value = ValueRef::Node {
                node: r.node,
                column: r.source_column.clone(),
            };
            exprs.push(Self::deferred_value(
                value,
                &Self::ref_spec(r),
                params,
                deferred,
            ));
        }
        if cols.is_empty() {
            return (String::new(), String::new());
        }
        (format!(" ({})", cols.join(", ")), exprs.join(", "))
    }

    /// `SET` list of a node's values; with none, a key column set to itself (the row is still
    /// found and returned).
    fn set_list(node: &WriteNode, key: &[WhereCondition], params: &mut Vec<String>) -> String {
        if node.values.is_empty() {
            let column = key.first().map(|k| k.column.clone()).unwrap_or_default();
            return format!("{0} = _u.{0}", quote_ident(&column));
        }
        node.values
            .iter()
            .map(|cv| {
                format!(
                    "{} = {}",
                    quote_ident(&cv.column),
                    Self::bind_value(params, cv)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `sync`: one `MERGE` making the children of a parent (whose key the node's `refs`
    /// take) exactly `rows` — rows matching a child by key update it, others are inserted,
    /// and children no row matches are deleted (`WHEN NOT MATCHED BY SOURCE`, PostgreSQL 17).
    /// Every row sets the same columns.
    fn compile_sync(
        node: &WriteNode,
        rows: &[Vec<ColumnValue>],
        key: &[(String, Option<String>)],
        target: &str,
        params: &mut Vec<String>,
        deferred: &mut Vec<(usize, ParamSource)>,
    ) -> String {
        // Every column some row sets (rows differ only in whether they set the key)
        let mut specs: Vec<ColumnValue> = Vec::new();
        for cv in rows.iter().flatten() {
            if !specs.iter().any(|s| s.column == cv.column) {
                specs.push(ColumnValue {
                    value: serde_json::Value::Null,
                    ..cv.clone()
                });
            }
        }
        // The rows, one text array per column, unnested in step
        let mut arrays = Vec::new();
        for spec in &specs {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| match r.iter().find(|cv| cv.column == spec.column) {
                    Some(cv) => match Self::cv_text(cv) {
                        Some(text) => serde_json::Value::String(text),
                        None => serde_json::Value::Null,
                    },
                    _ => serde_json::Value::Null,
                })
                .collect();
            arrays.push(format!(
                "${}::text::text[]",
                Self::bind(params, &pg_array_literal(&serde_json::Value::Array(items)))
            ));
        }
        let names: Vec<String> = (0..specs.len()).map(|j| format!("c{j}")).collect();
        let source = if specs.is_empty() {
            format!("(SELECT FROM generate_series(1, {})) AS _s", rows.len())
        } else {
            format!(
                "(SELECT * FROM unnest({}) AS _v({})) AS _s",
                arrays.join(", "),
                names.join(", ")
            )
        };
        let refs: Vec<(String, String)> = node
            .refs
            .iter()
            .map(|r| {
                let value = ValueRef::Node {
                    node: r.node,
                    column: r.source_column.clone(),
                };
                (
                    r.column.clone(),
                    Self::deferred_value(value, &Self::ref_spec(r), params, deferred),
                )
            })
            .collect();
        let of_parent: Vec<String> = refs
            .iter()
            .map(|(c, e)| format!("{} = {e}", col("_t", c)))
            .collect();
        let source_col = |column: &str| specs.iter().position(|s| s.column == column);
        // Rows match children by key; rows without it are new
        let key_match: Vec<String> = key
            .iter()
            .map(|(k, cast)| match source_col(k) {
                Some(j) => match cast {
                    Some(ty) => format!("{} = _s.c{j}::{ty}", col("_t", k)),
                    None => format!("{}::text = _s.c{j}", col("_t", k)),
                },
                None => "false".to_string(),
            })
            .collect();
        let on = key_match
            .iter()
            .chain(&of_parent)
            .cloned()
            .collect::<Vec<_>>()
            .join(" AND ");
        let assign =
            |j: usize, spec: &ColumnValue| Self::assign_expr(&format!("_s.c{j}"), spec, true);
        let updates: Vec<String> = specs
            .iter()
            .enumerate()
            .filter(|(_, s)| !key.iter().any(|(k, _)| *k == s.column))
            .map(|(j, s)| format!("{} = {}", quote_ident(&s.column), assign(j, s)))
            .collect();
        let matched = if updates.is_empty() {
            "DO NOTHING".to_string()
        } else {
            format!("UPDATE SET {}", updates.join(", "))
        };
        let insert = |skip_key: bool| {
            let mut cols = Vec::new();
            let mut values = Vec::new();
            for (j, s) in specs.iter().enumerate() {
                if skip_key && key.iter().any(|(k, _)| *k == s.column) {
                    continue;
                }
                cols.push(quote_ident(&s.column));
                values.push(assign(j, s));
            }
            for (c, e) in &refs {
                cols.push(quote_ident(c));
                values.push(e.clone());
            }
            format!(
                "INSERT ({}) VALUES ({})",
                cols.join(", "),
                values.join(", ")
            )
        };
        // Rows leaving the key out take its default
        let keyed: Vec<usize> = key.iter().filter_map(|(k, _)| source_col(k)).collect();
        let mut clauses = vec![format!("WHEN MATCHED THEN {matched}")];
        if !keyed.is_empty() {
            let unkeyed = keyed
                .iter()
                .map(|j| format!("_s.c{j} IS NULL"))
                .collect::<Vec<_>>()
                .join(" AND ");
            clauses.push(format!(
                "WHEN NOT MATCHED BY TARGET AND {unkeyed} THEN {}",
                insert(true)
            ));
        }
        clauses.push(format!("WHEN NOT MATCHED BY TARGET THEN {}", insert(false)));
        clauses.push(format!(
            "WHEN NOT MATCHED BY SOURCE AND {} THEN DELETE",
            of_parent.join(" AND ")
        ));
        format!(
            "MERGE INTO {target} AS _t USING {source} ON {on} {}",
            clauses.join(" ")
        )
    }

    /// Column assignment spec of a node reference (its value comes later).
    fn ref_spec(r: &NodeRef) -> ColumnValue {
        ColumnValue {
            column: r.column.clone(),
            value: serde_json::Value::Null,
            pg_cast: r.pg_cast.clone(),
            composite: false,
            composite_array: false,
        }
    }

    /// Assignment of a value known only when the statement runs.
    fn deferred_value(
        value: ValueRef,
        spec: &ColumnValue,
        params: &mut Vec<String>,
        deferred: &mut Vec<(usize, ParamSource)>,
    ) -> String {
        let idx = Self::bind_str(params, String::new());
        deferred.push((idx - 1, ParamSource::Value(value)));
        Self::assign_expr(&format!("${idx}::text"), spec, false)
    }

    /// `INSERT` of a batch of nodes setting the same columns (without RETURNING): `VALUES`
    /// for one row, else one text array per column unnested in step, so the statement is the
    /// same for any number of rows and its prepared form is reused.
    fn compile_insert_batch(
        nodes: &[WriteNode],
        batch: &[usize],
        on_conflict: Option<&OnConflict>,
        primary_key: &[(String, Option<String>)],
        params: &mut Vec<String>,
        deferred: &mut Vec<(usize, ParamSource)>,
    ) -> String {
        let first = &nodes[batch[0]];
        let target = quote_qualified(&first.schema, &first.table);
        let specs: Vec<ColumnValue> = first
            .values
            .iter()
            .map(|v| ColumnValue {
                value: serde_json::Value::Null,
                ..v.clone()
            })
            .chain(first.refs.iter().map(Self::ref_spec))
            .collect();
        let columns: Vec<&str> = specs.iter().map(|s| s.column.as_str()).collect();
        let conflict = on_conflict
            .map(|oc| Self::on_conflict_sql(oc, &columns, primary_key))
            .unwrap_or_default();
        let value_of = |node: &WriteNode, column: &str| -> ValueRef {
            if let Some(v) = node.values.iter().find(|v| v.column == column) {
                return ValueRef::Literal(Self::cv_text(v));
            }
            match node.refs.iter().find(|r| r.column == column) {
                Some(r) => ValueRef::Node {
                    node: r.node,
                    column: r.source_column.clone(),
                },
                None => ValueRef::Literal(None),
            }
        };
        let col_names = columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");

        if let [only] = batch {
            if specs.is_empty() {
                return format!("INSERT INTO {target} DEFAULT VALUES{conflict}");
            }
            let node = &nodes[*only];
            let exprs: Vec<String> = specs
                .iter()
                .map(|spec| match value_of(node, &spec.column) {
                    ValueRef::Literal(None) => "NULL".to_string(),
                    ValueRef::Literal(Some(text)) => {
                        let idx = Self::bind_str(params, text);
                        Self::assign_expr(&format!("${idx}::text"), spec, false)
                    }
                    node_ref => Self::deferred_value(node_ref, spec, params, deferred),
                })
                .collect();
            return format!(
                "INSERT INTO {target} ({col_names}) VALUES ({}){conflict}",
                exprs.join(", ")
            );
        }

        if specs.is_empty() {
            return format!(
                "INSERT INTO {target} SELECT FROM generate_series(1, {}){conflict}",
                batch.len()
            );
        }
        let mut arrays = Vec::new();
        let mut exprs = Vec::new();
        for (j, spec) in specs.iter().enumerate() {
            let values: Vec<ValueRef> = batch
                .iter()
                .map(|&n| value_of(&nodes[n], &spec.column))
                .collect();
            let idx = if values.iter().all(|v| matches!(v, ValueRef::Literal(_))) {
                let items = values
                    .into_iter()
                    .map(|v| match v {
                        ValueRef::Literal(Some(text)) => serde_json::Value::String(text),
                        _ => serde_json::Value::Null,
                    })
                    .collect();
                Self::bind(params, &pg_array_literal(&serde_json::Value::Array(items)))
            } else {
                let idx = Self::bind_str(params, String::new());
                deferred.push((idx - 1, ParamSource::Array(values)));
                idx
            };
            arrays.push(format!("${idx}::text::text[]"));
            exprs.push(Self::assign_expr(&format!("_v.c{j}"), spec, true));
        }
        let names: Vec<String> = (0..specs.len()).map(|j| format!("c{j}")).collect();
        format!(
            "INSERT INTO {target} ({col_names}) SELECT {} FROM unnest({}) WITH ORDINALITY AS _v({}, _ord) ORDER BY _v._ord{conflict}",
            exprs.join(", "),
            arrays.join(", "),
            names.join(", ")
        )
    }

    /// UPDATE or DELETE without RETURNING, aliasing the target `table_alias` (which filters
    /// refer to).
    fn compile_dml(
        target: &str,
        table_alias: &str,
        kind: &MutationKind,
        key_columns: &[(String, Option<String>)],
        params: &mut Vec<String>,
    ) -> String {
        match kind {
            MutationKind::Update {
                target: rows,
                column_values,
            } => {
                // An empty input still has to find (and return) the rows
                let set_exprs = if column_values.is_empty() {
                    let touched = match rows {
                        RowTarget::Key(key) => key.first().map(|k| k.column.as_str()),
                        RowTarget::Filter(_) => None,
                    };
                    let first = quote_ident(
                        touched
                            .or(key_columns.first().map(|(c, _)| c.as_str()))
                            .unwrap_or_default(),
                    );
                    format!("{first} = {first}")
                } else {
                    column_values
                        .iter()
                        .map(|cv| {
                            format!(
                                "{} = {}",
                                quote_ident(&cv.column),
                                Self::bind_value(params, cv)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let where_sql = Self::row_target_sql(rows, params);
                format!("UPDATE {target} AS {table_alias} SET {set_exprs}{where_sql}")
            }
            MutationKind::Delete { target: rows } => {
                let where_sql = Self::row_target_sql(rows, params);
                format!("DELETE FROM {target} AS {table_alias}{where_sql}")
            }
            MutationKind::Insert { .. } => unreachable!("inserts compile per write node batch"),
        }
    }

    /// ` WHERE ...` selecting the rows an update or delete applies to (empty for all rows).
    fn row_target_sql(rows: &RowTarget, params: &mut Vec<String>) -> String {
        let condition = match rows {
            RowTarget::Key(key) => key
                .iter()
                .map(|k| {
                    let idx = Self::bind(params, &k.value);
                    Self::compare(
                        &quote_ident(&k.column),
                        &k.operator,
                        idx,
                        k.pg_cast.as_deref(),
                    )
                })
                .collect::<Vec<_>>()
                .join(" AND "),
            RowTarget::Filter(Some(f)) => Self::compile_filter_expr(f, params),
            RowTarget::Filter(None) => String::new(),
        };
        if condition.is_empty() {
            String::new()
        } else {
            format!(" WHERE {condition}")
        }
    }

    /// ` ON CONFLICT (<key>) DO UPDATE SET ...` for rows setting `columns`: the configured
    /// columns take the proposed row's values, by default the ones set apart from the
    /// conflict key and the primary key (an upsert on another key does not re-key the row).
    /// With nothing to update the conflicting row is left alone (`DO NOTHING`).
    fn on_conflict_sql(
        oc: &OnConflict,
        columns: &[&str],
        primary_key: &[(String, Option<String>)],
    ) -> String {
        let key = oc
            .columns
            .iter()
            .map(|(c, _)| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let is_key = |c: &str| oc.columns.iter().chain(primary_key).any(|(k, _)| k == c);
        let update: Vec<&str> = match &oc.update {
            Some(cols) => cols.iter().map(String::as_str).collect(),
            None => columns.iter().copied().filter(|c| !is_key(c)).collect(),
        };
        if oc.do_nothing || update.is_empty() {
            return format!(" ON CONFLICT ({key}) DO NOTHING");
        }
        let set = update
            .iter()
            .map(|c| format!("{0} = EXCLUDED.{0}", quote_ident(c)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" ON CONFLICT ({key}) DO UPDATE SET {set}")
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_connection(
        schema: &str,
        table: &str,
        table_alias: &str,
        selections: &ConnectionSelections,
        where_conditions: &[WhereCondition],
        filter_expr: Option<&FilterExpr>,
        join_condition: &Option<JoinCondition>,
        first: Option<usize>,
        last: Option<usize>,
        offset: Option<usize>,
        after: &Option<String>,
        before: &Option<String>,
        sort: &[SortKey],
        source: Option<&FunctionSource>,
        id_cast: Option<&str>,
        cursor_columns: &[(String, Option<String>)],
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        // Estimated totals are read from statistics or a separate EXPLAIN: a connection
        // selecting nothing else reads no rows, and binds no conditions
        let estimating = ESTIMATING.with(|e| *e.borrow()) && join_condition.is_none();
        let needs_page =
            selections.include_edges || selections.include_nodes || selections.include_page_info;
        let totals: Vec<(String, bool)> =
            if selections.total_counts.is_empty() && selections.include_total_count {
                vec![("totalCount".to_string(), selections.estimate_total)]
            } else {
                selections.total_counts.clone()
            };
        let only_estimates = estimating
            && !needs_page
            && selections.aggregates.is_empty()
            && selections.grouped.is_empty()
            && !totals.is_empty()
            && totals.iter().all(|(_, estimate)| *estimate);

        // 1-2. Join condition with the parent, conditions and filter
        let mut where_clauses = if only_estimates {
            Vec::new()
        } else {
            Self::connection_where(
                table_alias,
                where_conditions,
                filter_expr,
                join_condition,
                params,
            )
        };
        let unconditioned =
            where_conditions.is_empty() && filter_expr.is_none() && source.is_none();

        // 3. Keyset order and cursor conditions (after / before). Plans without a sort
        // order by the table's key.
        let legacy_key = [("id".to_string(), id_cast.map(str::to_string))];
        let key = if cursor_columns.is_empty() {
            &legacy_key[..]
        } else {
            cursor_columns
        };
        let key_sort: Vec<SortKey>;
        let keyset = if sort.is_empty() {
            key_sort = key
                .iter()
                .map(|(column, cast)| SortKey {
                    column: column.clone(),
                    descending: false,
                    nullable: false,
                    compare: cast.clone().map_or(SortCompare::Text, SortCompare::Cast),
                    expr: None,
                })
                .collect();
            &key_sort[..]
        } else {
            sort
        };
        let key_exprs: Vec<String> = keyset
            .iter()
            .map(|k| match k.compare {
                SortCompare::Text => format!("({})::text", Self::sort_expression(table_alias, k)),
                _ => Self::sort_expression(table_alias, k),
            })
            .collect();
        // Window values (`rowNumber`, `runningSum`, ...) are computed over every matching row
        // in the connection's order, before cursors and limits cut the page out
        let mut window_cols: Vec<String> = Vec::new();
        let mut window_pairs: Vec<String> = Vec::new();
        for (i, w) in selections.windows.iter().enumerate() {
            let partition: Vec<String> = w
                .partition
                .iter()
                .map(|k| {
                    let c = col(table_alias, &k.column);
                    match (k.truncate.as_deref(), k.is_date) {
                        (Some(unit), false) if unit != "bin" => {
                            format!("date_trunc('{unit}', {c})")
                        }
                        (Some(unit), true) if unit != "bin" => {
                            format!("date_trunc('{unit}', {c}::timestamp)::date")
                        }
                        _ => c,
                    }
                })
                .collect();
            let partition = if partition.is_empty() {
                String::new()
            } else {
                format!("PARTITION BY {} ", partition.join(", "))
            };
            let order: Vec<String> = keyset
                .iter()
                .zip(&key_exprs)
                .map(|(k, e)| format!("{e} {}", if k.descending { "DESC" } else { "ASC" }))
                .collect();
            let over = format!(
                "OVER ({partition}ORDER BY {} ROWS UNBOUNDED PRECEDING)",
                order.join(", ")
            );
            if w.function == "running_sum" {
                let mut fields = Vec::new();
                for (j, f) in w.fields.iter().enumerate() {
                    window_cols.push(format!(
                        "sum({}) {over} AS \"__w{i}_{j}\"",
                        col(table_alias, &f.column_name)
                    ));
                    fields.push(Self::json_pair(
                        &f.field_alias,
                        &format!("{table_alias}.\"__w{i}_{j}\""),
                    ));
                }
                let obj = if fields.is_empty() {
                    "'{}'::json".to_string()
                } else {
                    Self::json_object(&fields)
                };
                window_pairs.push(Self::json_pair(&w.field_alias, &obj));
            } else {
                // Ranks compare the `orderBy` values only (row numbers follow the whole order),
                // and ignore the frame
                let over = if w.function == "row_number" || w.rank_keys == 0 {
                    over.replace(" ROWS UNBOUNDED PRECEDING", "")
                } else {
                    let ranked: Vec<&String> = order.iter().take(w.rank_keys).collect();
                    format!(
                        "OVER ({partition}ORDER BY {})",
                        ranked
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                window_cols.push(format!("{}() {over} AS \"__w{i}\"", w.function));
                window_pairs.push(Self::json_pair(
                    &w.field_alias,
                    &format!("{table_alias}.\"__w{i}\""),
                ));
            }
        }
        let base_where = where_clauses.len();

        for (cursor, after) in [(after, true), (before, false)] {
            let Some(cursor) = cursor.as_ref().filter(|_| !only_estimates) else {
                continue;
            };
            // Single-column keysets use the raw value as cursor; others a JSON array
            let values = match keyset {
                [_] => vec![serde_json::Value::String(Self::decode_cursor(cursor))],
                _ => Self::decode_composite_cursor(cursor),
            };
            where_clauses.push(Self::keyset_condition(
                &key_exprs, keyset, &values, after, params,
            ));
        }

        let where_sql = Self::where_sql(&where_clauses);

        // `last` (without `first`) pages backward: the page is read in reverse order, from
        // the end or from `before`, and flipped back when aggregated
        let backward = last.is_some() && first.is_none();
        let order_items: Vec<String> = keyset
            .iter()
            .zip(&key_exprs)
            .map(|(k, e)| {
                format!(
                    "{e} {}",
                    if k.descending != backward {
                        "DESC"
                    } else {
                        "ASC"
                    }
                )
            })
            .collect();
        let order_by_sql = format!("ORDER BY {}", order_items.join(", "));
        let rn_order = if backward {
            " ORDER BY rn DESC"
        } else {
            " ORDER BY rn"
        };

        // `offset` skips rows past the cursor (or the start), in the direction read
        let offset = offset.filter(|&o| o > 0);
        let skipped = offset.is_some();
        // Sizes are bound only when a page is read: a parameter the SQL leaves out is an error
        let mut bind_size = |n: usize| {
            if needs_page {
                Self::bind_count(params, n)
            } else {
                String::new()
            }
        };
        let limit_val = bind_size(first.or(last).unwrap_or(100));
        // `first` and `last` together: the last `last` rows of the first `first` (Relay)
        let tail = first.and(last).map(&mut bind_size);
        let has_more = format!("(count(rn) > {limit_val})");
        let (has_next, has_prev) = match &tail {
            _ if backward => ((before.is_some() || skipped).to_string(), has_more),
            Some(last) if !(after.is_some() || skipped) => (
                has_more,
                format!("(least(count(rn), {limit_val}) > {last})"),
            ),
            _ => (has_more, (after.is_some() || skipped).to_string()),
        };

        // 4. Build node fields expressions with lateral joins for nested relations
        let mut scope = JoinScope::default();
        let node_obj = Self::row_object(
            table_alias,
            &selections.node_fields,
            selections.node_polymorphism.as_ref(),
            "Node",
            &mut scope,
            plan,
            params,
        );
        let conn_laterals_sql = scope.sql;
        // Values are encoded as text, which casts back exactly (JSON numbers would round
        // through f64); `encode` wraps base64 lines at 76 characters, so newlines are removed.
        let cursor_value = match key_exprs.as_slice() {
            [single] => format!("{single}::text"),
            exprs => {
                let texts: Vec<String> = exprs.iter().map(|e| format!("{e}::text")).collect();
                format!("json_build_array({})::text", texts.join(", "))
            }
        };
        let cursor_expr =
            format!("translate(encode(convert_to({cursor_value}, 'UTF8'), 'base64'), chr(10), '')");

        let mut edge_pairs = vec![
            Self::json_pair("cursor", &cursor_expr),
            Self::json_pair("node", &node_obj),
        ];
        edge_pairs.extend(window_pairs);
        let edge_obj = Self::json_object(&edge_pairs);

        // 5. Build outer object fields. The page query fetches one extra row: its presence
        // answers hasNextPage, and it is filtered out of edges/nodes/cursors.
        let in_page = match &tail {
            Some(last) => format!("rn <= {limit_val} AND rn > least(n, {limit_val}) - {last}"),
            None => format!("rn <= {limit_val}"),
        };
        let from = match source {
            Some(source) => Self::function_from(source, table_alias, params),
            None => Self::from_item(schema, table, table_alias, join_condition.as_ref()),
        };
        let mut outer_pairs = Vec::new();

        // totalCount and aggregates are computed in one scan of the filtered rows (`_stats`),
        // instead of one scan each
        // `(expression, column name)` of the single `_stats` row
        let mut stats_cols: Vec<(String, String)> = Vec::new();
        let stats_ref = |col: &str| {
            if needs_page {
                // The page rows are aggregated alongside the single `_stats` row
                format!("any_value(_stats.\"{col}\")")
            } else {
                format!("_stats.\"{col}\"")
            }
        };

        for (key, estimate) in &totals {
            if !(*estimate && estimating) {
                if !stats_cols.iter().any(|(_, name)| name == "__total") {
                    stats_cols.push(("count(*)".to_string(), "__total".to_string()));
                }
                outer_pairs.push(Self::json_pair(key, &stats_ref("__total")));
                continue;
            }
            let target = quote_qualified(schema, table);
            let estimate = if unconditioned {
                // Table statistics (the leaf partitions' for a partitioned table), when analyzed
                format!(
                    "coalesce((SELECT CASE WHEN count(*) > 0 AND bool_and(c.reltuples >= 0) THEN sum(c.reltuples)::int8 END \
                     FROM pg_partition_tree({}::regclass) pt JOIN pg_class c ON c.oid = pt.relid \
                     WHERE pt.isleaf AND c.relkind IN ('r', 'm')), (SELECT count(*) FROM {target}))",
                    Self::sql_literal(&target)
                )
            } else {
                // The planner's estimate of the rows matching the conditions, from EXPLAIN
                let mut estimate_params = Vec::new();
                let clauses = Self::connection_where(
                    table_alias,
                    where_conditions,
                    filter_expr,
                    join_condition,
                    &mut estimate_params,
                );
                let from = match source {
                    Some(source) => Self::function_from(source, table_alias, &mut estimate_params),
                    None => Self::from_item(schema, table, table_alias, None),
                };
                let sql = format!("SELECT 1 FROM {from}{}", Self::where_sql(&clauses));
                let param = Self::bind_str(params, String::new());
                ESTIMATES.with(|e| {
                    e.borrow_mut().push(Estimate {
                        param: param - 1,
                        query: CompiledQuery::new(sql, estimate_params),
                    })
                });
                format!("${param}::text::int8")
            };
            outer_pairs.push(Self::json_pair(key, &format!("({estimate})::int")));
        }

        for (i, aggs) in selections.aggregates.iter().enumerate() {
            let agg_pairs = Self::aggregate_pairs(aggs, table_alias, params);
            let obj = if agg_pairs.is_empty() {
                "'{}'::json".to_string()
            } else {
                Self::json_object(&agg_pairs)
            };
            stats_cols.push((obj, format!("__aggs{i}")));
            outer_pairs.push(Self::json_pair(
                &aggs.field_alias,
                &stats_ref(&format!("__aggs{i}")),
            ));
        }
        for (i, grouped) in selections.grouped.iter().enumerate() {
            outer_pairs.push(Self::json_pair(
                &grouped.field_alias,
                &stats_ref(&format!("__g{i}")),
            ));
        }

        if selections.include_page_info {
            let page_cursors = format!("(array_agg(cur{rn_order}) FILTER (WHERE {in_page}))");
            let page_info = Self::json_object(&[
                Self::json_pair("hasNextPage", &has_next),
                Self::json_pair("hasPreviousPage", &has_prev),
                Self::json_pair("startCursor", &format!("{page_cursors}[1]")),
                Self::json_pair(
                    "endCursor",
                    &format!("{page_cursors}[array_length({page_cursors}, 1)]"),
                ),
            ]);
            outer_pairs.push(Self::json_pair("pageInfo", &page_info));
        }

        if selections.include_edges {
            outer_pairs.push(Self::json_pair(
                "edges",
                &Self::json_array_agg("edge", rn_order, &in_page),
            ));
        }

        if selections.include_nodes {
            outer_pairs.push(Self::json_pair(
                "nodes",
                &Self::json_array_agg("node_obj", rn_order, &in_page),
            ));
        }

        if outer_pairs.is_empty() {
            outer_pairs.push(Self::json_pair("__typename", "'Connection'"));
        }

        let stats_where_sql = Self::where_sql(&where_clauses[..base_where]);
        let stats_from = if !selections.grouped.is_empty() {
            Some(Self::grouped_stats(
                &selections.grouped,
                &stats_cols,
                table_alias,
                &from,
                &stats_where_sql,
                params,
            ))
        } else if stats_cols.is_empty() {
            None
        } else {
            let cols: Vec<String> = stats_cols
                .iter()
                .map(|(expr, name)| format!("{expr} AS \"{name}\""))
                .collect();
            Some(format!(
                "(SELECT {} FROM {from}{stats_where_sql}) _stats",
                cols.join(", ")
            ))
        };
        if !needs_page {
            let from = stats_from.map(|s| format!(" FROM {s}")).unwrap_or_default();
            format!("(SELECT {}{from})", Self::json_object(&outer_pairs))
        } else {
            // With stats, the page is LEFT JOINed to the one `_stats` row so an empty page still
            // yields a row (page aggregates then skip the NULL `rn`).
            let (stats_prefix, join_suffix) = match stats_from {
                Some(s) => (format!("{s} LEFT JOIN "), " ON true"),
                None => (String::new(), ""),
            };
            // Filter, order and limit the base rows first; node JSON and nested laterals are then
            // built only for the (limit + 1) rows of the page, not for every matching row.
            let page_limit = format!("{limit_val} + 1");
            // With window values, the page is cut out of the matching rows carrying them
            let (page_from, page_where) = if window_cols.is_empty() {
                (from.clone(), where_sql.clone())
            } else {
                (
                    format!(
                        "(SELECT {table_alias}.*, {} FROM {from}{}) {table_alias}",
                        window_cols.join(", "),
                        Self::where_sql(&where_clauses[..base_where])
                    ),
                    Self::where_sql(&where_clauses[base_where..]),
                )
            };
            let offset_sql = offset
                .map(|o| format!(" OFFSET {}", Self::bind_count(params, o)))
                .unwrap_or_default();
            let row_count = if tail.is_some() {
                " count(*) OVER () AS n,"
            } else {
                ""
            };
            format!(
                "(SELECT {} FROM {stats_prefix}(\
                    SELECT \
                    row_number() OVER ({order_by_sql}) AS rn,{row_count} \
                    {cursor_expr} AS cur, \
                    {edge_obj} AS edge, \
                    {node_obj} AS node_obj \
                FROM (\
                    SELECT {table_alias}.* FROM {page_from}{page_where} \
                    {order_by_sql} LIMIT {page_limit}{offset_sql}\
                ) {table_alias}\
                {conn_laterals_sql}\
            ) _conn{join_suffix})",
                Self::json_object(&outer_pairs)
            )
        }
    }

    /// Conditions of a connection's rows: joined to the parent row, its `condition` and
    /// `filter`.
    fn connection_where(
        table_alias: &str,
        where_conditions: &[WhereCondition],
        filter_expr: Option<&FilterExpr>,
        join_condition: &Option<JoinCondition>,
        params: &mut Vec<String>,
    ) -> Vec<String> {
        let mut where_clauses = Vec::new();
        if let Some(join) = join_condition {
            where_clauses.push(Self::join_clause(table_alias, join));
        }
        for cond in where_conditions {
            where_clauses.push(Self::where_clause(table_alias, cond, params));
        }
        if let Some(f) = filter_expr {
            let f_sql = Self::compile_filter_expr(f, params);
            if !f_sql.is_empty() {
                where_clauses.push(f_sql);
            }
        }
        where_clauses
    }

    /// JSON pairs of the aggregate objects of `aggs` over the rows of `table_alias` (in an
    /// aggregate query). min/max of a range read its bounds; every aggregate takes the
    /// selection's filter (`FILTER (WHERE ...)`).
    fn aggregate_pairs(
        aggs: &AggregatesSelections,
        table_alias: &str,
        params: &mut Vec<String>,
    ) -> Vec<String> {
        let filter = match &aggs.filter {
            Some(f) => {
                let f_sql = Self::compile_filter_expr(f, params);
                if f_sql.is_empty() {
                    String::new()
                } else {
                    format!(" FILTER (WHERE {f_sql})")
                }
            }
            None => String::new(),
        };
        let mut pairs = Vec::new();
        for part in &aggs.parts {
            let fraction = part
                .fraction
                .map(|f| Self::bind(params, &serde_json::json!(f)));
            let fields: Vec<String> = part
                .fields
                .iter()
                .map(|f| {
                    let c = col(table_alias, &f.column_name);
                    let expr = match part.function.as_str() {
                        "min" | "max" => {
                            let arg = match (f.is_range, part.function.as_str()) {
                                (true, "min") => format!("lower({c})"),
                                (true, _) => format!("upper({c})"),
                                _ => c,
                            };
                            Self::value_output(&format!("{}({arg}){filter}", part.function), &f.output, 0)
                        }
                        "count_distinct" => format!("count(DISTINCT {c}){filter}"),
                        "percentile_cont" => {
                            let fraction = fraction.expect("percentiles have a fraction");
                            format!("percentile_cont(${fraction}::text::float8) WITHIN GROUP (ORDER BY {c}){filter}")
                        }
                        // sum, avg, stddev_samp, stddev_pop, var_samp, var_pop
                        function => format!("{function}({c}){filter}"),
                    };
                    Self::json_pair(&f.field_alias, &expr)
                })
                .collect();
            let obj = if fields.is_empty() {
                "'{}'::json".to_string()
            } else {
                Self::json_object(&fields)
            };
            pairs.push(Self::json_pair(&part.field_alias, &obj));
        }
        pairs
    }

    /// The object of a group that has no rows (a gap `fillGaps` fills), whose single key is
    /// `key`: counts are 0, other aggregates null.
    fn empty_group(g: &GroupedAggregates, key: &str) -> String {
        let mut pairs = Vec::new();
        if g.include_keys {
            pairs.push(Self::json_pair(
                "keys",
                &format!("JSON_ARRAY({key} NULL ON NULL RETURNING json)"),
            ));
        }
        if g.include_count {
            pairs.push(Self::json_pair("count", "0"));
        }
        for part in &g.aggregates.parts {
            let value = if part.function == "count_distinct" {
                "0"
            } else {
                "NULL"
            };
            let fields: Vec<String> = part
                .fields
                .iter()
                .map(|f| Self::json_pair(&f.field_alias, value))
                .collect();
            let obj = if fields.is_empty() {
                "'{}'::json".to_string()
            } else {
                Self::json_object(&fields)
            };
            pairs.push(Self::json_pair(&part.field_alias, &obj));
        }
        if pairs.is_empty() {
            "'{}'::json".to_string()
        } else {
            Self::json_object(&pairs)
        }
    }

    /// The `_stats` row of a connection with `groupedAggregates`: one scan of the filtered
    /// rows grouped by `GROUPING SETS`, one set per grouping plus the empty set for the
    /// connection-wide `stats_cols` (totalCount, aggregates). `GROUPING(...)` tells the sets'
    /// rows apart; each grouping's groups are then aggregated into one JSON array.
    fn grouped_stats(
        grouped: &[GroupedAggregates],
        stats_cols: &[(String, String)],
        table_alias: &str,
        from: &str,
        where_sql: &str,
        params: &mut Vec<String>,
    ) -> String {
        // Grouping expressions, each written once: the select list and GROUP BY must match
        let key_expr = |key: &GroupKey, g: &GroupedAggregates, params: &mut Vec<String>| {
            let c = col(table_alias, &key.column);
            let mut tz = || {
                g.time_zone
                    .as_ref()
                    .map(|tz| format!(", ${}::text", Self::bind_str(params, tz.clone())))
            };
            match (key.truncate.as_deref(), key.is_date) {
                (None, _) => c,
                (Some("bin"), _) => {
                    let bins = g.bins.as_ref().expect("binned keys have bins");
                    let width = Self::bind_str(params, bins.width.clone());
                    let ty = key.sql_type.as_deref().unwrap_or("timestamptz");
                    let origin = match &bins.origin {
                        Some(origin) => {
                            format!("${}::text::{ty}", Self::bind_str(params, origin.clone()))
                        }
                        None => format!("'2000-01-01'::{ty}"),
                    };
                    format!("date_bin(${width}::text::interval, {c}, {origin})")
                }
                (Some(unit), false) if key.sql_type.as_deref() == Some("timestamptz") => {
                    format!("date_trunc('{unit}', {c}{})", tz().unwrap_or_default())
                }
                (Some(unit), false) => format!("date_trunc('{unit}', {c})"),
                (Some(unit), true) => format!("date_trunc('{unit}', {c}::timestamp)::date"),
            }
        };
        // Distinct grouping expressions, and each grouping's (in its `groupBy` order)
        let mut exprs: Vec<String> = Vec::new();
        let mut sets: Vec<Vec<usize>> = Vec::new();
        for g in grouped {
            let mut set = Vec::new();
            for k in &g.keys {
                let e = key_expr(k, g, params);
                let j = exprs.iter().position(|x| *x == e).unwrap_or_else(|| {
                    exprs.push(e);
                    exprs.len() - 1
                });
                set.push(j);
            }
            sets.push(set);
        }
        // `GROUPING(a, b, c)` sets a bit, first argument highest, for each expression the
        // row is not grouped by
        let n = exprs.len();
        let mask = |set: &[usize]| {
            (0..n)
                .filter(|j| !set.contains(j))
                .map(|j| 1u64 << (n - 1 - j))
                .sum::<u64>()
        };
        let all = mask(&[]);

        let mut inner = vec![format!("GROUPING({}) AS \"__set\"", exprs.join(", "))];
        inner.extend(
            exprs
                .iter()
                .enumerate()
                .map(|(j, e)| format!("{e} AS \"__k{j}\"")),
        );
        inner.extend(
            stats_cols
                .iter()
                .map(|(expr, name)| format!("{expr} AS \"{name}\"")),
        );
        let mut outer: Vec<String> = stats_cols
            .iter()
            .map(|(_, name)| {
                format!("any_value(\"{name}\") FILTER (WHERE \"__set\" = {all}) AS \"{name}\"")
            })
            .collect();
        let mut finals: Vec<String> = stats_cols
            .iter()
            .map(|(_, name)| format!("_s.\"{name}\""))
            .collect();
        for (i, (g, set)) in grouped.iter().zip(&sets).enumerate() {
            let mut pairs = Vec::new();
            if g.include_keys {
                let keys: Vec<String> = g
                    .keys
                    .iter()
                    .zip(set)
                    .map(|(k, &j)| Self::value_output(&exprs[j], &k.output, 0))
                    .collect();
                pairs.push(Self::json_pair(
                    "keys",
                    &format!(
                        "JSON_ARRAY({} NULL ON NULL RETURNING json)",
                        keys.join(", ")
                    ),
                ));
            }
            if g.include_count {
                pairs.push(Self::json_pair("count", "count(*)"));
            }
            pairs.extend(Self::aggregate_pairs(&g.aggregates, table_alias, params));
            let group = if pairs.is_empty() {
                "'{}'::json".to_string()
            } else {
                Self::json_object(&pairs)
            };
            let having: Vec<String> = g
                .having
                .iter()
                .map(|h| Self::having_sql(h, table_alias, params))
                .collect();
            let having = if having.is_empty() {
                "true".to_string()
            } else {
                having.join(" AND ")
            };
            inner.push(format!("{group} AS \"__g{i}\""));
            inner.push(format!("{having} AS \"__h{i}\""));
            let order: Vec<String> = set.iter().map(|j| format!("\"__k{j}\"")).collect();
            let rows = format!("FILTER (WHERE \"__set\" = {} AND \"__h{i}\")", mask(set));
            outer.push(format!(
                "coalesce(JSON_ARRAYAGG(\"__g{i}\" ORDER BY {} RETURNING json) {rows}, '[]'::json) AS \"__g{i}\"",
                order.join(", ")
            ));
            if !g.fill_gaps {
                finals.push(format!("_s.\"__g{i}\""));
                continue;
            }
            // Missing groups between the first and last are filled in: the groups' keys come
            // along as a second array, parallel to the groups
            let j = set[0];
            outer.push(format!("JSON_ARRAYAGG(\"__k{j}\" ORDER BY \"__k{j}\" RETURNING json) {rows} AS \"__gk{i}\""));
            let key = &g.keys[0];
            let ty = key
                .sql_type
                .clone()
                .unwrap_or_else(|| "timestamptz".to_string());
            let step = match key.truncate.as_deref() {
                Some("bin") => format!(
                    "${}::text::interval",
                    Self::bind_str(params, g.bins.as_ref().expect("binned").width.clone())
                ),
                Some("quarter") => "interval '3 months'".to_string(),
                Some(unit) => format!("interval '1 {unit}'"),
                None => unreachable!("fillGaps needs a truncated key"),
            };
            // Series of dates run in timestamps; truncated times step in the time zone
            let (series_ty, back) = if key.is_date {
                ("timestamp".to_string(), "::date")
            } else {
                (ty.clone(), "")
            };
            let tz = match (&g.time_zone, ty.as_str()) {
                (Some(tz), "timestamptz") => {
                    format!(", ${}::text", Self::bind_str(params, tz.clone()))
                }
                _ => String::new(),
            };
            let empty = Self::empty_group(g, &format!("_ser.k{back}"));
            finals.push(format!(
                "(SELECT coalesce(JSON_ARRAYAGG(coalesce(_m.g, {empty}) ORDER BY _ser.k RETURNING json), '[]'::json) \
                 FROM (SELECT min(_m0.k) AS lo, max(_m0.k) AS hi FROM (\
                     SELECT (_ks.v #>> '{{}}')::{ty} AS k FROM json_array_elements(_s.\"__gk{i}\") AS _ks(v)\
                 ) _m0) _b \
                 CROSS JOIN LATERAL generate_series(_b.lo::{series_ty}, _b.hi::{series_ty}, {step}{tz}) AS _ser(k) \
                 LEFT JOIN (\
                     SELECT (_ks.v #>> '{{}}')::{ty} AS k, _gs.v AS g \
                     FROM json_array_elements(_s.\"__gk{i}\") WITH ORDINALITY AS _ks(v, n) \
                     JOIN json_array_elements(_s.\"__g{i}\") WITH ORDINALITY AS _gs(v, n) USING (n)\
                 ) _m ON _m.k = _ser.k{back}) AS \"__g{i}\""
            ));
        }
        let mut grouping_sets: Vec<String> = Vec::new();
        if !stats_cols.is_empty() {
            grouping_sets.push("()".to_string());
        }
        for set in &sets {
            let s = format!(
                "({})",
                set.iter()
                    .map(|&j| exprs[j].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if !grouping_sets.contains(&s) {
                grouping_sets.push(s);
            }
        }
        format!(
            "(SELECT {} FROM (SELECT {} FROM (SELECT {} FROM {from}{where_sql} GROUP BY GROUPING SETS ({})) _gs) _s) _stats",
            finals.join(", "),
            outer.join(", "),
            inner.join(", "),
            grouping_sets.join(", ")
        )
    }

    /// A `having` condition on the aggregates of a group, compared as numbers.
    fn having_sql(h: &HavingCondition, table_alias: &str, params: &mut Vec<String>) -> String {
        let expr = match &h.column {
            Some(c) => format!("({}({}))::numeric", h.function, col(table_alias, c)),
            None => "count(*)::numeric".to_string(),
        };
        let mut value =
            |v: &serde_json::Value| format!("${}::text::numeric", Self::bind(params, v));
        match h.operator.as_str() {
            "IS NULL" | "IS NOT NULL" => format!("{expr} {}", h.operator),
            "IN" | "NOT IN" if h.values.is_empty() => (h.operator == "NOT IN").to_string(),
            "IN" | "NOT IN" => {
                let values: Vec<String> = h.values.iter().map(&mut value).collect();
                format!("{expr} {} ({})", h.operator, values.join(", "))
            }
            op => format!("{expr} {op} {}", value(&h.values[0])),
        }
    }

    /// A search field: the `limit` best matches, best first, as `{ hits, hasMore }`. Rows
    /// are ordered by the method's own distance or score, so its index returns the best
    /// rows first and the scan stops after `limit + 1` rows (the extra one answers
    /// `hasMore`); node JSON is built only for the hits.
    #[allow(clippy::too_many_arguments)]
    fn compile_search(
        schema: &str,
        table: &str,
        table_alias: &str,
        search: &SearchOperation,
        where_conditions: &[WhereCondition],
        filter_expr: Option<&FilterExpr>,
        limit: usize,
        selections: &SearchSelections,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        let a = table_alias;
        let target = quote_qualified(schema, table);
        let mut where_clauses: Vec<String> = where_conditions
            .iter()
            .map(|c| Self::where_clause(a, c, params))
            .collect();
        if let Some(f) = filter_expr {
            let f_sql = Self::compile_filter_expr(f, params);
            if !f_sql.is_empty() {
                where_clauses.push(f_sql);
            }
        }
        let page_limit = limit + 1;
        let limit_sql = Self::bind_count(params, limit);
        let Ranking {
            cte,
            from,
            rank,
            direction,
            score,
            extra,
        } = Self::ranking(
            &target,
            a,
            search,
            &mut where_clauses,
            selections.highlight.as_ref(),
            page_limit,
            params,
        );
        let where_sql = Self::where_sql(&where_clauses);

        let mut scope = JoinScope::default();
        let node_obj = Self::row_object(
            a,
            &selections.node_fields,
            selections.node_polymorphism.as_ref(),
            "Node",
            &mut scope,
            plan,
            params,
        );
        let laterals = scope.sql;
        let mut hit_pairs = vec![Self::json_pair("node", &node_obj)];
        if selections.include_score {
            hit_pairs.insert(0, Self::json_pair("score", &format!("{a}.__score")));
        }
        if let Some(h) = &selections.highlight {
            let value = if extra.iter().any(|e| e.ends_with("\"__headline\"")) {
                format!("{a}.\"__headline\"")
            } else {
                "NULL".to_string()
            };
            hit_pairs.push(Self::json_pair(&h.field_alias, &value));
        }
        if let Some(alias) = &selections.distance {
            let value = if extra.iter().any(|e| e.ends_with("\"__distance\"")) {
                format!("{a}.\"__distance\"")
            } else {
                "NULL".to_string()
            };
            hit_pairs.push(Self::json_pair(alias, &value));
        }
        let extra = extra.iter().map(|e| format!(", {e}")).collect::<String>();
        let hit_obj = Self::json_object(&hit_pairs);

        let mut pairs = Vec::new();
        if selections.include_hits {
            pairs.push(Self::json_pair(
                "hits",
                &Self::json_array_agg("hit", " ORDER BY rn", &format!("rn <= {limit_sql}")),
            ));
        }
        if selections.include_has_more {
            pairs.push(Self::json_pair(
                "hasMore",
                &format!("(count(rn) > {limit_sql})"),
            ));
        }
        if pairs.is_empty() {
            pairs.push(Self::json_pair("__typename", "'SearchResults'"));
        }
        // The rank is ordered by as an expression (not an output alias) so the index serves it
        format!(
            "({cte}SELECT {} FROM (\
                SELECT row_number() OVER (ORDER BY {a}.__rank {direction}) AS rn, {hit_obj} AS hit \
                FROM (\
                    SELECT {a}.*, {score} AS __score, {rank} AS __rank{extra} FROM {from}{where_sql} \
                    ORDER BY {rank} {direction} LIMIT {limit_sql} + 1\
                ) {a}{laterals}\
            ) _search)",
            Self::json_object(&pairs)
        )
    }

    /// How a search finds and ranks rows of `target` (aliased `a`): a CTE to prepend, the
    /// `FROM` item, the rank expression and its direction (best first), the score (higher is
    /// better), and extra columns computed for the ranked rows (highlights, distances). The
    /// conditions it needs are added to `where_clauses`.
    #[allow(clippy::too_many_arguments)]
    fn ranking(
        target: &str,
        a: &str,
        search: &SearchOperation,
        where_clauses: &mut Vec<String>,
        highlight: Option<&HighlightOptions>,
        page_limit: usize,
        params: &mut Vec<String>,
    ) -> Ranking {
        let text = |params: &mut Vec<String>, value: &str| {
            format!("${}::text", Self::bind_str(params, value.to_string()))
        };
        let vector = |params: &mut Vec<String>, v: &[f64]| {
            let literal = format!(
                "[{}]",
                v.iter().map(f64::to_string).collect::<Vec<_>>().join(",")
            );
            format!("${}::text::vector", Self::bind_str(params, literal))
        };
        // Distance operator and similarity (higher is closer) of a pgvector opclass
        let vector_ops = |opclass: &str, c: &str, v: &str| match opclass {
            "vector_l2_ops" => ("<->", format!("(1.0 / (1.0 + ({c} <-> {v})))")),
            "vector_ip_ops" => ("<#>", format!("(-({c} <#> {v}))")),
            _ => ("<=>", format!("(1.0 - ({c} <=> {v}))")),
        };
        // Values computed for the hits only, as extra columns of the ranked rows
        let mut extra: Vec<String> = Vec::new();

        // (CTE, FROM item, rank expression, rank direction, score expression)
        let (cte, from, rank, direction, score) = match search {
            SearchOperation::FullText {
                document,
                config,
                source,
                query,
            } => {
                // The document and its text are over the table's own columns, the only ones
                // in scope where they are evaluated
                let q = text(params, query);
                let tsquery = match config {
                    Some(config) => format!("websearch_to_tsquery({config}, {q})"),
                    None => format!("websearch_to_tsquery({q})"),
                };
                where_clauses.push(format!("({document}) @@ {tsquery}"));
                if let (Some(h), Some(source)) = (highlight, source) {
                    let mut options = vec![
                        format!("StartSel={}", h.start_sel),
                        format!("StopSel={}", h.stop_sel),
                    ];
                    options.extend(h.max_words.map(|n| format!("MaxWords={n}")));
                    options.extend(h.min_words.map(|n| format!("MinWords={n}")));
                    options.extend(h.max_fragments.map(|n| format!("MaxFragments={n}")));
                    let options = Self::bind_str(params, options.join(", "));
                    let config = config
                        .as_ref()
                        .map(|c| format!("{c}, "))
                        .unwrap_or_default();
                    extra.push(format!("ts_headline({config}{source}, {tsquery}, ${options}::text) AS \"__headline\""));
                }
                let rank = format!("ts_rank({document}, {tsquery})");
                (
                    String::new(),
                    format!("{target} {a}"),
                    rank.clone(),
                    "DESC",
                    format!("{rank}::float8"),
                )
            }
            SearchOperation::Nearest {
                column,
                geography,
                near,
                max_distance,
            } => {
                let c = col(a, column);
                let cast = if *geography { "::geography" } else { "" };
                let g = format!(
                    "ST_GeomFromGeoJSON(${}::text){cast}",
                    Self::bind_str(params, near.clone())
                );
                if let Some(max) = max_distance {
                    let max = Self::bind(params, &serde_json::json!(max));
                    where_clauses.push(format!("ST_DWithin({c}, {g}, ${max}::text::float8)"));
                }
                extra.push(format!("ST_Distance({c}, {g})::float8 AS \"__distance\""));
                (
                    String::new(),
                    format!("{target} {a}"),
                    format!("{c} <-> {g}"),
                    "ASC",
                    format!("(1.0 / (1.0 + ST_Distance({c}, {g})))::float8"),
                )
            }
            SearchOperation::Fuzzy {
                column,
                query,
                min_similarity,
            } => {
                // Word similarity: how well the query matches some part of the value, so a
                // partly typed "Ada Lov" matches "Ada Lovelace". `%>` (a trigram index finds the
                // matches) and `<->>` (1 - word similarity: a GiST index returns the nearest
                // first) take the column on the left, as index operators must.
                let c = col(a, column);
                let q = text(params, query);
                where_clauses.push(format!("{c} %> {q}"));
                if let Some(min) = min_similarity {
                    let min = Self::bind(params, &serde_json::json!(min));
                    where_clauses.push(format!("word_similarity({q}, {c}) >= ${min}::text::real"));
                }
                (
                    String::new(),
                    format!("{target} {a}"),
                    format!("{c} <->> {q}"),
                    "ASC",
                    format!("word_similarity({q}, {c})::numeric::float8"),
                )
            }
            SearchOperation::Semantic {
                column,
                vector: v,
                min_score,
                opclass,
            } => {
                let c = col(a, column);
                let v = vector(params, v);
                let (op, similarity) = vector_ops(opclass, &c, &v);
                if let Some(min) = min_score {
                    let min = Self::bind(params, &serde_json::json!(min));
                    where_clauses.push(format!("{similarity} >= ${min}::text::float8"));
                }
                (
                    String::new(),
                    format!("{target} {a}"),
                    format!("{c} {op} {v}"),
                    "ASC",
                    format!("{similarity}::float8"),
                )
            }
            SearchOperation::Bm25 { column, query } => {
                let c = col(a, if column.is_empty() { "id" } else { column });
                let q = text(params, query);
                where_clauses.push(format!("{c} @@@ paradedb.parse({q})"));
                (
                    String::new(),
                    format!("{target} {a}"),
                    format!("paradedb.score({c})"),
                    "DESC",
                    format!("paradedb.score({c})::float8"),
                )
            }
            SearchOperation::Hybrid {
                key_column,
                vector_column,
                vector_opclass,
                lexical_column,
                is_bm25,
                query,
                vector: v,
                lexical_weight,
                vector_weight,
                rrf_k,
            } => {
                // Each ranking takes its best candidates among the rows passing the conditions
                // and filter; the fused ranking keeps the best of their union
                let key = col(a, key_column);
                let v = vector(params, v);
                let q = text(params, query);
                let vector_w = Self::bind(params, &serde_json::json!(vector_weight));
                let lexical_w = Self::bind(params, &serde_json::json!(lexical_weight));
                let (op, _) = vector_ops(vector_opclass, "", "");
                let vector_rank = format!("{} {op} {v}", col(a, vector_column));
                let lexical_col = col(a, lexical_column);
                let (lexical_match, lexical_rank) = if *is_bm25 {
                    (
                        format!("{key} @@@ paradedb.parse({q})"),
                        format!("paradedb.score({key}) DESC"),
                    )
                } else {
                    (
                        format!("{lexical_col} %> {q}"),
                        format!("{lexical_col} <->> {q}"),
                    )
                };
                let vector_where = Self::where_sql(where_clauses);
                let lexical_where = Self::where_sql(
                    &where_clauses
                        .iter()
                        .cloned()
                        .chain([lexical_match])
                        .collect::<Vec<_>>(),
                );
                where_clauses.clear();
                let candidates = Self::bind_count(params, page_limit.max(50));
                let rrf_k = Self::bind_count(params, *rrf_k);
                let cte = format!(
                    "WITH _vector_matches AS (\
                        SELECT {key} AS _key, row_number() OVER (ORDER BY {vector_rank}) AS rank \
                        FROM {target} {a}{vector_where} ORDER BY {vector_rank} LIMIT {candidates}\
                    ), \
                    _lexical_matches AS (\
                        SELECT {key} AS _key, row_number() OVER (ORDER BY {lexical_rank}) AS rank \
                        FROM {target} {a}{lexical_where} ORDER BY {lexical_rank} LIMIT {candidates}\
                    ), \
                    _hybrid_scored AS (\
                        SELECT coalesce(v._key, l._key) AS _key, \
                            coalesce(1.0 / ({rrf_k} + v.rank), 0.0) * ${vector_w}::text::float8 + \
                            coalesce(1.0 / ({rrf_k} + l.rank), 0.0) * ${lexical_w}::text::float8 AS rrf_score \
                        FROM _vector_matches v FULL OUTER JOIN _lexical_matches l ON v._key = l._key\
                    ) "
                );
                let from = format!("_hybrid_scored _hs JOIN {target} {a} ON {key} = _hs._key");
                (
                    cte,
                    from,
                    "_hs.rrf_score".to_string(),
                    "DESC",
                    "_hs.rrf_score::float8".to_string(),
                )
            }
        };
        Ranking {
            cte,
            from,
            rank,
            direction,
            score,
            extra,
        }
    }

    /// `globalSearch`: each table's best `limit + 1` matches (by its own index), merged by
    /// score, best first; the extra row answers `hasMore`.
    fn compile_global_search(
        branches: &[GlobalSearchBranch],
        limit: usize,
        include_hits: bool,
        include_score: bool,
        include_has_more: bool,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        let limit_sql = Self::bind_count(params, limit);
        let mut parts = Vec::new();
        for b in branches {
            let a = &b.table_alias;
            let target = quote_qualified(&b.schema, &b.table);
            let mut where_clauses = Vec::new();
            let Ranking {
                from,
                rank,
                direction,
                score,
                ..
            } = Self::ranking(
                &target,
                a,
                &b.search,
                &mut where_clauses,
                None,
                limit + 1,
                params,
            );
            let mut scope = JoinScope::default();
            let node = Self::row_object(
                a,
                &b.node_fields,
                b.node_polymorphism.as_ref(),
                "Node",
                &mut scope,
                plan,
                params,
            );
            parts.push(format!(
                "(SELECT {a}.__score AS score, {node} AS node FROM (\
                    SELECT {a}.*, {score} AS __score FROM {from}{} ORDER BY {rank} {direction} LIMIT {limit_sql} + 1\
                ) {a}{})",
                Self::where_sql(&where_clauses),
                scope.sql
            ));
        }
        let mut hit = vec![Self::json_pair("node", "node")];
        if include_score {
            hit.insert(0, Self::json_pair("score", "score"));
        }
        let mut pairs = Vec::new();
        if include_hits {
            let hits = Self::json_array_agg(
                &Self::json_object(&hit),
                " ORDER BY rn",
                &format!("rn <= {limit_sql}"),
            );
            pairs.push(Self::json_pair("hits", &hits));
        }
        if include_has_more {
            pairs.push(Self::json_pair(
                "hasMore",
                &format!("(count(*) > {limit_sql})"),
            ));
        }
        if pairs.is_empty() {
            pairs.push(Self::json_pair("__typename", "'GlobalSearchResults'"));
        }
        format!(
            "(SELECT {} FROM (SELECT score, node, row_number() OVER (ORDER BY score DESC) AS rn FROM ({}) _u ORDER BY score DESC LIMIT {limit_sql} + 1) _g)",
            Self::json_object(&pairs),
            parts.join(" UNION ALL ")
        )
    }

    // ---- Polymorphism ------------------------------------------------------------------

    /// JSON object for one concrete type of a union, read from `set.table_alias`.
    /// A type with no selected fields (only other types were spread) is `{}`.
    fn typed_object(
        set: &TypedFieldSet,
        scope: &mut JoinScope,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        if set.polymorphism.is_none() && set.fields.is_empty() {
            return "'{}'::json".to_string();
        }
        let poly = set.polymorphism.as_deref();
        Self::row_object(
            &set.table_alias,
            &set.fields,
            poly,
            &set.type_name,
            scope,
            plan,
            params,
        )
    }

    /// JSON of a polymorphic row read from `table_alias`, resolved to its concrete type as a
    /// single `CASE` expression:
    ///
    /// - STI: `CASE t0."type"::text WHEN 'dog' THEN JSON_OBJECT(...) ... ELSE <base> END`
    /// - CTI: `CASE WHEN _p1."id" IS NOT NULL THEN JSON_OBJECT(...) ... ELSE <parent> END`,
    ///   with every child table LEFT JOINed on its key.
    ///
    /// The joins it needs (child tables, then the relations of every branch, each once) are
    /// added to `scope`, which belongs to the FROM item defining `table_alias`.
    fn polymorphic_object(
        table_alias: &str,
        poly: &RowPolymorphism,
        scope: &mut JoinScope,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        let mut whens = Vec::new();
        let (subject, fallback) = match poly {
            RowPolymorphism::SingleTableInheritance {
                discriminator_column,
                variants,
                fallback,
            } => {
                for branch in variants {
                    let obj = Self::typed_object(&branch.set, scope, plan, params);
                    whens.push(format!(
                        "WHEN {} THEN {obj}",
                        Self::sql_literal(&branch.discriminator_value)
                    ));
                }
                (
                    format!(" {}::text", col(table_alias, discriminator_column)),
                    fallback,
                )
            }
            RowPolymorphism::ConcreteTableInheritance { children, fallback } => {
                for child in children {
                    let alias = &child.set.table_alias;
                    let on = child
                        .join_columns
                        .iter()
                        .map(|(c, p)| format!("{} = {}", col(alias, c), col(table_alias, p)))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    scope.join(quote_qualified(&child.schema, &child.table), alias, on);
                    let obj = Self::typed_object(&child.set, scope, plan, params);
                    let present = child
                        .join_columns
                        .first()
                        .map(|(c, _)| c.as_str())
                        .unwrap_or("id");
                    whens.push(format!(
                        "WHEN {} IS NOT NULL THEN {obj}",
                        col(alias, present)
                    ));
                }
                (String::new(), fallback)
            }
        };
        let fallback_obj = match fallback {
            Some(set) => Self::typed_object(set, scope, plan, params),
            None => "NULL::json".to_string(),
        };
        if whens.is_empty() {
            fallback_obj
        } else {
            format!("CASE{subject} {} ELSE {fallback_obj} END", whens.join(" "))
        }
    }

    /// Exclusive-arc union value: the target row of whichever arc column is set.
    /// Each branch is a self-contained correlated subquery projecting the fields requested
    /// for that type via inline fragments.
    fn exclusive_arc_expr(
        table_alias: &str,
        arc: &ExclusiveArcMetadata,
        type_fields: &[TypedFieldSet],
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        let mut whens = Vec::new();
        for (i, (arc_col, schema, table, key)) in arc.branches("public").enumerate() {
            let type_name = arc.target_type_name(i);
            let (sub_alias, obj, laterals) =
                match type_fields.iter().find(|t| t.type_name == type_name) {
                    Some(set) => {
                        // The branch is its own subquery, so it gets its own join scope
                        let mut scope = JoinScope::default();
                        let obj = Self::typed_object(set, &mut scope, plan, params);
                        (set.table_alias.clone(), obj, scope.sql)
                    }
                    // Plans without per-type selections: typename + key only
                    None if type_fields.is_empty() => {
                        let alias = format!("sub_arc_{i}");
                        let obj = format!(
                            "JSON_OBJECT('__typename': {}, 'id': {} RETURNING json)",
                            Self::sql_literal(&type_name),
                            col(&alias, key)
                        );
                        (alias, obj, String::new())
                    }
                    None => continue,
                };
            whens.push(format!(
                "WHEN {} IS NOT NULL THEN (SELECT {obj} FROM {} {sub_alias}{laterals} WHERE {} = {})",
                col(table_alias, arc_col),
                quote_qualified(schema, table),
                col(&sub_alias, key),
                col(table_alias, arc_col)
            ));
        }
        if whens.is_empty() {
            "NULL".to_string()
        } else {
            format!("CASE {} ELSE NULL END", whens.join(" "))
        }
    }

    /// SQL string literal with embedded quotes escaped.
    fn sql_literal(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    // ---- JSON constructors -------------------------------------------------------------
    //
    // SQL/JSON standard constructors (PostgreSQL 16+). They explicitly return `json`, not
    // `jsonb`: `json` keeps keys in construction order, so response fields follow the
    // GraphQL selection order. Without `RETURNING json`, PostgreSQL switches the result to
    // `jsonb` (re-sorting keys) as soon as any input value is `jsonb`.

    /// One `key: value` entry of a JSON object constructor.
    fn json_pair(key: &str, value_expr: &str) -> String {
        format!("{}: {}", Self::sql_literal(key), value_expr)
    }

    /// `JSON_OBJECT(...)`. Uses the default `NULL ON NULL`, so null fields stay present
    /// as `null` as GraphQL requires.
    fn json_object(pairs: &[String]) -> String {
        format!("JSON_OBJECT({} RETURNING json)", pairs.join(", "))
    }

    /// Aggregates `expr` into a JSON array (`[]` when there are no rows).
    /// `order_sql` is an optional ` ORDER BY ...` suffix, `filter` an optional FILTER predicate.
    fn json_array_agg(expr: &str, order_sql: &str, filter: &str) -> String {
        let filter_sql = if filter.is_empty() {
            String::new()
        } else {
            format!(" FILTER (WHERE {})", filter)
        };
        format!("coalesce(JSON_ARRAYAGG({expr}{order_sql} NULL ON NULL RETURNING json){filter_sql}, '[]'::json)")
    }

    // ---- Predicates & parameters -------------------------------------------------------

    /// Binds a JSON value as a text parameter and returns its 1-based placeholder index.
    fn bind(params: &mut Vec<String>, value: &serde_json::Value) -> usize {
        Self::bind_str(params, Self::param_text(value))
    }

    /// Text form of a column value as bound parameter, formatting JSON/JSONB values as valid JSON.
    fn cv_text(cv: &ColumnValue) -> Option<String> {
        if cv.value.is_null() {
            return None;
        }
        let text = if matches!(cv.pg_cast.as_deref(), Some("json") | Some("jsonb")) {
            cv.value.to_string()
        } else {
            Self::param_text(&cv.value)
        };
        Some(text)
    }

    /// Text form a JSON value is bound as.
    fn param_text(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    /// Binds a column assignment, cast to the column's type: `$1::text::jsonb`, or
    /// `json_populate_record(NULL::"s"."t", $1::text::json)` for a composite type.
    fn bind_value(params: &mut Vec<String>, cv: &ColumnValue) -> String {
        // Parameters are bound as text, so an explicit GraphQL `null` becomes a SQL NULL literal
        let Some(text) = Self::cv_text(cv) else {
            return "NULL".to_string();
        };
        let idx = Self::bind_str(params, text);
        Self::assign_expr(&format!("${idx}::text"), cv, false)
    }

    /// The value assigned to `cv`'s column from the text expression `text`, converted to the
    /// column's type. `nullable`: `text` may be NULL (composites are built from JSON, which
    /// would turn NULL into a row or array of NULLs).
    fn assign_expr(text: &str, cv: &ColumnValue, nullable: bool) -> String {
        let expr = match (&cv.pg_cast, cv.composite, cv.composite_array) {
            (Some(ty), _, true) => {
                let elem = ty.trim_end_matches("[]");
                format!(
                    "ARRAY(SELECT CASE WHEN json_typeof(_v) = 'null' THEN NULL ELSE json_populate_record(NULL::{elem}, _v) END FROM json_array_elements({text}::json) WITH ORDINALITY AS _a(_v, _o) ORDER BY _o)"
                )
            }
            (Some(ty), true, false) => format!("json_populate_record(NULL::{ty}, {text}::json)"),
            // Geometries are written from GeoJSON
            (Some(ty), false, false) if ty == "geometry" => format!("ST_GeomFromGeoJSON({text})"),
            (Some(ty), false, false) if ty == "geography" => {
                format!("ST_GeomFromGeoJSON({text})::geography")
            }
            (Some(ty), false, false) => format!("{text}::{ty}"),
            (None, _, _) => text.to_string(),
        };
        if nullable && (cv.composite || cv.composite_array) {
            format!("CASE WHEN {text} IS NULL THEN NULL ELSE {expr} END")
        } else {
            expr
        }
    }

    /// Binds a row count (`LIMIT`, `OFFSET`, page sizes): page sizes then share one
    /// prepared statement instead of each producing its own SQL.
    fn bind_count(params: &mut Vec<String>, count: usize) -> String {
        format!("${}::text::int8", Self::bind_str(params, count.to_string()))
    }

    fn bind_str(params: &mut Vec<String>, value: String) -> usize {
        params.push(value);
        params.len()
    }

    /// Compares a column against a text-bound parameter. With a known cast the parameter is
    /// converted to the column's native type (`col = $1::text::uuid`) so the column stays
    /// bare and its indexes are usable; otherwise both sides are compared as text.
    fn compare(column_expr: &str, operator: &str, param_idx: usize, cast: Option<&str>) -> String {
        match cast {
            Some(ty) => format!("{} {} ${}::text::{}", column_expr, operator, param_idx, ty),
            None => format!("{}::text {} ${}", column_expr, operator, param_idx),
        }
    }

    fn where_clause(table_alias: &str, cond: &WhereCondition, params: &mut Vec<String>) -> String {
        let idx = Self::bind(params, &cond.value);
        Self::compare(
            &col(table_alias, &cond.column),
            &cond.operator,
            idx,
            cond.pg_cast.as_deref(),
        )
    }

    /// FK join to the parent row. Both sides share a type, so no casts — keeps the
    /// child's FK index usable for the per-parent lateral lookup.
    fn join_clause(table_alias: &str, join: &JoinCondition) -> String {
        let pairs: Vec<String> = match &join.junction {
            Some(junc) => junc
                .source
                .iter()
                .map(|(jc, pc)| format!("{} = {}", col("_junc", jc), col(&join.parent_alias, pc)))
                .collect(),
            None => join
                .columns
                .iter()
                .map(|(cc, pc)| {
                    format!("{} = {}", col(table_alias, cc), col(&join.parent_alias, pc))
                })
                .collect(),
        };
        let mut pairs = pairs;
        if let Some((cc, pc)) = &join.period {
            pairs.push(format!(
                "{} && {}",
                col(table_alias, cc),
                col(&join.parent_alias, pc)
            ));
        }
        pairs.join(" AND ")
    }

    fn where_sql(clauses: &[String]) -> String {
        if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        }
    }

    /// `"schema"."func"(args)`. Arguments taken from the parent row read `parent_alias`;
    /// values are cast to the argument types, which also picks the overload.
    fn function_call_sql(
        schema: &str,
        func_name: &str,
        parent_alias: Option<&str>,
        arguments: &[BoundArg],
        params: &mut Vec<String>,
    ) -> String {
        let args: Vec<String> = arguments
            .iter()
            .map(|arg| match &arg.value {
                ArgValue::Column(column) => col(parent_alias.unwrap_or("NULL"), column),
                ArgValue::Value(cv) if cv.value.is_null() => {
                    format!("NULL::{}", cv.pg_cast.as_deref().unwrap_or("text"))
                }
                ArgValue::Value(cv) => Self::bind_value(params, cv),
                ArgValue::Row {
                    alias,
                    schema,
                    table,
                    columns,
                } => {
                    let values: Vec<String> = columns.iter().map(|c| col(alias, c)).collect();
                    format!(
                        "ROW({})::{}",
                        values.join(", "),
                        quote_qualified(schema, table)
                    )
                }
            })
            .collect();
        format!(
            "{}({})",
            quote_qualified(schema, func_name),
            args.join(", ")
        )
    }

    /// The rows of a call returning them as an array.
    fn unnested(call: String, unnest: bool) -> String {
        if unnest {
            format!("unnest({call})")
        } else {
            call
        }
    }

    /// `(SELECT <json array of rows> FROM "schema"."func"(args) f)`: a mounted function's rows.
    fn compile_function_call(
        schema: &str,
        func_name: &str,
        parent_alias: Option<&str>,
        arguments: &[BoundArg],
        fields: &[SelectedField],
        params: &mut Vec<String>,
    ) -> String {
        let call = Self::function_call_sql(schema, func_name, parent_alias, arguments, params);
        format!(
            "(SELECT COALESCE(JSON_ARRAYAGG({} RETURNING json), '[]'::json) FROM {call} f)",
            Self::function_row_object(fields)
        )
    }

    /// The rows of a connection over a function, aliased `table_alias`. With `ordinality`
    /// they are numbered in the order the function returns them (`__ord`); without, the
    /// call stays a plain `FROM` item, which PostgreSQL can inline (a simple `LANGUAGE sql`
    /// function becomes part of the query, and its tables' indexes serve the filters).
    fn function_from(
        source: &FunctionSource,
        table_alias: &str,
        params: &mut Vec<String>,
    ) -> String {
        let call = Self::unnested(
            Self::function_call_sql(
                &source.schema,
                &source.func_name,
                None,
                &source.arguments,
                params,
            ),
            source.unnest,
        );
        if source.ordinality {
            format!("(SELECT _r.*, row_number() OVER () AS \"__ord\" FROM {call} _r) {table_alias}")
        } else {
            format!("{call} {table_alias}")
        }
    }

    /// JSON of a function root field's result (`call` is the function call).
    fn compile_function_result(
        call: &str,
        returns_set: bool,
        result: &FunctionResult,
        plan: &OperationPlan,
        params: &mut Vec<String>,
    ) -> String {
        match result {
            // A set-returning call in the select list keeps its values' order
            FunctionResult::Value(output) => {
                let value = Self::value_output("_f.v", output, 0);
                if returns_set {
                    format!(
                        "(SELECT {} FROM (SELECT {call} AS v) _f)",
                        Self::json_array_agg(&value, "", "")
                    )
                } else {
                    format!("(SELECT {value} FROM (SELECT {call} AS v) _f)")
                }
            }
            FunctionResult::Void => format!("(SELECT true FROM (SELECT {call}) _f)"),
            FunctionResult::Record(fields) => {
                let obj = Self::function_row_object(fields);
                if returns_set {
                    format!(
                        "(SELECT {} FROM {call} f)",
                        Self::json_array_agg(&obj, "", "")
                    )
                } else {
                    format!("(SELECT {obj} FROM {call} f LIMIT 1)")
                }
            }
            FunctionResult::Rows {
                table_alias,
                fields,
                polymorphism,
                key_columns,
                ..
            } => {
                let mut scope = JoinScope::default();
                let obj = Self::row_object(
                    table_alias,
                    fields,
                    polymorphism.as_ref(),
                    "Node",
                    &mut scope,
                    plan,
                    params,
                );
                let from = format!("{call} {table_alias}{}", scope.sql);
                if returns_set {
                    format!(
                        "(SELECT {} FROM {from})",
                        Self::json_array_agg(&obj, "", "")
                    )
                } else {
                    // A function returning no row yields one of NULLs: it has no key
                    let present = match key_columns.first() {
                        Some((c, _)) => format!(" WHERE {} IS NOT NULL", col(table_alias, c)),
                        None => String::new(),
                    };
                    format!("(SELECT {obj} FROM {from}{present} LIMIT 1)")
                }
            }
        }
    }

    /// A `VOLATILE` function as a mutation field. Rows of a table with a key are read back
    /// by key in a statement of their own, so their relations see what the function wrote;
    /// other results are read from the call itself.
    fn compile_function_mutation(
        root_alias: &str,
        step: &Step,
        plan: &OperationPlan,
    ) -> CompiledMutation {
        let Step::PgFunction {
            schema,
            func_name,
            arguments,
            returns_set,
            unnest,
            result,
            ..
        } = step
        else {
            unreachable!("compile_function_mutation called on another step");
        };
        let mutation = |writes, payload| CompiledMutation {
            field_alias: root_alias.to_string(),
            writes,
            payload,
            count_param: None,
            node_paths: Vec::new(),
        };
        let write = |sql: String, params, returning: Vec<String>| CompiledWrite {
            statement: CompiledStatement::new(CompiledQuery::new(sql, params)),
            collect: !returning.is_empty(),
            returning,
            row_nodes: Vec::new(),
            expect_rows: None,
            counts: false,
            guard: false,
        };
        let mut params = Vec::new();
        let call = Self::unnested(
            Self::function_call_sql(schema, func_name, None, arguments, &mut params),
            *unnest,
        );
        match result {
            FunctionResult::Void => {
                let payload =
                    CompiledQuery::new("SELECT 'true'::json AS data".to_string(), Vec::new());
                mutation(
                    vec![write(format!("SELECT {call}"), params, Vec::new())],
                    CompiledStatement::new(payload),
                )
            }
            FunctionResult::Rows {
                schema,
                table,
                table_alias,
                fields,
                polymorphism,
                key_columns,
            } if !key_columns.is_empty() => {
                let keys: Vec<String> = key_columns
                    .iter()
                    .map(|(c, _)| format!("{}::text", col("_r", c)))
                    .collect();
                let returning = key_columns.iter().map(|(c, _)| c.clone()).collect();
                let written = write(
                    format!("SELECT {} FROM {call} _r", keys.join(", ")),
                    params,
                    returning,
                );

                let mut params = vec![String::new(); key_columns.len()];
                let deferred = (0..key_columns.len())
                    .map(|column| (column, ParamSource::Returned { column }))
                    .collect();
                let source =
                    Self::located_source(&quote_qualified(schema, table), table_alias, key_columns);
                let mut scope = JoinScope::default();
                let obj = Self::row_object(
                    table_alias,
                    fields,
                    polymorphism.as_ref(),
                    "Payload",
                    &mut scope,
                    plan,
                    &mut params,
                );
                let from = format!("{source}{}", scope.sql);
                let data = if *returns_set {
                    format!(
                        "(SELECT {} FROM {from})",
                        Self::json_array_agg(&obj, " ORDER BY _k._ord", "")
                    )
                } else {
                    format!("(SELECT {obj} FROM {from} ORDER BY _k._ord LIMIT 1)")
                };
                let query = CompiledQuery::new(format!("SELECT {data} AS data"), params);
                mutation(
                    vec![written],
                    CompiledStatement {
                        query,
                        deferred,
                        strict: false,
                        context: None,
                    },
                )
            }
            _ => {
                // A value is JSON only inside the response object: here it stands alone
                let data =
                    Self::compile_function_result(&call, *returns_set, result, plan, &mut params);
                mutation(
                    Vec::new(),
                    CompiledStatement::new(CompiledQuery::new(
                        format!("SELECT to_json({data}) AS data"),
                        params,
                    )),
                )
            }
        }
    }

    /// JSON object for one row `f` of a set-returning function: its selected columns and
    /// `__typename`. A selection with no applicable fields yields `{}`.
    fn function_row_object(fields: &[SelectedField]) -> String {
        let pairs: Vec<String> = fields
            .iter()
            .filter_map(|f| match f {
                SelectedField::Column {
                    field_alias,
                    column_name,
                    output,
                } => Some(Self::json_pair(
                    field_alias,
                    &Self::value_output(&col("f", column_name), output, 0),
                )),
                SelectedField::Typename {
                    field_alias,
                    type_name,
                } => Some(Self::json_pair(field_alias, &Self::sql_literal(type_name))),
                _ => None,
            })
            .collect();
        if pairs.is_empty() {
            "'{}'::json".to_string()
        } else {
            Self::json_object(&pairs)
        }
    }

    fn compile_filter_expr(expr: &FilterExpr, params: &mut Vec<String>) -> String {
        match expr {
            FilterExpr::Binary {
                column,
                table_alias,
                operator,
                value,
                pg_cast,
            } => {
                let idx = Self::bind(params, value);
                Self::compare(&col(table_alias, column), operator, idx, pg_cast.as_deref())
            }
            FilterExpr::Pattern {
                column,
                table_alias,
                operator,
                pattern,
            } => {
                let idx = Self::bind_str(params, pattern.clone());
                format!("{} {} ${}", col(table_alias, column), operator, idx)
            }
            FilterExpr::InList {
                column,
                table_alias,
                values,
                is_not,
                pg_cast,
            } => {
                if values.is_empty() {
                    if *is_not {
                        "true".to_string()
                    } else {
                        "false".to_string()
                    }
                } else {
                    let placeholders: Vec<String> = values
                        .iter()
                        .map(|v| {
                            let idx = Self::bind(params, v);
                            match pg_cast.as_deref() {
                                Some(cast) => format!("${}::text::{}", idx, cast),
                                None => format!("${}", idx),
                            }
                        })
                        .collect();
                    let op = if *is_not { "NOT IN" } else { "IN" };
                    format!(
                        "{} {op} ({})",
                        col(table_alias, column),
                        placeholders.join(", ")
                    )
                }
            }
            FilterExpr::IsNull {
                column,
                table_alias,
                is_null,
            } => {
                if *is_null {
                    format!("{} IS NULL", col(table_alias, column))
                } else {
                    format!("{} IS NOT NULL", col(table_alias, column))
                }
            }
            FilterExpr::And(exprs) => {
                let parts: Vec<String> = exprs
                    .iter()
                    .map(|e| Self::compile_filter_expr(e, params))
                    .filter(|s| !s.is_empty())
                    .collect();
                if parts.is_empty() {
                    String::new()
                } else {
                    format!("({})", parts.join(" AND "))
                }
            }
            FilterExpr::Or(exprs) => {
                let parts: Vec<String> = exprs
                    .iter()
                    .map(|e| Self::compile_filter_expr(e, params))
                    .filter(|s| !s.is_empty())
                    .collect();
                if parts.is_empty() {
                    String::new()
                } else {
                    format!("({})", parts.join(" OR "))
                }
            }
            FilterExpr::Not(inner) => {
                let inner_sql = Self::compile_filter_expr(inner, params);
                if inner_sql.is_empty() {
                    String::new()
                } else {
                    format!("NOT ({})", inner_sql)
                }
            }
            FilterExpr::Exists { related, filter } => {
                let (from, mut clauses) = Self::related_rows(related);
                if let Some(f) = filter {
                    let f_sql = Self::compile_filter_expr(f, params);
                    if !f_sql.is_empty() {
                        clauses.push(f_sql);
                    }
                }
                format!(
                    "EXISTS (SELECT 1 FROM {from} WHERE {})",
                    clauses.join(" AND ")
                )
            }
            FilterExpr::Template {
                column,
                table_alias,
                sql,
                values,
            } => {
                let mut out = sql.replace("{c}", &col(table_alias, column));
                for (i, value) in values.iter().enumerate() {
                    let idx = Self::bind(params, value);
                    out = out.replace(&format!("{{{i}}}"), &format!("${idx}::text"));
                }
                out
            }
            FilterExpr::NotTrue(inner) => {
                let inner_sql = Self::compile_filter_expr(inner, params);
                if inner_sql.is_empty() {
                    "false".to_string()
                } else {
                    format!("({inner_sql}) IS NOT TRUE")
                }
            }
            FilterExpr::RelatedAggregate {
                related,
                function,
                column,
                operator,
                values,
            } => {
                let (from, clauses) = Self::related_rows(related);
                let arg = match column {
                    Some(c) => col(&related.alias, c),
                    None => "*".to_string(),
                };
                // count(*) of no rows is 0; sums and averages of no rows are NULL
                let expr = format!(
                    "(SELECT {function}({arg}) FROM {from} WHERE {})::numeric",
                    clauses.join(" AND ")
                );
                let mut value =
                    |v: &serde_json::Value| format!("${}::text::numeric", Self::bind(params, v));
                match operator.as_str() {
                    "IS NULL" | "IS NOT NULL" => format!("{expr} {operator}"),
                    "IN" | "NOT IN" if values.is_empty() => (operator == "NOT IN").to_string(),
                    "IN" | "NOT IN" => {
                        let values: Vec<String> = values.iter().map(&mut value).collect();
                        format!("{expr} {operator} ({})", values.join(", "))
                    }
                    op => format!("{expr} {op} {}", value(&values[0])),
                }
            }
        }
    }

    /// What rows read as `alias` are sorted by for `key`: a column, or an expression of the
    /// row (a related row's column, a count of related rows, a computed column).
    pub fn sort_expression(alias: &str, key: &SortKey) -> String {
        match &key.expr {
            None => col(alias, &key.column),
            Some(SortExpr::Related {
                schema,
                table,
                columns,
                period,
                column,
            }) => {
                let mut on: Vec<String> = columns
                    .iter()
                    .map(|(t, c)| format!("{} = {}", col("_so", t), col(alias, c)))
                    .collect();
                let mut order = String::new();
                if let Some((t, c)) = period {
                    on.push(format!("{} && {}", col("_so", t), col(alias, c)));
                    order = format!(" ORDER BY {} DESC", col("_so", t));
                }
                format!(
                    "(SELECT {} FROM {} _so WHERE {}{order} LIMIT 1)",
                    col("_so", column),
                    quote_qualified(schema, table),
                    on.join(" AND ")
                )
            }
            Some(SortExpr::Count { related }) => {
                let related = RelatedRows {
                    parent_alias: alias.to_string(),
                    alias: "_so".to_string(),
                    ..related.clone()
                };
                let (from, clauses) = Self::related_rows(&related);
                format!(
                    "(SELECT count(*) FROM {from} WHERE {})",
                    clauses.join(" AND ")
                )
            }
            Some(SortExpr::Function {
                schema,
                name,
                row_schema,
                row_table,
                columns,
            }) => {
                let values: Vec<String> = columns.iter().map(|c| col(alias, c)).collect();
                format!(
                    "{}(ROW({})::{})",
                    quote_qualified(schema, name),
                    values.join(", "),
                    quote_qualified(row_schema, row_table)
                )
            }
        }
    }

    /// `FROM` list and join conditions of the rows related to a parent row, for a correlated
    /// subquery.
    fn related_rows(related: &RelatedRows) -> (String, Vec<String>) {
        let alias = &related.alias;
        let mut from = format!(
            "{} {alias}",
            quote_qualified(&related.schema, &related.table)
        );
        let mut clauses: Vec<String> = Vec::new();
        match &related.junction {
            Some(j) => {
                let junction = format!("{alias}_j");
                let on: Vec<String> = j
                    .target
                    .iter()
                    .map(|(jc, tc)| format!("{} = {}", col(&junction, jc), col(alias, tc)))
                    .collect();
                from.push_str(&format!(
                    " JOIN {} {junction} ON {}",
                    quote_qualified(&j.junction_schema, &j.junction_table),
                    on.join(" AND ")
                ));
                clauses.extend(j.source.iter().map(|(jc, pc)| {
                    format!(
                        "{} = {}",
                        col(&junction, jc),
                        col(&related.parent_alias, pc)
                    )
                }));
            }
            None => clauses.extend(related.columns.iter().map(|(rc, pc)| {
                format!("{} = {}", col(alias, rc), col(&related.parent_alias, pc))
            })),
        }
        if let Some((rc, pc)) = &related.period {
            clauses.push(format!(
                "{} && {}",
                col(alias, rc),
                col(&related.parent_alias, pc)
            ));
        }
        if clauses.is_empty() {
            clauses.push("true".to_string());
        }
        (from, clauses)
    }

    /// Condition selecting the rows after (`after`) or before a cursor in a keyset order,
    /// where NULLs sort as the largest value. It is one row comparison when that is exact,
    /// `ROW(a, b) > ROW($1, $2)`: an index seek. Mixed directions and NULLs need the
    /// expansion, `a > $1 OR (a = $1 AND b > $2)`, with each column's direction and NULL
    /// spelled out.
    fn keyset_condition(
        exprs: &[String],
        keys: &[SortKey],
        values: &[serde_json::Value],
        after: bool,
        params: &mut Vec<String>,
    ) -> String {
        if values.len() != keys.len() {
            return "FALSE".to_string();
        }
        let bound: Vec<Option<String>> = keys
            .iter()
            .zip(values)
            .map(|(k, v)| {
                (!v.is_null()).then(|| {
                    let idx = Self::bind(params, v);
                    match &k.compare {
                        SortCompare::Cast(ty) => format!("${idx}::text::{ty}"),
                        SortCompare::Text => format!("${idx}"),
                    }
                })
            })
            .collect();
        // Whether the rows sought have larger values than the cursor in column `k`
        let greater = |k: &SortKey| after != k.descending;
        let op = |k: &SortKey| if greater(k) { ">" } else { "<" };

        // A row comparison is NULL (no match) at the first NULL pair, which is exact only
        // when no value is NULL, or when rows are sought below the cursor (NULLs are above)
        let uniform = keys.iter().all(|k| greater(k) == greater(&keys[0]));
        let has_nulls = keys.iter().any(|k| k.nullable) && greater(&keys[0]);
        if uniform && !has_nulls && bound.iter().all(Option::is_some) {
            let values: Vec<&str> = bound.iter().flatten().map(String::as_str).collect();
            return match (exprs, values.as_slice()) {
                ([e], [v]) => format!("{e} {} {v}", op(&keys[0])),
                _ => format!(
                    "ROW({}) {} ROW({})",
                    exprs.join(", "),
                    op(&keys[0]),
                    values.join(", ")
                ),
            };
        }

        let mut terms = Vec::new();
        let mut equal: Vec<String> = Vec::new();
        for ((k, e), v) in keys.iter().zip(exprs).zip(&bound) {
            let beyond = match (greater(k), v) {
                (true, Some(v)) if k.nullable => Some(format!("({e} > {v} OR {e} IS NULL)")),
                (true, Some(v)) => Some(format!("{e} > {v}")),
                // Nothing sorts above NULL
                (true, None) => None,
                (false, Some(v)) => Some(format!("{e} < {v}")),
                (false, None) => Some(format!("{e} IS NOT NULL")),
            };
            if let Some(beyond) = beyond {
                let parts: Vec<&str> = equal
                    .iter()
                    .map(String::as_str)
                    .chain([beyond.as_str()])
                    .collect();
                terms.push(if parts.len() == 1 {
                    beyond.clone()
                } else {
                    format!("({})", parts.join(" AND "))
                });
            }
            equal.push(match v {
                Some(v) => format!("{e} = {v}"),
                None => format!("{e} IS NULL"),
            });
        }
        if terms.is_empty() {
            return "FALSE".to_string();
        }
        let expansion = format!("({})", terms.join(" OR "));
        // A bound on the leading column keeps its index usable
        match (&keys[0], &bound[0]) {
            (k, Some(v)) if !k.nullable => format!("{} {}= {v} AND {expansion}", exprs[0], op(k)),
            _ => expansion,
        }
    }

    /// Decode a composite-key cursor (base64 of a JSON array of key values).
    pub fn decode_composite_cursor(cursor_str: &str) -> Vec<serde_json::Value> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(cursor_str)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }

    /// Decode base64 opaque cursor to its underlying record identifier.
    pub fn decode_cursor(cursor_str: &str) -> String {
        use base64::Engine;
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(cursor_str) {
            if let Ok(text) = String::from_utf8(bytes) {
                // Check if it's Crystal's JSON array cursor: ["f0b28c94c1","ca70ca70-0000-0000-0000-a11ce000ca70"]
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(arr) = val.as_array() {
                        if arr.len() >= 2 {
                            if let Some(id) = arr[1].as_str() {
                                return id.to_string();
                            }
                        }
                    }
                }
                return text;
            }
        }
        cursor_str.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(
            quote_qualified("App Public", "Users"),
            "\"App Public\".\"Users\""
        );
    }

    #[test]
    fn tags_statements_with_sqlcommenter_comments() {
        let mut op = CompiledOperation::Query(CompiledQuery::new(
            "SELECT 1 AS data;".to_string(),
            Vec::new(),
        ));
        op.tag(&[
            ("graphql_operation", "Get Users*/--"),
            ("application", "graphpg"),
        ]);
        let CompiledOperation::Query(q) = op else {
            unreachable!()
        };
        assert_eq!(
            q.sql,
            "SELECT 1 AS data /*application='graphpg',graphql_operation='Get%20Users%2A%2F--'*/"
        );
    }

    #[test]
    fn escapes_like_patterns() {
        assert_eq!(escape_like("100%_off\\"), "100\\%\\_off\\\\");
        assert_eq!(escape_like("plain"), "plain");
    }
}
