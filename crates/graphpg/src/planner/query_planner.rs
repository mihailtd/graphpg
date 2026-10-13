use super::scalars;
pub use super::scalars::InputError;
use crate::catalog::{
    conflict_key_value, pg_array_literal, pg_type_enum_key, to_camel_case, ColumnMetadata, CtiKind,
    DatabaseCatalog, FieldKind, FunctionMetadata, FunctionReturn, MutationAction, OrderTarget,
    PgType, Polymorphism, RelationInfo, SearchIndexKind, SearchIndexMetadata, SearchMethod,
    TableMetadata, TypeLimits,
};
use crate::compiler::{escape_like, quote_ident, SqlCompiler};
use crate::plan::{
    AggregateField, AggregatePart, AggregatesSelections, ArgValue, BoundArg, BoundPart,
    ColumnValue, CompositeField, CompositeSource, ConnectionSelections, CtiRowEntry, DetachThrough,
    EntityGroup, FilterExpr, FunctionResult, FunctionSource, GlobalSearchBranch, GroupKey,
    GroupedAggregates, HavingCondition, HighlightOptions, JoinCondition, JunctionJoin,
    MutationKind, NodeRef, OnConflict, OperationPlan, PayloadField, RangeField, RangePart,
    RelatedRows, RowPolymorphism, RowTarget, SearchOperation, SearchSelections, SelectedField,
    SortCompare, SortExpr, SortKey, Step, StepId, StiBranch, TimeBins, TreeSource, TypedFieldSet,
    ValueOutput, VersionGuard, WhereCondition, WindowSelection, WriteNode, WriteNodeKind,
};
use crate::planner::request::{
    flatten_selections_for, GraphQLAdapter, OperationKind, RequestField, RequestOperation,
};
use crate::schema::SchemaBuilder;
use apollo_compiler::ast::{Value, VariableDefinition};
use apollo_compiler::executable::{Field, Selection};
use apollo_compiler::ExecutableDocument;
use apollo_compiler::Node;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PlannerError {
    #[error("No executable operation found in document")]
    NoOperationFound,

    #[error("Unknown root field: {0}")]
    UnknownRootField(String),

    #[error("Table not found in catalog for field: {0}")]
    TableNotFound(String),

    #[error("Unknown field `{field}` on the type of `{table}`")]
    UnknownField { table: String, field: String },

    #[error("{0}")]
    InvalidInput(String),

    /// Input values rejected while planning, each with its path and error code.
    #[error("{}", .0.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join("; "))]
    Input(Vec<InputError>),

    /// The plan exceeds a safety limit (page size, depth, cost, bulk rows).
    #[error("{0}")]
    Limit(#[from] crate::plan::LimitError),
}

pub struct QueryPlanner<'a> {
    pub catalog: &'a DatabaseCatalog,
    pub default_schema: String,
    pub document: Option<&'a ExecutableDocument>,
    pub variables: Option<&'a serde_json::Value>,
    /// Safety limits every plan is checked against (none by default).
    pub limits: crate::plan::Limits,
}

impl<'a> QueryPlanner<'a> {
    pub fn new(catalog: &'a DatabaseCatalog, default_schema: impl Into<String>) -> Self {
        Self {
            catalog,
            default_schema: default_schema.into(),
            document: None,
            variables: None,
            limits: crate::plan::Limits::default(),
        }
    }

    /// Checks every plan against `limits` (see [`crate::plan::limits`]).
    pub fn with_limits(mut self, limits: crate::plan::Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_context(
        catalog: &'a DatabaseCatalog,
        default_schema: impl Into<String>,
        document: Option<&'a ExecutableDocument>,
        variables: Option<&'a serde_json::Value>,
    ) -> Self {
        Self {
            catalog,
            default_schema: default_schema.into(),
            document,
            variables,
            limits: crate::plan::Limits::default(),
        }
    }

    /// Whether a fragment with `type_condition` applies to an object of one of `target_types`
    /// (the concrete type plus the abstract types it belongs to). No targets accepts everything.
    fn matches_type(type_condition: Option<&str>, target_types: &[&str]) -> bool {
        match type_condition {
            None => true,
            Some(cond) => {
                target_types.is_empty()
                    || cond == "Node"
                    || target_types.iter().any(|t| *t == cond || *t == "Node")
            }
        }
    }

    /// Recursively flattens a SelectionSet, expanding FragmentSpreads and InlineFragments
    /// whose type condition matches `target_type`.
    pub fn flatten_selections<'b>(
        &'b self,
        selection_set: &'b apollo_compiler::executable::SelectionSet,
        target_type: Option<&str>,
    ) -> Vec<&'b Field> {
        let targets: Vec<&str> = target_type.into_iter().collect();
        self.flatten_selections_for(selection_set, &targets)
    }

    /// Like `flatten_selections`, for an object that satisfies any of `target_types`
    /// (e.g. `["Dog", "Animal"]`).
    pub fn flatten_selections_for<'b>(
        &'b self,
        selection_set: &'b apollo_compiler::executable::SelectionSet,
        target_types: &[&str],
    ) -> Vec<&'b Field> {
        let mut fields = Vec::new();
        let mut visited = HashSet::new();
        self.collect_fields(selection_set, target_types, &mut visited, &mut fields);
        fields
    }

    fn collect_fields<'b>(
        &'b self,
        selection_set: &'b apollo_compiler::executable::SelectionSet,
        target_type: &[&str],
        visited: &mut HashSet<String>,
        out: &mut Vec<&'b Field>,
    ) {
        for sel in &selection_set.selections {
            match sel {
                Selection::Field(f) => {
                    out.push(f);
                }
                Selection::FragmentSpread(spread) => {
                    let name = spread.fragment_name.as_str();
                    if visited.insert(name.to_string()) {
                        if let Some(doc) = self.document {
                            if let Some(frag) = doc.fragments.get(name) {
                                let cond = frag.type_condition().as_str();
                                if Self::matches_type(Some(cond), target_type) {
                                    self.collect_fields(
                                        &frag.selection_set,
                                        target_type,
                                        visited,
                                        out,
                                    );
                                }
                            }
                        }
                        visited.remove(name);
                    }
                }
                Selection::InlineFragment(inline) => {
                    let cond = inline.type_condition.as_ref().map(|tc| tc.as_str());
                    if Self::matches_type(cond, target_type) {
                        self.collect_fields(&inline.selection_set, target_type, visited, out);
                    }
                }
            }
        }
    }

    /// Resolves an AST Value into a serde_json::Value, interpolating variables from
    /// `self.variables` or operation-level variable default values.
    pub fn resolve_value(
        &self,
        val: &Value,
        op_variables: &[Node<VariableDefinition>],
    ) -> serde_json::Value {
        match val {
            Value::Variable(var_name) => {
                let vname = var_name.as_str();
                // 1. Look up in provided variables map
                if let Some(serde_json::Value::Object(map)) = self.variables {
                    if let Some(val) = map.get(vname) {
                        return val.clone();
                    }
                }
                // 2. Look up in operation variable default values
                if let Some(var_def) = op_variables.iter().find(|v| v.name.as_str() == vname) {
                    if let Some(default_val) = &var_def.default_value {
                        return self.resolve_value(default_val, op_variables);
                    }
                }
                serde_json::Value::Null
            }
            Value::List(l) => serde_json::Value::Array(
                l.iter()
                    .map(|item| self.resolve_value(item, op_variables))
                    .collect(),
            ),
            Value::Object(o) => serde_json::Value::Object(
                o.iter()
                    .map(|(k, v)| (k.as_str().to_string(), self.resolve_value(v, op_variables)))
                    .collect(),
            ),
            other => Self::convert_ast_value(other),
        }
    }

    /// Literal conversion (without variable resolution).
    pub fn convert_ast_value(val: &Value) -> serde_json::Value {
        match val {
            Value::Null => serde_json::Value::Null,
            Value::Boolean(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => {
                let i_str = i.as_str();
                match i_str.parse::<i64>() {
                    Ok(num) => serde_json::json!(num),
                    Err(_) => serde_json::json!(i_str),
                }
            }
            Value::Float(f) => {
                let f_str = f.as_str();
                match f_str.parse::<f64>() {
                    Ok(num) => serde_json::json!(num),
                    Err(_) => serde_json::json!(f_str),
                }
            }
            Value::String(s) => serde_json::Value::String(s.clone()),
            Value::Enum(e) => serde_json::Value::String(e.as_str().to_string()),
            Value::List(l) => {
                serde_json::Value::Array(l.iter().map(|v| Self::convert_ast_value(v)).collect())
            }
            Value::Object(o) => serde_json::Value::Object(
                o.iter()
                    .map(|(k, v)| (k.as_str().to_string(), Self::convert_ast_value(v)))
                    .collect(),
            ),
            Value::Variable(v) => serde_json::json!({ "$var": v.as_str() }),
        }
    }

    /// Map an `<Type>OrderBy` enum value to a PostgreSQL `ORDER BY` item list.
    pub fn order_by_enum_to_sql(
        catalog: &DatabaseCatalog,
        enum_str: &str,
        table_alias: &str,
        table_meta: &TableMetadata,
    ) -> Option<String> {
        let keys = sort_keys_of(catalog, enum_str, table_meta);
        let items: Vec<String> = keys
            .iter()
            .map(|k| {
                format!(
                    "{} {}",
                    SqlCompiler::sort_expression(table_alias, k),
                    k.direction_sql(false)
                )
            })
            .collect();
        (!items.is_empty()).then(|| items.join(", "))
    }

    /// Compile a GraphQL ExecutableDocument into an OperationPlan DAG.
    pub fn plan(&self, document: &ExecutableDocument) -> Result<OperationPlan, PlannerError> {
        let operation = document
            .operations
            .iter()
            .next()
            .ok_or(PlannerError::NoOperationFound)?;
        self.plan_operation(operation)
    }

    /// Plans a specific operation of a document (e.g. the one selected by `operationName`).
    pub fn plan_operation(
        &self,
        operation: &apollo_compiler::executable::Operation,
    ) -> Result<OperationPlan, PlannerError> {
        let request = GraphQLAdapter::adapt_operation(operation, self.document, self.variables);
        self.plan_request(&request)
    }

    /// Compile a protocol-neutral RequestOperation into an OperationPlan DAG.
    pub fn plan_request(&self, request: &RequestOperation) -> Result<OperationPlan, PlannerError> {
        let ctx = PlanningContext {
            planner: self,
            step_memo: RefCell::new(HashMap::new()),
            join_aliases: RefCell::new(HashMap::new()),
        };
        ctx.plan(request)
    }

    /// The fields of a row of `table` read from `alias`, for `selection` (the REST API's
    /// columns, say), with the plan holding the steps they need: the row as a response
    /// shows it.
    pub fn plan_row(
        &self,
        selection: &[RequestField],
        table: &TableMetadata,
        alias: &str,
    ) -> Result<(Vec<SelectedField>, Option<RowPolymorphism>, OperationPlan), PlannerError> {
        let ctx = PlanningContext {
            planner: self,
            step_memo: RefCell::new(HashMap::new()),
            join_aliases: RefCell::new(HashMap::new()),
        };
        ctx.plan_row(selection, table, alias)
    }

    pub fn to_snake_case(s: &str) -> String {
        crate::catalog::to_snake_case(s)
    }

    /// Column values of one imported row of `table` (a `Create<Type>Input` of plain fields),
    /// checked and converted as a mutation's input is; `path` names it in errors.
    pub fn import_values(
        &self,
        table: &TableMetadata,
        row: &serde_json::Value,
        path: &str,
    ) -> Result<Vec<ColumnValue>, PlannerError> {
        let ctx = PlanningContext {
            planner: self,
            step_memo: RefCell::new(HashMap::new()),
            join_aliases: RefCell::new(HashMap::new()),
        };
        scalars::start();
        let values = ctx.plain_values(table, row, path, true)?;
        let invalid = scalars::finish();
        if !invalid.is_empty() {
            return Err(PlannerError::Input(invalid));
        }
        Ok(values)
    }

    /// Type conditions a fragment may use to select fields of an object of concrete type
    /// `type_name`: the type itself, the interfaces it implements and the unions containing it.
    fn type_conditions(&self, type_name: &str) -> Vec<String> {
        let mut out = vec![type_name.to_string()];
        out.extend(self.catalog.interfaces_of(type_name));
        if self.catalog.global_search_field_any().is_some()
            && self.catalog.is_global_search_type(type_name)
        {
            out.push("GlobalSearchNode".to_string());
        }
        for arc in &self.catalog.exclusive_arcs {
            if self
                .catalog
                .arc_member_types(arc, &self.default_schema)
                .iter()
                .any(|m| m == type_name)
            {
                out.push(arc.union_name.clone());
            }
        }
        if self.catalog.federation_enabled {
            out.push("_Entity".to_string());
        }
        out
    }
}

/// Unwraps a Relay global ID (`base64("Type:pk")`) passed where a raw key is expected.
fn unwrap_global_id(value: serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::String(s) = &value {
        if let Some((_, raw_pk)) = SchemaBuilder::decode_global_id(s) {
            return serde_json::Value::String(raw_pk);
        }
    }
    value
}

/// A GraphQL input value in database terms: enum values become their labels, composite
/// objects get attribute-named keys (recursively), range objects range literals, and scalar
/// values are checked and converted to the text their type parses (see `scalars`). `label`
/// names the value in error messages.
/// `limits` (writes only) are those of the value's type modifier: a longer string or a
/// decimal of more digits is rejected.
fn database_value(
    catalog: &DatabaseCatalog,
    pg_type: &PgType,
    enum_key: Option<&str>,
    limits: Option<&TypeLimits>,
    value: serde_json::Value,
    label: &str,
) -> serde_json::Value {
    use serde_json::Value as J;
    let enum_key = enum_key.or(match pg_type {
        PgType::Enum(k) => Some(k.as_str()),
        _ => None,
    });
    match (pg_type, value) {
        (_, J::Null) => J::Null,
        (PgType::Array(elem), J::Array(items)) => J::Array(
            items
                .into_iter()
                .map(|v| database_value(catalog, elem, enum_key, limits, v, label))
                .collect(),
        ),
        (PgType::Composite(key), J::Object(fields)) => match catalog.find_composite(key) {
            Some(composite) => J::Object(
                fields
                    .into_iter()
                    .filter_map(|(k, v)| {
                        let attr = composite.attribute_by_field(&k)?;
                        let label = format!("{label}.{}", attr.field_name);
                        let limits = limits.map(|_| &attr.limits);
                        Some((
                            attr.name.clone(),
                            database_value(
                                catalog,
                                &attr.pg_type,
                                attr.enum_key.as_deref(),
                                limits,
                                v,
                                &label,
                            ),
                        ))
                    })
                    .collect(),
            ),
            None => J::Object(fields),
        },
        (range, value) if range.range_element().is_some() => {
            let (element, _) = range.range_element().expect("checked");
            scalars::range_literal(&element, &value, label)
        }
        // A multirange literal of its ranges: `{[1,5),[7,9)}`
        (PgType::Multirange(range), value @ J::Array(_)) => {
            database_value_multirange(range, &value, label)
        }
        // GeoJSON, bound as its text
        (spatial, value) if spatial.is_spatial() => scalars::geojson(value, label),
        (_, J::String(name)) if enum_key.is_some() => {
            match enum_key
                .and_then(|k| catalog.find_enum(k))
                .and_then(|e| e.label_of(&name))
            {
                Some(db_label) => J::String(db_label.to_string()),
                None => J::String(name),
            }
        }
        (pg_type, value) => {
            let value = scalars::convert(pg_type, value, label);
            if let Some(limits) = limits.filter(|l| !l.is_empty()) {
                scalars::check_limits(&value, limits, label);
            }
            value
        }
    }
}

/// `text` as a dollar-quoted SQL string literal, whose tag the text does not contain (no
/// escaping applies inside, whatever the server's settings).
fn dollar_quoted(text: &str) -> String {
    let mut n = 0;
    loop {
        let tag = format!("$q{n}$");
        if !text.contains(&tag) {
            return format!("{tag}{text}{tag}");
        }
        n += 1;
    }
}

/// A multirange literal from a list of range inputs.
fn database_value_multirange(
    range: &PgType,
    value: &serde_json::Value,
    label: &str,
) -> serde_json::Value {
    let Some(items) = value.as_array() else {
        return serde_json::Value::Null;
    };
    let (element, _) = range
        .range_element()
        .expect("multiranges are of range types");
    let ranges: Vec<String> = items
        .iter()
        .filter_map(|item| match scalars::range_literal(&element, item, label) {
            serde_json::Value::String(r) => Some(r),
            _ => None,
        })
        .collect();
    serde_json::Value::String(format!("{{{}}}", ranges.join(",")))
}

/// A column value from GraphQL input, as bound to SQL: key-like columns accept Relay global
/// IDs; enum values and composite fields are mapped to the database's; lists become
/// PostgreSQL array literals.
fn input_value(
    catalog: &DatabaseCatalog,
    col: &ColumnMetadata,
    value: serde_json::Value,
) -> serde_json::Value {
    converted_value(catalog, col, value, &col.field_name, None)
}

/// Like [`input_value`], for a value written to the column: checked against its limits and
/// reported at `label` (`input.name`).
fn write_value(
    catalog: &DatabaseCatalog,
    col: &ColumnMetadata,
    value: serde_json::Value,
    label: &str,
) -> serde_json::Value {
    converted_value(catalog, col, value, label, Some(&col.limits))
}

fn converted_value(
    catalog: &DatabaseCatalog,
    col: &ColumnMetadata,
    value: serde_json::Value,
    label: &str,
    limits: Option<&TypeLimits>,
) -> serde_json::Value {
    let key_like = col.is_primary_key || col.name.ends_with("_id") || col.pg_type == PgType::Uuid;
    let value = if key_like {
        unwrap_global_id(value)
    } else {
        value
    };
    let value = database_value(
        catalog,
        &col.pg_type,
        col.enum_key.as_deref(),
        limits,
        value,
        label,
    );
    if col.pg_type.binds_as_array_literal() {
        pg_array_literal(&value)
    } else {
        value
    }
}

/// A key column of `table` by GraphQL name: exposed columns and primary key columns (also
/// when hidden, since row lookups and mutations still address rows by them).
fn key_column_by_field<'t>(
    table: &'t TableMetadata,
    field_name: &str,
) -> Option<&'t ColumnMetadata> {
    table
        .columns
        .iter()
        .find(|c| c.field_name == field_name && (c.is_primary_key || !c.omit))
}

/// `(child column, parent column)` pairs and junction of a relation, as a plan join.
fn join_for(rel: &RelationInfo, parent_alias: &str, unique_target: bool) -> JoinCondition {
    JoinCondition {
        columns: rel.columns.clone(),
        parent_alias: parent_alias.to_string(),
        junction: rel.junction.as_ref().map(|j| JunctionJoin {
            junction_schema: j.junction_schema.clone(),
            junction_table: j.junction_table.clone(),
            source: j.source_columns.clone(),
            target: j.target_columns.clone(),
        }),
        unique_target,
        period: rel.period.clone(),
        batch: false,
    }
}

/// `fields` with every `rowVersion` null: they describe a row that is not stored.
fn without_versions(fields: Vec<SelectedField>) -> Vec<SelectedField> {
    fields
        .into_iter()
        .map(|f| match f {
            SelectedField::RowVersion {
                field_alias,
                key_columns,
                ..
            } => SelectedField::Column {
                field_alias,
                column_name: key_columns.into_iter().next().unwrap_or_default(),
                output: ValueOutput::Template {
                    sql: "NULL::text".to_string(),
                    list: false,
                },
            },
            SelectedField::OnAlias { table_alias, field } => {
                let field = without_versions(vec![*field])
                    .pop()
                    .expect("one field in, one out");
                SelectedField::OnAlias {
                    table_alias,
                    field: Box::new(field),
                }
            }
            other => other,
        })
        .collect()
}

fn without_versions_poly(poly: RowPolymorphism) -> RowPolymorphism {
    let set = |s: TypedFieldSet| TypedFieldSet {
        fields: without_versions(s.fields),
        polymorphism: s.polymorphism.map(|p| Box::new(without_versions_poly(*p))),
        ..s
    };
    match poly {
        RowPolymorphism::SingleTableInheritance {
            discriminator_column,
            variants,
            fallback,
        } => RowPolymorphism::SingleTableInheritance {
            discriminator_column,
            variants: variants
                .into_iter()
                .map(|v| StiBranch {
                    set: set(v.set),
                    ..v
                })
                .collect(),
            fallback: fallback.map(set),
        },
        RowPolymorphism::ConcreteTableInheritance { children, fallback } => {
            RowPolymorphism::ConcreteTableInheritance {
                children: children
                    .into_iter()
                    .map(|c| CtiRowEntry {
                        set: set(c.set),
                        ..c
                    })
                    .collect(),
                fallback: fallback.map(set),
            }
        }
    }
}

