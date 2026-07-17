//! Column pruning: drop CTE output columns nobody reads.
//!
//! A [`CommonTableExpression`](crate::schema::CommonTableExpression) may
//! project more columns than the outer query actually consumes. Each
//! unused column forces the CTE to compute and materialize a value that
//! is thrown away. [`ColumnPruning`] removes those columns from a CTE's
//! `SELECT` list when it can prove they are unreferenced.
//!
//! Because the plan model's [`FromClause`](crate::schema::FromClause) is
//! a bare table name (no inline subquery), CTEs are the only relations
//! with a prunable projection, so this rule targets them.
//!
//! # What counts as "used"
//!
//! A CTE column is kept when any of the following holds:
//!
//! * the outer query (its `SELECT`, `WHERE`, `GROUP BY`, `HAVING`,
//!   `ORDER BY`, or join `ON` clauses) references it by the CTE's
//!   name/alias,
//! * some relation qualifies the column with a `None` table (an
//!   unqualified reference the rule cannot attribute), in which case the
//!   rule keeps *every* column of every CTE to stay safe,
//! * a downstream CTE references it, or
//! * it is a primary-key or foreign-key column of the CTE's own base
//!   table — these are preserved so join and policy checks stay correct
//!   even if the current query happens not to select them.
//!
//! # Safety
//!
//! The rule never prunes a CTE whose projection it cannot fully reason
//! about (a `SELECT *`, an aliased expression that renames a column, or
//! an aggregate/`GROUP BY` body). When in doubt it keeps the column, so
//! pruning can only ever remove provably-dead outputs.

use std::collections::BTreeSet;

use crate::errors::VlorQLError;
use crate::schema::{ArcSchemaSnapshot, CommonTableExpression, Projection, QueryPlan};

use super::analyze::{
    columns_in_expression, columns_in_order_by, columns_in_predicate, columns_in_projection,
    ColumnRef,
};
use super::rules::PlanRewriter;

/// Removes CTE `SELECT` columns that no consumer references.
///
/// Construct with [`ColumnPruning::new`] for structural pruning, or
/// [`ColumnPruning::with_schema`] to additionally preserve primary- and
/// foreign-key columns of each CTE's base table.
#[derive(Debug, Clone, Default)]
pub struct ColumnPruning {
    /// Optional schema snapshot used to identify primary/foreign keys.
    /// When `None` the pruner is conservative and keeps all columns.
    pub schema: Option<ArcSchemaSnapshot>,
}

impl ColumnPruning {
    /// Creates a pruner with no schema. Primary/foreign keys are *not*
    /// specially preserved (there is no schema to consult), so this is
    /// best used after the plan has already been policy-checked.
    pub fn new() -> Self {
        Self { schema: None }
    }

    /// Creates a pruner that consults `schema` to preserve primary-key
    /// and foreign-key columns of each CTE's base table.
    pub fn with_schema(schema: ArcSchemaSnapshot) -> Self {
        Self {
            schema: Some(schema),
        }
    }
}

impl PlanRewriter for ColumnPruning {
    fn rewrite(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        Ok(self.prune_plan(plan))
    }
}

impl ColumnPruning {
    fn prune_plan(&self, plan: &QueryPlan) -> QueryPlan {
        let Some(ctes) = plan.ctes.as_ref() else {
            return plan.clone();
        };
        if ctes.is_empty() {
            return plan.clone();
        }

        // Collect every column the outer query reads, keyed by qualifier.
        let used = self.used_columns(plan);

        // A single unqualified reference anywhere means we cannot know
        // which CTE it targets; keep all columns to preserve semantics.
        if used.iter().any(|(table, _)| table.is_none()) {
            return plan.clone();
        }

        // Recurse first so a CTE defined over other CTEs is pruned using
        // its own body's demands, then prune this level.
        let pruned: Vec<CommonTableExpression> = ctes
            .iter()
            .map(|cte| {
                let inner = self.prune_plan(&cte.query);
                self.prune_cte(cte, &inner, &used)
            })
            .collect();

        QueryPlan {
            ctes: Some(pruned),
            ..plan.clone()
        }
    }

    /// Prunes a single CTE's projection against the set of columns its
    /// consumers reference. `inner` is the already-pruned CTE body.
    fn prune_cte(
        &self,
        cte: &CommonTableExpression,
        inner: &QueryPlan,
        used: &BTreeSet<ColumnRef>,
    ) -> CommonTableExpression {
        let keep_all = || CommonTableExpression {
            name: cte.name.clone(),
            query: Box::new(inner.clone()),
        };

        // A consumer that reads `<this cte>.*` needs every column.
        if used.contains(&(Some(cte.name.clone()), "*".to_owned())) {
            return keep_all();
        }

        // Only prune a projection made entirely of plain, non-aliased
        // column selects: anything else (a `*`, a computed expression, or
        // a rename) makes "which output column is column X" ambiguous.
        let mut output_columns: Vec<String> = Vec::with_capacity(inner.select.len());
        for projection in &inner.select {
            match projection {
                Projection::Column {
                    column,
                    alias: None,
                    ..
                } => output_columns.push(column.clone()),
                _ => return keep_all(),
            }
        }

        // Aggregation changes row identity; pruning a grouped column would
        // change the result, so leave grouped CTEs alone.
        if inner.group_by.is_some() || inner.having.is_some() {
            return keep_all();
        }

        // Columns of this CTE that a consumer references, by output name.
        let referenced: BTreeSet<&str> = used
            .iter()
            .filter(|(table, _)| table.as_deref() == Some(cte.name.as_str()))
            .map(|(_, column)| column.as_str())
            .collect();

        // Primary/foreign-key columns of the CTE's base table are always
        // preserved when a schema is available.
        let protected = self.protected_columns(inner);

        let kept: Vec<Projection> = inner
            .select
            .iter()
            .zip(&output_columns)
            .filter(|(_, name)| referenced.contains(name.as_str()) || protected.contains(*name))
            .map(|(projection, _)| projection.clone())
            .collect();

        // Never produce an empty projection; if nothing survived, keep the
        // original list rather than emit invalid SQL.
        if kept.is_empty() || kept.len() == inner.select.len() {
            return keep_all();
        }

        let mut body = inner.clone();
        body.select = kept;
        CommonTableExpression {
            name: cte.name.clone(),
            query: Box::new(body),
        }
    }

