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
//! * it appears in a `GROUP BY` expression, an aggregation function
//!   argument, or a `HAVING` predicate.
//!
//! # Safety
//!
//! The rule never prunes a CTE whose projection it cannot fully reason
//! about (a `SELECT *`, or an aliased expression that renames a column).
//! When in doubt it keeps the column, so pruning can only ever remove
//! provably-dead outputs.

use std::collections::HashSet;

use crate::errors::VlorQLError;
use crate::schema::{ArcSchemaSnapshot, CommonTableExpression, Expression, Projection, QueryPlan};

use super::analyze::{
    ColumnRef, columns_in_expression, columns_in_order_by, columns_in_predicate,
    columns_in_projection,
};
use super::rules::PlanRewriter;
use super::visitor::ExpressionFold;

/// Removes CTE `SELECT` columns that no consumer references.
///
/// Construct with [`ColumnPruning::new`] for structural pruning, or
/// [`ColumnPruning::with_schema`] to additionally preserve primary- and
/// foreign-key columns of each CTE's base table.
#[derive(Debug, Clone, Default)]
pub struct ColumnPruning {
    /// Optional schema snapshot used to identify primary/foreign keys.
    schema: Option<ArcSchemaSnapshot>,
}

impl ColumnPruning {
    /// Creates a pruner with no schema.
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
        used: &HashSet<ColumnRef>,
    ) -> CommonTableExpression {
        let keep_all = || CommonTableExpression {
            name: cte.name.clone(),
            query: Box::new(inner.clone()),
        };

        // A consumer that reads `<this cte>.*` needs every column.
        if used.contains(&(Some(cte.name.clone()), "*".to_owned())) {
            return keep_all();
        }

        // Build a map from output column name to the projection that
        // produces it, and check whether every projection is a simple
        // column reference (either bare or aliased).  If a projection
        // renames a column with an alias, the output name is the alias;
        // otherwise it is the table column name.
        let mut output_map: Vec<(String, &Projection)> = Vec::with_capacity(inner.select.len());
        let mut all_simple = true;
        for projection in &inner.select {
            match projection {
                Projection::Column {
                    column,
                    alias: None,
                    ..
                } => {
                    output_map.push((column.clone(), projection));
                }
                Projection::Column {
                    column: _,
                    alias: Some(alias),
                    ..
                } => {
                    output_map.push((alias.clone(), projection));
                }
                Projection::Expr { alias: None, .. } => {
                    all_simple = false;
                }
                Projection::Expr {
                    alias: Some(alias), ..
                } => {
                    output_map.push((alias.clone(), projection));
                }
                Projection::Star { .. } => {
                    return keep_all();
                }
            }
        }
        if !all_simple {
            return keep_all();
        }

        // Columns of this CTE that a consumer references, by output name.
        let referenced: HashSet<&str> = used
            .iter()
            .filter(|(table, _)| table.as_deref() == Some(cte.name.as_str()))
            .map(|(_, column)| column.as_str())
            .collect();

        // If the CTE has GROUP BY or HAVING, determine which columns
        // must be preserved (group keys + aggregation arguments from
        // referenced projections + HAVING-referenced). Aggregate args
        // from projections that are not referenced by any consumer are
        // not included, allowing those projections to be pruned.
        let group_required: HashSet<String> = if inner.group_by.is_some() || inner.having.is_some()
        {
            self.group_required_columns(inner, &output_map, &referenced)
        } else {
            HashSet::new()
        };

        // Primary/foreign-key columns of the CTE's base table are always
        // preserved when a schema is available.
        let protected = self.protected_columns(inner);

        let kept: Vec<Projection> = output_map
            .iter()
            .filter(|(name, _)| {
                referenced.contains(name.as_str())
                    || protected.contains(name.as_str())
                    || group_required.contains(name.as_str())
            })
            .map(|(_, projection)| (*projection).clone())
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

    /// Determines the set of column names that must be preserved when
    /// the CTE has GROUP BY or HAVING.
    ///
    /// Aggregate-function arguments are only collected from projections
    /// that are actually referenced by a consumer — an unreferenced
    /// aggregation (e.g. `SUM(b) AS s` where `s` is never read) does
    /// not force its arguments to be preserved.
    fn group_required_columns(
        &self,
        cte_body: &QueryPlan,
        output_map: &[(String, &Projection)],
        referenced: &HashSet<&str>,
    ) -> HashSet<String> {
        let mut required = HashSet::new();

        // GROUP BY expressions: the columns they reference are group keys
        // and must be preserved.
        if let Some(exprs) = &cte_body.group_by {
            for expr in exprs {
                for (_, col) in columns_in_expression(expr) {
                    required.insert(col);
                }
            }
        }

        // HAVING predicate columns.
        if let Some(pred) = &cte_body.having {
            for (_, col) in columns_in_predicate(pred) {
                required.insert(col);
            }
        }

        // Aggregate-function arguments: only collect them from
        // projections that consumers actually reference. An unreferenced
        // aggregate's argument columns do not need to be preserved.
        for (name, projection) in output_map {
            if referenced.contains(name.as_str())
                && let Projection::Expr { expression, .. } = projection
            {
                required.extend(self.aggregate_args(expression));
            }
        }

        // ORDER BY columns must also be preserved when grouping.
        if let Some(terms) = &cte_body.order_by {
            for term in terms {
                for (_, col) in columns_in_expression(&term.expr) {
                    required.insert(col);
                }
            }
        }

        required
    }

    /// Collects column names referenced inside aggregate-function
    /// arguments within an expression.
    fn aggregate_args(&self, expr: &Expression) -> HashSet<String> {
        struct ArgCollector(HashSet<String>);
        impl ExpressionFold for ArgCollector {
            fn fold_expression(&mut self, expr: &Expression) -> Expression {
                if let Expression::FunctionCall { args, .. } = expr {
                    for arg in args {
                        for (_, col) in crate::optimizer::analyze::columns_in_expression(arg) {
                            self.0.insert(col);
                        }
                    }
                }
                super::visitor::default_fold_expression(self, expr)
            }
        }
        let mut collector = ArgCollector(HashSet::new());
        collector.fold_expression(expr);
        collector.0
    }

    /// Returns the primary-key and foreign-key column names of the CTE
    /// body's base table, or an empty set when no schema is configured or
    /// the base table is unknown.
    fn protected_columns(&self, cte_body: &QueryPlan) -> HashSet<String> {
        let mut protected = HashSet::new();
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
    fn used_columns(&self, plan: &QueryPlan) -> HashSet<ColumnRef> {
        let mut used = HashSet::new();

        for projection in &plan.select {
            match columns_in_projection(projection) {
                Some(columns) => used.extend(columns),
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
    /// outputs are consumed.
    fn columns_used_by_cte_body(&self, body: &QueryPlan) -> HashSet<ColumnRef> {
        let mut used = HashSet::new();
        for projection in &body.select {
            match projection {
                Projection::Star { table: Some(table) } => {
                    used.insert((Some(table.clone()), "*".to_owned()));
                }
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
        used.retain(|(table, _)| table.is_some());
        used
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        BinaryOperator, ColumnSchema, ComparisonOperator, DataType, Expression, ForeignKey,
        FromClause, Predicate, Projection, SchemaMetadata, SchemaSnapshot, TableSchema,
    };
    use std::sync::Arc;

    fn col(table: Option<&str>, column: &str) -> Expression {
        Expression::ColumnRef {
            table: table.map(str::to_owned),
            column: column.to_owned(),
        }
    }

    fn int(value: i64) -> Expression {
        Expression::Literal {
            value: value.into(),
            data_type: DataType::Int,
        }
    }

    fn lit_int(value: i64) -> Expression {
        int(value)
    }

    fn column_projection(table: Option<&str>, column: &str) -> Projection {
        Projection::Column {
            table: table.map(str::to_owned),
            column: column.to_owned(),
            alias: None,
        }
    }

    fn compare(left: Expression, op: ComparisonOperator, right: Expression) -> Predicate {
        Predicate::Comparison { left, op, right }
    }

    fn eq(left: Expression, right: Expression) -> Predicate {
        compare(left, ComparisonOperator::Eq, right)
    }

    fn plan_with_cte(
        outer_select: Vec<Projection>,
        cte_name: &str,
        cte_projection: Vec<Projection>,
        cte_gb: Option<Vec<Expression>>,
        cte_having: Option<Predicate>,
        outer_where: Option<Predicate>,
    ) -> QueryPlan {
        let cte = CommonTableExpression {
            name: cte_name.to_owned(),
            query: Box::new(QueryPlan {
                select: cte_projection,
                from: FromClause {
                    table: "orders".to_owned(),
                    alias: None,
                },
                r#where: None,
                group_by: cte_gb,
                having: cte_having,
                order_by: None,
                limit: None,
                offset: None,
                joins: None,
                ctes: None,
            }),
        };
        QueryPlan {
            select: outer_select,
            from: FromClause {
                table: cte_name.to_owned(),
                alias: None,
            },
            r#where: outer_where,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: Some(vec![cte]),
        }
    }

    #[test]
    fn prunes_unreferenced_cte_columns() {
        let plan = plan_with_cte(
            vec![column_projection(Some("recent"), "id")],
            "recent",
            vec![
                column_projection(Some("orders"), "id"),
                column_projection(Some("orders"), "user_id"),
                column_projection(Some("orders"), "status"),
            ],
            None,
            None,
            Some(eq(col(Some("recent"), "id"), lit_int(1))),
        );
        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        assert_eq!(cte_select.len(), 1);
        assert_eq!(cte_select[0], column_projection(Some("orders"), "id"));
    }

    #[test]
    fn preserves_primary_and_foreign_keys() {
        let schema = Arc::new(SchemaSnapshot::new(
            vec![TableSchema {
                name: "orders".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: None,
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "user_id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: None,
                        is_primary_key: false,
                        foreign_key: Some(ForeignKey {
                            foreign_table: "users".to_owned(),
                            foreign_column: "id".to_owned(),
                        }),
                    },
                    ColumnSchema {
                        name: "status".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: None,
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: None,
                primary_key: Some(vec!["id".to_owned()]),
            }],
            SchemaMetadata::default(),
        ));

        let plan = plan_with_cte(
            vec![column_projection(Some("recent"), "status")],
            "recent",
            vec![
                column_projection(Some("orders"), "id"),
                column_projection(Some("orders"), "user_id"),
                column_projection(Some("orders"), "status"),
            ],
            None,
            None,
            Some(eq(col(Some("recent"), "status"), lit_int(1))),
        );

        let rewritten = ColumnPruning::with_schema(schema).rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        let kept: Vec<&str> = cte_select
            .iter()
            .filter_map(|p| match p {
                Projection::Column { column, .. } => Some(column.as_str()),
                _ => None,
            })
            .collect();

        assert!(kept.contains(&"status"), "referenced column kept");
        assert!(kept.contains(&"id"), "primary key preserved");
        assert!(kept.contains(&"user_id"), "foreign key preserved");
    }

    #[test]
    fn keeps_all_columns_under_unqualified_reference() {
        let mut plan = plan_with_cte(
            vec![column_projection(None, "id")],
            "recent",
            vec![
                column_projection(Some("orders"), "id"),
                column_projection(Some("orders"), "user_id"),
                column_projection(Some("orders"), "status"),
            ],
            None,
            None,
            Some(eq(col(None, "status"), lit_int(1))),
        );
        plan.select = vec![column_projection(None, "id")];

        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        assert_eq!(rewritten.ctes.as_ref().unwrap()[0].query.select.len(), 3);
    }

    // --- GROUP BY pruning tests ------------------------------------------

    #[test]
    fn prunes_unused_columns_from_group_by_cte() {
        // CTE: SELECT a, b, c FROM t GROUP BY a, b
        // The outer query only references `a` and `b` (group keys).
        // `c` is neither a group key nor used in an aggregate → prunable.
        let plan = plan_with_cte(
            vec![
                column_projection(Some("recent"), "a"),
                column_projection(Some("recent"), "b"),
            ],
            "recent",
            vec![
                column_projection(Some("t"), "a"),
                column_projection(Some("t"), "b"),
                column_projection(Some("t"), "c"),
            ],
            Some(vec![col(None, "a"), col(None, "b")]),
            None,
            None,
        );

        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        let kept: Vec<&str> = cte_select
            .iter()
            .filter_map(|p| match p {
                Projection::Column { column, .. } => Some(column.as_str()),
                _ => None,
            })
            .collect();

        assert!(kept.contains(&"a"), "group key `a` kept");
        assert!(kept.contains(&"b"), "group key `b` kept");
        assert!(
            !kept.contains(&"c"),
            "`c` is neither key nor aggregate → pruned"
        );
    }

    #[test]
    fn keeps_only_referenced_columns_from_group_by_cte() {
        // CTE: SELECT a, SUM(b) AS total FROM t GROUP BY a
        // The outer query only reads `a`, so `total` (aggregate) is
        // unreferenced and can be pruned — the GROUP BY is unaffected.
        let plan = plan_with_cte(
            vec![column_projection(Some("recent"), "a")],
            "recent",
            vec![
                column_projection(Some("t"), "a"),
                Projection::Expr {
                    expression: Expression::FunctionCall {
                        name: "SUM".to_owned(),
                        args: vec![col(None, "b")],
                        distinct: false,
                    },
                    alias: Some("total".to_owned()),
                },
            ],
            Some(vec![col(None, "a")]),
            None,
            None,
        );

        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        let kept: Vec<&str> = cte_select
            .iter()
            .filter_map(|p| match p {
                Projection::Column { column, .. } => Some(column.as_str()),
                Projection::Expr { alias, .. } => alias.as_deref(),
                _ => None,
            })
            .collect();

        assert!(kept.contains(&"a"), "group key `a` kept");
        assert!(
            !kept.contains(&"total"),
            "unreferenced aggregate `total` pruned"
        );
    }

    #[test]
    fn prunes_expression_projection_not_referenced_by_alias() {
        // CTE: SELECT id, col + 1 AS plus_one FROM t
        // Outer query references only `id`.
        // `plus_one` can be pruned because no consumer uses it by alias.
        let plan = plan_with_cte(
            vec![column_projection(Some("recent"), "id")],
            "recent",
            vec![
                column_projection(Some("t"), "id"),
                Projection::Expr {
                    expression: Expression::BinaryOp {
                        left: Box::new(col(None, "col")),
                        op: BinaryOperator::Add,
                        right: Box::new(lit_int(1)),
                    },
                    alias: Some("plus_one".to_owned()),
                },
            ],
            None,
            None,
            None,
        );

        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        let kept: Vec<&str> = cte_select
            .iter()
            .filter_map(|p| match p {
                Projection::Column { column, .. } => Some(column.as_str()),
                Projection::Expr { alias, .. } => alias.as_deref(),
                _ => None,
            })
            .collect();

        assert!(kept.contains(&"id"), "referenced column `id` kept");
        assert!(
            !kept.contains(&"plus_one"),
            "unreferenced expression `plus_one` pruned"
        );
    }

    #[test]
    fn keeps_expression_projection_when_referenced_by_alias() {
        let plan = plan_with_cte(
            vec![column_projection(Some("recent"), "plus_one")],
            "recent",
            vec![
                column_projection(Some("t"), "id"),
                Projection::Expr {
                    expression: Expression::BinaryOp {
                        left: Box::new(col(None, "col")),
                        op: BinaryOperator::Add,
                        right: Box::new(lit_int(1)),
                    },
                    alias: Some("plus_one".to_owned()),
                },
            ],
            None,
            None,
            None,
        );

        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        let kept: Vec<&str> = cte_select
            .iter()
            .filter_map(|p| match p {
                Projection::Column { column, .. } => Some(column.as_str()),
                Projection::Expr { alias, .. } => alias.as_deref(),
                _ => None,
            })
            .collect();

        assert!(kept.contains(&"plus_one"), "referenced expression kept");
        // `id` is not referenced → should be pruned.
        assert!(!kept.contains(&"id"), "unreferenced column `id` pruned");
    }

    #[test]
    fn keeps_having_referenced_columns_in_group_by_cte() {
        // CTE: SELECT a, b FROM t GROUP BY a, b HAVING b > 0
        // The outer query only reads `a`, but `b` must be kept because
        // it appears in HAVING.
        let plan = plan_with_cte(
            vec![column_projection(Some("recent"), "a")],
            "recent",
            vec![
                column_projection(Some("t"), "a"),
                column_projection(Some("t"), "b"),
            ],
            Some(vec![col(None, "a"), col(None, "b")]),
            Some(compare(col(None, "b"), ComparisonOperator::Gt, lit_int(0))),
            None,
        );

        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        let kept: Vec<&str> = cte_select
            .iter()
            .filter_map(|p| match p {
                Projection::Column { column, .. } => Some(column.as_str()),
                _ => None,
            })
            .collect();

        assert!(kept.contains(&"a"), "group key `a` kept");
        assert!(kept.contains(&"b"), "HAVING-referenced `b` kept");
    }
}
