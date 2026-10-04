use serde::{Deserialize, Serialize};

/// Unique identifier for an execution step within an OperationPlan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepId(pub usize);

impl std::fmt::Display for StepId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.0)
    }
}

/// Information about an intermediate junction table for Many-to-Many traversal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct JunctionJoin {
    pub junction_schema: String,
    pub junction_table: String,
    /// `(junction column, parent column)` pairs.
    pub source: Vec<(String, String)>,
    /// `(junction column, child column)` pairs.
    pub target: Vec<(String, String)>,
}

/// One column (or expression) of a keyset order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SortKey {
    /// The column, or a name unique within the order for an expression.
    pub column: String,
    pub descending: bool,
    /// The column can hold NULLs, which sort as its largest value (Postgres' default:
    /// last ascending, first descending).
    pub nullable: bool,
    pub compare: SortCompare,
    /// Sorted by this expression of the row instead of `column`.
    #[serde(default)]
    pub expr: Option<SortExpr>,
}

/// A value of a row rows are sorted by, other than one of its columns. Its values go into
/// cursors like column values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SortExpr {
    /// Column `column` of the row a to-one relation leads to: `(target column, source
    /// column)` pairs, and the range pair of a temporal foreign key (the latest version).
    Related {
        schema: String,
        table: String,
        columns: Vec<(String, String)>,
        period: Option<(String, String)>,
        column: String,
    },
    /// The number of rows of a to-many relation (`parent_alias` of `related` is the row's).
    Count { related: RelatedRows },
    /// A computed column: `schema.name(<the row as a row of row_schema.row_table>)`.
    Function {
        schema: String,
        name: String,
        row_schema: String,
        row_table: String,
        columns: Vec<String>,
    },
}

/// How a column of a keyset order is compared with a text-bound cursor value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SortCompare {
    /// The value is cast to the column's type: `col > $1::text::uuid`.
    Cast(String),
    /// Column and value are both compared (and the column sorted) as text.
    Text,
}

/// A join condition connecting a child PgSelect/PgConnection step to a parent step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JoinCondition {
    /// `(child column, parent column)` pairs, e.g. `[("forum_id", "id")]`. Empty for
    /// many-to-many joins, which go through `junction`.
    pub columns: Vec<(String, String)>,
    /// SQL alias of the parent table (e.g. "t0")
    pub parent_alias: String,
    /// Intermediate junction join for Many-to-Many relationships
    #[serde(default)]
    pub junction: Option<JunctionJoin>,
    /// The child columns are the target table's primary key, so each parent row matches at
    /// most one target row and the relation can be a plain `LEFT JOIN`.
    #[serde(default)]
    pub unique_target: bool,
    /// `(child column, parent column)` ranges that must overlap (a temporal foreign key).
    #[serde(default)]
    pub period: Option<(String, String)>,
}

impl JoinCondition {
    pub fn new(
        child_column: impl Into<String>,
        parent_column: impl Into<String>,
        parent_alias: impl Into<String>,
    ) -> Self {
        Self {
            columns: vec![(child_column.into(), parent_column.into())],
            parent_alias: parent_alias.into(),
            junction: None,
            unique_target: false,
            period: None,
        }
    }
}

/// How a stored value becomes a response value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum ValueOutput {
    /// As stored, optionally cast (`bigint` → `text`).
    #[default]
    Plain,
    Cast(String),
    /// Enum labels mapped to GraphQL enum values: `(label, value)` pairs; `list` for arrays.
    Enum {
        values: Vec<(String, String)>,
        list: bool,
    },
    /// The selected attributes of a composite value (of each element, for `list`).
    Composite {
        fields: Vec<CompositeField>,
        list: bool,
    },
    /// SQL around the value, `{v}` standing for it (see `PgType::output_sql`); `list`
    /// applies it to each element.
    Template {
        sql: String,
        list: bool,
    },
    /// The selected parts of a range value (of each element, for `list`).
    Range {
        fields: Vec<RangeField>,
        bound: Box<ValueOutput>,
        list: bool,
    },
}

