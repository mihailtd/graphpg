//! Safety limits, enforced on the [`OperationPlan`] so every API that plans onto it (GraphQL,
//! REST) shares one implementation and cannot diverge.
//!
//! A plan knows everything that makes a request expensive: the page size of each connection,
//! how relations nest, how many rows a write touches. [`enforce`] applies the defaults the
//! limits imply and rejects a plan that exceeds them, before any SQL is compiled:
//!
//! - **Page size.** A connection without `first` / `last` reads `page_default` rows; one that
//!   asks for more than `page_max` is rejected. Lists without a page size (plain list fields,
//!   to-many relations) are cut at `page_max` rows: page through a connection to read more.
//! - **Depth.** How many levels of rows the operation nests (a root list is 1, the rows of a
//!   relation under it 2, ...).
//! - **Cost.** An estimate of the rows the operation reads: each step's rows (its page size, 1
//!   for a single row) times the rows of the steps it hangs from, summed. Fetching 100
//!   forums with their last 100 messages and each message's author costs
//!   `100 + 100 × 100 + 100 × 100`.
//! - **Bulk rows.** The rows one write inserts, nested writes included.

use super::operation_plan::OperationPlan;
use super::step::{
    FunctionResult, MutationKind, PayloadField, RowPolymorphism, SelectedField, Step, StepId,
    TypedFieldSet,
};
use std::collections::HashSet;

/// The limits of one deployment. `None` means no limit; [`Limits::default`] has none, so a
/// planner used on its own behaves as it always did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Rows a connection reads without `first` / `last`; `None` leaves such connections as
    /// they are (the compiler's page size, or all rows for an export).
    pub page_default: Option<usize>,
    /// Largest `first` / `last`, and the most rows of a list without a page size.
    pub page_max: Option<usize>,
    /// Deepest nesting of rows.
    pub depth: Option<usize>,
    /// Largest estimated cost (rows read).
    pub cost: Option<u64>,
    /// Most rows one write inserts.
    pub bulk_rows: Option<usize>,
    /// An update or delete by `where` must have a condition: `where: {}` (every row) needs
    /// `unfiltered: true`.
    pub require_filter: bool,
}

impl Limits {
    /// The page size connections have always had when none is asked for.
    pub const DEFAULT_PAGE_SIZE: usize = 100;

    pub const fn unlimited() -> Self {
        Self {
            page_default: None,
            page_max: None,
            depth: None,
            cost: None,
            bulk_rows: None,
            require_filter: false,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// Which limit a plan exceeds.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LimitError {
    #[error("`{argument}` is {requested}, more than the maximum page size of {max}")]
    PageSize {
        argument: &'static str,
        requested: usize,
        max: usize,
    },
    #[error("the operation nests rows {depth} levels deep; at most {max} are allowed")]
    Depth { depth: usize, max: usize },
    #[error(
        "the operation is too expensive: it would read an estimated {cost} rows; at most {max} are allowed"
    )]
    Cost { cost: u64, max: u64 },
    #[error("the operation writes {rows} rows; at most {max} are allowed")]
    BulkRows { rows: usize, max: usize },
}

impl LimitError {
    /// A stable, machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::PageSize { .. } => "PAGE_SIZE_EXCEEDED",
            Self::Depth { .. } => "DEPTH_LIMIT_EXCEEDED",
            Self::Cost { .. } => "COST_LIMIT_EXCEEDED",
            Self::BulkRows { .. } => "BULK_LIMIT_EXCEEDED",
        }
    }

    /// The limit that was exceeded.
    pub fn limit(&self) -> u64 {
        match self {
            Self::PageSize { max, .. } | Self::BulkRows { max, .. } => *max as u64,
            Self::Depth { max, .. } => *max as u64,
            Self::Cost { max, .. } => *max,
        }
    }

    /// What the operation asked for.
    pub fn actual(&self) -> u64 {
        match self {
            Self::PageSize { requested, .. } => *requested as u64,
            Self::Depth { depth, .. } => *depth as u64,
            Self::Cost { cost, .. } => *cost,
            Self::BulkRows { rows, .. } => *rows as u64,
        }
    }
}

/// What [`enforce`] measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanStats {
    pub depth: usize,
    pub cost: u64,
    pub bulk_rows: usize,
}

/// Applies the page defaults to `plan` and checks it against `limits`.
pub fn enforce(plan: &mut OperationPlan, limits: &Limits) -> Result<PlanStats, LimitError> {
    apply_page_limits(plan, limits)?;
    if limits.depth.is_none() && limits.cost.is_none() && limits.bulk_rows.is_none() {
        return Ok(PlanStats::default());
    }
    let stats = measure(plan, limits);
    if let Some(max) = limits.bulk_rows {
        if stats.bulk_rows > max {
            return Err(LimitError::BulkRows {
                rows: stats.bulk_rows,
                max,
            });
        }
    }
    if let Some(max) = limits.depth {
        if stats.depth > max {
            return Err(LimitError::Depth {
                depth: stats.depth,
                max,
            });
        }
    }
    if let Some(max) = limits.cost {
        if stats.cost > max {
            return Err(LimitError::Cost {
                cost: stats.cost,
                max,
            });
        }
    }
    Ok(stats)
}