/// `period @> <asOf>`: the versions of a table with a temporal key valid at the `asOf` point.
fn as_of_condition(table: &TableMetadata, value: serde_json::Value) -> Option<WhereCondition> {
    let key = table.period_key()?;
    let (element, _) = table.get_column(&key.period)?.pg_type.range_element()?;
    if value.is_null() {
        return None;
    }
    Some(WhereCondition {
        column: key.period,
        operator: "@>".to_string(),
        pg_cast: element.sql_cast(),
        value: scalars::convert(&element, value, "asOf"),
    })
}

/// Default order of rows of a table with a temporal key: a single row is its latest version,
/// lists go by entity, then period.
fn version_order(table: &TableMetadata, alias: &str, single: bool) -> Option<String> {
    let key = table.period_key()?;
    let period = format!("{alias}.{}", quote_ident(&key.period));
    if single {
        return Some(format!("{period} DESC"));
    }
    let mut items: Vec<String> = key
        .entity
        .iter()
        .map(|c| format!("{alias}.{} ASC", quote_ident(c)))
        .collect();
    items.push(format!("{period} ASC"));
    Some(items.join(", "))
}

struct PlanningContext<'a> {
    planner: &'a QueryPlanner<'a>,
    /// Relation steps already planned, by (parent alias, field pointer address). The same field
    /// reached again for the same parent row (e.g. an interface field under every concrete
    /// type) reuses the step, and the compiler emits its subquery once.
    step_memo: RefCell<HashMap<(String, usize), StepId>>,
    /// Table alias per distinct to-one join (parent alias + foreign key), so fields following
    /// the same foreign key (`author`, `a2: author`, two field names for one FK) share a join.
    join_aliases: RefCell<HashMap<String, String>>,
}