/// One selected field of a range value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RangeField {
    pub field_alias: String,
    pub part: RangePart,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RangePart {
    /// The lower (`upper: false`) or upper bound: null when unbounded (or the range is
    /// empty), else its selected parts.
    Bound {
        upper: bool,
        fields: Vec<(String, BoundPart)>,
    },
    IsEmpty,
    Typename(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BoundPart {
    Value,
    Inclusive,
    Typename(String),
}

impl ValueOutput {
    pub fn cast(cast: Option<String>) -> Self {
        cast.map_or(Self::Plain, Self::Cast)
    }
}

/// One selected field of a composite value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompositeField {
    pub field_alias: String,
    pub source: CompositeSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CompositeSource {
    Attribute { name: String, output: ValueOutput },
    Typename(String),
}

/// Where a function argument's value comes from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ArgValue {
    /// A column of the parent row (mounted functions).
    Column(String),
    /// A bound value, converted to the argument's type like a column assignment.
    Value(ColumnValue),
    /// The row read from `alias`, as a value of table `schema.table` built from its
    /// `columns` (computed columns). The alias may carry other columns too, such as a
    /// search's score, so the row is built rather than passed whole.
    Row {
        alias: String,
        schema: String,
        table: String,
        columns: Vec<String>,
    },
}

/// Rows of a connection produced by a function (`RETURNS SETOF <table>`) instead of read
/// from its table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionSource {
    pub schema: String,
    pub func_name: String,
    pub arguments: Vec<BoundArg>,
    /// The function returns its rows as an array, read with `unnest`.
    #[serde(default)]
    pub unnest: bool,
    /// Number the rows in the order the function returns them, as column `__ord`.
    pub ordinality: bool,
}

/// What a function root field reads from the function's result.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FunctionResult {
    /// The value (or each value of a set).
    Value(ValueOutput),
    /// Nothing (`void`): `true` once it ran.
    Void,
    /// Its own columns, as a `<Func>Row` object.
    Record(Vec<SelectedField>),
    /// Rows of a table, read from `table_alias`.
    Rows {
        schema: String,
        table: String,
        table_alias: String,
        fields: Vec<SelectedField>,
        polymorphism: Option<RowPolymorphism>,
        /// Key `(column, sql cast)`: a mutation reads the rows back by key in a statement
        /// of its own, so their relations see what the function wrote. Empty without a key.
        key_columns: Vec<(String, Option<String>)>,
    },
}

/// One positional argument of a function call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoundArg {
    pub name: String,
    pub value: ArgValue,
}

/// A selected field in a PgSelect step. Can be a scalar column or a nested relation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SelectedField {
    Column {
        field_alias: String,
        column_name: String,
        /// Conversion of the value for the response.
        #[serde(default)]
        output: ValueOutput,
    },
    /// Relay Global ID: base64("<TypeName>:<pk_value>").
    /// The SQL compiler emits `encode(concat('<type_name>:', alias."pk_col")::bytea, 'base64')`.
    GlobalId {
        field_alias: String,
        /// GraphQL type name, e.g. "User"
        type_name: String,
        /// Physical primary-key column name in PostgreSQL, e.g. "id"
        pk_column: String,
    },
    /// Constant string typename (e.g. '__typename' -> 'User'). Emitted as SQL string literal.
    Typename {
        field_alias: String,
        type_name: String,
    },
    Relation {
        field_alias: String,
        step_id: StepId,
    },
    ExclusiveArcTarget {
        field_alias: String,
        arc: crate::catalog::ExclusiveArcMetadata,
        /// Fields requested per concrete type via GraphQL inline fragments (... on User { ... })
        #[serde(default)]
        type_fields: Vec<TypedFieldSet>,
    },
    FunctionCall {
        field_alias: String,
        func: crate::catalog::FunctionMetadata,
        arguments: Vec<BoundArg>,
        fields: Vec<SelectedField>,
    },
    /// The row's version: its `xmin`, read from the table by key (rows may come from
    /// subqueries, functions or returned rows, which have no system columns).
    RowVersion {
        field_alias: String,
        schema: String,
        table: String,
        key_columns: Vec<String>,
    },
    /// `field` read from another table alias in scope (e.g. a CTI child type's parent-owned
    /// fields read from the parent row).
    OnAlias {
        table_alias: String,
        field: Box<SelectedField>,
    },
    /// Not a response field: LEFT JOINs another row into scope for `OnAlias` fields
    /// (e.g. a CTI child's parent row). Must precede the fields that read from it.
    JoinRow {
        schema: String,
        table: String,
        table_alias: String,
        /// `(joined_column, from_column)` pairs.
        join_columns: Vec<(String, String)>,
    },
}