    /// Returns the primary-key and foreign-key column names of the CTE
    /// body's base table, or an empty set when no schema is configured or
    /// the base table is unknown.
    fn protected_columns(&self, cte_body: &QueryPlan) -> BTreeSet<String> {
        let mut protected = BTreeSet::new();
        let Some(schema) = self.schema.as_ref() else {
            return protected;
        };
        let Some(table) = schema.get_table(&cte_body.from.table) else {
            return protected;
        };
        for column in &table.columns {
            if column.is_primary_key || column.foreign_key.is_some() {
                protected.insert(column.name.clone());
            }
        }
        protected
    }

    /// Collects every column referenced by the outer query itself (not by
    /// the CTE bodies). A `SELECT *` short-circuits to a sentinel
    /// unqualified entry so the caller keeps every CTE column.
    fn used_columns(&self, plan: &QueryPlan) -> BTreeSet<ColumnRef> {
        let mut used = BTreeSet::new();

        for projection in &plan.select {
            match columns_in_projection(projection) {
                Some(columns) => used.extend(columns),
                // `SELECT *` reads everything: insert an unqualified
                // sentinel that forces the "keep all" path.
                None => {
                    used.insert((None, "*".to_owned()));
                }
            }
        }

        if let Some(pred) = plan.r#where.as_ref() {
            used.extend(columns_in_predicate(pred));
        }
        if let Some(exprs) = plan.group_by.as_ref() {
            for expr in exprs {
                used.extend(columns_in_expression(expr));
            }
        }
        if let Some(pred) = plan.having.as_ref() {
            used.extend(columns_in_predicate(pred));
        }
        if let Some(terms) = plan.order_by.as_ref() {
            used.extend(columns_in_order_by(terms));
        }
        if let Some(joins) = plan.joins.as_ref() {
            for join in joins {
                used.extend(columns_in_predicate(&join.on));
            }
        }

        // Downstream CTEs may reference an earlier CTE's columns.
        if let Some(ctes) = plan.ctes.as_ref() {
            for cte in ctes {
                used.extend(self.columns_used_by_cte_body(&cte.query));
            }
        }

        used
    }

    /// Columns a CTE body reads from a *sibling* relation, i.e. references
    /// qualified by another CTE's name.
    ///
    /// Only qualified references are returned: an *unqualified* column in a
    /// CTE body binds to that body's own `FROM` table, never to a sibling
    /// CTE's output, so it says nothing about which of this level's CTE
    /// outputs are consumed. (After predicate pushdown a CTE's own `WHERE`
    /// holds unqualified columns; propagating those would spuriously look
    /// like an unattributable reference and defeat pruning.)
    fn columns_used_by_cte_body(&self, body: &QueryPlan) -> BTreeSet<ColumnRef> {
        let mut used = BTreeSet::new();
        for projection in &body.select {
            match projection {
                // A qualified `sibling.*` reads every column of that sibling
                // CTE: record a per-table wildcard the pruner treats as
                // "keep all of that CTE's columns".
                Projection::Star { table: Some(table) } => {
                    used.insert((Some(table.clone()), "*".to_owned()));
                }
                // A bare `*` binds to this body's own `FROM`; it is dropped
                // by the qualifier filter below like any unqualified column.
                Projection::Star { table: None } => {}
                other => {
                    if let Some(columns) = columns_in_projection(other) {
                        used.extend(columns);
                    }
                }
            }
        }
        if let Some(pred) = body.r#where.as_ref() {
            used.extend(columns_in_predicate(pred));
        }
        if let Some(exprs) = body.group_by.as_ref() {
            for expr in exprs {
                used.extend(columns_in_expression(expr));
            }
        }
        if let Some(pred) = body.having.as_ref() {
            used.extend(columns_in_predicate(pred));
        }
        if let Some(terms) = body.order_by.as_ref() {
            used.extend(columns_in_order_by(terms));
        }
        if let Some(joins) = body.joins.as_ref() {
            for join in joins {
                used.extend(columns_in_predicate(&join.on));
            }
        }
        // Keep only sibling-qualified references (see doc comment).
        used.retain(|(table, _)| table.is_some());
        used
    }
}