/// Fills in page sizes and rejects the ones that are too large.
fn apply_page_limits(plan: &mut OperationPlan, limits: &Limits) -> Result<(), LimitError> {
    let check =
        |argument: &'static str, requested: Option<usize>| match (requested, limits.page_max) {
            (Some(requested), Some(max)) if requested > max => Err(LimitError::PageSize {
                argument,
                requested,
                max,
            }),
            _ => Ok(()),
        };
    for step in plan.steps_mut() {
        match step {
            Step::PgConnection { first, last, .. } => {
                check("first", *first)?;
                check("last", *last)?;
                if let (None, None, Some(default)) = (*first, *last, limits.page_default) {
                    *first = Some(default);
                }
            }
            Step::PgSelect {
                is_single: false,
                limit,
                ..
            } => {
                check("first", *limit)?;
                if limit.is_none() {
                    *limit = limits.page_max;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

struct Measure<'a> {
    plan: &'a OperationPlan,
    limits: &'a Limits,
    stats: PlanStats,
    seen: HashSet<StepId>,
}

/// Depth, estimated cost and bulk rows of the plan.
fn measure(plan: &OperationPlan, limits: &Limits) -> PlanStats {
    let mut m = Measure {
        plan,
        limits,
        stats: PlanStats::default(),
        seen: HashSet::new(),
    };
    for (_, id) in plan.root_fields() {
        m.step(*id, 1, 1);
    }
    m.stats
}

impl Measure<'_> {
    fn add_cost(&mut self, rows: u64) {
        self.stats.cost = self.stats.cost.saturating_add(rows);
    }

    fn step(&mut self, id: StepId, parents: u64, depth: usize) {
        // A step several fields lead to is planned (and costs) once
        if !self.seen.insert(id) {
            return;
        }
        let Some(step) = self.plan.get_step(id) else {
            return;
        };
        self.stats.depth = self.stats.depth.max(depth);
        let default = self
            .limits
            .page_default
            .unwrap_or(Limits::DEFAULT_PAGE_SIZE) as u64;
        match step {
            Step::PgConnection {
                first,
                last,
                selections,
                ..
            } => {
                let page = first.or(*last).map_or(default, |n| n as u64);
                let rows = parents.saturating_mul(page);
                self.add_cost(rows);
                self.fields(&selections.node_fields, rows, depth + 1);
                if let Some(p) = &selections.node_polymorphism {
                    self.polymorphism(p, rows, depth + 1);
                }
            }
            Step::PgSelect {
                is_single,
                limit,
                fields,
                polymorphism,
                ..
            } => {
                let page = if *is_single {
                    1
                } else {
                    limit.map_or(default, |n| n as u64)
                };
                let rows = parents.saturating_mul(page);
                self.add_cost(rows);
                self.fields(fields, rows, depth + 1);
                if let Some(p) = polymorphism {
                    self.polymorphism(p, rows, depth + 1);
                }
            }
            Step::PgSearch {
                limit, selections, ..
            } => {
                let rows = parents.saturating_mul(*limit as u64);
                self.add_cost(rows);
                self.fields(&selections.node_fields, rows, depth + 1);
            }
            Step::PgGlobalSearch { limit, .. } => {
                self.add_cost(parents.saturating_mul(*limit as u64));
            }
            Step::PgMutation {
                kind,
                payload,
                many,
                ..
            } => {
                let written = match kind {
                    MutationKind::Insert { nodes, .. } => {
                        self.stats.bulk_rows += nodes.len();
                        nodes.len() as u64
                    }
                    // Rows matched by a filter are not known before the write
                    MutationKind::Update { .. } | MutationKind::Delete { .. } => {
                        if *many {
                            self.limits.page_max.map_or(default, |m| m as u64)
                        } else {
                            1
                        }
                    }
                };
                self.add_cost(written.max(1));
                for field in payload {
                    match field {
                        PayloadField::Rows {
                            fields,
                            polymorphism,
                            ..
                        }
                        | PayloadField::Previous {
                            fields,
                            polymorphism,
                            ..
                        } => {
                            self.fields(fields, written.max(1), depth + 1);
                            if let Some(p) = polymorphism {
                                self.polymorphism(p, written.max(1), depth + 1);
                            }
                        }
                        PayloadField::AffectedCount { .. } | PayloadField::Typename { .. } => {}
                    }
                }
            }
            Step::PgFunction {
                returns_set,
                result,
                ..
            } => {
                let rows = if *returns_set { default } else { 1 };
                self.add_cost(parents.saturating_mul(rows));
                match result {
                    FunctionResult::Rows {
                        fields,
                        polymorphism,
                        ..
                    } => {
                        let rows = parents.saturating_mul(rows);
                        self.fields(fields, rows, depth + 1);
                        if let Some(p) = polymorphism {
                            self.polymorphism(p, rows, depth + 1);
                        }
                    }
                    FunctionResult::Record(fields) => self.fields(fields, parents, depth + 1),
                    FunctionResult::Value(_) | FunctionResult::Void => {}
                }
            }
            Step::Entities {
                count,
                groups,
                singles,
            } => {
                self.add_cost(*count as u64);
                for group in groups {
                    self.step(group.step, parents, depth + 1);
                }
                for (_, step) in singles {
                    self.step(*step, parents, depth + 1);
                }
            }
            Step::Constant { .. } | Step::FieldAccess { .. } => {}
        }
    }

    /// Fields of rows (`rows` of them): relations nest a level deeper.
    fn fields(&mut self, fields: &[SelectedField], rows: u64, depth: usize) {
        for field in fields {
            match field {
                SelectedField::Relation { step_id, .. } => self.step(*step_id, rows, depth),
                SelectedField::ExclusiveArcTarget { type_fields, .. } => {
                    for set in type_fields {
                        self.typed(set, rows, depth);
                    }
                }
                SelectedField::FunctionCall { fields, .. } => {
                    self.add_cost(rows);
                    self.fields(fields, rows, depth);
                }
                SelectedField::OnAlias { field, .. } => {
                    self.fields(std::slice::from_ref(field.as_ref()), rows, depth);
                }
                SelectedField::Column { .. }
                | SelectedField::GlobalId { .. }
                | SelectedField::Typename { .. }
                | SelectedField::RowVersion { .. }
                | SelectedField::JoinRow { .. } => {}
            }
        }
    }

    fn typed(&mut self, set: &TypedFieldSet, rows: u64, depth: usize) {
        self.fields(&set.fields, rows, depth);
        if let Some(p) = &set.polymorphism {
            self.polymorphism(p, rows, depth);
        }
    }

    fn polymorphism(&mut self, polymorphism: &RowPolymorphism, rows: u64, depth: usize) {
        match polymorphism {
            RowPolymorphism::SingleTableInheritance {
                variants, fallback, ..
            } => {
                for variant in variants {
                    self.typed(&variant.set, rows, depth);
                }
                if let Some(fallback) = fallback {
                    self.typed(fallback, rows, depth);
                }
            }
            RowPolymorphism::ConcreteTableInheritance { children, fallback } => {
                for child in children {
                    self.typed(&child.set, rows, depth);
                }
                if let Some(fallback) = fallback {
                    self.typed(fallback, rows, depth);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// forums(first: `root`) { messages(first: `nested`) { author } }
    fn nested_plan(root: Option<usize>, nested: Option<usize>) -> OperationPlan {
        let mut plan = OperationPlan::new();
        let author = plan.add_step(Step::pg_select("s", "users", "a", true));
        let mut messages = Step::pg_connection("s", "messages", "m");
        if let Step::PgConnection {
            first, selections, ..
        } = &mut messages
        {
            *first = nested;
            selections.node_fields = vec![SelectedField::relation("author", author)];
        }
        let messages = plan.add_step(messages);
        let mut forums = Step::pg_connection("s", "forums", "f");
        if let Step::PgConnection {
            first, selections, ..
        } = &mut forums
        {
            *first = root;
            selections.node_fields = vec![SelectedField::relation("messages", messages)];
        }
        let forums = plan.add_step(forums);
        plan.add_root_field("forums", forums);
        plan
    }

    fn limits() -> Limits {
        Limits {
            page_default: Some(10),
            page_max: Some(50),
            depth: Some(3),
            cost: Some(1_000_000),
            bulk_rows: Some(5),
            require_filter: false,
        }
    }

    #[test]
    fn unlimited_changes_nothing() {
        let mut plan = nested_plan(None, Some(7));
        let stats = enforce(&mut plan, &Limits::unlimited()).unwrap();
        assert_eq!(stats, PlanStats::default());
        let (_, root) = plan.root_fields()[0].clone();
        let Some(Step::PgConnection { first, .. }) = plan.get_step(root) else {
            panic!("a connection");
        };
        assert_eq!(*first, None, "an export reads every row");
    }

    #[test]
    fn page_sizes_are_defaulted_and_bounded() {
        let mut plan = nested_plan(None, Some(50));
        enforce(&mut plan, &limits()).unwrap();
        let (_, root) = plan.root_fields()[0].clone();
        let Some(Step::PgConnection { first, .. }) = plan.get_step(root) else {
            panic!("a connection");
        };
        assert_eq!(*first, Some(10), "the configured default");

        let err = enforce(&mut nested_plan(Some(51), None), &limits()).unwrap_err();
        assert_eq!(
            err,
            LimitError::PageSize {
                argument: "first",
                requested: 51,
                max: 50
            }
        );
        assert_eq!(err.code(), "PAGE_SIZE_EXCEEDED");
        assert_eq!((err.limit(), err.actual()), (50, 51));
        // nested connections too
        assert!(enforce(&mut nested_plan(Some(5), Some(500)), &limits()).is_err());
    }

    #[test]
    fn lists_without_a_page_size_are_cut_at_the_maximum() {
        let mut plan = OperationPlan::new();
        let list = plan.add_step(Step::pg_select("s", "forums", "f", false));
        plan.add_root_field("forums", list);
        enforce(&mut plan, &limits()).unwrap();
        let Some(Step::PgSelect { limit, .. }) = plan.get_step(list) else {
            panic!("a select");
        };
        assert_eq!(*limit, Some(50));
        // a single row keeps its limit of one
        let mut plan = OperationPlan::new();
        let one = plan.add_step(Step::pg_select("s", "forums", "f", true));
        plan.add_root_field("forum", one);
        enforce(&mut plan, &limits()).unwrap();
        let Some(Step::PgSelect { limit, .. }) = plan.get_step(one) else {
            panic!("a select");
        };
        assert_eq!(*limit, Some(1));
    }

    #[test]
    fn depth_counts_the_levels_of_rows() {
        let mut plan = nested_plan(Some(2), Some(2));
        // forums (1) > messages (2) > author (3)
        let stats = enforce(&mut plan, &limits()).unwrap();
        assert_eq!(stats.depth, 3);
        let tight = Limits {
            depth: Some(2),
            ..limits()
        };
        let err = enforce(&mut nested_plan(Some(2), Some(2)), &tight).unwrap_err();
        assert_eq!(err, LimitError::Depth { depth: 3, max: 2 });
        assert_eq!(err.code(), "DEPTH_LIMIT_EXCEEDED");
    }

    #[test]
    fn cost_multiplies_down_the_nesting() {
        // 10 forums, 20 messages each, each with an author: 10 + 10*20 + 10*20
        let mut plan = nested_plan(Some(10), Some(20));
        let stats = enforce(&mut plan, &limits()).unwrap();
        assert_eq!(stats.cost, 10 + 200 + 200);

        let tight = Limits {
            cost: Some(400),
            ..limits()
        };
        let err = enforce(&mut nested_plan(Some(10), Some(20)), &tight).unwrap_err();
        assert_eq!(
            err,
            LimitError::Cost {
                cost: 410,
                max: 400
            }
        );
        assert_eq!(err.code(), "COST_LIMIT_EXCEEDED");
        // the same shape one page smaller fits
        assert!(enforce(&mut nested_plan(Some(10), Some(19)), &tight).is_ok());
    }

    #[test]
    fn cost_saturates_instead_of_overflowing() {
        let huge = Limits {
            page_max: None,
            cost: Some(u64::MAX - 1),
            ..limits()
        };
        let mut plan = nested_plan(Some(usize::MAX / 2), Some(usize::MAX / 2));
        let err = enforce(&mut plan, &huge).unwrap_err();
        assert!(matches!(err, LimitError::Cost { .. }));
    }

    #[test]
    fn bulk_writes_are_bounded() {
        use super::super::step::{MutationKind, WriteNode, WriteNodeKind};
        let node = || WriteNode {
            schema: "s".into(),
            table: "t".into(),
            kind: WriteNodeKind::Insert,
            values: vec![],
            refs: vec![],
            path: "inputs[0]".into(),
            after: vec![],
        };
        let write = |rows: usize| {
            let mut plan = OperationPlan::new();
            let step = plan.add_step(Step::PgMutation {
                schema: "s".into(),
                table: "t".into(),
                table_alias: "t".into(),
                kind: MutationKind::Insert {
                    nodes: (0..rows).map(|_| node()).collect(),
                    roots: (0..rows).collect(),
                    many: true,
                    on_conflict: None,
                },
                many: true,
                payload: vec![],
                key_columns: vec![],
                version_guard: None,
                affected_limit: None,
            });
            plan.add_root_field("createT", step);
            plan
        };
        assert_eq!(enforce(&mut write(5), &limits()).unwrap().bulk_rows, 5);
        let err = enforce(&mut write(6), &limits()).unwrap_err();
        assert_eq!(err, LimitError::BulkRows { rows: 6, max: 5 });
        assert_eq!(err.code(), "BULK_LIMIT_EXCEEDED");
    }
}