/// Fields planned for one concrete type of a polymorphic row (interface implementation or union member).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypedFieldSet {
    /// GraphQL concrete type name, e.g. "Dog"
    pub type_name: String,
    /// SQL alias the fields are read from (the arc target alias for exclusive arcs).
    #[serde(default)]
    pub table_alias: String,
    pub fields: Vec<SelectedField>,
    /// Set when the row itself is polymorphic (e.g. an arc target table with subtypes):
    /// the row resolves to its concrete type instead of using `fields`.
    #[serde(default)]
    pub polymorphism: Option<Box<RowPolymorphism>>,
}

/// How to resolve the concrete type of a polymorphic row. Every variant compiles to one
/// `CASE` expression over the row, so no extra round-trips are needed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RowPolymorphism {
    /// Single Table Inheritance: the discriminator column's value selects the concrete type.
    SingleTableInheritance {
        discriminator_column: String,
        variants: Vec<StiBranch>,
        /// Rows whose discriminator matches no variant (e.g. NULL); `None` when the
        /// discriminator cannot be NULL.
        fallback: Option<TypedFieldSet>,
    },
    /// Table-per-type: child tables are LEFT JOINed on their key, the first child with a
    /// matching row wins, otherwise the row resolves to the parent type.
    ConcreteTableInheritance {
        children: Vec<CtiRowEntry>,
        fallback: Option<TypedFieldSet>,
    },
}

/// One variant of a Single Table Inheritance `CASE`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StiBranch {
    pub discriminator_value: String,
    pub set: TypedFieldSet,
}

/// One child in a CTI polymorphic join.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CtiRowEntry {
    pub schema: String,
    pub table: String,
    /// `(child_column, parent_column)` join pairs.
    pub join_columns: Vec<(String, String)>,
    /// Fields read from the child join; `set.table_alias` is the child's unique SQL alias.
    pub set: TypedFieldSet,
}

impl SelectedField {
    pub fn column(field_alias: impl Into<String>, column_name: impl Into<String>) -> Self {
        Self::Column {
            field_alias: field_alias.into(),
            column_name: column_name.into(),
            output: ValueOutput::Plain,
        }
    }

    pub fn relation(field_alias: impl Into<String>, step_id: StepId) -> Self {
        Self::Relation {
            field_alias: field_alias.into(),
            step_id,
        }
    }

    /// Returns the alias used in the JSON response for this selected field.
    /// Used by the query planner to deduplicate fields when flattening fragments.
    pub fn field_alias(&self) -> &str {
        match self {
            Self::Column { field_alias, .. } => field_alias,
            Self::GlobalId { field_alias, .. } => field_alias,
            Self::Typename { field_alias, .. } => field_alias,
            Self::Relation { field_alias, .. } => field_alias,
            Self::ExclusiveArcTarget { field_alias, .. } => field_alias,
            Self::FunctionCall { field_alias, .. } => field_alias,
            Self::RowVersion { field_alias, .. } => field_alias,
            Self::OnAlias { field, .. } => field.field_alias(),
            Self::JoinRow { .. } => "",
        }
    }
}

/// A WHERE condition in a PgSelect or PgConnection step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WhereCondition {
    pub column: String,
    pub operator: String,
    pub value: serde_json::Value,
    /// SQL type the text-bound parameter is cast to (e.g. "uuid"), so the comparison
    /// runs on the column's native type and can use its index. `None` compares as text.
    #[serde(default)]
    pub pg_cast: Option<String>,
}

/// A boolean filter expression for Advanced Filtering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FilterExpr {
    Binary {
        column: String,
        table_alias: String,
        operator: String,
        value: serde_json::Value,
        #[serde(default)]
        pg_cast: Option<String>,
    },
    Pattern {
        column: String,
        table_alias: String,
        operator: String, // "LIKE", "ILIKE", "NOT LIKE", "NOT ILIKE"
        pattern: String,
    },
    InList {
        column: String,
        table_alias: String,
        values: Vec<serde_json::Value>,
        is_not: bool,
        #[serde(default)]
        pg_cast: Option<String>,
    },
    IsNull {
        column: String,
        table_alias: String,
        is_null: bool,
    },
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
    /// Some related row (matching `filter`) exists.
    Exists {
        related: RelatedRows,
        filter: Option<Box<FilterExpr>>,
    },
    /// A condition the planner writes as SQL around the column: `{c}` stands for the column,
    /// `{0}`, `{1}`, ... for `values`, each bound as a text parameter (`$1::text`, cast further
    /// by the template). Templates never contain input; values are always bound.
    Template {
        column: String,
        table_alias: String,
        sql: String,
        values: Vec<serde_json::Value>,
    },
    /// The expression is not true (false or NULL): `every` related row matching means no
    /// related row for which the filter is not true.
    NotTrue(Box<FilterExpr>),
    /// An aggregate of the related rows compared with values: `count(*) > 5`.
    RelatedAggregate {
        related: RelatedRows,
        /// `count`, `sum`, `avg`, `min` or `max`.
        function: String,
        /// Aggregated column; `None` for `count`.
        column: Option<String>,
        /// `=`, `<>`, `<`, `<=`, `>`, `>=`, `IN`, `NOT IN`, `IS NULL`, `IS NOT NULL`.
        operator: String,
        values: Vec<serde_json::Value>,
    },
}