impl<'a> PlanningContext<'a> {
    fn catalog(&self) -> &'a DatabaseCatalog {
        self.planner.catalog
    }

    fn find_table(&self, schema: &str, table: &str) -> Result<&'a TableMetadata, PlannerError> {
        self.catalog()
            .find_table(schema, table)
            .ok_or_else(|| PlannerError::TableNotFound(table.to_string()))
    }

    fn plan(&self, operation: &RequestOperation) -> Result<OperationPlan, PlannerError> {
        let mut plan = OperationPlan::new();
        let mut alias_counter = 0;
        let schema = self.planner.default_schema.clone();
        scalars::start();

        match operation.kind {
            OperationKind::Query => {
                for field in &operation.root_fields {
                    let field_name = field.name.as_str();
                    if field_name.starts_with("__") {
                        continue;
                    }
                    let root_alias = field.response_key().to_string();

                    if let Some((_, func)) = self
                        .catalog()
                        .resolve_function_query(&schema, field_name)
                        .or_else(|| self.catalog().resolve_function_query_any(field_name))
                    {
                        let step_id = self.plan_function(
                            &mut plan,
                            field,
                            func,
                            false,
                            None,
                            &mut alias_counter,
                        )?;
                        plan.add_root_field(root_alias, step_id);
                        continue;
                    }

                    if field_name == "node" {
                        let step_id = self.plan_node(&mut plan, field, &mut alias_counter)?;
                        plan.add_root_field(root_alias, step_id);
                        continue;
                    }

                    if field_name == "_service" {
                        let step_id = self.plan_service(&mut plan)?;
                        plan.add_root_field(root_alias, step_id);
                        continue;
                    }

                    if field_name == "_entities" {
                        let step_id = self.plan_entities(&mut plan, field, &mut alias_counter)?;
                        plan.add_root_field(root_alias, step_id);
                        continue;
                    }

                    if Some(field_name)
                        == self
                            .catalog()
                            .global_search_field(&schema)
                            .or_else(|| self.catalog().global_search_field_any())
                    {
                        let step_id =
                            self.plan_global_search(&mut plan, field, &mut alias_counter)?;
                        plan.add_root_field(root_alias, step_id);
                        continue;
                    }

                    let search = match self.catalog().resolve_search_field(&schema, field_name) {
                        Some((table_name, method)) => Some((schema.as_str(), table_name, method)),
                        None => self.catalog().resolve_search_field_any(field_name),
                    };
                    if let Some((search_schema, table_name, method)) = search {
                        let step_id = self.plan_search(
                            &mut plan,
                            field,
                            search_schema,
                            table_name,
                            method,
                            &mut alias_counter,
                        )?;
                        plan.add_root_field(root_alias, step_id);
                        continue;
                    }

                    let (field_schema, table_name, is_single, is_connection) =
                        self.resolve_table_name(field_name)?;
                    let step_id = if is_connection {
                        self.plan_connection(
                            &mut plan,
                            field,
                            &field_schema,
                            &table_name,
                            None,
                            None,
                            &mut alias_counter,
                        )?
                    } else {
                        self.plan_field(
                            &mut plan,
                            field,
                            &field_schema,
                            &table_name,
                            is_single,
                            None,
                            None,
                            &mut alias_counter,
                        )?
                    };
                    plan.add_root_field(root_alias, step_id);
                }
            }
            OperationKind::Mutation => {
                for field in &operation.root_fields {
                    if field.name.starts_with("__") {
                        continue;
                    }
                    let root_alias = field.response_key().to_string();
                    let step_id = match self
                        .catalog()
                        .resolve_function_mutation(&schema, field.name.as_str())
                        .or_else(|| {
                            self.catalog()
                                .resolve_function_mutation_any(field.name.as_str())
                        }) {
                        Some((_, func)) => self.plan_function(
                            &mut plan,
                            field,
                            func,
                            true,
                            None,
                            &mut alias_counter,
                        )?,
                        None => self.plan_mutation(&mut plan, field, &mut alias_counter)?,
                    };
                    plan.add_root_field(root_alias, step_id);
                }
            }
        }

        let invalid = scalars::finish();
        if !invalid.is_empty() {
            return Err(PlannerError::Input(invalid));
        }
        crate::plan::limits::enforce(&mut plan, &self.planner.limits)?;
        Ok(plan)
    }

    /// Relay `node(id: ID!)`: decodes `base64("<Type>:<pk>")` and selects that row, or null
    /// for IDs that do not name a row of a known type.
    fn plan_node(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        alias_counter: &mut usize,
    ) -> Result<StepId, PlannerError> {
        let null = |plan: &mut OperationPlan| {
            Ok(plan.add_step(Step::Constant {
                value: serde_json::Value::Null,
            }))
        };
        let id_val = field.argument("id").cloned();
        let Some(serde_json::Value::String(global_id)) = id_val else {
            return null(plan);
        };
        let Some((type_name, raw_pk)) = SchemaBuilder::decode_global_id(&global_id) else {
            return null(plan);
        };
        let default_schema = &self.planner.default_schema;
        let (schema, table_name) = match self.catalog().table_for_type(default_schema, &type_name) {
            Some(t) => (default_schema.as_str(), t),
            None => match self.catalog().table_for_type_any(&type_name) {
                Some((s, t)) => (s, t),
                None => return null(plan),
            },
        };
        let table_meta = self.find_table(schema, table_name)?;
        // A type without root reads (`omit_operations: [read]`) is reached through its
        // relations only: a global ID, which anyone can write, must not reach it either
        if !table_meta.operations.read {
            return null(plan);
        }
        let [pk_col] = table_meta.primary_key.as_slice() else {
            return null(plan);
        };
        let pk_cast = column_cast(table_meta, pk_col);

        // A malformed UUID would fail the cast in SQL: no such node
        if pk_cast.as_deref() == Some("uuid") && uuid::Uuid::parse_str(&raw_pk).is_err() {
            return null(plan);
        }
        let table_alias = format!("t{}", *alias_counter);
        *alias_counter += 1;
        let (node_fields, node_polymorphism) = self.plan_row_shape(
            plan,
            &field.children,
            schema,
            table_name,
            &table_alias,
            table_meta,
            alias_counter,
        )?;

        let mut pg_select = Step::pg_select(schema, table_name, &table_alias, true);
        if let Step::PgSelect {
            fields,
            where_conditions,
            polymorphism,
            ..
        } = &mut pg_select
        {
            *fields = node_fields;
            *polymorphism = node_polymorphism;
            where_conditions.push(WhereCondition {
                column: pk_col.clone(),
                operator: "=".to_string(),
                value: serde_json::Value::String(raw_pk),
                pg_cast: pk_cast,
            });
        }
        Ok(plan.add_step(pg_select))
    }

    /// Apollo Federation `_service`: returns `{ sdl: "..." }` containing the subgraph SDL.
    fn plan_service(&self, plan: &mut OperationPlan) -> Result<StepId, PlannerError> {
        let sdl = SchemaBuilder::generate_federation_subgraph_sdl(self.catalog());
        let val = serde_json::json!({
            "sdl": sdl
        });
        Ok(plan.add_step(Step::Constant { value: val }))
    }

    /// Apollo Federation `_entities(representations: [_Any!]!)`: resolves a heterogeneous list
    /// of entity representations in a single execution step.
    /// The columns and values that identify the row a representation names: its primary
    /// key, else a unique key it gives every column of. `None` when it gives neither.
    fn entity_key(
        &self,
        map: &serde_json::Map<String, serde_json::Value>,
        table_meta: &TableMetadata,
    ) -> Option<Vec<WhereCondition>> {
        let mut where_conditions = Vec::new();
        let mut matched_key = false;

        if !table_meta.primary_key.is_empty() {
            let mut all_found = true;
            let mut conds = Vec::new();
            for pk_col_name in &table_meta.primary_key {
                let Some(pk_col) = table_meta.get_column(pk_col_name) else {
                    all_found = false;
                    break;
                };

                let val = map
                    .get(&pk_col.field_name)
                    .or_else(|| map.get(&pk_col.name))
                    .or_else(|| {
                        if table_meta.primary_key.len() == 1 {
                            map.get("id")
                        } else {
                            None
                        }
                    });

                let Some(v) = val else {
                    all_found = false;
                    break;
                };

                let unwrapped = unwrap_global_id(v.clone());
                let pk_cast = column_cast(table_meta, pk_col_name);
                if pk_cast.as_deref() == Some("uuid") {
                    if let Some(s) = unwrapped.as_str() {
                        if uuid::Uuid::parse_str(s).is_err() {
                            all_found = false;
                            break;
                        }
                    } else {
                        all_found = false;
                        break;
                    }
                }

                conds.push(WhereCondition {
                    column: pk_col_name.clone(),
                    operator: "=".to_string(),
                    value: unwrapped,
                    pg_cast: pk_cast,
                });
            }

            if all_found {
                where_conditions = conds;
                matched_key = true;
            }
        }

        if !matched_key {
            for (_, unique_cols) in self.catalog().unique_lookup_fields(table_meta) {
                let mut all_found = true;
                let mut conds = Vec::new();
                for col in &unique_cols {
                    let val = map.get(&col.field_name).or_else(|| map.get(&col.name));
                    let Some(v) = val else {
                        all_found = false;
                        break;
                    };
                    let unwrapped = unwrap_global_id(v.clone());
                    let cast = column_cast(table_meta, &col.name);
                    if cast.as_deref() == Some("uuid") {
                        if let Some(s) = unwrapped.as_str() {
                            if uuid::Uuid::parse_str(s).is_err() {
                                all_found = false;
                                break;
                            }
                        } else {
                            all_found = false;
                            break;
                        }
                    }
                    conds.push(WhereCondition {
                        column: col.name.clone(),
                        operator: "=".to_string(),
                        value: unwrapped,
                        pg_cast: cast,
                    });
                }
                if all_found && !conds.is_empty() {
                    where_conditions = conds;
                    matched_key = true;
                    break;
                }
            }
        }

        (matched_key && !where_conditions.is_empty()).then_some(where_conditions)
    }

    /// `_entities(representations: ...)`: the entity each representation names, in order.
    /// Representations of one type keyed by the same columns become one group, looked up in
    /// one join against their keys, so a batch costs a lookup per type rather than per
    /// representation.
    fn plan_entities(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        alias_counter: &mut usize,
    ) -> Result<StepId, PlannerError> {
        let reps = match field.argument("representations") {
            Some(serde_json::Value::Array(arr)) => arr.clone(),
            _ => Vec::new(),
        };
        let schema = &self.planner.default_schema;

        /// Representations of one type keyed by the same columns.
        struct RepGroup {
            /// `(type, schema, table, key columns)`.
            key: (String, String, String, Vec<String>),
            casts: Vec<String>,
            rows: Vec<(usize, Vec<serde_json::Value>)>,
        }
        let mut groups: Vec<RepGroup> = Vec::new();
        let mut singles = Vec::new();
        for (position, rep) in reps.iter().enumerate() {
            let Some(map) = rep.as_object() else { continue };
            let Some(type_name) = map.get("__typename").and_then(|v| v.as_str()) else {
                continue;
            };
            let (entity_schema, table_name) = match self.catalog().table_for_type(schema, type_name)
            {
                Some(t) => (schema.as_str(), t),
                None => match self.catalog().table_for_type_any(type_name) {
                    Some(found) => found,
                    None => continue,
                },
            };
            let Ok(table_meta) = self.find_table(entity_schema, table_name) else {
                continue;
            };
            // Like `node(id:)`: no lookup of a type whose reads are omitted
            if !table_meta.operations.read {
                continue;
            }
            let Some(key) = self.entity_key(map, table_meta) else {
                continue;
            };

            // A key without a safe cast (json, user-defined types) is looked up on its own
            let casts: Option<Vec<String>> = key.iter().map(|c| c.pg_cast.clone()).collect();
            let Some(casts) = casts else {
                let step = self.plan_entity_lookup(
                    plan,
                    field,
                    type_name,
                    entity_schema,
                    table_name,
                    table_meta,
                    Lookup::Values(key),
                    alias_counter,
                )?;
                if let Some(step) = step {
                    singles.push((position, step));
                }
                continue;
            };
            let group_key = (
                type_name.to_string(),
                entity_schema.to_string(),
                table_name.to_string(),
                key.iter().map(|c| c.column.clone()).collect(),
            );
            let values = key.into_iter().map(|c| c.value).collect();
            match groups.iter_mut().find(|g| g.key == group_key) {
                Some(group) => group.rows.push((position, values)),
                None => groups.push(RepGroup {
                    key: group_key,
                    casts,
                    rows: vec![(position, values)],
                }),
            }
        }

        let mut entity_groups = Vec::new();
        for RepGroup {
            key: (type_name, entity_schema, table_name, columns),
            casts: key_casts,
            rows,
        } in groups
        {
            let table_meta = self.find_table(&entity_schema, &table_name)?;
            let keys_alias = format!("r{}", *alias_counter);
            *alias_counter += 1;
            let join = JoinCondition {
                columns: columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (c.clone(), format!("k{i}")))
                    .collect(),
                parent_alias: keys_alias.clone(),
                batch: false,
                junction: None,
                unique_target: true,
                period: None,
            };
            let step = self.plan_entity_lookup(
                plan,
                field,
                &type_name,
                &entity_schema,
                &table_name,
                table_meta,
                Lookup::Join(join),
                alias_counter,
            )?;
            if let Some(step) = step {
                entity_groups.push(EntityGroup {
                    keys_alias,
                    key_casts,
                    rows,
                    step,
                });
            }
        }

        Ok(plan.add_step(Step::Entities {
            count: reps.len(),
            groups: entity_groups,
            singles,
        }))
    }

    /// The single-row step reading an entity of `type_name`: its selections (with
    /// `__typename`) from the row `lookup` identifies. `None` when its selections cannot be
    /// planned for the type, which then resolves to null.
    #[allow(clippy::too_many_arguments)]
    fn plan_entity_lookup(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        type_name: &str,
        entity_schema: &str,
        table_name: &str,
        table_meta: &TableMetadata,
        lookup: Lookup,
        alias_counter: &mut usize,
    ) -> Result<Option<StepId>, PlannerError> {
        let table_alias = format!("e{}", *alias_counter);
        *alias_counter += 1;
        let Ok((mut node_fields, node_polymorphism)) = self.plan_row_shape(
            plan,
            &field.children,
            entity_schema,
            table_name,
            &table_alias,
            table_meta,
            alias_counter,
        ) else {
            return Ok(None);
        };
        if node_polymorphism.is_none()
            && !node_fields.iter().any(|f| f.field_alias() == "__typename")
        {
            node_fields.insert(
                0,
                SelectedField::Typename {
                    field_alias: "__typename".to_string(),
                    type_name: type_name.to_string(),
                },
            );
        }
        let mut pg_select = Step::pg_select(entity_schema, table_name, &table_alias, true);
        if let Step::PgSelect {
            fields,
            where_conditions,
            join_condition,
            polymorphism,
            ..
        } = &mut pg_select
        {
            *fields = node_fields;
            *polymorphism = node_polymorphism;
            match lookup {
                Lookup::Values(conditions) => *where_conditions = conditions,
                Lookup::Join(join) => *join_condition = Some(join),
            }
        }
        Ok(Some(plan.add_step(pg_select)))
    }

    /// Plan a GraphQL mutation (create, update, delete of one or many rows) returning
    /// affected fields.
    fn plan_mutation(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        alias_counter: &mut usize,
    ) -> Result<StepId, PlannerError> {
        let field_name = field.name.as_str();
        let table_alias = format!("m{}", *alias_counter);
        *alias_counter += 1;

        let (schema, mutation) = if let Some(m) = self
            .catalog()
            .resolve_mutation_field(&self.planner.default_schema, field_name)
        {
            (self.planner.default_schema.clone(), m)
        } else if let Some((s, m)) = self.catalog().resolve_mutation_field_any(field_name) {
            (s.to_string(), m)
        } else {
            return Err(PlannerError::UnknownRootField(field_name.to_string()));
        };
        let action = mutation.action;
        let table_name = mutation.table.as_str();
        let table_meta = self.find_table(&schema, table_name)?;

        // Payload selections, in selection order (a response key selected twice is one field)
        let rows_field = action.payload_field_name(table_meta);
        let previous_field =
            self.catalog()
                .previous_field_name(table_meta, action, table_meta.has_upsert());
        let mut payload = Vec::new();
        let mut seen = HashSet::new();
        for sub_field in &field.children {
            let field_alias = sub_field.response_key().to_string();
            if !seen.insert(field_alias.clone()) {
                continue;
            }
            match sub_field.name.as_str() {
                "__typename" => payload.push(PayloadField::Typename {
                    field_alias,
                    type_name: action.payload_type_name(table_meta),
                }),
                "affectedCount" => payload.push(PayloadField::AffectedCount { field_alias }),
                name if name == rows_field => {
                    let (fields, polymorphism) = self.plan_row_shape(
                        plan,
                        &sub_field.children,
                        &schema,
                        table_name,
                        &table_alias,
                        table_meta,
                        alias_counter,
                    )?;
                    payload.push(PayloadField::Rows {
                        field_alias,
                        fields,
                        polymorphism,
                    });
                }
                name if Some(name) == previous_field.as_deref() => {
                    let previous_alias = format!("o{}", *alias_counter);
                    *alias_counter += 1;
                    let (fields, polymorphism) = self.plan_row_shape(
                        plan,
                        &sub_field.children,
                        &schema,
                        table_name,
                        &previous_alias,
                        table_meta,
                        alias_counter,
                    )?;
                    // The version of a row's previous state is not stored any more
                    let (fields, polymorphism) = (
                        without_versions(fields),
                        polymorphism.map(without_versions_poly),
                    );
                    payload.push(PayloadField::Previous {
                        field_alias,
                        fields,
                        polymorphism,
                        table_alias: previous_alias,
                    });
                }
                _ => {}
            }
        }

        let arg = |name: &str| field.argument(name).cloned();
        let column_values = |input: Option<serde_json::Value>, path: &str| -> Vec<ColumnValue> {
            let Some(serde_json::Value::Object(input)) = input else {
                return Vec::new();
            };
            input
                .into_iter()
                .filter_map(|(k, v)| {
                    let col = table_meta.writable_columns().find(|c| c.field_name == k)?;
                    Some(self.column_value(col, v, path))
                })
                .collect()
        };
        // The row addressed by a unique key (one argument per column), or by the primary key:
        // `id` for a single-column key (a Relay ID or raw value), else one argument per column
        let key = || -> RowTarget {
            let condition = |col: &ColumnMetadata, value: serde_json::Value| WhereCondition {
                column: col.name.clone(),
                operator: "=".to_string(),
                value: input_value(self.catalog(), col, value),
                pg_cast: col.pg_type.sql_cast(),
            };
            let conditions = if mutation.unique_key.is_empty() {
                table_meta
                    .primary_key
                    .iter()
                    .filter_map(|pk| {
                        let col = table_meta.get_column(pk)?;
                        let value = if table_meta.implements_node() {
                            arg("id")
                        } else {
                            arg(&col.field_name)
                        }?;
                        Some(condition(col, value))
                    })
                    .collect()
            } else {
                mutation
                    .unique_key
                    .iter()
                    .filter_map(|c| {
                        let col = table_meta.get_column(c)?;
                        Some(condition(col, arg(&col.field_name)?))
                    })
                    .collect()
            };
            RowTarget::Key(conditions)
        };
        let filter = |alias_counter: &mut usize| -> Result<RowTarget, PlannerError> {
            let value = arg("where").unwrap_or_default();
            // Every row needs saying so
            if self.planner.limits.require_filter
                && value
                    .as_object()
                    .is_none_or(|conditions| conditions.is_empty())
                && arg("unfiltered").and_then(|v| v.as_bool()) != Some(true)
            {
                return Err(PlannerError::InvalidInput(format!(
                    "{field_name}: `where` has no condition, so it matches every row: add one, or pass `unfiltered: true` to mean it"
                )));
            }
            Ok(RowTarget::Filter(self.plan_filter_object(
                &value,
                &schema,
                table_name,
                &table_alias,
                alias_counter,
            )?))
        };

        let kind = match action {
            MutationAction::Create | MutationAction::CreateMany => {
                let inputs: Vec<(String, serde_json::Value)> = if action == MutationAction::Create {
                    vec![("input".to_string(), arg("input").unwrap_or_default())]
                } else {
                    list_items(arg("inputs"))
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| (format!("inputs[{i}]"), v))
                        .collect()
                };
                let mut nodes = Vec::new();
                let mut roots = Vec::new();
                for (path, input) in inputs {
                    roots.push(self.plan_row_node(
                        &mut nodes,
                        table_meta,
                        input,
                        &path,
                        Vec::new(),
                        None,
                    )?);
                }
                let rows: Vec<&[ColumnValue]> =
                    roots.iter().map(|&r| nodes[r].values.as_slice()).collect();
                let on_conflict =
                    self.plan_on_conflict(arg("onConflict"), table_meta, &rows, action.is_many())?;
                if let Some(oc) = &on_conflict {
                    // An upsert that leaves a conflicting row alone does not return it: rows
                    // nested under it take its key from a lookup by the conflict key instead
                    for &root in &roots {
                        if !nodes.iter().any(|n| n.refs.iter().any(|r| r.node == root)) {
                            continue;
                        }
                        let key = oc
                            .columns
                            .iter()
                            .map(|(column, cast)| WhereCondition {
                                column: column.clone(),
                                operator: "=".to_string(),
                                value: nodes[root]
                                    .values
                                    .iter()
                                    .find(|v| &v.column == column)
                                    .map(|v| v.value.clone())
                                    .unwrap_or_default(),
                                pg_cast: cast.clone(),
                            })
                            .collect();
                        let path = nodes[root].path.clone();
                        let lookup = push_node(
                            &mut nodes,
                            table_meta,
                            WriteNodeKind::Lookup { key },
                            Vec::new(),
                            Vec::new(),
                            path,
                        );
                        nodes[lookup].after.push(root);
                        for node in nodes.iter_mut().take(lookup) {
                            for r in node.refs.iter_mut().filter(|r| r.node == root) {
                                r.node = lookup;
                            }
                        }
                    }
                }
                // The payload of a keyless table comes from the rows its insert returns, in the
                // same statement: nothing can depend on them
                let depends_on_root = nodes
                    .iter()
                    .any(|n| n.refs.iter().any(|r| roots.contains(&r.node)));
                if table_meta.primary_key.is_empty() && depends_on_root {
                    return Err(PlannerError::InvalidInput(format!(
                        "{field_name}: rows of `{}` have no primary key, so nested to-many relations cannot refer to them",
                        table_meta.type_name
                    )));
                }
                MutationKind::Insert {
                    nodes,
                    roots,
                    many: action.is_many(),
                    on_conflict,
                }
            }
            MutationAction::Update => {
                let input = arg("input").unwrap_or_default();
                let nested = self.catalog().nested_relations(table_meta);
                let has_relations = nested
                    .iter()
                    .any(|(name, _)| input.get(name).is_some_and(|v| !v.is_null()));
                if has_relations {
                    // Nested writes: the updated row is the root of a write graph
                    if table_meta.primary_key.is_empty() {
                        return Err(PlannerError::InvalidInput(format!(
                            "{field_name}: rows of `{}` have no primary key, so nested relations cannot refer to them",
                            table_meta.type_name
                        )));
                    }
                    let RowTarget::Key(key) = key() else {
                        unreachable!("single-row updates address a key")
                    };
                    let mut nodes = Vec::new();
                    let root = self.plan_row_node(
                        &mut nodes,
                        table_meta,
                        input,
                        "input",
                        Vec::new(),
                        Some(key),
                    )?;
                    MutationKind::Insert {
                        nodes,
                        roots: vec![root],
                        many: false,
                        on_conflict: None,
                    }
                } else {
                    MutationKind::Update {
                        target: key(),
                        column_values: column_values(Some(input), "input"),
                    }
                }
            }
            MutationAction::UpdateMany => {
                let set = arg("set").unwrap_or_default();
                if let Some((name, _)) = self
                    .catalog()
                    .nested_relations(table_meta)
                    .iter()
                    .find(|(name, _)| set.get(name).is_some_and(|v| !v.is_null()))
                {
                    return Err(PlannerError::InvalidInput(format!(
                        "{field_name}: `set.{name}`: nested relations are written by single-row updates only"
                    )));
                }
                let column_values = column_values(Some(set), "set");
                // An empty `set` rewrites a key column onto itself to return the rows
                if column_values.is_empty() && table_meta.primary_key.is_empty() {
                    return Err(PlannerError::InvalidInput(format!(
                        "{field_name}: `set` must set at least one field"
                    )));
                }
                MutationKind::Update {
                    target: filter(alias_counter)?,
                    column_values,
                }
            }
            MutationAction::Delete => MutationKind::Delete { target: key() },
            MutationAction::DeleteMany => MutationKind::Delete {
                target: filter(alias_counter)?,
            },
        };

        // `expectedVersion`: the row is written only if its version is still this one
        let version_guard = match (action, arg("expectedVersion")) {
            (
                MutationAction::Update | MutationAction::Delete,
                Some(serde_json::Value::String(expected)),
            ) if table_meta.has_row_version() => {
                let RowTarget::Key(key) = key() else {
                    unreachable!("single-row writes address a key")
                };
                Some(VersionGuard {
                    key,
                    column: table_meta.row_version_column().to_string(),
                    expected,
                })
            }
            _ => None,
        };
        // An update or delete by filter may affect at most `limits.bulk_rows` rows: its
        // transaction checks the count the payload reports, adding the field when not selected
        let affected_limit = match (action, self.planner.limits.bulk_rows) {
            (MutationAction::UpdateMany | MutationAction::DeleteMany, Some(max)) => {
                let existing = payload.iter().find_map(|f| match f {
                    PayloadField::AffectedCount { field_alias } => Some(field_alias.clone()),
                    _ => None,
                });
                let hidden = existing.is_none();
                let alias = existing.unwrap_or_else(|| "__affectedCount".to_string());
                if hidden {
                    payload.push(PayloadField::AffectedCount {
                        field_alias: alias.clone(),
                    });
                }
                Some(crate::plan::AffectedLimit {
                    max: max as u64,
                    alias,
                    hidden,
                })
            }
            _ => None,
        };
        let step = Step::PgMutation {
            schema: schema.clone(),
            table: table_name.to_string(),
            table_alias,
            kind,
            many: action.is_many(),
            payload,
            key_columns: table_meta
                .primary_key
                .iter()
                .map(|pk| (pk.clone(), column_cast(table_meta, pk)))
                .collect(),
            version_guard,
            affected_limit,
        };
        Ok(plan.add_step(step))
    }

    /// `onConflict: { key, action, update }` of a create. Every row must set the conflict
    /// key's columns: the payload reads the rows back by them.
    fn plan_on_conflict(
        &self,
        value: Option<serde_json::Value>,
        table: &TableMetadata,
        rows: &[&[ColumnValue]],
        many: bool,
    ) -> Result<Option<OnConflict>, PlannerError> {
        let Some(serde_json::Value::Object(obj)) = value else {
            return Ok(None);
        };
        let invalid = |msg: String| PlannerError::InvalidInput(msg);
        let key_name = obj
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or("PRIMARY_KEY");
        let (_, columns) = table
            .conflict_keys()
            .find(|(k, cols)| conflict_key_value(k, cols) == key_name)
            .ok_or_else(|| {
                invalid(format!(
                    "onConflict: `{}` has no unique key {key_name}",
                    table.type_name
                ))
            })?;
        let update = match obj.get("update") {
            Some(serde_json::Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|v| {
                        let name = v.as_str().unwrap_or_default();
                        table
                            .writable_columns()
                            .find(|c| c.order_by_name() == name)
                            .map(|c| c.name.clone())
                            .ok_or_else(|| {
                                invalid(format!(
                                    "onConflict.update: `{}` has no writable field {name}",
                                    table.type_name
                                ))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            _ => None,
        };
        for (i, row) in rows.iter().enumerate() {
            for col in &columns {
                if !row
                    .iter()
                    .any(|cv| cv.column == col.name && !cv.value.is_null())
                {
                    let input = if many {
                        format!("inputs[{i}]")
                    } else {
                        "input".to_string()
                    };
                    return Err(invalid(format!(
                        "onConflict key {key_name}: `{input}` must set `{}` (rows are matched by it)",
                        col.field_name
                    )));
                }
            }
        }
        Ok(Some(OnConflict {
            // Rows are matched with the column type's own equality, as the unique index does
            // (e.g. case-insensitively for `citext`)
            columns: columns
                .iter()
                .map(|c| (c.name.clone(), Some(c.pg_type.assignment_cast())))
                .collect(),
            update,
            do_nothing: obj.get("action").and_then(|v| v.as_str()) == Some("NOTHING"),
        }))
    }

    /// A column assignment from a GraphQL input value at `path` (`input`: the value is
    /// `input.<field>`).
    fn column_value(
        &self,
        col: &ColumnMetadata,
        value: serde_json::Value,
        path: &str,
    ) -> ColumnValue {
        let label = if path.is_empty() {
            col.field_name.clone()
        } else {
            format!("{path}.{}", col.field_name)
        };
        ColumnValue {
            column: col.name.clone(),
            pg_cast: Some(col.pg_type.assignment_cast()),
            composite: matches!(col.pg_type, PgType::Composite(_)),
            composite_array: matches!(&col.pg_type, PgType::Array(e) if matches!(**e, PgType::Composite(_))),
            value: write_value(self.catalog(), col, value, &label),
        }
    }

    /// Plans the write of one row of `table` at `path` in the input, and of the rows its
    /// nested relations create, connect or detach, into `nodes`; returns its node. The row
    /// is inserted (a `Create<Type>Input`), or with `update` the existing row with that key
    /// is updated (an `Update<Type>Input`).
    ///
    /// To-one relations come first (the parent row is created, or found, and fills this row's
    /// foreign key; an update can also `disconnect` it); to-many and many-to-many relations
    /// after (their rows, or junction rows, take this row's key; an update can also
    /// `disconnect` rows or `set` the related rows to exactly those listed). `filled`:
    /// foreign key columns the parent row fills, when this row is created through a to-many
    /// relation.
    fn plan_row_node(
        &self,
        nodes: &mut Vec<WriteNode>,
        table: &TableMetadata,
        input: serde_json::Value,
        path: &str,
        filled: Vec<NodeRef>,
        update: Option<Vec<WhereCondition>>,
    ) -> Result<usize, PlannerError> {
        let invalid = |msg: String| PlannerError::InvalidInput(msg);
        if update.is_none() && !table.operations.create {
            return Err(invalid(format!(
                "`{path}`: `{}` rows cannot be created",
                table.type_name
            )));
        }
        let nested = self.catalog().nested_relations(table);
        let mut values = Vec::new();
        let mut relations = Vec::new();
        if let serde_json::Value::Object(input) = input {
            for (k, v) in input {
                if let Some(col) = table.writable_columns().find(|c| c.field_name == k) {
                    values.push(self.column_value(col, v, path));
                } else if let Some((name, rel)) = nested.iter().find(|(n, _)| *n == k) {
                    if !v.is_null() {
                        relations.push((name.clone(), rel.clone(), v));
                    }
                }
            }
        }
        let field_of = |column: &str| {
            table
                .get_column(column)
                .map(|c| c.field_name.clone())
                .unwrap_or_else(|| column.to_string())
        };
        let mut refs = filled;
        let mut set_by_relation: Vec<(String, String)> = refs
            .iter()
            .map(|r| (r.column.clone(), "the parent row".to_string()))
            .collect();

        // To-one relations: `{ connect: <key> }`, `{ create: <input> }`, `{ connectOrCreate:
        // { where, create } }`, or (updates) `{ disconnect: true }` and `{ update: <input> }`
        for (name, rel, value) in relations.iter().filter(|(_, rel, _)| !rel.is_many) {
            let target = self.find_table(&rel.target_schema, &rel.target_table)?;
            let at = format!("{path}.{name}");
            let (connect, create) = one_of(value);
            let fk_cast = |fk: &str| table.get_column(fk).map(|c| c.pg_type.assignment_cast());
            let disconnect =
                update.is_some() && value.get("disconnect").and_then(|v| v.as_bool()) == Some(true);
            let connect_or_create = value.get("connectOrCreate").filter(|v| !v.is_null());
            let update_referenced = if update.is_some() {
                value.get("update").filter(|v| !v.is_null())
            } else {
                None
            };
            let chosen = [
                connect.is_some(),
                create.is_some(),
                disconnect,
                connect_or_create.is_some(),
                update_referenced.is_some(),
            ];
            if chosen.iter().filter(|c| **c).count() != 1 {
                let options = if update.is_some() {
                    "`connect`, `create`, `connectOrCreate`, `update` and `disconnect`"
                } else {
                    "`connect`, `create` and `connectOrCreate`"
                };
                return Err(invalid(format!("`{at}` must set exactly one of {options}")));
            }
            if let Some(data) = update_referenced {
                // The row it refers to now is updated (its foreign key is unchanged)
                let key = update.clone().expect("updates only");
                let values = self.plain_values(target, data, &format!("{at}.update"), false)?;
                if !values.is_empty() {
                    let kind = WriteNodeKind::UpdateReferenced {
                        from_schema: table.schema.clone(),
                        from_table: table.name.clone(),
                        from_key: key,
                        columns: rel.columns.clone(),
                    };
                    push_node(
                        nodes,
                        target,
                        kind,
                        values,
                        Vec::new(),
                        format!("{at}.update"),
                    );
                }
                continue;
            }
            if let Some(coc) = connect_or_create {
                let at = format!("{at}.connectOrCreate");
                let key = self.plan_row_key(
                    target,
                    coc.get("where").unwrap_or(&serde_json::Value::Null),
                    &format!("{at}.where"),
                )?;
                let values = self.plain_values(
                    target,
                    coc.get("create").unwrap_or(&serde_json::Value::Null),
                    &format!("{at}.create"),
                    true,
                )?;
                let found = push_node(
                    nodes,
                    target,
                    WriteNodeKind::LookupOrInsert { key },
                    values,
                    Vec::new(),
                    at,
                );
                refs.extend(rel.columns.iter().map(|(target_col, fk)| NodeRef {
                    column: fk.clone(),
                    pg_cast: fk_cast(fk),
                    node: found,
                    source_column: target_col.clone(),
                }));
                set_by_relation.extend(
                    rel.columns
                        .iter()
                        .map(|(_, fk)| (fk.clone(), format!("`{name}`"))),
                );
                continue;
            }
            if disconnect {
                for (_, fk) in &rel.columns {
                    if table.get_column(fk).is_some_and(|c| !c.is_nullable) {
                        return Err(invalid(format!("`{at}.disconnect`: `{}` is required, so the row cannot be disconnected", field_of(fk))));
                    }
                    values.push(ColumnValue {
                        column: fk.clone(),
                        pg_cast: fk_cast(fk),
                        composite: false,
                        composite_array: false,
                        value: serde_json::Value::Null,
                    });
                }
                set_by_relation.extend(
                    rel.columns
                        .iter()
                        .map(|(_, fk)| (fk.clone(), format!("`{name}`"))),
                );
                continue;
            }
            match (connect, create) {
                (Some(key), None) => {
                    let key = self.plan_row_key(target, key, &format!("{at}.connect"))?;
                    // Connected by the referenced columns themselves: no need to read the row
                    // (the foreign key constraint checks it exists)
                    let direct = rel.columns.len() == key.len()
                        && rel
                            .columns
                            .iter()
                            .all(|(t, _)| key.iter().any(|k| &k.column == t));
                    if direct {
                        for (target_col, fk) in &rel.columns {
                            let cond = key
                                .iter()
                                .find(|k| &k.column == target_col)
                                .expect("checked above");
                            values.push(ColumnValue {
                                column: fk.clone(),
                                pg_cast: fk_cast(fk),
                                composite: false,
                                composite_array: false,
                                value: cond.value.clone(),
                            });
                        }
                    } else {
                        let lookup = push_node(
                            nodes,
                            target,
                            WriteNodeKind::Lookup { key },
                            Vec::new(),
                            Vec::new(),
                            format!("{at}.connect"),
                        );
                        refs.extend(rel.columns.iter().map(|(target_col, fk)| NodeRef {
                            column: fk.clone(),
                            pg_cast: fk_cast(fk),
                            node: lookup,
                            source_column: target_col.clone(),
                        }));
                    }
                }
                (None, Some(input)) => {
                    let parent = self.plan_row_node(
                        nodes,
                        target,
                        input.clone(),
                        &format!("{at}.create"),
                        Vec::new(),
                        None,
                    )?;
                    refs.extend(rel.columns.iter().map(|(target_col, fk)| NodeRef {
                        column: fk.clone(),
                        pg_cast: fk_cast(fk),
                        node: parent,
                        source_column: target_col.clone(),
                    }));
                }
                _ => {
                    return Err(invalid(format!(
                        "`{at}` must set exactly one of `connect` and `create`"
                    )))
                }
            }
            set_by_relation.extend(
                rel.columns
                    .iter()
                    .map(|(_, fk)| (fk.clone(), format!("`{name}`"))),
            );
        }

        // A foreign key column is set once: by its field, its relation or the parent row
        let mut seen: Vec<&str> = Vec::new();
        for column in values
            .iter()
            .map(|v| v.column.as_str())
            .chain(refs.iter().map(|r| r.column.as_str()))
        {
            if seen.contains(&column) {
                let by = set_by_relation
                    .iter()
                    .filter(|(c, _)| c == column)
                    .map(|(_, by)| by.as_str())
                    .collect::<Vec<_>>();
                return Err(invalid(format!(
                    "`{path}` sets `{}` both directly and through {}",
                    field_of(column),
                    by.join(" and ")
                )));
            }
            seen.push(column);
        }
        // Foreign key columns a relation can fill are optional in the input type: required
        // ones must be set one way or the other (an updated row has them already)
        for (name, rel) in nested
            .iter()
            .filter(|(_, rel)| !rel.is_many && update.is_none())
        {
            for (_, fk) in &rel.columns {
                let required = table
                    .get_column(fk)
                    .is_some_and(|c| !c.is_nullable && c.default_value.is_none());
                if required && !seen.contains(&fk.as_str()) {
                    return Err(invalid(format!(
                        "`{path}` must set `{}` or `{name}`",
                        field_of(fk)
                    )));
                }
            }
        }
        let kind = match update {
            None => WriteNodeKind::Insert,
            // Nothing to set: the row is only read, for the key its relations take
            Some(key) if values.is_empty() && refs.is_empty() => WriteNodeKind::Lookup { key },
            Some(key) => WriteNodeKind::Update { key },
        };
        let updating = !matches!(kind, WriteNodeKind::Insert);
        let node = push_node(nodes, table, kind, values, refs, path.to_string());

        // To-many and many-to-many relations: `{ create: [<input>], connect: [<key>] }`, and
        // for updates `disconnect: [<key>]` and `set: [<key>]`
        for (name, rel, value) in relations.iter().filter(|(_, rel, _)| rel.is_many) {
            let target = self.find_table(&rel.target_schema, &rel.target_table)?;
            let at = format!("{path}.{name}");
            let creates = list_items(value.get("create").cloned());
            let item_list = |field: &str| {
                if updating {
                    value
                        .get(field)
                        .filter(|v| !v.is_null())
                        .map(|v| list_items(Some(v.clone())))
                } else {
                    None
                }
            };
            let set = item_list("set");
            let disconnects = item_list("disconnect").unwrap_or_default();
            // `set` connects exactly the rows it lists, detaching the others
            let connects: Vec<(String, serde_json::Value)> = match &set {
                Some(items) => items
                    .iter()
                    .enumerate()
                    .map(|(i, k)| (format!("{at}.set[{i}]"), k.clone()))
                    .collect(),
                None => list_items(value.get("connect").cloned())
                    .into_iter()
                    .enumerate()
                    .map(|(i, k)| (format!("{at}.connect[{i}]"), k))
                    .collect(),
            };
            if set.is_some() && value.get("connect").is_some_and(|v| !v.is_null()) {
                return Err(invalid(format!(
                    "`{at}` cannot set both `set` and `connect`"
                )));
            }
            let keys_of = |items: &[(String, serde_json::Value)]| -> Result<Vec<Vec<WhereCondition>>, PlannerError> {
                items.iter().map(|(at, k)| self.plan_row_key(target, k, at)).collect()
            };
            let disconnects: Vec<(String, serde_json::Value)> = disconnects
                .into_iter()
                .enumerate()
                .map(|(i, k)| (format!("{at}.disconnect[{i}]"), k))
                .collect();
            let connect_or_create: Vec<(String, serde_json::Value)> =
                list_items(value.get("connectOrCreate").cloned())
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| (format!("{at}.connectOrCreate[{i}]"), v))
                    .collect();
            let child_updates: Vec<(String, serde_json::Value)> = item_list("update")
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(i, v)| (format!("{at}.update[{i}]"), v))
                .collect();
            let child_deletes: Vec<(String, serde_json::Value)> = item_list("delete")
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(i, v)| (format!("{at}.delete[{i}]"), v))
                .collect();
            let sync = item_list("sync");
            if sync.is_some() {
                let others = [
                    "create",
                    "connect",
                    "disconnect",
                    "set",
                    "connectOrCreate",
                    "update",
                    "delete",
                ];
                if let Some(other) = others
                    .iter()
                    .find(|o| value.get(**o).is_some_and(|v| !v.is_null()))
                {
                    return Err(invalid(format!("`{at}`: `sync` makes the related rows exactly those listed, so it cannot be combined with `{other}`")));
                }
                if self.catalog().server_version < 170000 {
                    return Err(invalid(format!(
                        "`{at}.sync` needs PostgreSQL 17 (`MERGE ... WHEN NOT MATCHED BY SOURCE`)"
                    )));
                }
            }
            match &rel.junction {
                None => {
                    // Children take this row's key in their foreign key
                    let link = |node: usize| -> Vec<NodeRef> {
                        rel.columns
                            .iter()
                            .map(|(child_col, col)| NodeRef {
                                column: child_col.clone(),
                                pg_cast: target
                                    .get_column(child_col)
                                    .map(|c| c.pg_type.assignment_cast()),
                                node,
                                source_column: col.clone(),
                            })
                            .collect()
                    };
                    // Detaching comes first, so it leaves the rows created or connected alone
                    let detaching = set.is_some() || !disconnects.is_empty();
                    if (!connects.is_empty() || detaching) && !target.operations.update {
                        return Err(invalid(format!(
                            "`{at}`: `{}` rows cannot be updated to connect or disconnect them",
                            target.type_name
                        )));
                    }
                    if detaching {
                        if let Some((child_col, _)) = rel
                            .columns
                            .iter()
                            .find(|(c, _)| target.get_column(c).is_some_and(|c| !c.is_nullable))
                        {
                            let field = target
                                .get_column(child_col)
                                .map(|c| c.field_name.clone())
                                .unwrap_or_default();
                            return Err(invalid(format!(
                                "`{at}`: `{}.{field}` is required, so rows cannot be disconnected (delete them instead)",
                                target.type_name
                            )));
                        }
                    }
                    if set.is_some() {
                        let except = keys_of(&connects)?;
                        let detach = WriteNodeKind::Detach {
                            key: None,
                            except,
                            through: None,
                        };
                        push_node(
                            nodes,
                            target,
                            detach,
                            Vec::new(),
                            link(node),
                            format!("{at}.set"),
                        );
                    }
                    for (at, key) in &disconnects {
                        let detach = WriteNodeKind::Detach {
                            key: Some(self.plan_row_key(target, key, at)?),
                            except: Vec::new(),
                            through: None,
                        };
                        push_node(nodes, target, detach, Vec::new(), link(node), at.clone());
                    }
                    for (i, input) in creates.into_iter().enumerate() {
                        self.plan_row_node(
                            nodes,
                            target,
                            input,
                            &format!("{at}.create[{i}]"),
                            link(node),
                            None,
                        )?;
                    }
                    for (at, key) in &connects {
                        let key = self.plan_row_key(target, key, at)?;
                        push_node(
                            nodes,
                            target,
                            WriteNodeKind::Update { key },
                            Vec::new(),
                            link(node),
                            at.clone(),
                        );
                    }
                    for (at, item) in &connect_or_create {
                        let key = self.plan_row_key(
                            target,
                            item.get("where").unwrap_or(&serde_json::Value::Null),
                            &format!("{at}.where"),
                        )?;
                        let values = self.plain_values(
                            target,
                            item.get("create").unwrap_or(&serde_json::Value::Null),
                            &format!("{at}.create"),
                            true,
                        )?;
                        let values = values
                            .into_iter()
                            .filter(|v| !rel.columns.iter().any(|(c, _)| *c == v.column))
                            .collect();
                        push_node(
                            nodes,
                            target,
                            WriteNodeKind::UpdateOrInsert { key },
                            values,
                            link(node),
                            at.clone(),
                        );
                    }
                    // Its own children only: the key, and this row's key in their foreign key
                    for (at, item) in &child_updates {
                        let key = self.plan_row_key(
                            target,
                            item.get("where").unwrap_or(&serde_json::Value::Null),
                            &format!("{at}.where"),
                        )?;
                        let values = self.plain_values(
                            target,
                            item.get("data").unwrap_or(&serde_json::Value::Null),
                            &format!("{at}.data"),
                            false,
                        )?;
                        push_node(
                            nodes,
                            target,
                            WriteNodeKind::UpdateRelated { key },
                            values,
                            link(node),
                            at.clone(),
                        );
                    }
                    for (at, key) in &child_deletes {
                        let key = self.plan_row_key(target, key, at)?;
                        push_node(
                            nodes,
                            target,
                            WriteNodeKind::DeleteRelated { key },
                            Vec::new(),
                            link(node),
                            at.clone(),
                        );
                    }
                    if let Some(items) = &sync {
                        let at = format!("{at}.sync");
                        let mut rows: Vec<Vec<ColumnValue>> = Vec::new();
                        for (i, item) in items.iter().enumerate() {
                            let values: Vec<ColumnValue> = self
                                .plain_values(target, item, &format!("{at}[{i}]"), true)?
                                .into_iter()
                                .filter(|v| !rel.columns.iter().any(|(c, _)| *c == v.column))
                                .collect();
                            // Rows leaving the key out are new rows (the key takes its default)
                            let columns = |r: &[ColumnValue]| -> Vec<String> {
                                let mut c: Vec<String> = r
                                    .iter()
                                    .map(|v| v.column.clone())
                                    .filter(|c| !target.primary_key.contains(c))
                                    .collect();
                                c.sort();
                                c
                            };
                            if let Some(first) = rows.first() {
                                if columns(first) != columns(&values) {
                                    return Err(invalid(format!("`{at}[{i}]` must set the same fields as `{at}[0]` (apart from the key)")));
                                }
                            }
                            rows.push(values);
                        }
                        let key = target
                            .primary_key
                            .iter()
                            .map(|k| {
                                (
                                    k.clone(),
                                    target.get_column(k).and_then(|c| c.pg_type.sql_cast()),
                                )
                            })
                            .collect();
                        push_node(
                            nodes,
                            target,
                            WriteNodeKind::Sync { rows, key },
                            Vec::new(),
                            link(node),
                            at,
                        );
                    }
                }
                Some(j) => {
                    if let Some(op) = ["update", "delete", "sync"]
                        .iter()
                        .find(|op| value.get(**op).is_some_and(|v| !v.is_null()))
                    {
                        return Err(invalid(format!(
                            "`{at}.{op}`: rows related through `{}` are linked, not owned; use `connect`, `disconnect` or `set`",
                            j.junction_table
                        )));
                    }
                    let junction = self.find_table(&j.junction_schema, &j.junction_table)?;
                    let junction_cast =
                        |c: &str| junction.get_column(c).map(|c| c.pg_type.assignment_cast());
                    let source_refs: Vec<NodeRef> = j
                        .source_columns
                        .iter()
                        .map(|(jc, col)| NodeRef {
                            column: jc.clone(),
                            pg_cast: junction_cast(jc),
                            node,
                            source_column: col.clone(),
                        })
                        .collect();
                    let target_refs = |t: usize| -> Vec<NodeRef> {
                        j.target_columns
                            .iter()
                            .map(|(jc, tc)| NodeRef {
                                column: jc.clone(),
                                pg_cast: junction_cast(jc),
                                node: t,
                                source_column: tc.clone(),
                            })
                            .collect()
                    };
                    let through = || {
                        Some(DetachThrough {
                            schema: target.schema.clone(),
                            table: target.name.clone(),
                            columns: j.target_columns.clone(),
                        })
                    };
                    if set.is_some() {
                        let detach = WriteNodeKind::Detach {
                            key: None,
                            except: keys_of(&connects)?,
                            through: through(),
                        };
                        push_node(
                            nodes,
                            junction,
                            detach,
                            Vec::new(),
                            source_refs.clone(),
                            format!("{at}.set"),
                        );
                    }
                    for (at, key) in &disconnects {
                        let detach = WriteNodeKind::Detach {
                            key: Some(self.plan_row_key(target, key, at)?),
                            except: Vec::new(),
                            through: through(),
                        };
                        push_node(
                            nodes,
                            junction,
                            detach,
                            Vec::new(),
                            source_refs.clone(),
                            at.clone(),
                        );
                    }
                    for (i, input) in creates.into_iter().enumerate() {
                        let at = format!("{at}.create[{i}]");
                        let t = self.plan_row_node(nodes, target, input, &at, Vec::new(), None)?;
                        let refs = source_refs.iter().cloned().chain(target_refs(t)).collect();
                        push_node(nodes, junction, WriteNodeKind::Insert, Vec::new(), refs, at);
                    }
                    // An existing row may be related already: its junction row is kept
                    let link_kind = || {
                        if updating {
                            WriteNodeKind::Link
                        } else {
                            WriteNodeKind::Insert
                        }
                    };
                    for (at, item) in &connect_or_create {
                        let key = self.plan_row_key(
                            target,
                            item.get("where").unwrap_or(&serde_json::Value::Null),
                            &format!("{at}.where"),
                        )?;
                        let values = self.plain_values(
                            target,
                            item.get("create").unwrap_or(&serde_json::Value::Null),
                            &format!("{at}.create"),
                            true,
                        )?;
                        let found = push_node(
                            nodes,
                            target,
                            WriteNodeKind::LookupOrInsert { key },
                            values,
                            Vec::new(),
                            at.clone(),
                        );
                        let refs = source_refs
                            .iter()
                            .cloned()
                            .chain(target_refs(found))
                            .collect();
                        push_node(
                            nodes,
                            junction,
                            WriteNodeKind::Link,
                            Vec::new(),
                            refs,
                            at.clone(),
                        );
                    }
                    for (at, key) in &connects {
                        let at = at.clone();
                        let key = self.plan_row_key(target, key, &at)?;
                        let direct = j.target_columns.len() == key.len()
                            && j.target_columns
                                .iter()
                                .all(|(_, t)| key.iter().any(|k| &k.column == t));
                        let (values, refs) = if direct {
                            let values = j
                                .target_columns
                                .iter()
                                .map(|(jc, tc)| ColumnValue {
                                    column: jc.clone(),
                                    pg_cast: junction_cast(jc),
                                    composite: false,
                                    composite_array: false,
                                    value: key
                                        .iter()
                                        .find(|k| &k.column == tc)
                                        .expect("checked above")
                                        .value
                                        .clone(),
                                })
                                .collect();
                            (values, source_refs.clone())
                        } else {
                            let lookup = push_node(
                                nodes,
                                target,
                                WriteNodeKind::Lookup { key },
                                Vec::new(),
                                Vec::new(),
                                at.clone(),
                            );
                            (
                                Vec::new(),
                                source_refs
                                    .iter()
                                    .cloned()
                                    .chain(target_refs(lookup))
                                    .collect(),
                            )
                        };
                        push_node(nodes, junction, link_kind(), values, refs, at);
                    }
                }
            }
        }
        Ok(node)
    }

    /// Column values of a `Create<Type>Input` (`create`) or `Update<Type>Input` at `path`
    /// whose nested relations are not written (inside `connectOrCreate`, `update` and
    /// `sync`): those set are an error.
    fn plain_values(
        &self,
        table: &TableMetadata,
        input: &serde_json::Value,
        path: &str,
        create: bool,
    ) -> Result<Vec<ColumnValue>, PlannerError> {
        let serde_json::Value::Object(input) = input else {
            return Err(PlannerError::InvalidInput(format!(
                "`{path}` must be an input object"
            )));
        };
        if create && !table.operations.create {
            return Err(PlannerError::InvalidInput(format!(
                "`{path}`: `{}` rows cannot be created",
                table.type_name
            )));
        }
        let mut values = Vec::new();
        for (k, v) in input {
            match table.writable_columns().find(|c| c.field_name == *k) {
                Some(col) => values.push(self.column_value(col, v.clone(), path)),
                None if !v.is_null() => {
                    return Err(PlannerError::InvalidInput(format!(
                        "`{path}.{k}`: nested relations cannot be written here"
                    )));
                }
                None => {}
            }
        }
        Ok(values)
    }

    /// The row of `table` a `<Type>KeyInput` at `path` identifies: by the first of its keys
    /// (primary key, then unique keys) whose fields are all set.
    fn plan_row_key(
        &self,
        table: &TableMetadata,
        key: &serde_json::Value,
        path: &str,
    ) -> Result<Vec<WhereCondition>, PlannerError> {
        let keys = table.row_keys();
        let set = |field: &str| key.get(field).is_some_and(|v| !v.is_null());
        if let Some(fields) = keys
            .iter()
            .find(|fields| fields.iter().all(|(f, _)| set(f)))
        {
            return Ok(fields
                .iter()
                .map(|(f, col)| WhereCondition {
                    column: col.name.clone(),
                    operator: "=".to_string(),
                    value: input_value(self.catalog(), col, key[f.as_str()].clone()),
                    pg_cast: col.pg_type.sql_cast(),
                })
                .collect());
        }
        let options: Vec<String> = keys
            .iter()
            .map(|fields| {
                fields
                    .iter()
                    .map(|(f, _)| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            })
            .collect();
        Err(PlannerError::InvalidInput(format!(
            "`{path}` must identify a `{}` by {}",
            table.type_name,
            options.join(", or by ")
        )))
    }

    /// Recursively plan a field selection set into a PgSelect step and its nested relation steps.
    #[allow(clippy::too_many_arguments)]
    fn plan_field(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        schema: &str,
        table_name: &str,
        is_single: bool,
        join_condition: Option<JoinCondition>,
        table_alias: Option<String>,
        alias_counter: &mut usize,
    ) -> Result<StepId, PlannerError> {
        let table_alias = table_alias.unwrap_or_else(|| {
            *alias_counter += 1;
            format!("t{}", *alias_counter - 1)
        });

        let table_meta = self.find_table(schema, table_name)?;
        let mut join_condition = join_condition;

        let mut where_conditions = Vec::new();
        let mut filter_expr = None;
        let mut order_by = None;
        let mut limit = None;
        let mut offset = None;

        // 1. Arguments: key columns of single-row lookups (e.g. `id: ...`), list arguments
        for (arg_name, value) in &field.arguments {
            let arg_name = arg_name.as_str();
            let value = value.clone();
            if arg_name == "asOf" {
                // The versions valid then, rather than those overlapping the parent's period
                if let Some(cond) = as_of_condition(table_meta, value) {
                    where_conditions.push(cond);
                    if let Some(join) = join_condition.as_mut() {
                        join.period = None;
                    }
                }
                continue;
            }
            if let Some(col) = key_column_by_field(table_meta, arg_name) {
                where_conditions.push(WhereCondition {
                    pg_cast: col.pg_type.sql_cast(),
                    column: col.name.clone(),
                    operator: "=".to_string(),
                    value: input_value(self.catalog(), col, value),
                });
                continue;
            }
            match arg_name {
                "filter" => {
                    filter_expr = self.plan_filter_object(
                        &value,
                        schema,
                        table_name,
                        &table_alias,
                        alias_counter,
                    )?
                }
                "condition" => where_conditions.extend(Self::plan_condition(
                    self.catalog(),
                    &value,
                    table_meta,
                )),
                "orderBy" => order_by = self.plan_order_by(&value, &table_alias, table_meta),
                "first" => limit = Self::as_usize(&value),
                "offset" => offset = Self::as_usize(&value),
                _ => {}
            }
        }

        if order_by.is_none() {
            order_by = version_order(table_meta, &table_alias, is_single);
        }

        // 2. Process Field Selections (scalar columns, nested relations, and nested connections)
        let (fields, row_polymorphism) = self.plan_row_shape(
            plan,
            &field.children,
            schema,
            table_name,
            &table_alias,
            table_meta,
            alias_counter,
        )?;

        let mut pg_select = Step::pg_select(schema, table_name, &table_alias, is_single);
        if let Step::PgSelect {
            fields: step_fields,
            where_conditions: step_wheres,
            filter_expr: step_filter,
            join_condition: step_join,
            order_by: step_order,
            limit: step_limit,
            offset: step_offset,
            polymorphism: step_poly,
            ..
        } = &mut pg_select
        {
            *step_fields = fields;
            *step_wheres = where_conditions;
            *step_filter = filter_expr;
            *step_join = join_condition;
            *step_order = order_by;
            if limit.is_some() {
                *step_limit = limit;
            }
            *step_offset = offset;
            *step_poly = row_polymorphism;
        }

        Ok(plan.add_step(pg_select))
    }

    /// Plan a Relay Connection field (PgConnection step).
    #[allow(clippy::too_many_arguments)]
    fn plan_connection(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        schema: &str,
        table_name: &str,
        join_condition: Option<JoinCondition>,
        func: Option<(&FunctionMetadata, Option<ArgValue>)>,
        alias_counter: &mut usize,
    ) -> Result<StepId, PlannerError> {
        let table_alias = format!("c{}", *alias_counter);
        *alias_counter += 1;

        let table_meta = self.find_table(schema, table_name)?;
        let mut join_condition = join_condition;
        let (func, row) = match func {
            Some((f, row)) => (Some(f), row),
            None => (None, None),
        };

        let mut first = None;
        let mut last = None;
        let mut offset = None;
        let mut after = None;
        let mut before = None;
        let mut order_by = None;
        let mut where_conditions = Vec::new();
        let mut filter_expr = None;

        // 1. Process Connection Arguments (a function's own come first: they are its call's)
        for (arg_name, value) in &field.arguments {
            let arg_name = arg_name.as_str();
            if func.is_some_and(|f| f.arguments.iter().any(|a| a.graphql_name() == arg_name)) {
                continue;
            }
            let value = value.clone();
            match arg_name {
                "first" => first = Self::as_usize(&value),
                "last" => last = Self::as_usize(&value),
                "offset" => offset = Self::as_usize(&value),
                "after" => after = value.as_str().map(str::to_string),
                "before" => before = value.as_str().map(str::to_string),
                "orderBy" | "order_by" => order_by = Some(value),
                "filter" => {
                    filter_expr = self.plan_filter_object(
                        &value,
                        schema,
                        table_name,
                        &table_alias,
                        alias_counter,
                    )?
                }
                "condition" => where_conditions.extend(Self::plan_condition(
                    self.catalog(),
                    &value,
                    table_meta,
                )),
                "asOf" => {
                    if let Some(cond) = as_of_condition(table_meta, value) {
                        where_conditions.push(cond);
                        if let Some(join) = join_condition.as_mut() {
                            join.period = None;
                        }
                    }
                }
                other => {
                    if let Some(col) = table_meta.column_by_field(other) {
                        where_conditions.push(WhereCondition {
                            pg_cast: col.pg_type.sql_cast(),
                            column: col.name.clone(),
                            operator: "=".to_string(),
                            value: input_value(self.catalog(), col, value),
                        });
                    }
                }
            }
        }

        // 2. Process Selection Set
        let mut selections = ConnectionSelections::default();

        for conn_sel in &field.children {
            match conn_sel.name.as_str() {
                "totalCount" => {
                    selections.include_total_count = true;
                    let estimate = conn_sel
                        .argument("estimate")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    selections.estimate_total |= estimate;
                    let key = conn_sel.response_key().to_string();
                    if !selections.total_counts.iter().any(|(k, _)| *k == key) {
                        if let Some(n) = conn_sel
                            .argument("exactBelow")
                            .and_then(|v| v.as_u64())
                            .filter(|_| estimate)
                        {
                            selections.exact_below.push((key.clone(), n));
                        }
                        selections.total_counts.push((key, estimate));
                    }
                }
                "pageInfo" => selections.include_page_info = true,
                "aggregates" => {
                    let key = conn_sel.response_key().to_string();
                    if selections.aggregates.iter().any(|a| a.field_alias == key) {
                        continue;
                    }
                    let mut aggs = self.plan_aggregates(&conn_sel.children, table_meta)?;
                    aggs.field_alias = key;
                    // `aggregates(filter:)`: only matching rows are aggregated
                    if let Some(filter) = conn_sel.argument("filter") {
                        aggs.filter = self.plan_filter_object(
                            filter,
                            schema,
                            table_name,
                            &table_alias,
                            alias_counter,
                        )?;
                    }
                    selections.aggregates.push(aggs);
                }
                "groupedAggregates" => selections
                    .grouped
                    .push(self.plan_grouped_aggregates(conn_sel, table_meta)?),
                "edges" => {
                    selections.include_edges = true;
                    let edge_fields: Vec<&RequestField> = conn_sel.children.iter().collect();
                    for window in self.plan_windows(&edge_fields, table_meta)? {
                        if !selections
                            .windows
                            .iter()
                            .any(|w| w.field_alias == window.field_alias)
                        {
                            selections.windows.push(window);
                        }
                    }
                    for edge_f in &conn_sel.children {
                        if edge_f.name == "node" {
                            (selections.node_fields, selections.node_polymorphism) = self
                                .plan_row_shape(
                                    plan,
                                    &edge_f.children,
                                    schema,
                                    table_name,
                                    &table_alias,
                                    table_meta,
                                    alias_counter,
                                )?;
                        }
                    }
                }
                "nodes" => {
                    selections.include_nodes = true;
                    if selections.node_fields.is_empty() && selections.node_polymorphism.is_none() {
                        (selections.node_fields, selections.node_polymorphism) = self
                            .plan_row_shape(
                                plan,
                                &conn_sel.children,
                                schema,
                                table_name,
                                &table_alias,
                                table_meta,
                                alias_counter,
                            )?;
                    }
                }
                _ => {}
            }
        }

        // Cursor / default order key: the primary key, or the first column without one
        let key: Vec<&String> = if table_meta.primary_key.is_empty() {
            table_meta
                .columns
                .first()
                .map(|c| &c.name)
                .into_iter()
                .collect()
        } else {
            table_meta.primary_key.iter().collect()
        };
        // The `orderBy` columns, made a total order by the key so cursors mark an exact position.
        // A function's rows keep the order it returns them in (`__ord`) without an `orderBy`,
        // and break ties by it without a key.
        let by_function_order =
            func.is_some() && (order_by.is_none() || table_meta.primary_key.is_empty());
        let tiebreak: Vec<SortKey> = if by_function_order {
            vec![SortKey {
                column: "__ord".to_string(),
                descending: false,
                nullable: false,
                compare: SortCompare::Cast("int8".to_string()),
                expr: None,
                nulls_first: None,
            }]
        } else {
            key.iter().map(|c| sort_key(table_meta, c, false)).collect()
        };
        let items = order_by
            .map(|v| self.plan_sort_keys(&v, table_meta))
            .unwrap_or_default();
        let mut sort: Vec<SortKey> = Vec::new();
        for sort_key in items.into_iter() {
            if !sort.iter().any(|k| k.column == sort_key.column) {
                sort.push(sort_key);
            }
        }
        // Ranks compare the `orderBy` values only
        let ordered = sort.len();
        for window in &mut selections.windows {
            window.rank_keys = ordered;
        }
        for sort_key in tiebreak {
            if !sort.iter().any(|k| k.column == sort_key.column) {
                sort.push(sort_key);
            }
        }
        for cursor in [&after, &before].into_iter().flatten() {
            if sort.len() > 1 && SqlCompiler::decode_composite_cursor(cursor).len() != sort.len() {
                return Err(PlannerError::InvalidInput(format!(
                    "Invalid cursor `{cursor}`: it was not issued by this connection with this `orderBy`"
                )));
            }
        }
        let step = Step::PgConnection {
            schema: schema.to_string(),
            table: table_name.to_string(),
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
            source: func.map(|f| FunctionSource {
                schema: f.schema.clone(),
                func_name: f.name.clone(),
                arguments: self.bind_function_args(f, field, false, row.as_ref()),
                unnest: f.returns_array,
                ordinality: by_function_order,
            }),
            sort,
            id_cast: column_cast(table_meta, "id"),
            hypertable: matches!(
                table_meta.timescale,
                Some(crate::catalog::models::Timescale::Hypertable { .. })
            )
            .then(|| {
                self.catalog()
                    .integration(crate::catalog::integrations::Integration::TimescaleDb)
                    .map(|i| i.schema.clone())
            })
            .flatten(),
            cursor_columns: key
                .into_iter()
                .map(|c| (c.clone(), column_cast(table_meta, c)))
                .collect(),
        };

        Ok(plan.add_step(step))
    }

    /// Plan a search root field (`usersFuzzySearch`): the method's arguments, the rows'
    /// conditions and filter, and `first` within the configured limits.
    fn plan_search(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        schema: &str,
        table_name: &str,
        method: SearchMethod,
        alias_counter: &mut usize,
    ) -> Result<StepId, PlannerError> {
        let table_alias = format!("s{}", *alias_counter);
        *alias_counter += 1;
        let table_meta = self.find_table(schema, table_name)?;
        let limits = self.catalog().search_limits;

        let mut limit = limits.default;
        let mut where_conditions = Vec::new();
        let mut filter_expr = None;
        let mut method_args = serde_json::Map::new();
        for (arg_name, value) in &field.arguments {
            let value = value.clone();
            match arg_name.as_str() {
                "first" if value.is_null() => {}
                "first" => {
                    limit = Self::as_usize(&value)
                        .filter(|&n| n <= limits.max)
                        .ok_or_else(|| {
                            PlannerError::InvalidInput(format!(
                                "`first` must be between 0 and {}",
                                limits.max
                            ))
                        })?;
                }
                "filter" => {
                    filter_expr = self.plan_filter_object(
                        &value,
                        schema,
                        table_name,
                        &table_alias,
                        alias_counter,
                    )?
                }
                "condition" => where_conditions.extend(Self::plan_condition(
                    self.catalog(),
                    &value,
                    table_meta,
                )),
                name => {
                    method_args.insert(name.to_string(), value);
                }
            }
        }
        let method_args = serde_json::Value::Object(method_args);
        let search = match method {
            SearchMethod::Fuzzy => self.plan_fuzzy(&method_args, schema, table_name),
            SearchMethod::Semantic => self.plan_semantic(&method_args, schema, table_name),
            SearchMethod::Bm25 => self.plan_bm25(&method_args, schema, table_name),
            SearchMethod::Hybrid => self.plan_hybrid(&method_args, schema, table_name),
            SearchMethod::FullText => self.plan_full_text(&method_args, schema, table_name),
            SearchMethod::Nearest => self.plan_nearest(&method_args, schema, table_name),
        }
        .ok_or_else(|| {
            PlannerError::InvalidInput(format!("`{}`: invalid search arguments", field.name))
        })?;

        let mut selections = SearchSelections::default();
        for sel in &field.children {
            match sel.name.as_str() {
                "hasMore" => selections.include_has_more = true,
                "hits" => {
                    selections.include_hits = true;
                    for hit_field in &sel.children {
                        match hit_field.name.as_str() {
                            "score" => selections.include_score = true,
                            "distance" => {
                                selections.distance = Some(hit_field.response_key().to_string())
                            }
                            "highlight" => {
                                let arg = |name: &str| hit_field.argument(name).cloned();
                                let text = |name: &str, default: &str| {
                                    arg(name)
                                        .and_then(|v| v.as_str().map(str::to_string))
                                        .unwrap_or_else(|| default.to_string())
                                };
                                // Option values are separated by commas and cannot quote them
                                let (start_sel, stop_sel) =
                                    (text("startSel", "<b>"), text("stopSel", "</b>"));
                                if [&start_sel, &stop_sel]
                                    .iter()
                                    .any(|s| s.contains([',', '"']) || s.is_empty())
                                {
                                    return Err(PlannerError::InvalidInput(
                                        "highlight: `startSel` and `stopSel` must be non-empty and contain no `,` or `\"`".to_string(),
                                    ));
                                }
                                selections.highlight = Some(HighlightOptions {
                                    field_alias: hit_field.response_key().to_string(),
                                    start_sel,
                                    stop_sel,
                                    max_words: arg("maxWords").and_then(|v| v.as_i64()),
                                    min_words: arg("minWords").and_then(|v| v.as_i64()),
                                    max_fragments: arg("maxFragments").and_then(|v| v.as_i64()),
                                });
                            }
                            "node" => {
                                (selections.node_fields, selections.node_polymorphism) = self
                                    .plan_row_shape(
                                        plan,
                                        &hit_field.children,
                                        schema,
                                        table_name,
                                        &table_alias,
                                        table_meta,
                                        alias_counter,
                                    )?;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(plan.add_step(Step::PgSearch {
            schema: schema.to_string(),
            table: table_name.to_string(),
            table_alias,
            search,
            where_conditions,
            filter_expr,
            limit,
            selections,
        }))
    }

    /// `globalSearch(query:, first:)`: the full-text (or fuzzy) search of every searchable
    /// table, each read as its own node type.
    fn plan_global_search(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        alias_counter: &mut usize,
    ) -> Result<StepId, PlannerError> {
        let limits = self.catalog().search_limits;
        let arg = |name: &str| field.argument(name).cloned();
        let query = arg("query")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let limit = match arg("first").filter(|v| !v.is_null()) {
            None => limits.default,
            Some(v) => Self::as_usize(&v)
                .filter(|&n| n <= limits.max)
                .ok_or_else(|| {
                    PlannerError::InvalidInput(format!(
                        "`first` must be between 0 and {}",
                        limits.max
                    ))
                })?,
        };
        let (mut include_hits, mut include_score, mut include_has_more) = (false, false, false);
        let mut node_selections: Vec<&[RequestField]> = Vec::new();
        for sel in &field.children {
            match sel.name.as_str() {
                "hasMore" => include_has_more = true,
                "hits" => {
                    include_hits = true;
                    for hit in &sel.children {
                        match hit.name.as_str() {
                            "score" => include_score = true,
                            "node" => node_selections.push(&hit.children),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        let mut branches = Vec::new();
        for (table, idx) in self.catalog().global_search_tables() {
            let table_alias = format!("g{}", *alias_counter);
            *alias_counter += 1;
            let search = match &idx.kind {
                SearchIndexKind::FullText {
                    document,
                    config,
                    source,
                } => SearchOperation::FullText {
                    document: document.clone(),
                    config: config.clone(),
                    source: source.clone(),
                    query: query.clone(),
                },
                _ => SearchOperation::Fuzzy {
                    column: self
                        .catalog()
                        .search_column(idx)
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                    query: query.clone(),
                    min_similarity: None,
                },
            };
            let (mut node_fields, mut node_polymorphism) = (Vec::new(), None);
            if let Some(selection) = node_selections.first() {
                (node_fields, node_polymorphism) = self.plan_row_shape(
                    plan,
                    selection,
                    &table.schema,
                    &table.name,
                    &table_alias,
                    table,
                    alias_counter,
                )?;
            }
            branches.push(GlobalSearchBranch {
                schema: table.schema.clone(),
                table: table.name.clone(),
                table_alias,
                search,
                node_fields,
                node_polymorphism,
            });
        }
        Ok(plan.add_step(Step::PgGlobalSearch {
            branches,
            limit,
            include_hits,
            include_score,
            include_has_more,
        }))
    }

    /// Search index named by a `<Type>FuzzyField`-style enum value, or the first index.
    fn search_index<'i>(
        &self,
        indexes: Vec<&'i SearchIndexMetadata>,
        value: Option<&str>,
    ) -> Option<&'i SearchIndexMetadata> {
        match value {
            Some(v) => indexes
                .into_iter()
                .find(|i| self.catalog().search_field_value(i) == v),
            None => indexes.into_iter().next(),
        }
    }

    /// Column an index searches; indexes without a plain column (BM25 over several fields)
    /// are addressed through the table's key.
    fn search_column_name(&self, idx: &SearchIndexMetadata, table: &TableMetadata) -> String {
        match self.catalog().search_column(idx) {
            Some(col) => col.name.clone(),
            None => table
                .primary_key
                .first()
                .cloned()
                .unwrap_or_else(|| "id".to_string()),
        }
    }

    fn plan_fuzzy(
        &self,
        value: &serde_json::Value,
        schema: &str,
        table_name: &str,
    ) -> Option<SearchOperation> {
        let serde_json::Value::Object(obj) = value else {
            return None;
        };
        let min_similarity = obj.get("minSimilarity").and_then(|v| v.as_f64());
        let indexes = self.catalog().fuzzy_indexes_for_table(schema, table_name);
        let explicit_query = obj.get("query").and_then(|v| v.as_str());

        // `{ field: NAME, query: "..." }`, or the `{ <field>: "..." }` shorthand
        let shorthand = obj.iter().find_map(|(k, v)| {
            let idx = indexes.iter().find(|i| {
                self.catalog()
                    .search_column(i)
                    .is_some_and(|c| &c.field_name == k)
            })?;
            Some((*idx, v.as_str()?))
        });
        let (idx, query) = match (explicit_query, shorthand) {
            (Some(q), _) => (
                self.search_index(indexes, obj.get("field").and_then(|v| v.as_str()))?,
                q,
            ),
            (None, Some((idx, q))) => (idx, q),
            (None, None) => return None,
        };
        let column = self.catalog().search_column(idx)?.name.clone();
        Some(SearchOperation::Fuzzy {
            column,
            query: query.to_string(),
            min_similarity,
        })
    }

    fn plan_semantic(
        &self,
        value: &serde_json::Value,
        schema: &str,
        table_name: &str,
    ) -> Option<SearchOperation> {
        let serde_json::Value::Object(obj) = value else {
            return None;
        };
        let indexes = self.catalog().vector_indexes_for_table(schema, table_name);
        let idx = self.search_index(indexes, obj.get("field").and_then(|v| v.as_str()))?;
        let vector = obj
            .get("vector")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_f64())
            .collect();
        let opclass = match &idx.kind {
            SearchIndexKind::Vector { opclass } => opclass.clone(),
            _ => "vector_cosine_ops".to_string(),
        };
        Some(SearchOperation::Semantic {
            column: self.catalog().search_column(idx)?.name.clone(),
            vector,
            min_score: obj.get("minScore").and_then(|v| v.as_f64()),
            opclass,
        })
    }

    fn plan_bm25(
        &self,
        value: &serde_json::Value,
        schema: &str,
        table_name: &str,
    ) -> Option<SearchOperation> {
        let serde_json::Value::Object(obj) = value else {
            return None;
        };
        let table = self.catalog().find_table(schema, table_name)?;
        let query = obj
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let indexes = self.catalog().bm25_indexes_for_table(schema, table_name);
        let idx = self.search_index(indexes, obj.get("field").and_then(|v| v.as_str()))?;
        Some(SearchOperation::Bm25 {
            column: self.search_column_name(idx, table),
            query,
        })
    }

    fn plan_hybrid(
        &self,
        value: &serde_json::Value,
        schema: &str,
        table_name: &str,
    ) -> Option<SearchOperation> {
        let serde_json::Value::Object(obj) = value else {
            return None;
        };
        let catalog = self.catalog();
        let table = catalog.find_table(schema, table_name)?;
        let [key_column] = table.primary_key.as_slice() else {
            return None;
        };
        let vec_idx = catalog
            .vector_indexes_for_table(schema, table_name)
            .into_iter()
            .next()?;
        let (lexical_column, is_bm25) =
            if let Some(b) = catalog.bm25_indexes_for_table(schema, table_name).first() {
                (self.search_column_name(b, table), true)
            } else {
                (
                    catalog
                        .search_column(
                            catalog
                                .fuzzy_indexes_for_table(schema, table_name)
                                .first()?,
                        )?
                        .name
                        .clone(),
                    false,
                )
            };
        let vector = obj
            .get("vector")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_f64())
            .collect();
        let vector_opclass = match &vec_idx.kind {
            SearchIndexKind::Vector { opclass } => opclass.clone(),
            _ => "vector_cosine_ops".to_string(),
        };
        Some(SearchOperation::Hybrid {
            key_column: key_column.clone(),
            vector_column: catalog.search_column(vec_idx)?.name.clone(),
            vector_opclass,
            lexical_column,
            is_bm25,
            query: obj
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            vector,
            lexical_weight: obj
                .get("lexicalWeight")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.5),
            vector_weight: obj
                .get("vectorWeight")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.5),
            rrf_k: obj
                .get("rrfK")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(60),
        })
    }

    fn plan_full_text(
        &self,
        value: &serde_json::Value,
        schema: &str,
        table_name: &str,
    ) -> Option<SearchOperation> {
        let serde_json::Value::Object(obj) = value else {
            return None;
        };
        let indexes = self
            .catalog()
            .full_text_indexes_for_table(schema, table_name);
        let idx = self.search_index(indexes, obj.get("field").and_then(|v| v.as_str()))?;
        let SearchIndexKind::FullText {
            document,
            config,
            source,
        } = &idx.kind
        else {
            return None;
        };
        Some(SearchOperation::FullText {
            document: document.clone(),
            config: config.clone(),
            source: source.clone(),
            query: obj.get("query")?.as_str()?.to_string(),
        })
    }

    fn plan_nearest(
        &self,
        value: &serde_json::Value,
        schema: &str,
        table_name: &str,
    ) -> Option<SearchOperation> {
        let serde_json::Value::Object(obj) = value else {
            return None;
        };
        let indexes = self.catalog().spatial_indexes_for_table(schema, table_name);
        let idx = self.search_index(indexes, obj.get("field").and_then(|v| v.as_str()))?;
        let column = self.catalog().search_column(idx)?;
        let near = match scalars::geojson(obj.get("near")?.clone(), "near") {
            serde_json::Value::String(text) => text,
            other => other.to_string(),
        };
        Some(SearchOperation::Nearest {
            column: column.name.clone(),
            geography: column.pg_type == PgType::Geography,
            near,
            max_distance: obj.get("maxDistance").and_then(|v| v.as_f64()),
        })
    }

    /// Conversion of a stored value of `pg_type` into the response: enum labels to GraphQL
    /// values, composites to the attributes `selection` picks (recursively), `bigint`-like
    /// values to text.
    fn plan_output(
        &self,
        pg_type: &PgType,
        enum_key: Option<&str>,
        selection: Option<&[RequestField]>,
    ) -> ValueOutput {
        let catalog = self.catalog();
        let (elem, list) = match pg_type {
            PgType::Array(elem) => (elem.as_ref(), true),
            // A multirange is output as the list of its ranges
            PgType::Multirange(range) => (range.as_ref(), true),
            other => (other, false),
        };
        let enum_key = enum_key.or(match elem {
            PgType::Enum(k) => Some(k.as_str()),
            _ => None,
        });
        if let Some(e) = enum_key.and_then(|k| catalog.find_enum(k)) {
            if !e.needs_mapping() {
                return ValueOutput::cast(pg_type.output_cast());
            }
            let values = e
                .values
                .iter()
                .map(|v| (v.label.clone(), v.name.clone()))
                .collect();
            return ValueOutput::Enum { values, list };
        }
        if let (PgType::Composite(key), Some(selection)) = (elem, selection) {
            if let Some(composite) = catalog.find_composite(key) {
                let mut fields: Vec<CompositeField> = Vec::new();
                for f in flatten_selections_for(selection, &[&composite.type_name]) {
                    let field_alias = f.response_key().to_string();
                    if fields
                        .iter()
                        .any(|existing| existing.field_alias == field_alias)
                    {
                        continue;
                    }
                    let source = if f.name == "__typename" {
                        CompositeSource::Typename(composite.type_name.clone())
                    } else if let Some(attr) = composite.attribute_by_field(f.name.as_str()) {
                        let output = self.plan_output(
                            &attr.pg_type,
                            attr.enum_key.as_deref(),
                            Some(&f.children),
                        );
                        CompositeSource::Attribute {
                            name: attr.name.clone(),
                            output,
                        }
                    } else {
                        continue;
                    };
                    fields.push(CompositeField {
                        field_alias,
                        source,
                    });
                }
                return ValueOutput::Composite { fields, list };
            }
        }
        if let Some((element, scalar)) = elem.range_element() {
            let range_type = format!("{scalar}Range");
            let bound_type = format!("{scalar}RangeBound");
            let mut fields: Vec<RangeField> = Vec::new();
            for f in selection
                .map(|sel| flatten_selections_for(sel, &[&range_type]))
                .unwrap_or_default()
            {
                let field_alias = f.response_key().to_string();
                if fields
                    .iter()
                    .any(|existing| existing.field_alias == field_alias)
                {
                    continue;
                }
                let part = match f.name.as_str() {
                    "__typename" => RangePart::Typename(range_type.clone()),
                    "isEmpty" => RangePart::IsEmpty,
                    name @ ("start" | "end") => {
                        let mut parts: Vec<(String, BoundPart)> = Vec::new();
                        for b in flatten_selections_for(&f.children, &[&bound_type]) {
                            let alias = b.response_key().to_string();
                            let part = match b.name.as_str() {
                                "value" => BoundPart::Value,
                                "inclusive" => BoundPart::Inclusive,
                                "__typename" => BoundPart::Typename(bound_type.clone()),
                                _ => continue,
                            };
                            if !parts.iter().any(|(a, _)| *a == alias) {
                                parts.push((alias, part));
                            }
                        }
                        RangePart::Bound {
                            upper: name == "end",
                            fields: parts,
                        }
                    }
                    _ => continue,
                };
                fields.push(RangeField { field_alias, part });
            }
            let bound = Box::new(self.plan_output(&element, None, None));
            return ValueOutput::Range {
                fields,
                bound,
                list,
            };
        }
        if let Some(sql) = elem.output_sql() {
            return ValueOutput::Template {
                sql: sql.to_string(),
                list,
            };
        }
        ValueOutput::cast(pg_type.output_cast())
    }

    /// Positional arguments of `func` from `field`'s arguments. When `mounted`, arguments
    /// mapped to parent columns read the parent row. Arguments after the last one that is
    /// given, mapped or required are left out, so their SQL defaults apply.
    /// A computed column's first argument takes `row`.
    fn bind_function_args(
        &self,
        func: &FunctionMetadata,
        field: &RequestField,
        mounted: bool,
        row: Option<&ArgValue>,
    ) -> Vec<BoundArg> {
        let parent_column = |name: &str| mounted.then(|| func.parent_column_for(name)).flatten();
        let provided = |name: &str, graphql_name: &str| {
            field
                .argument(graphql_name)
                .or_else(|| field.argument(name))
                .cloned()
        };
        let row = row.filter(|_| func.row_argument);
        let last = func.arguments.iter().enumerate().rposition(|(i, a)| {
            (i == 0 && row.is_some())
                || parent_column(&a.name).is_some()
                || provided(&a.name, &a.graphql_name()).is_some()
                || !a.has_default
        });
        let Some(last) = last else { return Vec::new() };

        func.arguments[..=last]
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let value = match (row.filter(|_| i == 0), parent_column(&arg.name)) {
                    (Some(row), _) => row.clone(),
                    (None, Some(column)) => ArgValue::Column(column.to_string()),
                    (None, None) => {
                        let mut value = provided(&arg.name, &arg.graphql_name())
                            .unwrap_or(serde_json::Value::Null);
                        if arg.is_key_like() {
                            value = unwrap_global_id(value);
                        }
                        ArgValue::Value(self.column_value(&arg.as_column(), value, ""))
                    }
                };
                BoundArg {
                    name: arg.name.clone(),
                    value,
                }
            })
            .collect()
    }

    /// Recursively plan selections on a table type (handling scalar columns, relations, and connections).
    #[allow(clippy::too_many_arguments)]
    fn plan_selection_fields(
        &self,
        plan: &mut OperationPlan,
        selection_set: &[RequestField],
        schema: &str,
        table_name: &str,
        parent_alias: &str,
        table_meta: &TableMetadata,
        fields: &mut Vec<SelectedField>,
        alias_counter: &mut usize,
    ) -> Result<(), PlannerError> {
        let type_name = table_meta.type_name.clone();
        self.plan_object_fields(
            plan,
            selection_set,
            schema,
            table_name,
            parent_alias,
            table_meta,
            &type_name,
            &[],
            fields,
            alias_counter,
        )
    }

    /// Plans the fields of `table_name`'s row (read from `parent_alias`) exposed as GraphQL type
    /// `entity_type_name` (an STI variant reads the base table). Fragments on the type, its
    /// interfaces and its unions apply; `__typename` reports `entity_type_name`. With `only`,
    /// just the fields whose names it accepts are planned.
    #[allow(clippy::too_many_arguments)]
    fn plan_selection_fields_as(
        &self,
        plan: &mut OperationPlan,
        selection_set: &[RequestField],
        schema: &str,
        table_name: &str,
        parent_alias: &str,
        table_meta: &TableMetadata,
        entity_type_name: &str,
        only: Option<&dyn Fn(&str) -> bool>,
        fields: &mut Vec<SelectedField>,
        alias_counter: &mut usize,
    ) -> Result<(), PlannerError> {
        let catalog = self.catalog();
        let accepted = self.planner.type_conditions(entity_type_name);
        let accepted: Vec<&str> = accepted.iter().map(String::as_str).collect();
        for sub_field in flatten_selections_for(selection_set, &accepted) {
            let sub_name = sub_field.name.as_str();
            if only.is_some_and(|keep| !keep(sub_name)) {
                continue;
            }
            let sub_alias = sub_field.response_key().to_string();

            // Deduplicate if already selected
            if fields.iter().any(|f| f.field_alias() == sub_alias) {
                continue;
            }

            // __typename: emit as a constant string (not a DB column)
            if sub_name == "__typename" {
                fields.push(SelectedField::Typename {
                    field_alias: sub_alias,
                    type_name: entity_type_name.to_string(),
                });
                continue;
            }

            let Some(object_field) = catalog.object_field(schema, table_name, sub_name) else {
                return Err(PlannerError::UnknownField {
                    table: table_name.to_string(),
                    field: sub_name.to_string(),
                });
            };

            let memo_key = (
                parent_alias.to_string(),
                sub_field as *const RequestField as usize,
            );
            if let Some(step_id) = self.step_memo.borrow().get(&memo_key).copied() {
                fields.push(SelectedField::relation(sub_alias, step_id));
                continue;
            }

            match &object_field.kind {
                // Global IDs always name the table's own type (the hierarchy root for subtypes),
                // so one row has one ID and `node(id:)` can resolve it
                FieldKind::GlobalId => fields.push(SelectedField::GlobalId {
                    field_alias: sub_alias,
                    type_name: table_meta.type_name.clone(),
                    pk_column: table_meta.primary_key[0].clone(),
                }),
                FieldKind::DatabaseId | FieldKind::Column(_) => {
                    let column = match &object_field.kind {
                        FieldKind::Column(column) => column,
                        _ => &table_meta.primary_key[0],
                    };
                    let json_path = sub_field
                        .argument("path")
                        .and_then(|v| v.as_str().map(str::to_string));
                    let output = match (table_meta.get_column(column), json_path) {
                        // The first item a JSON path selects
                        (Some(col), Some(path))
                            if matches!(col.pg_type, PgType::Json | PgType::Jsonb) =>
                        {
                            ValueOutput::Template {
                                sql: format!(
                                    "jsonb_path_query_first({{v}}::jsonb, {}::jsonpath)",
                                    dollar_quoted(&path)
                                ),
                                list: false,
                            }
                        }
                        (Some(col), _) => self.plan_output(
                            &col.pg_type,
                            col.enum_key.as_deref(),
                            Some(&sub_field.children),
                        ),
                        (None, _) => ValueOutput::Plain,
                    };
                    fields.push(SelectedField::Column {
                        field_alias: sub_alias,
                        column_name: column.clone(),
                        output,
                    });
                }
                FieldKind::Tree(info) => {
                    let step_id = self.plan_field(
                        plan,
                        sub_field,
                        schema,
                        table_name,
                        false,
                        None,
                        None,
                        alias_counter,
                    )?;
                    let max_depth = sub_field.argument("maxDepth").and_then(Self::as_usize);
                    let ordered = sub_field.arguments.contains_key("orderBy");
                    if let Some(Step::PgSelect {
                        tree,
                        order_by,
                        table_alias,
                        ..
                    }) = plan.get_step_mut(step_id)
                    {
                        // Nearest first
                        if !ordered {
                            let mut items = vec![format!("{table_alias}.\"__depth\" ASC")];
                            items.extend(
                                table_meta
                                    .primary_key
                                    .iter()
                                    .map(|k| format!("{table_alias}.{} ASC", quote_ident(k))),
                            );
                            *order_by = Some(items.join(", "));
                        }
                        *tree = Some(TreeSource {
                            parent_alias: parent_alias.to_string(),
                            ancestors: info.ancestors,
                            link: info.link.clone(),
                            key: table_meta.primary_key.clone(),
                            max_depth,
                        });
                    }
                    self.step_memo.borrow_mut().insert(memo_key, step_id);
                    fields.push(SelectedField::relation(sub_alias, step_id));
                }
                FieldKind::RowVersion => fields.push(match &table_meta.version_column {
                    Some(column) => SelectedField::Column {
                        field_alias: sub_alias,
                        column_name: column.clone(),
                        output: ValueOutput::Cast("text".to_string()),
                    },
                    None => SelectedField::RowVersion {
                        field_alias: sub_alias,
                        schema: table_meta.schema.clone(),
                        table: table_meta.name.clone(),
                        key_columns: table_meta.primary_key.clone(),
                    },
                }),
                FieldKind::Connection(rel) => {
                    let join = join_for(rel, parent_alias, false);
                    let step_id = self.plan_connection(
                        plan,
                        sub_field,
                        &rel.target_schema,
                        &rel.target_table,
                        Some(join),
                        None,
                        alias_counter,
                    )?;
                    self.step_memo.borrow_mut().insert(memo_key, step_id);
                    fields.push(SelectedField::relation(sub_alias, step_id));
                }
                FieldKind::Relation(rel) => {
                    // A to-one relation onto the target's primary key can't multiply parent rows
                    let unique_target = !rel.is_many
                        && rel.junction.is_none()
                        && catalog
                            .find_table(&rel.target_schema, &rel.target_table)
                            .is_some_and(|t| {
                                let mut key = t.primary_key.clone();
                                let mut cols: Vec<String> =
                                    rel.columns.iter().map(|(c, _)| c.clone()).collect();
                                key.sort();
                                cols.sort();
                                !key.is_empty() && key == cols
                            });
                    let shared_alias =
                        (unique_target && sub_field.arguments.is_empty()).then(|| {
                            let key = format!(
                                "{parent_alias}|{}.{}|{:?}",
                                rel.target_schema, rel.target_table, rel.columns
                            );
                            self.join_aliases
                                .borrow_mut()
                                .entry(key)
                                .or_insert_with(|| {
                                    *alias_counter += 1;
                                    format!("t{}", *alias_counter - 1)
                                })
                                .clone()
                        });
                    let mut join = join_for(rel, parent_alias, unique_target);
                    // A foreign table's relation marked `batch`: its children are read for
                    // all parents at once (see `SqlCompiler::batched_body`)
                    join.batch = rel.is_many
                        && rel.junction.is_none()
                        && rel.period.is_none()
                        && rel.columns.len() == 1
                        && catalog
                            .find_table(&rel.target_schema, &rel.target_table)
                            .is_some_and(|child| {
                                child.foreign_keys.iter().any(|fk| {
                                    fk.batch
                                        && fk.foreign_schema == schema
                                        && fk.foreign_table == table_name
                                        && fk.columns == [rel.columns[0].0.clone()]
                                })
                            });
                    let step_id = self.plan_field(
                        plan,
                        sub_field,
                        &rel.target_schema,
                        &rel.target_table,
                        !rel.is_many,
                        Some(join),
                        shared_alias,
                        alias_counter,
                    )?;
                    self.step_memo.borrow_mut().insert(memo_key, step_id);
                    fields.push(SelectedField::relation(sub_alias, step_id));
                }
                FieldKind::ArcTarget => {
                    let Some(arc) = catalog
                        .exclusive_arcs
                        .iter()
                        .find(|a| a.table_name == table_name)
                    else {
                        continue;
                    };
                    // Plan each member type's fields (inline fragments) against its own alias
                    let mut type_fields = Vec::new();
                    for (i, (_, target_schema, target_table, _)) in arc.branches(schema).enumerate()
                    {
                        let Some(target_meta) = catalog.find_table(target_schema, target_table)
                        else {
                            continue;
                        };
                        let target_alias = format!("a{}", *alias_counter);
                        *alias_counter += 1;
                        let (target_fields, target_poly) = self.plan_row_shape(
                            plan,
                            &sub_field.children,
                            target_schema,
                            target_table,
                            &target_alias,
                            target_meta,
                            alias_counter,
                        )?;
                        type_fields.push(TypedFieldSet {
                            type_name: arc.target_type_name(i),
                            table_alias: target_alias,
                            fields: target_fields,
                            polymorphism: target_poly.map(Box::new),
                        });
                    }
                    fields.push(SelectedField::ExclusiveArcTarget {
                        field_alias: sub_alias,
                        arc: arc.clone(),
                        type_fields,
                    });
                }
                // A computed column: a function of the row, planned like a root function field
                FieldKind::Function(i) if catalog.functions[*i].row_argument => {
                    let row = ArgValue::Row {
                        alias: parent_alias.to_string(),
                        schema: table_meta.schema.clone(),
                        table: table_meta.name.clone(),
                        columns: table_meta.columns.iter().map(|c| c.name.clone()).collect(),
                    };
                    let step_id = self.plan_function(
                        plan,
                        sub_field,
                        &catalog.functions[*i],
                        false,
                        Some(row),
                        alias_counter,
                    )?;
                    fields.push(SelectedField::relation(sub_alias, step_id));
                }
                FieldKind::Function(i) => {
                    let func = &catalog.functions[*i];
                    fields.push(SelectedField::FunctionCall {
                        field_alias: sub_alias,
                        func: func.clone(),
                        arguments: self.bind_function_args(func, sub_field, true, None),
                        fields: self.plan_function_row_fields(&sub_field.children, func),
                    });
                }
            }
        }

        Ok(())
    }

    /// Aggregate selections (of an `aggregates` object or an aggregate group): `min`, `max`,
    /// `sum`, `avg`, `distinctCount`, `stddevSample`, `stddevPopulation`, `varianceSample`,
    /// `variancePopulation`, `median` and `percentile(fraction:)`, each over some columns.
    fn plan_aggregates(
        &self,
        selection_set: &[RequestField],
        table_meta: &TableMetadata,
    ) -> Result<AggregatesSelections, PlannerError> {
        let mut aggs = AggregatesSelections::default();
        for agg_sel in selection_set {
            let (function, fraction) = match agg_sel.name.as_str() {
                "min" => ("min", None),
                "max" => ("max", None),
                "sum" => ("sum", None),
                "avg" => ("avg", None),
                "distinctCount" => ("count_distinct", None),
                "stddevSample" => ("stddev_samp", None),
                "stddevPopulation" => ("stddev_pop", None),
                "varianceSample" => ("var_samp", None),
                "variancePopulation" => ("var_pop", None),
                "median" => ("percentile_cont", Some(0.5)),
                "percentile" => {
                    let fraction = agg_sel
                        .argument("fraction")
                        .and_then(|v| v.as_f64())
                        .filter(|f| (0.0..=1.0).contains(f))
                        .ok_or_else(|| {
                            PlannerError::InvalidInput(
                                "percentile: `fraction` must be between 0 and 1".to_string(),
                            )
                        })?;
                    ("percentile_cont", Some(fraction))
                }
                _ => continue,
            };
            let alias = agg_sel.response_key().to_string();
            if aggs.parts.iter().any(|p| p.field_alias == alias) {
                continue;
            }
            let mut fields = Vec::new();
            for col_sel in &agg_sel.children {
                let Some(col) = table_meta.column_by_field(col_sel.name.as_str()) else {
                    continue;
                };
                let key = col_sel.response_key().to_string();
                if fields.iter().any(|f: &AggregateField| f.field_alias == key) {
                    continue;
                }
                fields.push(AggregateField {
                    field_alias: key,
                    column_name: col.name.clone(),
                    is_range: col.pg_type.is_range(),
                    output: self.plan_output(&col.pg_type, col.enum_key.as_deref(), None),
                });
            }
            aggs.parts.push(AggregatePart {
                field_alias: alias,
                function: function.to_string(),
                fraction,
                fields,
            });
        }
        Ok(aggs)
    }

    /// Window values selected on a connection's edges: positions (`rowNumber`, `rank`,
    /// `denseRank`, `percentRank`) and running sums, over the matching rows in the
    /// connection's order, within `partitionBy` groups.
    fn plan_windows(
        &self,
        edge_fields: &[&RequestField],
        table_meta: &TableMetadata,
    ) -> Result<Vec<WindowSelection>, PlannerError> {
        let mut windows = Vec::new();
        for f in edge_fields {
            let function = match f.name.as_str() {
                "rowNumber" => "row_number",
                "rank" => "rank",
                "denseRank" => "dense_rank",
                "percentRank" => "percent_rank",
                "runningSum" => "running_sum",
                _ => continue,
            };
            let partition_arg = f.argument("partitionBy").cloned();
            let partition = self.plan_group_keys(partition_arg, table_meta, "partitionBy")?;
            if partition
                .iter()
                .any(|k| k.truncate.as_deref() == Some("bin"))
            {
                return Err(PlannerError::InvalidInput(
                    "partitionBy: `_BINNED` values need a bin width; truncate instead".to_string(),
                ));
            }
            let fields = if function == "running_sum" {
                f.children
                    .iter()
                    .filter_map(|c| {
                        let col = table_meta
                            .column_by_field(c.name.as_str())
                            .filter(|c| c.pg_type.is_numeric())?;
                        Some(AggregateField {
                            field_alias: c.response_key().to_string(),
                            column_name: col.name.clone(),
                            is_range: false,
                            output: ValueOutput::Plain,
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            windows.push(WindowSelection {
                field_alias: f.response_key().to_string(),
                function: function.to_string(),
                fields,
                partition,
                rank_keys: 0,
            });
        }
        Ok(windows)
    }

    /// `<Type>GroupBy` values as group keys.
    fn plan_group_keys(
        &self,
        value: Option<serde_json::Value>,
        table_meta: &TableMetadata,
        what: &str,
    ) -> Result<Vec<GroupKey>, PlannerError> {
        let values = table_meta.group_by_values();
        list_items(value)
            .iter()
            .map(|v| {
                let name = v.as_str().unwrap_or_default();
                let (_, col, unit) = values
                    .iter()
                    .find(|(value, _, _)| value == name)
                    .ok_or_else(|| {
                        PlannerError::InvalidInput(format!(
                            "{what}: `{}` cannot be grouped by {name}",
                            table_meta.type_name
                        ))
                    })?;
                Ok(GroupKey {
                    column: col.name.clone(),
                    truncate: unit.map(str::to_string),
                    is_date: col.pg_type == PgType::Date,
                    // A truncated date or timestamp is output as it is
                    output: match unit {
                        Some(_) => ValueOutput::Plain,
                        None => self.plan_output(&col.pg_type, col.enum_key.as_deref(), None),
                    },
                    sql_type: col.pg_type.sql_cast(),
                })
            })
            .collect()
    }

    /// A `groupedAggregates(groupBy:, having:)` selection of a connection.
    fn plan_grouped_aggregates(
        &self,
        field: &RequestField,
        table_meta: &TableMetadata,
    ) -> Result<GroupedAggregates, PlannerError> {
        let arg = |name: &str| field.argument(name).cloned();
        let keys = self.plan_group_keys(arg("groupBy"), table_meta, "groupBy")?;
        if keys.is_empty() {
            return Err(PlannerError::InvalidInput(
                "groupedAggregates: `groupBy` must name at least one value".to_string(),
            ));
        }

        // `having: { count: <IntFilter>, sum: { total: <FloatFilter> }, ... }`
        let mut having = Vec::new();
        let conditions = |function: &str,
                          column: Option<String>,
                          filter: &serde_json::Value,
                          having: &mut Vec<HavingCondition>| {
            for (operator, values) in comparisons(filter) {
                having.push(HavingCondition {
                    function: function.to_string(),
                    column: column.clone(),
                    operator,
                    values,
                });
            }
        };
        if let Some(serde_json::Value::Object(input)) = arg("having") {
            for (function, value) in &input {
                if function == "count" {
                    conditions("count", None, value, &mut having);
                    continue;
                }
                let serde_json::Value::Object(columns) = value else {
                    continue;
                };
                for (field_name, filter) in columns {
                    if let Some(col) = table_meta
                        .column_by_field(field_name)
                        .filter(|c| c.pg_type.is_numeric())
                    {
                        conditions(function, Some(col.name.clone()), filter, &mut having);
                    }
                }
            }
        }

        let text = |name: &str| arg(name).and_then(|v| v.as_str().map(str::to_string));
        let bins = match text("binWidth") {
            Some(width) => {
                let width = match scalars::convert(
                    &PgType::Interval,
                    serde_json::Value::String(width),
                    "binWidth",
                ) {
                    serde_json::Value::String(w) => w,
                    other => other.to_string(),
                };
                let origin = text("binOrigin").map(|o| {
                    match scalars::convert(
                        &PgType::TimestampTz,
                        serde_json::Value::String(o),
                        "binOrigin",
                    ) {
                        serde_json::Value::String(o) => o,
                        other => other.to_string(),
                    }
                });
                Some(TimeBins { width, origin })
            }
            None => None,
        };
        if keys.iter().any(|k| k.truncate.as_deref() == Some("bin")) && bins.is_none() {
            return Err(PlannerError::InvalidInput(
                "groupedAggregates: `_BINNED` keys need `binWidth`".to_string(),
            ));
        }
        let fill_gaps = arg("fillGaps").and_then(|v| v.as_bool()).unwrap_or(false);
        if fill_gaps && !matches!(keys.as_slice(), [k] if k.truncate.is_some()) {
            return Err(PlannerError::InvalidInput(
                "groupedAggregates: `fillGaps` needs a single truncated or binned date or timestamp key".to_string(),
            ));
        }
        let mut grouped = GroupedAggregates {
            field_alias: field.response_key().to_string(),
            keys,
            having,
            include_keys: false,
            include_count: false,
            aggregates: AggregatesSelections::default(),
            time_zone: text("timeZone"),
            bins,
            fill_gaps,
        };
        for sel in &field.children {
            match sel.name.as_str() {
                "keys" => grouped.include_keys = true,
                "count" => grouped.include_count = true,
                _ => {}
            }
        }
        grouped.aggregates = self.plan_aggregates(&field.children, table_meta)?;
        Ok(grouped)
    }

    /// Plan a root field backed by a function. Rows of a table returned as a set are a
    /// connection (queries only: a mutation returns the list of rows it produced).
    /// A computed column's first argument takes `row`.
    fn plan_function(
        &self,
        plan: &mut OperationPlan,
        field: &RequestField,
        func: &FunctionMetadata,
        mutation: bool,
        row: Option<ArgValue>,
        alias_counter: &mut usize,
    ) -> Result<StepId, PlannerError> {
        if let FunctionReturn::Table { schema, table } = &func.returns {
            if func.returns_set && !mutation {
                return self.plan_connection(
                    plan,
                    field,
                    schema,
                    table,
                    None,
                    Some((func, row)),
                    alias_counter,
                );
            }
        }
        let result = match &func.returns {
            FunctionReturn::Value(pg_type) => FunctionResult::Value(self.plan_output(
                pg_type,
                pg_type_enum_key(pg_type).as_deref(),
                Some(&field.children),
            )),
            FunctionReturn::Void => FunctionResult::Void,
            FunctionReturn::Record => {
                FunctionResult::Record(self.plan_function_row_fields(&field.children, func))
            }
            FunctionReturn::Table { schema, table } => {
                let table_meta = self.find_table(schema, table)?;
                let table_alias = format!("f{}", *alias_counter);
                *alias_counter += 1;
                let (fields, polymorphism) = self.plan_row_shape(
                    plan,
                    &field.children,
                    schema,
                    table,
                    &table_alias,
                    table_meta,
                    alias_counter,
                )?;
                FunctionResult::Rows {
                    schema: schema.clone(),
                    table: table.clone(),
                    table_alias,
                    fields,
                    polymorphism,
                    key_columns: table_meta
                        .primary_key
                        .iter()
                        .map(|c| (c.clone(), column_cast(table_meta, c)))
                        .collect(),
                }
            }
        };
        Ok(plan.add_step(Step::PgFunction {
            schema: func.schema.clone(),
            func_name: func.name.clone(),
            arguments: self.bind_function_args(func, field, false, row.as_ref()),
            returns_set: func.returns_set,
            unnest: func.returns_array,
            mutation,
            result,
        }))
    }

    /// Selections on a table-valued function's `<Func>Row` type: columns and `__typename`.
    fn plan_function_row_fields(
        &self,
        selection_set: &[RequestField],
        func: &FunctionMetadata,
    ) -> Vec<SelectedField> {
        let row_type = func.row_type_name();
        let mut fields: Vec<SelectedField> = Vec::new();
        for f in flatten_selections_for(selection_set, &[&row_type]) {
            let alias = f.response_key();
            if fields
                .iter()
                .any(|existing| existing.field_alias() == alias)
            {
                continue;
            }
            if f.name == "__typename" {
                fields.push(SelectedField::Typename {
                    field_alias: alias.to_string(),
                    type_name: row_type.clone(),
                });
            } else if let Some(column) = func
                .return_columns
                .iter()
                .find(|c| to_camel_case(&c.name) == f.name.as_str())
            {
                let enum_key = pg_type_enum_key(&column.pg_type);
                fields.push(SelectedField::Column {
                    field_alias: alias.to_string(),
                    column_name: column.name.clone(),
                    output: self.plan_output(
                        &column.pg_type,
                        enum_key.as_deref(),
                        Some(&f.children),
                    ),
                });
            }
        }
        fields
    }

    fn plan_row(
        &self,
        selection: &[RequestField],
        table: &TableMetadata,
        alias: &str,
    ) -> Result<(Vec<SelectedField>, Option<RowPolymorphism>, OperationPlan), PlannerError> {
        let mut plan = OperationPlan::new();
        let mut alias_counter = 1;
        let (fields, polymorphism) = self.plan_row_shape(
            &mut plan,
            selection,
            &table.schema,
            &table.name,
            alias,
            table,
            &mut alias_counter,
        )?;
        Ok((fields, polymorphism, plan))
    }

    /// Plans a selection on rows of `table_name` read from `alias`: plain fields, or — for a
    /// polymorphic table — a `RowPolymorphism` resolving every row to its concrete type.
    #[allow(clippy::too_many_arguments)]
    fn plan_row_shape(
        &self,
        plan: &mut OperationPlan,
        selection_set: &[RequestField],
        schema: &str,
        table_name: &str,
        alias: &str,
        table_meta: &TableMetadata,
        alias_counter: &mut usize,
    ) -> Result<(Vec<SelectedField>, Option<RowPolymorphism>), PlannerError> {
        if let Some(poly) = self.plan_row_polymorphism(
            plan,
            selection_set,
            schema,
            table_name,
            alias,
            alias_counter,
        )? {
            return Ok((Vec::new(), Some(poly)));
        }
        let mut fields = Vec::new();
        self.plan_selection_fields(
            plan,
            selection_set,
            schema,
            table_name,
            alias,
            table_meta,
            &mut fields,
            alias_counter,
        )?;
        Ok((fields, None))
    }

    /// Fields of a row of `table_name` (read from `alias`) exposed as `type_name`.
    ///
    /// A CTI child type also exposes every field of its parent interfaces. Those fields are
    /// planned against the parent table and read from the parent row: from `alias` itself for
    /// INHERITS children (which carry the parent columns), from a parent alias already in scope
    /// (`parent_aliases`, when the child was reached through its parent), or from a parent row
    /// LEFT JOINed here. Response keys keep the selection order.
    #[allow(clippy::too_many_arguments)]
    fn plan_object_fields(
        &self,
        plan: &mut OperationPlan,
        selection_set: &[RequestField],
        schema: &str,
        table_name: &str,
        alias: &str,
        table_meta: &TableMetadata,
        type_name: &str,
        parent_aliases: &[(String, String)],
        fields: &mut Vec<SelectedField>,
        alias_counter: &mut usize,
    ) -> Result<(), PlannerError> {
        let catalog = self.catalog();
        let parents = catalog.cti_parents_of(schema, table_name);
        if parents.is_empty() {
            return self.plan_selection_fields_as(
                plan,
                selection_set,
                schema,
                table_name,
                alias,
                table_meta,
                type_name,
                None,
                fields,
                alias_counter,
            );
        }

        let mut claimed: HashSet<String> = HashSet::new();
        let mut joins = Vec::new();
        let mut planned = Vec::new();
        for (cti, child) in parents {
            let Some(parent_meta) = catalog.find_table(&cti.schema, &cti.parent_table) else {
                continue;
            };
            let owned: HashSet<String> = SchemaBuilder::type_field_names(catalog, parent_meta)
                .into_iter()
                .filter(|n| !claimed.contains(n))
                .collect();
            claimed.extend(owned.iter().cloned());

            let parent_alias = if cti.kind == CtiKind::Inherits {
                alias.to_string()
            } else if let Some((_, a)) = parent_aliases.iter().find(|(t, _)| *t == cti.parent_table)
            {
                a.clone()
            } else {
                let a = format!("j{}", *alias_counter);
                *alias_counter += 1;
                joins.push(SelectedField::JoinRow {
                    schema: cti.schema.clone(),
                    table: cti.parent_table.clone(),
                    table_alias: a.clone(),
                    join_columns: child
                        .join_columns
                        .iter()
                        .map(|(c, p)| (p.clone(), c.clone()))
                        .collect(),
                });
                a
            };

            let mut parent_fields = Vec::new();
            let keep = |n: &str| owned.contains(n);
            self.plan_selection_fields_as(
                plan,
                selection_set,
                &cti.schema,
                &cti.parent_table,
                &parent_alias,
                parent_meta,
                type_name,
                Some(&keep),
                &mut parent_fields,
                alias_counter,
            )?;
            planned.extend(parent_fields.into_iter().map(|f| {
                if parent_alias == alias {
                    f
                } else {
                    SelectedField::OnAlias {
                        table_alias: parent_alias.clone(),
                        field: Box::new(f),
                    }
                }
            }));
        }

        let keep_own = |n: &str| !claimed.contains(n);
        self.plan_selection_fields_as(
            plan,
            selection_set,
            schema,
            table_name,
            alias,
            table_meta,
            type_name,
            Some(&keep_own),
            &mut planned,
            alias_counter,
        )?;

        // Restore selection order across the partitions
        let conditions = self.planner.type_conditions(type_name);
        let conditions: Vec<&str> = conditions.iter().map(String::as_str).collect();
        let order: Vec<String> = flatten_selections_for(selection_set, &conditions)
            .iter()
            .map(|f| f.response_key().to_string())
            .collect();
        planned.sort_by_key(|f| {
            order
                .iter()
                .position(|a| a == f.field_alias())
                .unwrap_or(usize::MAX)
        });

        fields.extend(joins);
        fields.extend(planned);
        Ok(())
    }

    /// Resolves rows of a polymorphic table (read from `table_alias`) to their concrete types.
    /// Each concrete type gets the fields its fragments (or fragments on the interface) select:
    ///
    /// - STI: every variant reads the base row under its own type name.
    /// - CTI: every child table is joined under a fresh alias; its parent fields come from
    ///   the base row already in scope.
    ///
    /// Returns `None` if the table is not polymorphic.
    fn plan_row_polymorphism(
        &self,
        plan: &mut OperationPlan,
        selection_set: &[RequestField],
        schema: &str,
        table_name: &str,
        table_alias: &str,
        alias_counter: &mut usize,
    ) -> Result<Option<RowPolymorphism>, PlannerError> {
        let catalog = self.catalog();
        let Some(poly) = catalog.find_polymorphism(schema, table_name) else {
            return Ok(None);
        };
        let table_meta = self.find_table(schema, table_name)?;

        // A concrete type reading the base row directly (STI variants, fallbacks)
        let base_row = |plan: &mut OperationPlan,
                        alias_counter: &mut usize,
                        type_name: &str|
         -> Result<TypedFieldSet, PlannerError> {
            let mut fields = Vec::new();
            self.plan_selection_fields_as(
                plan,
                selection_set,
                schema,
                table_name,
                table_alias,
                table_meta,
                type_name,
                None,
                &mut fields,
                alias_counter,
            )?;
            Ok(TypedFieldSet {
                type_name: type_name.to_string(),
                table_alias: table_alias.to_string(),
                fields,
                polymorphism: None,
            })
        };
        let fallback = match poly.fallback_type_name() {
            Some(name) => Some(base_row(plan, alias_counter, name)?),
            None => None,
        };

        let polymorphism = match poly {
            Polymorphism::Sti(sti) => {
                let mut variants = Vec::new();
                for v in &sti.variants {
                    variants.push(StiBranch {
                        discriminator_value: v.discriminator_value.clone(),
                        set: base_row(plan, alias_counter, &v.type_name)?,
                    });
                }
                RowPolymorphism::SingleTableInheritance {
                    discriminator_column: sti.discriminator_column.clone(),
                    variants,
                    fallback,
                }
            }
            Polymorphism::Cti(cti) => {
                let in_scope = [(table_name.to_string(), table_alias.to_string())];
                let mut children = Vec::new();
                for child in &cti.children {
                    let Some(child_meta) = catalog.find_table(&child.schema, &child.table) else {
                        continue;
                    };
                    let child_alias = format!("p{}", *alias_counter);
                    *alias_counter += 1;
                    let mut fields = Vec::new();
                    self.plan_object_fields(
                        plan,
                        selection_set,
                        &child.schema,
                        &child.table,
                        &child_alias,
                        child_meta,
                        &child.type_name,
                        &in_scope,
                        &mut fields,
                        alias_counter,
                    )?;
                    children.push(CtiRowEntry {
                        schema: child.schema.clone(),
                        table: child.table.clone(),
                        join_columns: child.join_columns.clone(),
                        set: TypedFieldSet {
                            type_name: child.type_name.clone(),
                            table_alias: child_alias,
                            fields,
                            polymorphism: None,
                        },
                    });
                }
                RowPolymorphism::ConcreteTableInheritance { children, fallback }
            }
        };
        Ok(Some(polymorphism))
    }

    /// `condition: { field: value }` → equality WHERE conditions.
    fn plan_condition(
        catalog: &DatabaseCatalog,
        cond_val: &serde_json::Value,
        table_meta: &TableMetadata,
    ) -> Vec<WhereCondition> {
        let mut where_conditions = Vec::new();
        let serde_json::Value::Object(obj) = cond_val else {
            return where_conditions;
        };
        for (k, v) in obj {
            let Some(col) = table_meta.column_by_field(k) else {
                continue;
            };
            if v.is_null() {
                continue;
            }
            where_conditions.push(WhereCondition {
                pg_cast: col.pg_type.sql_cast(),
                column: col.name.clone(),
                operator: "=".to_string(),
                value: input_value(catalog, col, v.clone()),
            });
        }
        where_conditions
    }

    /// `orderBy: [ENUM, ...]` (or a single enum) → SQL ORDER BY list.
    fn plan_order_by(
        &self,
        val: &serde_json::Value,
        table_alias: &str,
        table_meta: &TableMetadata,
    ) -> Option<String> {
        let clauses: Vec<String> = order_items(val)
            .into_iter()
            .filter_map(|s| {
                QueryPlanner::order_by_enum_to_sql(self.catalog(), s, table_alias, table_meta)
            })
            .collect();
        (!clauses.is_empty()).then(|| clauses.join(", "))
    }

    /// `orderBy: [ENUM, ...]` (or a single enum) of a connection → its keyset columns.
    fn plan_sort_keys(&self, val: &serde_json::Value, table_meta: &TableMetadata) -> Vec<SortKey> {
        order_items(val)
            .into_iter()
            .flat_map(|item| sort_keys_of(self.catalog(), item, table_meta))
            .collect()
    }

    fn as_usize(val: &serde_json::Value) -> Option<usize> {
        val.as_u64()
            .map(|n| n as usize)
            .or_else(|| val.as_str().and_then(|s| s.parse().ok()))
    }

    /// Resolve field name to schema name, table name, single vs collection semantics, and connection flag.
    fn resolve_table_name(
        &self,
        field_name: &str,
    ) -> Result<(String, String, bool, bool), PlannerError> {
        if let Some((table, single, connection)) = self
            .catalog()
            .resolve_root_field(&self.planner.default_schema, field_name)
        {
            return Ok((
                self.planner.default_schema.clone(),
                table.to_string(),
                single,
                connection,
            ));
        }
        if let Some((schema, table, single, connection)) =
            self.catalog().resolve_root_field_any(field_name)
        {
            return Ok((schema.to_string(), table.to_string(), single, connection));
        }
        Err(PlannerError::UnknownRootField(field_name.to_string()))
    }

    /// Recursively plan a GraphQL `filter: { ... }` object into a `FilterExpr`.
    fn plan_filter_object(
        &self,
        filter_val: &serde_json::Value,
        schema: &str,
        table_name: &str,
        table_alias: &str,
        alias_counter: &mut usize,
    ) -> Result<Option<FilterExpr>, PlannerError> {
        let serde_json::Value::Object(map) = filter_val else {
            return Ok(None);
        };
        let table_meta = self.find_table(schema, table_name)?;

        let mut and_clauses = Vec::new();
        for (k, v) in map {
            match k.as_str() {
                "and" | "or" => {
                    let serde_json::Value::Array(items) = v else {
                        continue;
                    };
                    let mut sub_exprs = Vec::new();
                    for item in items {
                        if let Some(sub) = self.plan_filter_object(
                            item,
                            schema,
                            table_name,
                            table_alias,
                            alias_counter,
                        )? {
                            sub_exprs.push(sub);
                        }
                    }
                    if !sub_exprs.is_empty() {
                        and_clauses.push(if k == "and" {
                            FilterExpr::And(sub_exprs)
                        } else {
                            FilterExpr::Or(sub_exprs)
                        });
                    }
                }
                "not" => {
                    if let Some(sub) =
                        self.plan_filter_object(v, schema, table_name, table_alias, alias_counter)?
                    {
                        and_clauses.push(FilterExpr::Not(Box::new(sub)));
                    }
                }
                field_key => {
                    if let Some(col) = table_meta.column_by_field(field_key) {
                        if let serde_json::Value::Object(op_map) = v {
                            for (op_k, op_v) in op_map {
                                if let Some(expr) = Self::plan_column_filter(
                                    self.catalog(),
                                    col,
                                    table_alias,
                                    op_k,
                                    op_v,
                                ) {
                                    and_clauses.push(expr);
                                }
                            }
                        }
                        continue;
                    }
                    // A column of a subtype of a table-per-type hierarchy: a subtype row
                    // having it matches
                    if let Some((_, members)) = self
                        .catalog()
                        .cti_columns(table_meta)
                        .into_iter()
                        .find(|(name, _)| name == field_key)
                    {
                        let serde_json::Value::Object(op_map) = v else {
                            continue;
                        };
                        let mut per_subtype = Vec::new();
                        for (child, col) in members {
                            let alias = format!("_filt{}", *alias_counter);
                            *alias_counter += 1;
                            let conditions: Vec<FilterExpr> = op_map
                                .iter()
                                .filter_map(|(op_k, op_v)| {
                                    Self::plan_column_filter(
                                        self.catalog(),
                                        col,
                                        &alias,
                                        op_k,
                                        op_v,
                                    )
                                })
                                .collect();
                            if conditions.is_empty() {
                                continue;
                            }
                            per_subtype.push(FilterExpr::Exists {
                                related: RelatedRows {
                                    schema: child.schema.clone(),
                                    table: child.table.clone(),
                                    alias,
                                    columns: child.join_columns.clone(),
                                    parent_alias: table_alias.to_string(),
                                    period: None,
                                    junction: None,
                                },
                                filter: Some(Box::new(FilterExpr::And(conditions))),
                            });
                        }
                        match per_subtype.len() {
                            0 => {}
                            1 => and_clauses.push(per_subtype.remove(0)),
                            _ => and_clauses.push(FilterExpr::Or(per_subtype)),
                        }
                        continue;
                    }
                    // Relation filters: a to-one relation's row matches
                    // (`author: { username: { startsWith: "dr_" } }`); for to-many relations,
                    // `some`, `every` or `none` of the rows match, or their aggregates do
                    // (`messages: { count: { greaterThan: 5 } }`)
                    let rel = match self
                        .catalog()
                        .object_field(schema, table_name, field_key)
                        .map(|f| &f.kind)
                    {
                        Some(FieldKind::Relation(rel)) => rel.clone(),
                        _ => continue,
                    };
                    let target = self.find_table(&rel.target_schema, &rel.target_table)?;
                    let related = |alias_counter: &mut usize| {
                        let alias = format!("_filt{}", *alias_counter);
                        *alias_counter += 1;
                        RelatedRows {
                            schema: rel.target_schema.clone(),
                            table: rel.target_table.clone(),
                            alias,
                            columns: rel.columns.clone(),
                            parent_alias: table_alias.to_string(),
                            period: rel.period.clone(),
                            junction: join_for(&rel, table_alias, false).junction,
                        }
                    };
                    if !rel.is_many {
                        let rows = related(alias_counter);
                        let nested = self.plan_filter_object(
                            v,
                            &rel.target_schema,
                            &rel.target_table,
                            &rows.alias,
                            alias_counter,
                        )?;
                        and_clauses.push(FilterExpr::Exists {
                            related: rows,
                            filter: nested.map(Box::new),
                        });
                        continue;
                    }
                    let serde_json::Value::Object(ops) = v else {
                        continue;
                    };
                    for (op, value) in ops {
                        match op.as_str() {
                            "some" | "every" | "none" => {
                                if value.is_null() {
                                    continue;
                                }
                                let rows = related(alias_counter);
                                let nested = self.plan_filter_object(
                                    value,
                                    &rel.target_schema,
                                    &rel.target_table,
                                    &rows.alias,
                                    alias_counter,
                                )?;
                                and_clauses.push(match (op.as_str(), nested) {
                                    ("some", nested) => FilterExpr::Exists {
                                        related: rows,
                                        filter: nested.map(Box::new),
                                    },
                                    ("none", nested) => {
                                        FilterExpr::Not(Box::new(FilterExpr::Exists {
                                            related: rows,
                                            filter: nested.map(Box::new),
                                        }))
                                    }
                                    // Every row matches: none fails to (an empty filter matches every row)
                                    (_, Some(nested)) => {
                                        FilterExpr::Not(Box::new(FilterExpr::Exists {
                                            related: rows,
                                            filter: Some(Box::new(FilterExpr::NotTrue(Box::new(
                                                nested,
                                            )))),
                                        }))
                                    }
                                    (_, None) => continue,
                                });
                            }
                            "count" => {
                                for (operator, values) in comparisons(value) {
                                    let rows = related(alias_counter);
                                    and_clauses.push(FilterExpr::RelatedAggregate {
                                        related: rows,
                                        function: "count".to_string(),
                                        column: None,
                                        operator,
                                        values,
                                    });
                                }
                            }
                            function @ ("sum" | "avg" | "min" | "max") => {
                                let serde_json::Value::Object(columns) = value else {
                                    continue;
                                };
                                for (field_name, filter) in columns {
                                    let Some(col) = target
                                        .column_by_field(field_name)
                                        .filter(|c| c.pg_type.is_numeric())
                                    else {
                                        continue;
                                    };
                                    for (operator, values) in comparisons(filter) {
                                        let rows = related(alias_counter);
                                        and_clauses.push(FilterExpr::RelatedAggregate {
                                            related: rows,
                                            function: function.to_string(),
                                            column: Some(col.name.clone()),
                                            operator,
                                            values,
                                        });
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(match and_clauses.len() {
            0 => None,
            1 => Some(and_clauses.remove(0)),
            _ => Some(FilterExpr::And(and_clauses)),
        })
    }

    /// Operators of filters on types with operators of their own: full-text documents, JSON
    /// paths and keys, geometries, multiranges. `None`: not one of these (`Some(None)`: an
    /// operator of such a type without a usable value).
    fn plan_typed_filter(
        col: &ColumnMetadata,
        op: &str,
        value: &serde_json::Value,
        template: &dyn Fn(&str, Vec<serde_json::Value>) -> FilterExpr,
    ) -> Option<Option<FilterExpr>> {
        let text = || {
            value
                .as_str()
                .map(|s| serde_json::Value::String(s.to_string()))
        };
        let texts = || -> Option<serde_json::Value> {
            let items: Vec<String> = value
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            Some(pg_array_literal(&serde_json::json!(items)))
        };
        match (&col.pg_type, op) {
            (PgType::TsVector, "matches") => {
                Some(text().map(|q| template("{c} @@ websearch_to_tsquery({0})", vec![q])))
            }
            (PgType::Json | PgType::Jsonb, _) => {
                let doc = if col.pg_type == PgType::Json {
                    "{c}::jsonb"
                } else {
                    "{c}"
                };
                let sql = match op {
                    "matchesPath" => format!("{doc} @@ {{0}}::jsonpath"),
                    "existsPath" => format!("{doc} @? {{0}}::jsonpath"),
                    "hasKey" => format!("{doc} ? {{0}}"),
                    "hasAnyKeys" => {
                        return Some(
                            texts().map(|v| template(&format!("{doc} ?| {{0}}::text[]"), vec![v])),
                        )
                    }
                    "hasAllKeys" => {
                        return Some(
                            texts().map(|v| template(&format!("{doc} ?& {{0}}::text[]"), vec![v])),
                        )
                    }
                    _ => return None,
                };
                Some(text().map(|v| template(&sql, vec![v])))
            }
            (spatial, _) if spatial.is_spatial() => {
                let geo = |v: &serde_json::Value| scalars::geojson(v.clone(), &col.field_name);
                let cast = if *spatial == PgType::Geography {
                    "::geography"
                } else {
                    ""
                };
                let shape = format!("ST_GeomFromGeoJSON({{0}}){cast}");
                let (within, contains) = if cast.is_empty() {
                    ("ST_Within", "ST_Contains")
                } else {
                    ("ST_CoveredBy", "ST_Covers")
                };
                Some(match op {
                    "intersects" => Some(template(
                        &format!("ST_Intersects({{c}}, {shape})"),
                        vec![geo(value)],
                    )),
                    "within" => Some(template(
                        &format!("{within}({{c}}, {shape})"),
                        vec![geo(value)],
                    )),
                    "contains" => Some(template(
                        &format!("{contains}({{c}}, {shape})"),
                        vec![geo(value)],
                    )),
                    "withinDistance" => {
                        let shape_value = geo(value.get("geometry")?);
                        let distance =
                            serde_json::Value::String(value.get("distance")?.as_f64()?.to_string());
                        Some(template(
                            &format!("ST_DWithin({{c}}, {shape}, {{1}}::float8)"),
                            vec![shape_value, distance],
                        ))
                    }
                    "intersectsBox" => {
                        let n = |k: &str| {
                            value
                                .get(k)
                                .and_then(|v| v.as_f64())
                                .map(|f| serde_json::Value::String(f.to_string()))
                        };
                        let srid = value
                            .get("srid")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(4326)
                            .to_string();
                        let values = vec![
                            n("xmin")?,
                            n("ymin")?,
                            n("xmax")?,
                            n("ymax")?,
                            serde_json::Value::String(srid),
                        ];
                        Some(template(
                            &format!("{{c}} && ST_MakeEnvelope({{0}}::float8, {{1}}::float8, {{2}}::float8, {{3}}::float8, {{4}}::int){cast}"),
                            values,
                        ))
                    }
                    _ => return None,
                })
            }
            (PgType::Multirange(range), _) => {
                let (element, _) = range.range_element()?;
                let multirange = col.pg_type.sql_cast()?;
                let range_cast = range.sql_cast()?;
                let range_value =
                    |v: &serde_json::Value| scalars::range_literal(&element, v, &col.field_name);
                let element_cast = element.sql_cast()?;
                Some(match op {
                    "contains" => Some(template(
                        &format!("{{c}} @> {{0}}::{element_cast}"),
                        vec![scalars::convert(&element, value.clone(), &col.field_name)],
                    )),
                    "containsRange" => Some(template(
                        &format!("{{c}} @> {{0}}::{range_cast}"),
                        vec![range_value(value)],
                    )),
                    "overlaps" => Some(template(
                        &format!("{{c}} && {{0}}::{range_cast}"),
                        vec![range_value(value)],
                    )),
                    "containedBy" => Some(template(
                        &format!("{{c}} <@ {{0}}::{range_cast}"),
                        vec![range_value(value)],
                    )),
                    "isEmpty" => Some(template(
                        if value.as_bool()? {
                            "isempty({c})"
                        } else {
                            "NOT isempty({c})"
                        },
                        Vec::new(),
                    )),
                    "equalTo" | "notEqualTo" => {
                        let lit = match database_value_multirange(range, value, &col.field_name) {
                            serde_json::Value::String(s) => serde_json::Value::String(s),
                            _ => return Some(None),
                        };
                        let operator = if op == "equalTo" { "=" } else { "<>" };
                        Some(template(
                            &format!("{{c}} {operator} {{0}}::{multirange}"),
                            vec![lit],
                        ))
                    }
                    _ => return None,
                })
            }
            _ => None,
        }
    }

    /// One operator of a column filter (`{ equalTo: ... }`, `{ startsWith: ... }`, ...).
    fn plan_column_filter(
        catalog: &DatabaseCatalog,
        col: &ColumnMetadata,
        table_alias: &str,
        op: &str,
        value: &serde_json::Value,
    ) -> Option<FilterExpr> {
        let template = |sql: &str, values: Vec<serde_json::Value>| FilterExpr::Template {
            column: col.name.clone(),
            table_alias: table_alias.to_string(),
            sql: sql.to_string(),
            values,
        };
        if let Some(expr) = Self::plan_typed_filter(col, op, value, &template) {
            return expr;
        }
        let input = |value: &serde_json::Value| input_value(catalog, col, value.clone());
        let binary = |operator: &str, value: serde_json::Value, pg_cast: Option<String>| {
            FilterExpr::Binary {
                column: col.name.clone(),
                table_alias: table_alias.to_string(),
                operator: operator.to_string(),
                value,
                pg_cast,
            }
        };
        let pattern = |operator: &str, pattern: String| FilterExpr::Pattern {
            column: col.name.clone(),
            table_alias: table_alias.to_string(),
            operator: operator.to_string(),
            pattern,
        };
        let in_list = |is_not: bool| {
            let serde_json::Value::Array(arr) = value else {
                return None;
            };
            Some(FilterExpr::InList {
                column: col.name.clone(),
                table_alias: table_alias.to_string(),
                values: arr.iter().map(input).collect(),
                is_not,
                pg_cast: col.pg_type.sql_cast(),
            })
        };
        let text = || value.as_str().map(str::to_string);
        let cast = col.pg_type.sql_cast();
        let is_array = col.pg_type.is_array();

        // Regular expressions, `IS DISTINCT FROM`, and a comparison against any or all of a list
        let is_text = matches!(
            col.pg_type,
            PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext
        );
        match op {
            "regex" if is_text => return Some(template("{c} ~ {0}", vec![value.clone()])),
            "iregex" if is_text => return Some(template("{c} ~* {0}", vec![value.clone()])),
            // Parameters are text: a null is no value to bind
            "isDistinctFrom" if value.is_null() => {
                return Some(template("{c} IS NOT NULL", vec![]))
            }
            "isDistinctFrom" => {
                let cast = cast
                    .as_deref()
                    .map(|c| format!("::{c}"))
                    .unwrap_or_default();
                return Some(template(
                    &format!("{{c}} IS DISTINCT FROM {{0}}{cast}"),
                    vec![input(value)],
                ));
            }
            "likeAny" | "likeAll" | "ilikeAny" | "ilikeAll" if is_text => {
                let like = if op.starts_with('i') { "ILIKE" } else { "LIKE" };
                let quantifier = if op.ends_with("All") { "ALL" } else { "ANY" };
                let list = value.as_array()?;
                return Some(template(
                    &format!("{{c}} {like} {quantifier}({{0}}::text[])"),
                    vec![pg_array_literal(&serde_json::Value::Array(list.clone()))],
                ));
            }
            _ => {}
        }
        for (name, sql) in [
            ("lessThan", "<"),
            ("lessThanOrEqualTo", "<="),
            ("greaterThan", ">"),
            ("greaterThanOrEqualTo", ">="),
        ] {
            let quantifier = match op.strip_prefix(name) {
                Some("Any") => "ANY",
                Some("All") => "ALL",
                _ => continue,
            };
            let (list, cast) = (value.as_array()?, cast.as_deref()?);
            let items: Vec<serde_json::Value> = list.iter().map(input).collect();
            return Some(template(
                &format!("{{c}} {sql} {quantifier}({{0}}::{cast}[])"),
                vec![pg_array_literal(&serde_json::Value::Array(items))],
            ));
        }

        Some(match op {
            "isNull" => FilterExpr::IsNull {
                column: col.name.clone(),
                table_alias: table_alias.to_string(),
                is_null: value.as_bool()?,
            },
            "equalTo" => binary("=", input(value), cast),
            "notEqualTo" => binary("<>", input(value), cast),
            "in" => in_list(false)?,
            "notIn" => in_list(true)?,
            "lessThan" => binary("<", input(value), cast),
            "lessThanOrEqualTo" => binary("<=", input(value), cast),
            "greaterThan" => binary(">", input(value), cast),
            "greaterThanOrEqualTo" => binary(">=", input(value), cast),
            // User-supplied patterns keep their wildcards
            "like" => pattern("LIKE", text()?),
            "notLike" => pattern("NOT LIKE", text()?),
            "ilike" => pattern("ILIKE", text()?),
            "notIlike" => pattern("NOT ILIKE", text()?),
            // Literal substrings: `%` and `_` in the value match themselves
            "startsWith" => pattern("LIKE", format!("{}%", escape_like(&text()?))),
            "notStartsWith" => pattern("NOT LIKE", format!("{}%", escape_like(&text()?))),
            "endsWith" => pattern("LIKE", format!("%{}", escape_like(&text()?))),
            "notEndsWith" => pattern("NOT LIKE", format!("%{}", escape_like(&text()?))),
            // `ltree` paths: above or below a path, matching a pattern
            "ancestorOf" if col.pg_type == PgType::Ltree => binary("@>", input(value), cast),
            "descendantOf" if col.pg_type == PgType::Ltree => binary("<@", input(value), cast),
            "matches" if col.pg_type == PgType::Ltree => {
                binary("~", value.clone(), Some("lquery".to_string()))
            }
            "matchesText" if col.pg_type == PgType::Ltree => {
                binary("@", value.clone(), Some("ltxtquery".to_string()))
            }
            // A range contains an element
            "contains" | "includes" if col.pg_type.is_range() => {
                let (element, _) = col.pg_type.range_element()?;
                binary(
                    "@>",
                    scalars::convert(&element, value.clone(), &col.field_name),
                    element.sql_cast(),
                )
            }
            "contains" | "includes" if matches!(col.pg_type, PgType::Json | PgType::Jsonb) => {
                binary("@>", value.clone(), Some("jsonb".to_string()))
            }
            "contains" | "includes" if is_array => binary("@>", input(value), cast),
            "contains" | "includes" => pattern("LIKE", format!("%{}%", escape_like(&text()?))),
            "notContains" => pattern("NOT LIKE", format!("%{}%", escape_like(&text()?))),
            "containedBy" if is_array => binary("<@", input(value), cast),
            "containedBy" if matches!(col.pg_type, PgType::Json | PgType::Jsonb) => {
                binary("<@", value.clone(), Some("jsonb".to_string()))
            }
            "containedBy" => binary("<@", input(value), cast),
            "overlaps" if is_array => binary("&&", input(value), cast),
            "overlaps" => binary("&&", input(value), cast),
            "strictlyLeftOf" => binary("<<", input(value), cast),
            "strictlyRightOf" => binary(">>", input(value), cast),
            "adjacentTo" => binary("-|-", input(value), cast),
            _ => return None,
        })
    }
}

/// The comparisons of a numeric filter (`IntFilter`, `FloatFilter`) as `(operator, values)`:
/// `{ greaterThan: 5 }` is `(">", [5])`, `{ isNull: true }` is `("IS NULL", [])`.
fn comparisons(filter: &serde_json::Value) -> Vec<(String, Vec<serde_json::Value>)> {
    let serde_json::Value::Object(ops) = filter else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (op, value) in ops {
        let (operator, values) = match op.as_str() {
            "equalTo" => ("=", vec![value.clone()]),
            "notEqualTo" => ("<>", vec![value.clone()]),
            "lessThan" => ("<", vec![value.clone()]),
            "lessThanOrEqualTo" => ("<=", vec![value.clone()]),
            "greaterThan" => (">", vec![value.clone()]),
            "greaterThanOrEqualTo" => (">=", vec![value.clone()]),
            "in" => ("IN", list_items(Some(value.clone()))),
            "notIn" => ("NOT IN", list_items(Some(value.clone()))),
            "isNull" if value.as_bool() == Some(true) => ("IS NULL", Vec::new()),
            "isNull" if value.as_bool() == Some(false) => ("IS NOT NULL", Vec::new()),
            _ => continue,
        };
        if value.is_null() && !operator.starts_with("IS") {
            continue;
        }
        out.push((operator.to_string(), values));
    }
    out
}

/// Native SQL cast for comparing a text-bound parameter against `column`.
/// Items of a list input; a single value where a list is expected is a list of one (input
/// coercion), null or absent is empty.
fn list_items(value: Option<serde_json::Value>) -> Vec<serde_json::Value> {
    match value {
        Some(serde_json::Value::Array(items)) => items,
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(other) => vec![other],
    }
}

/// The non-null `connect` and `create` of a to-one relation input.
fn one_of(value: &serde_json::Value) -> (Option<&serde_json::Value>, Option<&serde_json::Value>) {
    let field = |name: &str| value.get(name).filter(|v| !v.is_null());
    (field("connect"), field("create"))
}

/// Appends a node for a row of `table` and returns its index.
fn push_node(
    nodes: &mut Vec<WriteNode>,
    table: &TableMetadata,
    kind: WriteNodeKind,
    values: Vec<ColumnValue>,
    refs: Vec<NodeRef>,
    path: String,
) -> usize {
    nodes.push(WriteNode {
        schema: table.schema.clone(),
        table: table.name.clone(),
        kind,
        values,
        refs,
        path,
        after: Vec::new(),
    });
    nodes.len() - 1
}

/// Items of an `orderBy` argument: a list of enum values, or a single one.
fn order_items(val: &serde_json::Value) -> Vec<&str> {
    match val {
        serde_json::Value::String(s) => vec![s.as_str()],
        serde_json::Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
        _ => Vec::new(),
    }
}

/// The keyset columns (or expressions) of one `<Type>OrderBy` value: `NAME_ASC`, `NAME_DESC`,
/// and either with `_NULLS_FIRST` / `_NULLS_LAST`.
fn sort_keys_of(catalog: &DatabaseCatalog, item: &str, table_meta: &TableMetadata) -> Vec<SortKey> {
    let (item, nulls_first) = match item.strip_suffix("_NULLS_FIRST") {
        Some(rest) => (rest, Some(true)),
        None => match item.strip_suffix("_NULLS_LAST") {
            Some(rest) => (rest, Some(false)),
            None => (item, None),
        },
    };
    let mut keys = sort_keys_in_default_nulls(catalog, item, table_meta);
    for key in &mut keys {
        key.nulls_first = nulls_first;
    }
    keys
}

fn sort_keys_in_default_nulls(
    catalog: &DatabaseCatalog,
    item: &str,
    table_meta: &TableMetadata,
) -> Vec<SortKey> {
    let key = |descending| -> Vec<SortKey> {
        table_meta
            .primary_key
            .iter()
            .map(|pk| sort_key(table_meta, pk, descending))
            .collect()
    };
    let (name, descending) = match item {
        "NATURAL" | "PRIMARY_KEY_ASC" => return key(false),
        "PRIMARY_KEY_DESC" => return key(true),
        other => match other.strip_suffix("_ASC") {
            Some(name) => (name, false),
            None => match other.strip_suffix("_DESC") {
                Some(name) => (name, true),
                None => return Vec::new(),
            },
        },
    };
    let Some((_, target)) = catalog
        .order_options(table_meta)
        .into_iter()
        .find(|(n, _)| n == name)
    else {
        return Vec::new();
    };
    let expression = |expr: SortExpr, compare: SortCompare, nullable: bool| SortKey {
        column: format!("__{name}"),
        descending,
        nullable,
        compare,
        expr: Some(expr),
        nulls_first: None,
    };
    let cast = |t: &PgType| t.sql_cast().map_or(SortCompare::Text, SortCompare::Cast);
    let key = match target {
        OrderTarget::Column(column) => sort_key(table_meta, &column, descending),
        OrderTarget::Related { rel, column } => {
            let Some(target) = catalog.find_table(&rel.target_schema, &rel.target_table) else {
                return Vec::new();
            };
            let Some(col) = target.get_column(&column) else {
                return Vec::new();
            };
            let expr = SortExpr::Related {
                schema: rel.target_schema.clone(),
                table: rel.target_table.clone(),
                columns: rel.columns.clone(),
                period: rel.period.clone(),
                column,
            };
            // A missing related row sorts as NULL
            expression(expr, cast(&col.pg_type), true)
        }
        OrderTarget::Count(rel) => {
            let related = RelatedRows {
                schema: rel.target_schema.clone(),
                table: rel.target_table.clone(),
                alias: String::new(),
                columns: rel.columns.clone(),
                parent_alias: String::new(),
                period: rel.period.clone(),
                junction: join_for(&rel, "", false).junction,
            };
            expression(
                SortExpr::Count { related },
                SortCompare::Cast("int8".to_string()),
                false,
            )
        }
        OrderTarget::Function(i) => {
            let f = &catalog.functions[i];
            let FunctionReturn::Value(returns) = &f.returns else {
                return Vec::new();
            };
            let expr = SortExpr::Function {
                schema: f.schema.clone(),
                name: f.name.clone(),
                row_schema: table_meta.schema.clone(),
                row_table: table_meta.name.clone(),
                columns: table_meta.columns.iter().map(|c| c.name.clone()).collect(),
            };
            expression(expr, cast(returns), true)
        }
    };
    vec![key]
}

/// Keyset order column; a column missing from the metadata compares as text.
fn sort_key(table_meta: &TableMetadata, column: &str, descending: bool) -> SortKey {
    let meta = table_meta.get_column(column);
    let compare = match meta.map(|c| (&c.pg_type, c.pg_type.sql_cast())) {
        Some((_, Some(cast))) => SortCompare::Cast(cast),
        _ => SortCompare::Text,
    };
    SortKey {
        column: column.to_string(),
        descending,
        nullable: meta.is_none_or(|c| c.is_nullable),
        compare,
        expr: None,
        nulls_first: None,
    }
}

/// How an entity lookup finds its row: by key values, or joined to a group's keys.
enum Lookup {
    Values(Vec<WhereCondition>),
    Join(JoinCondition),
}

fn column_cast(table_meta: &TableMetadata, column: &str) -> Option<String> {
    table_meta
        .get_column(column)
        .and_then(|c| c.pg_type.sql_cast())
}