/// The rows of a to-many (or to-one) relation of a row read from `parent_alias`, read as
/// `alias` in a correlated subquery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelatedRows {
    pub schema: String,
    pub table: String,
    pub alias: String,
    /// `(related column, parent column)` pairs; for many-to-many relations the parent side
    /// is compared through `junction`.
    pub columns: Vec<(String, String)>,
    pub parent_alias: String,
    /// `(related column, parent column)` ranges that must overlap (a temporal foreign key).
    #[serde(default)]
    pub period: Option<(String, String)>,
    /// Junction rows linking parent and related rows (many-to-many).
    #[serde(default)]
    pub junction: Option<JunctionJoin>,
}

/// A selected field in an aggregation group (min/max/sum/avg).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AggregateField {
    pub field_alias: String,
    pub column_name: String,
    pub is_range: bool,
    /// Conversion of min/max values for the response.
    #[serde(default)]
    pub output: ValueOutput,
}

/// Native SQL aggregations selected on a connection (`aggregates`) or an aggregate group.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AggregatesSelections {
    /// Response key of a connection's `aggregates` field (empty in groups).
    #[serde(default)]
    pub field_alias: String,
    /// The aggregate objects selected, in selection order.
    #[serde(default)]
    pub parts: Vec<AggregatePart>,
    /// Only rows matching it are aggregated (`FILTER (WHERE ...)`).
    #[serde(default)]
    pub filter: Option<FilterExpr>,
}

/// One aggregate object: `min { price }`, `percentile(fraction: 0.9) { price }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AggregatePart {
    pub field_alias: String,
    /// `min`, `max`, `sum`, `avg`, `count_distinct`, `stddev_samp`, `stddev_pop`,
    /// `var_samp`, `var_pop` or `percentile_cont`.
    pub function: String,
    /// The fraction of `percentile_cont`.
    pub fraction: Option<f64>,
    pub fields: Vec<AggregateField>,
}

/// A value rows are grouped by: a column, optionally truncated to a unit of time or binned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupKey {
    pub column: String,
    /// `date_trunc` unit (`month`, ...); the column is a date or timestamp. `bin`: binned
    /// by `date_bin` into the group's `bin` width.
    pub truncate: Option<String>,
    /// The column is a `date` (truncated in `timestamp` arithmetic, then cast back).
    pub is_date: bool,
    /// Conversion of the value for the response.
    pub output: ValueOutput,
    /// SQL type of the key's values (`timestamptz`, ...), for series of missing groups.
    #[serde(default)]
    pub sql_type: Option<String>,
}

/// `date_bin` bins of a grouping: `width` (an interval) from `origin`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimeBins {
    pub width: String,
    pub origin: Option<String>,
}

/// A value computed over the matching rows in a connection's order (a window function),
/// selected on its edges.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowSelection {
    pub field_alias: String,
    /// `row_number`, `rank`, `dense_rank`, `percent_rank` or `running_sum`.
    pub function: String,
    /// Columns of `running_sum`.
    pub fields: Vec<AggregateField>,
    /// Rows are numbered (or summed) within groups of equal values of these.
    pub partition: Vec<GroupKey>,
    /// Leading sort keys the connection's `orderBy` gave: ranks compare these only (the
    /// key breaking their ties would leave no ties to rank). 0: all of them.
    #[serde(default)]
    pub rank_keys: usize,
}

/// A condition on a group's aggregates: `sum(total) > 10`, `count(*) >= 2`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HavingCondition {
    /// `count`, `sum`, `avg`, `min` or `max`.
    pub function: String,
    /// Aggregated column; `None` for `count`.
    pub column: Option<String>,
    /// `=`, `<>`, `<`, `<=`, `>`, `>=`, `IN`, `NOT IN`, `IS NULL`, `IS NOT NULL`.
    pub operator: String,
    pub values: Vec<serde_json::Value>,
}

/// A `groupedAggregates(groupBy:, having:)` field of a connection: one object per group of
/// the connection's rows (before paging).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupedAggregates {
    pub field_alias: String,
    pub keys: Vec<GroupKey>,
    pub having: Vec<HavingCondition>,
    pub include_keys: bool,
    pub include_count: bool,
    pub aggregates: AggregatesSelections,
    /// Time zone dates and times are truncated in (`timeZone`).
    #[serde(default)]
    pub time_zone: Option<String>,
    /// Width and origin of `_BINNED` keys.
    #[serde(default)]
    pub bins: Option<TimeBins>,
    /// Missing groups between the first and the last one are included, empty (a single
    /// truncated or binned key).
    #[serde(default)]
    pub fill_gaps: bool,
}

/// Selected parts of a Relay Connection.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ConnectionSelections {
    pub include_total_count: bool,
    pub include_page_info: bool,
    pub include_edges: bool,
    pub include_nodes: bool,
    /// Fields selected inside edge { node { ... } } or nodes { ... }
    pub node_fields: Vec<SelectedField>,
    /// Set when the connection's table is polymorphic: nodes resolve to their concrete type
    /// instead of using `node_fields`.
    #[serde(default)]
    pub node_polymorphism: Option<RowPolymorphism>,
    /// `aggregates` fields, in selection order.
    #[serde(default)]
    pub aggregates: Vec<AggregatesSelections>,
    /// Window values selected on edges.
    #[serde(default)]
    pub windows: Vec<WindowSelection>,
    /// `groupedAggregates` fields, in selection order.
    #[serde(default)]
    pub grouped: Vec<GroupedAggregates>,
    /// `totalCount(estimate: true)`: an estimate is enough.
    #[serde(default)]
    pub estimate_total: bool,
    /// Each selected `totalCount`: its response key, and whether it is an estimate.
    #[serde(default)]
    pub total_counts: Vec<(String, bool)>,
}

/// Fields selected on a `<Type>SearchResults` object.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchSelections {
    pub include_hits: bool,
    pub include_score: bool,
    pub include_has_more: bool,
    /// Fields selected inside `hits { node { ... } }`.
    pub node_fields: Vec<SelectedField>,
    /// Set when the table is polymorphic (see `ConnectionSelections::node_polymorphism`).
    pub node_polymorphism: Option<RowPolymorphism>,
    /// `highlight` of full-text hits: the matching fragments of the document's text.
    #[serde(default)]
    pub highlight: Option<HighlightOptions>,
    /// Response key of the `distance` of nearest-search hits, when selected.
    #[serde(default)]
    pub distance: Option<String>,
}

/// The rows of one table `globalSearch` searches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobalSearchBranch {
    pub schema: String,
    pub table: String,
    pub table_alias: String,
    /// Full-text or fuzzy search.
    pub search: SearchOperation,
    pub node_fields: Vec<SelectedField>,
    pub node_polymorphism: Option<RowPolymorphism>,
}

/// `ts_headline` options of a `highlight` selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HighlightOptions {
    pub field_alias: String,
    pub start_sel: String,
    pub stop_sel: String,
    pub max_words: Option<i64>,
    pub min_words: Option<i64>,
    pub max_fragments: Option<i64>,
}

/// A search operation (Fuzzy trigram, Vector semantic, BM25 lexical, or Hybrid RRF).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SearchOperation {
    Fuzzy {
        column: String,
        query: String,
        min_similarity: Option<f64>,
    },
    Semantic {
        column: String,
        vector: Vec<f64>,
        min_score: Option<f64>,
        opclass: String,
    },
    Bm25 {
        column: String,
        query: String,
    },
    /// Full-text search: the rows whose document matches `query` (web search syntax), best
    /// `ts_rank` first.
    FullText {
        document: String,
        config: Option<String>,
        /// Text of the document, for highlights.
        source: Option<String>,
        query: String,
    },
    /// The rows whose geometry is nearest `near` (GeoJSON text) first.
    Nearest {
        column: String,
        geography: bool,
        near: String,
        max_distance: Option<f64>,
    },
    Hybrid {
        /// Single-column key the vector and lexical rankings are fused on.
        key_column: String,
        vector_column: String,
        vector_opclass: String,
        lexical_column: String,
        is_bm25: bool,
        query: String,
        vector: Vec<f64>,
        lexical_weight: f64,
        vector_weight: f64,
        rrf_k: usize,
    },
}

/// A column assignment in an INSERT or UPDATE.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColumnValue {
    pub column: String,
    pub value: serde_json::Value,
    /// SQL type the text-bound value is cast to (e.g. "uuid", "jsonb", "text[]").
    pub pg_cast: Option<String>,
    /// The column has composite type `pg_cast` (or is an array of it, `composite_array`):
    /// the value is JSON with attribute-named keys, converted with `json_populate_record`.
    #[serde(default)]
    pub composite: bool,
    #[serde(default)]
    pub composite_array: bool,
}

/// The type and parameters of a database mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MutationKind {
    /// Writes the `roots` rows of `nodes` (exactly one unless `many`), and the rows their
    /// nested relations create, connect or detach. The roots are inserted, or for an update
    /// with nested writes the root is the updated row. Rows may set different columns.
    Insert {
        nodes: Vec<WriteNode>,
        roots: Vec<usize>,
        many: bool,
        #[serde(default)]
        on_conflict: Option<OnConflict>,
    },
    Update {
        target: RowTarget,
        column_values: Vec<ColumnValue>,
    },
    Delete {
        target: RowTarget,
    },
}

/// A row a create writes or reads: a top-level row or one reached through its nested
/// relations. Nodes reference each other by index into `MutationKind::Insert::nodes`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WriteNode {
    pub schema: String,
    pub table: String,
    pub kind: WriteNodeKind,
    /// Column values from the input.
    pub values: Vec<ColumnValue>,
    /// Columns taken from another node's row: the foreign keys of nested relations.
    pub refs: Vec<NodeRef>,
    /// Where the row is in the input (`inputs[0].author.connect`), for error messages.
    pub path: String,
    /// Nodes that must be written first although no value is taken from them (a lookup of
    /// an upserted row runs after the upsert).
    #[serde(default)]
    pub after: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WriteNodeKind {
    Insert,
    /// An existing row a relation connects to, found by key (read only).
    Lookup {
        key: Vec<WhereCondition>,
    },
    /// An existing row, updated: its `values` and the columns it takes from other rows
    /// (`refs`; connecting a row to a new parent sets its foreign key).
    Update {
        key: Vec<WhereCondition>,
    },
    /// A junction row relating two rows, inserted unless they are already related.
    Link,
    /// `connectOrCreate` of a row a relation refers to: the row with `key`, or else a new
    /// row from the node's values.
    LookupOrInsert {
        key: Vec<WhereCondition>,
    },
    /// `connectOrCreate` of a child: the row with `key` takes the parent's key (`refs`), or
    /// else a new row is created with it.
    UpdateOrInsert {
        key: Vec<WhereCondition>,
    },
    /// `update` of a child: the row with `key` that is related to the parent (its `refs`
    /// columns equal the parent's) gets the node's values.
    UpdateRelated {
        key: Vec<WhereCondition>,
    },
    /// `delete` of a child: the row with `key` related to the parent is deleted.
    DeleteRelated {
        key: Vec<WhereCondition>,
    },
    /// `update` of the row a to-one relation refers to: the row of this table whose columns
    /// the `from` row's foreign key holds (`columns`: `(column, from column)`).
    UpdateReferenced {
        from_schema: String,
        from_table: String,
        from_key: Vec<WhereCondition>,
        columns: Vec<(String, String)>,
    },
    /// `sync` of a child set (one `MERGE`): the parent's children are exactly `rows` —
    /// those whose key matches a child are updated, the others inserted, and the children
    /// no row matches deleted. `key`: the children's key `(column, cast)`.
    Sync {
        rows: Vec<Vec<ColumnValue>>,
        key: Vec<(String, Option<String>)>,
    },
    /// Rows related to another row, whose key columns their `refs` columns equal, are
    /// detached from it: children get NULL foreign keys, junction rows are deleted.
    Detach {
        /// Only the related row with this key (`disconnect`); `None`: every related row
        /// except those matching `except` (`set`).
        key: Option<Vec<WhereCondition>>,
        except: Vec<Vec<WhereCondition>>,
        /// Junction rows (deleted), whose `columns` `(junction column, target column)`
        /// point at rows of the target table the keys identify.
        through: Option<DetachThrough>,
    },
}

/// The target table of junction rows a `Detach` node deletes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetachThrough {
    pub schema: String,
    pub table: String,
    pub columns: Vec<(String, String)>,
}

/// Column `column` takes the value of `source_column` of node `node`'s row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeRef {
    pub column: String,
    /// Type the value is assigned as.
    pub pg_cast: Option<String>,
    pub node: usize,
    pub source_column: String,
}

/// Rows an update or delete applies to.
// One per mutation field, so variant sizes do not matter
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RowTarget {
    /// The row whose key columns equal these values.
    Key(Vec<WhereCondition>),
    /// The rows matching a filter on the mutated table's alias (every row without one).
    Filter(Option<FilterExpr>),
}

/// `INSERT ... ON CONFLICT (<columns>) DO UPDATE / DO NOTHING`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnConflict {
    /// The conflict key, `(column, sql cast)`. Every inserted row sets these columns, and
    /// the payload reads the rows back by them (inserted, updated or left alone).
    pub columns: Vec<(String, Option<String>)>,
    /// Columns updated from the proposed row (`EXCLUDED`). `None`: the columns each row
    /// sets, minus the key. Nothing to update means `DO NOTHING`.
    pub update: Option<Vec<String>>,
    pub do_nothing: bool,
}

/// Rows above or below a row in a hierarchy of its table (see `TreeInfo`). They are numbered
/// by distance from it, `__depth` (1 for its parent or children).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TreeSource {
    pub parent_alias: String,
    pub ancestors: bool,
    pub link: crate::catalog::TreeLink,
    /// Key columns of the table: rows met again (a cycle) end the walk.
    pub key: Vec<String>,
    /// Rows at most this far from the row.
    pub max_depth: Option<usize>,
}

/// Optimistic concurrency: the row a mutation writes is locked, and the write fails unless
/// its version is still `expected`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VersionGuard {
    pub key: Vec<WhereCondition>,
    /// Version column; `xmin` without one.
    pub column: String,
    pub expected: String,
}

/// A field selected on a mutation payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PayloadField {
    /// The affected row (or rows, for a mutation of many rows).
    Rows {
        field_alias: String,
        fields: Vec<SelectedField>,
        /// Set when the mutated table is polymorphic: rows resolve to their concrete type
        /// instead of using `fields`.
        polymorphism: Option<RowPolymorphism>,
    },
    /// The rows as they were before the write (`RETURNING OLD`), in the order of `Rows`.
    Previous {
        field_alias: String,
        fields: Vec<SelectedField>,
        polymorphism: Option<RowPolymorphism>,
        /// SQL alias the previous rows are read from.
        table_alias: String,
    },
    /// Number of rows the statement(s) inserted, updated or deleted.
    AffectedCount { field_alias: String },
    Typename {
        field_alias: String,
        type_name: String,
    },
}

/// A Step represents a discrete planned operation in Graphile Crystal's compiler model.
// Plans hold a handful of steps per request, so variant sizes do not matter
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Step {
    /// A constant value.
    Constant { value: serde_json::Value },

    /// A field access on a previous step.
    FieldAccess { source: StepId, field_name: String },

    /// A planned PostgreSQL query (PgSelectStep).
    /// Emits single-query SQL with native JSON aggregation.
    PgSelect {
        schema: String,
        table: String,
        table_alias: String,
        /// If true, returns a single object (or null). If false, returns a JSON array.
        is_single: bool,
        fields: Vec<SelectedField>,
        where_conditions: Vec<WhereCondition>,
        #[serde(default)]
        filter_expr: Option<FilterExpr>,
        join_condition: Option<JoinCondition>,
        limit: Option<usize>,
        offset: Option<usize>,
        order_by: Option<String>,
        /// Row-level polymorphism (STI or CTI). None means plain homogeneous rows.
        #[serde(default)]
        polymorphism: Option<RowPolymorphism>,
        /// The rows are the ancestors or descendants of the row read from a parent alias,
        /// rather than rows of the table joined to it.
        #[serde(default)]
        tree: Option<TreeSource>,
    },

    /// A planned Relay Connection step (PgConnectionStep).
    /// Emits single-scan SQL aggregating totalCount, pageInfo, edges, and nodes.
    PgConnection {
        schema: String,
        table: String,
        table_alias: String,
        selections: ConnectionSelections,
        where_conditions: Vec<WhereCondition>,
        #[serde(default)]
        filter_expr: Option<FilterExpr>,
        join_condition: Option<JoinCondition>,
        first: Option<usize>,
        last: Option<usize>,
        offset: Option<usize>,
        after: Option<String>,
        before: Option<String>,
        /// Keyset order: the `orderBy` columns, then the key columns they leave out. Cursors
        /// hold these columns' values. Empty in older plans, meaning `cursor_columns`.
        #[serde(default)]
        sort: Vec<SortKey>,
        /// SQL type of the `id` column, used to compare cursor values natively.
        id_cast: Option<String>,
        /// Rows come from this function instead of the table.
        #[serde(default)]
        source: Option<FunctionSource>,
        /// Cursor/default-order key: `(column, sql_cast)` of the primary key columns.
        /// Empty in older plans, meaning the `id` column with `id_cast`.
        #[serde(default)]
        cursor_columns: Vec<(String, Option<String>)>,
    },

    /// A search root field: the `limit` best matches of `search` among the rows passing
    /// the conditions and filter, best first, and whether more matched.
    PgSearch {
        schema: String,
        table: String,
        table_alias: String,
        search: SearchOperation,
        where_conditions: Vec<WhereCondition>,
        filter_expr: Option<FilterExpr>,
        limit: usize,
        selections: SearchSelections,
    },

    /// `globalSearch`: the best matches of a text among the rows of several tables, merged
    /// by score.
    PgGlobalSearch {
        branches: Vec<GlobalSearchBranch>,
        limit: usize,
        include_hits: bool,
        include_score: bool,
        include_has_more: bool,
    },

    /// A planned PostgreSQL mutation step (INSERT, UPDATE, DELETE with RETURNING).
    PgMutation {
        schema: String,
        table: String,
        table_alias: String,
        kind: MutationKind,
        /// The payload holds a list of rows rather than one row.
        many: bool,
        payload: Vec<PayloadField>,
        /// Primary key `(column, sql cast)`: inserted/updated rows are read back by key in a
        /// statement of their own, so the payload sees the write. Empty (no key, or a
        /// delete): the payload is built from the rows the statement returns.
        #[serde(default)]
        key_columns: Vec<(String, Option<String>)>,
        /// `expectedVersion` of a single-row update or delete.
        #[serde(default)]
        version_guard: Option<VersionGuard>,
    },

    /// A root field backed by a function: a query, or a mutation for a `VOLATILE` one
    /// (connections over a function's rows are `PgConnection`s with a `FunctionSource`).
    PgFunction {
        schema: String,
        func_name: String,
        arguments: Vec<BoundArg>,
        returns_set: bool,
        /// The rows come as an array, read with `unnest`.
        #[serde(default)]
        unnest: bool,
        mutation: bool,
        result: FunctionResult,
    },

    /// The entities of an Apollo Federation `_entities` query, one per representation, in
    /// order (null for those that resolve to nothing). Representations of one type keyed by
    /// the same columns are looked up together.
    Entities {
        /// Number of representations.
        count: usize,
        groups: Vec<EntityGroup>,
        /// Representations whose key has no safe SQL cast, each looked up on its own:
        /// `(position, single-row step)`.
        singles: Vec<(usize, StepId)>,
    },
}

/// Representations of one type keyed by the same columns, looked up in one join.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityGroup {
    /// Alias of the representations' keys: columns `ord` (the position) and `k0`, `k1`, ...
    pub keys_alias: String,
    /// SQL type of each key column.
    pub key_casts: Vec<String>,
    /// `(position, key values)` of each representation.
    pub rows: Vec<(usize, Vec<serde_json::Value>)>,
    /// Single-row step joined to the keys (`join_condition` on `keys_alias`).
    pub step: StepId,
}

impl Step {
    pub fn pg_select(
        schema: impl Into<String>,
        table: impl Into<String>,
        table_alias: impl Into<String>,
        is_single: bool,
    ) -> Self {
        Self::PgSelect {
            schema: schema.into(),
            table: table.into(),
            table_alias: table_alias.into(),
            is_single,
            fields: Vec::new(),
            where_conditions: Vec::new(),
            filter_expr: None,
            join_condition: None,
            limit: if is_single { Some(1) } else { None },
            offset: None,
            order_by: None,
            polymorphism: None,
            tree: None,
        }
    }

    pub fn pg_connection(
        schema: impl Into<String>,
        table: impl Into<String>,
        table_alias: impl Into<String>,
    ) -> Self {
        Self::PgConnection {
            schema: schema.into(),
            table: table.into(),
            table_alias: table_alias.into(),
            selections: ConnectionSelections::default(),
            where_conditions: Vec::new(),
            filter_expr: None,
            join_condition: None,
            first: None,
            last: None,
            offset: None,
            after: None,
            before: None,
            sort: Vec::new(),
            source: None,
            id_cast: None,
            cursor_columns: Vec::new(),
        }
    }
}
