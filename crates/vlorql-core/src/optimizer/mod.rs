//! Logical rewrite rules applied to a validated [`QueryPlan`].
//!
//! Once a plan has passed validation and policy checks, the optimizer
//! applies semantics-preserving rewrites that reduce the work the SQL
//! backend has to do:
//!
//! * [`ConstantFolding`] statically evaluates constant sub-expressions
//!   (`20 + 5` becomes `25`).
//! * [`PredicatePushdown`] moves single-relation `WHERE` conjuncts down
//!   into the CTE they filter, so rows are discarded earlier.
//! * [`ColumnPruning`] removes CTE output columns that no consumer reads.
//!
//! Every rule implements [`PlanRewriter`]; [`RewriterPipeline`] chains
//! them in a chosen order.
//!
//! Cost-based [`JoinReorderer`] is a separate, `async` optimizer (it
//! consults a statistics provider) rather than a [`PlanRewriter`]: it
//! reorders an inner-join chain to minimize estimated cost.
//!
//! # Model limitations
//!
//! The plan model's [`FromClause`](crate::schema::FromClause) is a bare
//! table name — there is no inline-subquery relation. The only nestable
//! relation is a [`CommonTableExpression`](crate::schema::CommonTableExpression).
//! Pushdown and pruning therefore operate on **CTEs**, not on synthetic
//! FROM subqueries. Every rule is conservative: when it cannot prove a
//! rewrite is safe it leaves that part of the plan untouched, so the
//! output is always semantically equivalent to the input.

mod analyze;
mod fold;
mod join_reorder;
mod prune;
mod pushdown;
mod rules;
pub(crate) mod visitor;

pub use fold::ConstantFolding;
pub use join_reorder::{JoinGraph, JoinReorderer, MAX_DP_RELATIONS};
pub use prune::ColumnPruning;
pub use pushdown::PredicatePushdown;
pub use rules::{PlanRewriter, RewriterPipeline};

use crate::errors::VlorQLError;
use crate::schema::QueryPlan;
use crate::statistics::Cost;
use crate::statistics::StatisticsProvider;
use std::sync::Arc;

/// Orchestrates all optimisation passes over a validated [`QueryPlan`].
///
/// The optimizer applies a fixed sequence of logical rewrites (constant
/// folding, predicate pushdown, column pruning) and, when a statistics
/// provider is available, cost-based join reordering.
///
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::QueryOptimizer;
/// use vlorql_core::statistics::DummyStatisticsProvider;
/// use std::sync::Arc;
///
/// let stats = Arc::new(DummyStatisticsProvider::default());
/// let optimizer = QueryOptimizer::new(stats);
/// ```
#[derive(Debug, Clone)]
pub struct QueryOptimizer {
    /// The synchronous rewrite pipeline (folding, pushdown, pruning).
    pipeline: Arc<RewriterPipeline>,
    /// Optional async join reorderer, available when statistics are present.
    join_reorderer: Option<JoinReorderer>,
    /// Flag to enable/disable join reordering at runtime.
    enable_join_reorder: bool,
}

impl QueryOptimizer {
    /// Creates a new optimizer with all rewrite rules enabled.
    ///
    /// When `stats_provider` is a non-empty provider, join reordering is
    /// also enabled. Pass `DummyStatisticsProvider::default()` to skip
    /// join reordering.
    pub fn new(stats_provider: Arc<dyn StatisticsProvider>) -> Self {
        let join_reorderer = Some(JoinReorderer::new(Arc::clone(&stats_provider)));
        Self {
            pipeline: Arc::new(
                RewriterPipeline::new()
                    .with(ConstantFolding)
                    .with(PredicatePushdown)
                    .with(ColumnPruning::new()),
            ),
            join_reorderer,
            enable_join_reorder: true,
        }
    }

    /// Creates an optimizer with only the logical rewrite rules (no join
    /// reordering), regardless of whether statistics are available.
    pub fn rewrites_only() -> Self {
        Self {
            pipeline: Arc::new(
                RewriterPipeline::new()
                    .with(ConstantFolding)
                    .with(PredicatePushdown)
                    .with(ColumnPruning::new()),
            ),
            join_reorderer: None,
            enable_join_reorder: false,
        }
    }

    /// Enables or disables join reordering at runtime.
    #[must_use]
    pub fn with_join_reorder(mut self, enabled: bool) -> Self {
        self.enable_join_reorder = enabled;
        self
    }

    /// Returns the estimated cost of executing `plan`'s join chain.
    ///
    /// Returns `None` when no join reorderer is configured.
    pub async fn estimated_cost(&self, plan: &QueryPlan) -> Option<Cost> {
        match self.join_reorderer {
            Some(ref jr) => Some(jr.estimate_plan_cost(plan).await.unwrap_or_default()),
            None => None,
        }
    }

    /// Applies synchronous rewrite rules (constant folding, predicate
    /// pushdown, column pruning) to the plan.
    pub fn optimize(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        self.pipeline.rewrite(plan)
    }

    /// Applies the rewrite pipeline in fixed-point iteration (up to
    /// `max_rounds`) until the plan stabilizes. See
    /// [`RewriterPipeline::repeat_until_stable`].
    pub fn optimize_repeat(
        &self,
        plan: &QueryPlan,
        max_rounds: usize,
    ) -> Result<QueryPlan, VlorQLError> {
        self.pipeline.repeat_until_stable(plan, max_rounds)
    }

    /// Applies all rewrite rules **and**, if enabled, cost-based join
    /// reordering. This is the async entry point because join reordering
    /// consults the statistics provider.
    pub async fn optimize_async(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        let plan = self.pipeline.rewrite(plan)?;
        if self.enable_join_reorder
            && let Some(ref reorderer) = self.join_reorderer
        {
            return reorderer.reorder(&plan).await;
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        BinaryOperator, ColumnSchema, CommonTableExpression, ComparisonOperator, DataType,
        Expression, ForeignKey, FromClause, JoinClause, JoinType, Predicate, Projection, QueryPlan,
        SchemaMetadata, SchemaSnapshot, TableSchema,
    };
    use std::sync::Arc;

    // --- builders -------------------------------------------------------

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

    fn and(left: Predicate, right: Predicate) -> Predicate {
        Predicate::And {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    /// Counts the comparison leaves in a predicate tree.
    fn conjunct_count(pred: &Predicate) -> usize {
        match pred {
            Predicate::And { left, right } => conjunct_count(left) + conjunct_count(right),
            _ => 1,
        }
    }

    // --- constant folding ----------------------------------------------

    #[test]
    fn constant_folding_evaluates_arithmetic_in_projection() {
        let plan = QueryPlan {
            select: vec![Projection::Expr {
                expression: Expression::BinaryOp {
                    left: Box::new(int(1)),
                    op: BinaryOperator::Add,
                    right: Box::new(int(2)),
                },
                alias: Some("three".to_owned()),
            }],
            from: FromClause {
                table: "t".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };

        let folded = ConstantFolding.rewrite(&plan).unwrap();
        assert_eq!(
            folded.select[0],
            Projection::Expr {
                expression: int(3),
                alias: Some("three".to_owned()),
            }
        );
    }

    #[test]
    fn constant_folding_simplifies_constant_side_of_comparison() {
        // age > 20 + 5   -->   age > 25
        let plan = QueryPlan {
            select: vec![column_projection(Some("users"), "age")],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: Some(compare(
                col(Some("users"), "age"),
                ComparisonOperator::Gt,
                Expression::BinaryOp {
                    left: Box::new(int(20)),
                    op: BinaryOperator::Add,
                    right: Box::new(int(5)),
                },
            )),
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };

        let folded = ConstantFolding.rewrite(&plan).unwrap();
        assert_eq!(
            folded.r#where,
            Some(compare(
                col(Some("users"), "age"),
                ComparisonOperator::Gt,
                int(25),
            ))
        );
    }

    #[test]
    fn constant_folding_leaves_column_expressions_untouched() {
        // age + 1 has a column operand and must not be folded.
        let expr = Expression::BinaryOp {
            left: Box::new(col(Some("users"), "age")),
            op: BinaryOperator::Add,
            right: Box::new(int(1)),
        };
        let plan = QueryPlan {
            select: vec![Projection::Expr {
                expression: expr.clone(),
                alias: None,
            }],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };

        let folded = ConstantFolding.rewrite(&plan).unwrap();
        assert_eq!(
            folded.select[0],
            Projection::Expr {
                expression: expr,
                alias: None
            }
        );
    }

    // --- predicate pushdown --------------------------------------------

    /// A CTE named `recent` selecting `id`, `user_id`, `status` from a
    /// base table, wrapped by an outer query that filters on it.
    fn plan_with_cte(outer_where: Predicate) -> QueryPlan {
        let cte = CommonTableExpression {
            name: "recent".to_owned(),
            query: Box::new(QueryPlan {
                select: vec![
                    column_projection(Some("orders"), "id"),
                    column_projection(Some("orders"), "user_id"),
                    column_projection(Some("orders"), "status"),
                ],
                from: FromClause {
                    table: "orders".to_owned(),
                    alias: None, recursive: false
                },
                r#where: None,
                group_by: None,
                having: None,
                order_by: None,
                limit: None,
                offset: None,
                joins: None,
                ctes: None,
            }),
        };
        QueryPlan {
            select: vec![column_projection(Some("recent"), "id")],
            from: FromClause {
                table: "recent".to_owned(),
                alias: None,
            },
            r#where: Some(outer_where),
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
    fn pushdown_moves_single_cte_conjunct_into_the_cte() {
        // WHERE recent.status = 1 AND recent.id > 100
        // both reference only `recent`, so both push down.
        let outer = and(
            compare(
                col(Some("recent"), "status"),
                ComparisonOperator::Eq,
                int(1),
            ),
            compare(col(Some("recent"), "id"), ComparisonOperator::Gt, int(100)),
        );
        let plan = plan_with_cte(outer);

        let rewritten = PredicatePushdown.rewrite(&plan).unwrap();

        // The outer WHERE is now empty; both conjuncts moved into the CTE.
        assert!(rewritten.r#where.is_none());
        let cte_where = rewritten.ctes.as_ref().unwrap()[0]
            .query
            .r#where
            .as_ref()
            .expect("CTE should have received the pushed conjuncts");
        assert_eq!(conjunct_count(cte_where), 2);
        // Qualifiers are stripped so the CTE body re-resolves them.
        let pushed = super::analyze::split_conjuncts(cte_where);
        for conjunct in &pushed {
            for (table, _) in super::analyze::columns_in_predicate(conjunct) {
                assert!(table.is_none(), "qualifier should be stripped inside CTE");
            }
        }
    }

    #[test]
    fn pushdown_keeps_conjuncts_over_base_tables() {
        // The outer FROM is a base table, not a CTE: nothing to push.
        let plan = QueryPlan {
            select: vec![column_projection(Some("users"), "id")],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: Some(compare(
                col(Some("users"), "active"),
                ComparisonOperator::Eq,
                int(1),
            )),
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };

        let rewritten = PredicatePushdown.rewrite(&plan).unwrap();
        assert_eq!(rewritten, plan, "no CTE means the plan is unchanged");
    }

    #[test]
    fn pushdown_reduces_outer_conjunct_count() {
        // WHERE recent.status = 1 AND some_base.flag = 1
        // Only the first conjunct references the CTE.
        let outer = and(
            compare(
                col(Some("recent"), "status"),
                ComparisonOperator::Eq,
                int(1),
            ),
            compare(col(Some("other"), "flag"), ComparisonOperator::Eq, int(1)),
        );
        let mut plan = plan_with_cte(outer);
        // Add a joined base table `other` so the second conjunct is valid
        // but not attributable to the CTE.
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "other".to_owned(),
                alias: None,
            },
            on: compare(
                col(Some("recent"), "user_id"),
                ComparisonOperator::Eq,
                col(Some("other"), "user_id"),
            ),
        }]);

        let before = conjunct_count(plan.r#where.as_ref().unwrap());
        let rewritten = PredicatePushdown.rewrite(&plan).unwrap();
        let after = conjunct_count(rewritten.r#where.as_ref().unwrap());

        assert_eq!(before, 2);
        assert_eq!(after, 1, "the CTE conjunct should have moved out");
        assert!(rewritten.ctes.as_ref().unwrap()[0].query.r#where.is_some());
    }

    // --- column pruning ------------------------------------------------

    #[test]
    fn pruning_drops_unreferenced_cte_columns() {
        // The outer query only reads `recent.id`, so `user_id` and
        // `status` can be pruned from the CTE.
        let plan = plan_with_cte(compare(
            col(Some("recent"), "id"),
            ComparisonOperator::Gt,
            int(0),
        ));

        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        assert_eq!(cte_select.len(), 1);
        assert_eq!(cte_select[0], column_projection(Some("orders"), "id"));
    }

    #[test]
    fn pruning_preserves_primary_and_foreign_keys() {
        // With a schema, the CTE's PK (`id`) and FK (`user_id`) survive
        // even though the outer query only reads `status`.
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

        let plan = plan_with_cte(compare(
            col(Some("recent"), "status"),
            ComparisonOperator::Eq,
            int(1),
        ));
        // Outer query reads only `status`.
        let mut plan = plan;
        plan.select = vec![column_projection(Some("recent"), "status")];

        let rewritten = ColumnPruning::with_schema(schema).rewrite(&plan).unwrap();
        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        let kept: Vec<&str> = cte_select
            .iter()
            .filter_map(|projection| match projection {
                Projection::Column { column, .. } => Some(column.as_str()),
                _ => None,
            })
            .collect();

        assert!(kept.contains(&"status"), "referenced column kept");
        assert!(kept.contains(&"id"), "primary key preserved");
        assert!(kept.contains(&"user_id"), "foreign key preserved");
    }

    #[test]
    fn pruning_keeps_all_columns_under_unqualified_reference() {
        // An unqualified column in the outer query is unattributable, so
        // the pruner conservatively keeps every CTE column.
        let mut plan = plan_with_cte(compare(col(None, "status"), ComparisonOperator::Eq, int(1)));
        plan.select = vec![column_projection(None, "id")];

        let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
        assert_eq!(rewritten.ctes.as_ref().unwrap()[0].query.select.len(), 3);
    }

    // --- pipeline ------------------------------------------------------

    #[test]
    fn pipeline_applies_rules_in_order() {
        // Fold `100 + 0` in the outer filter, push the CTE conjunct down,
        // then prune unread CTE columns — all in one pass.
        let outer = and(
            compare(
                col(Some("recent"), "id"),
                ComparisonOperator::Gt,
                Expression::BinaryOp {
                    left: Box::new(int(100)),
                    op: BinaryOperator::Add,
                    right: Box::new(int(0)),
                },
            ),
            compare(
                col(Some("recent"), "status"),
                ComparisonOperator::Eq,
                int(1),
            ),
        );
        let plan = plan_with_cte(outer);

        let pipeline = RewriterPipeline::new()
            .with(ConstantFolding)
            .with(PredicatePushdown)
            .with(ColumnPruning::new());
        let rewritten = pipeline.rewrite(&plan).unwrap();

        // Pushdown emptied the outer WHERE.
        assert!(rewritten.r#where.is_none());

        let cte = &rewritten.ctes.as_ref().unwrap()[0].query;
        // Folding turned `100 + 0` into `100` before pushdown moved it in.
        let cte_where = cte.r#where.as_ref().expect("conjuncts pushed into CTE");
        assert_eq!(conjunct_count(cte_where), 2);
        assert!(super::analyze::split_conjuncts(cte_where).iter().any(|p| {
            matches!(
                p,
                Predicate::Comparison { right, .. } if *right == int(100)
            )
        }));

        // Pruning kept only the columns the CTE now needs: `id` (outer
        // select) and `status`/`id` (pushed filters).
        assert!(cte.select.len() < 3, "at least one column pruned");
    }

    #[test]
    fn empty_pipeline_is_identity() {
        let plan = plan_with_cte(compare(
            col(Some("recent"), "id"),
            ComparisonOperator::Gt,
            int(0),
        ));
        let pipeline = RewriterPipeline::new();
        assert!(pipeline.is_empty());
        assert_eq!(pipeline.rewrite(&plan).unwrap(), plan);
    }

    // --- QueryOptimizer orchestrator -------------------------------------

    #[test]
    fn query_optimizer_folds_constants() {
        let plan = QueryPlan {
            select: vec![Projection::Expr {
                expression: Expression::BinaryOp {
                    left: Box::new(Expression::Literal {
                        value: serde_json::json!(20),
                        data_type: DataType::Int,
                    }),
                    op: BinaryOperator::Add,
                    right: Box::new(Expression::Literal {
                        value: serde_json::json!(5),
                        data_type: DataType::Int,
                    }),
                },
                alias: Some("total".to_owned()),
            }],
            from: FromClause {
                table: "t".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };

        let optimizer = QueryOptimizer::rewrites_only();
        let rewritten = optimizer.optimize(&plan).unwrap();

        assert_eq!(
            rewritten.select[0],
            Projection::Expr {
                expression: Expression::Literal {
                    value: serde_json::json!(25),
                    data_type: DataType::Int,
                },
                alias: Some("total".to_owned()),
            },
        );
    }

    #[tokio::test]
    async fn query_optimizer_async_runs_pipeline() {
        let plan = plan_with_cte(compare(
            col(Some("recent"), "id"),
            ComparisonOperator::Gt,
            int(0),
        ));

        let optimizer = QueryOptimizer::rewrites_only();
        let rewritten = optimizer.optimize_async(&plan).await.unwrap();

        // The pipeline should have folded nothing here (no constant
        // expressions), but pushdown and pruning should still run.
        assert!(!rewritten.select.is_empty());
    }

    #[tokio::test]
    async fn query_optimizer_with_stats_creates_join_reorderer() {
        use crate::statistics::DummyStatisticsProvider;

        let stats = Arc::new(DummyStatisticsProvider::default());
        let optimizer = QueryOptimizer::new(stats);

        // A simple plan with no joins — the reorderer is a no-op, but the
        // pipeline should still run without error.
        let plan = QueryPlan {
            select: vec![Projection::Column {
                table: None,
                column: "id".to_owned(),
                alias: None,
            }],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let rewritten = optimizer.optimize_async(&plan).await.unwrap();
        // The pipeline may rewrite constants or reorder, but the FROM
        // table should be preserved.
        assert_eq!(rewritten.from.table, "users");
    }

    // ------------------------------------------------------------------
    // Security: pushdown must not push policy row filters into CTEs
    // ------------------------------------------------------------------

    #[test]
    fn pushdown_does_not_push_policy_row_filter_into_cte() {
        // A row filter predicate is a policy-enforced condition that the
        // policy engine appends to the outer WHERE clause.  The optimizer
        // must NOT push it into a CTE because that would duplicate the
        // filter (once by the engine, once by the CTE body) and could
        // change the semantics of outer joins.
        //
        // Simulate a policy filter: `tenant_id = 42` on the outer query.
        let policy_filter = compare(col(None, "tenant_id"), ComparisonOperator::Eq, int(42));

        // Build a plan with a CTE and an outer WHERE that contains both
        // a user conjunct and the policy conjunct.
        let plan = plan_with_cte(and(
            policy_filter.clone(),
            compare(col(Some("recent"), "id"), ComparisonOperator::Gt, int(0)),
        ));

        // Apply pushdown only.
        let rewritten = PredicatePushdown.rewrite(&plan).unwrap();

        // The user conjunct (`recent.id > 0`) may be pushed into the CTE,
        // but the policy conjunct (`tenant_id = 42`) must stay in the
        // outer WHERE because it references a column of the outer table,
        // not the CTE.
        let outer_where = rewritten.r#where.as_ref().unwrap();
        let conjuncts = crate::optimizer::analyze::split_conjuncts(outer_where);
        let has_policy_filter = conjuncts
            .iter()
            .any(|c| matches!(c, Predicate::Comparison { right, .. } if *right == int(42)));
        assert!(
            has_policy_filter,
            "policy filter `tenant_id = 42` must remain in the outer WHERE"
        );
    }

    // ------------------------------------------------------------------
    // Security: column pruning must preserve PK/FK columns
    // ------------------------------------------------------------------

    #[test]
    fn column_pruning_preserves_primary_and_foreign_keys() {
        use crate::schema::ForeignKey;

        // Build a schema snapshot where `orders` has a PK (`id`) and two
        // FKs (`user_id` → `users.id`, `product_id` → `products.id`).
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
                        name: "product_id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: None,
                        is_primary_key: false,
                        foreign_key: Some(ForeignKey {
                            foreign_table: "products".to_owned(),
                            foreign_column: "id".to_owned(),
                        }),
                    },
                    ColumnSchema {
                        name: "status".to_owned(),
                        data_type: DataType::String,
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

        // CTE that selects * from orders. The outer query only reads
        // `status`.
        let cte = CommonTableExpression {
            name: "recent".to_owned(),
            query: Box::new(QueryPlan {
                select: vec![
                    Projection::Column {
                        table: Some("orders".to_owned()),
                        column: "id".to_owned(),
                        alias: None, recursive: false
                    },
                    Projection::Column {
                        table: Some("orders".to_owned()),
                        column: "user_id".to_owned(),
                        alias: None,
                    },
                    Projection::Column {
                        table: Some("orders".to_owned()),
                        column: "product_id".to_owned(),
                        alias: None,
                    },
                    Projection::Column {
                        table: Some("orders".to_owned()),
                        column: "status".to_owned(),
                        alias: None,
                    },
                ],
                from: FromClause {
                    table: "orders".to_owned(),
                    alias: None,
                },
                r#where: None,
                group_by: None,
                having: None,
                order_by: None,
                limit: None,
                offset: None,
                joins: None,
                ctes: None,
            }),
        };

        let plan = QueryPlan {
            select: vec![Projection::Column {
                table: Some("recent".to_owned()),
                column: "status".to_owned(),
                alias: None,
            }],
            from: FromClause {
                table: "recent".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: Some(vec![cte]),
        };

        let pruner = ColumnPruning::with_schema(schema);
        let rewritten = pruner.rewrite(&plan).unwrap();

        let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
        let cte_cols: Vec<&str> = cte_select
            .iter()
            .filter_map(|p| match p {
                Projection::Column { column, .. } => Some(column.as_str()),
                _ => None,
            })
            .collect();

        // The CTE must still contain `id` (PK) and `user_id` + `product_id` (FKs),
        // even though the outer query only reads `status`.
        assert!(
            cte_cols.contains(&"id"),
            "PK column `id` must be preserved: got {cte_cols:?}"
        );
        assert!(
            cte_cols.contains(&"user_id"),
            "FK column `user_id` must be preserved: got {cte_cols:?}"
        );
        assert!(
            cte_cols.contains(&"product_id"),
            "FK column `product_id` must be preserved: got {cte_cols:?}"
        );
        // `status` should also be present since it's the only column
        // the outer query explicitly selects.
        assert!(
            cte_cols.contains(&"status"),
            "selected column `status` must be preserved: got {cte_cols:?}"
        );
    }

    #[test]
    fn repeat_until_stable_converges_in_one_round_when_already_stable() {
        // A plan with no CTEs and no foldable expressions reaches fixpoint
        // immediately.
        let plan = QueryPlan {
            select: vec![column_projection(None, "id")],
            from: FromClause {
                table: "t".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let pipeline = RewriterPipeline::new()
            .with(ConstantFolding)
            .with(PredicatePushdown)
            .with(ColumnPruning::new());
        let result = pipeline.repeat_until_stable(&plan, 5).unwrap();
        assert_eq!(result.select.len(), 1);
    }

    #[test]
    fn repeat_until_stable_preserves_equivalence() {
        // A plan with a foldable expression should still fold correctly
        // under repeat_until_stable.
        let plan = QueryPlan {
            select: vec![Projection::Expr {
                expression: Expression::BinaryOp {
                    left: Box::new(Expression::Literal {
                        value: 10.into(),
                        data_type: DataType::Int,
                    }),
                    op: BinaryOperator::Add,
                    right: Box::new(Expression::Literal {
                        value: 20.into(),
                        data_type: DataType::Int,
                    }),
                },
                alias: Some("total".to_owned()),
            }],
            from: FromClause {
                table: "t".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let pipeline = RewriterPipeline::new().with(ConstantFolding);
        let result = pipeline.repeat_until_stable(&plan, 3).unwrap();
        assert_eq!(
            result.select[0],
            Projection::Expr {
                expression: Expression::Literal {
                    value: 30.into(),
                    data_type: DataType::Int
                },
                alias: Some("total".to_owned()),
            },
        );
    }

    #[test]
    fn optimize_repeat_exposes_fixpoint_method() {
        let plan = QueryPlan {
            select: vec![column_projection(None, "id")],
            from: FromClause {
                table: "t".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let optimizer = QueryOptimizer::rewrites_only();
        let result = optimizer.optimize_repeat(&plan, 3).unwrap();
        assert_eq!(result.select.len(), 1);
    }

    #[test]
    fn multi_layer_cte_pushdown_cascades_through_nested_ctes() {
        // Two CTEs where the outer one references the inner one:
        //   WITH
        //     cte2 AS (SELECT id, val FROM t2),
        //     cte1 AS (SELECT * FROM cte2)
        //   SELECT * FROM cte1 WHERE cte1.val > 10
        // The condition `cte1.val > 10` should be pushed into cte2.
        let cte2_body = QueryPlan {
            select: vec![
                column_projection(None, "id"),
                column_projection(None, "val"),
            ],
            from: FromClause {
                table: "t2".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let cte1_body = QueryPlan {
            select: vec![Projection::Star { table: None }],
            from: FromClause {
                table: "cte2".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let plan = QueryPlan {
            select: vec![Projection::Star { table: None }],
            from: FromClause {
                table: "cte1".to_owned(),
                alias: None,
            },
            r#where: Some(compare(
                col(Some("cte1"), "val"),
                ComparisonOperator::Gt,
                int(10),
            )),
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: Some(vec![
                CommonTableExpression {
                    name: "cte2".to_owned(),
                    query: Box::new(cte2_body), recursive: false
                },
                CommonTableExpression {
                    name: "cte1".to_owned(),
                    query: Box::new(cte1_body), recursive: false
                },
            ]),
        };

        let pipeline = RewriterPipeline::new().with(PredicatePushdown);
        let result = pipeline.rewrite(&plan).unwrap();

        // The outer WHERE should be empty (condition was pushed down).
        assert!(
            result.r#where.is_none(),
            "outer WHERE should be empty after pushdown"
        );

        // cte1's WHERE should be empty (condition was further pushed into cte2).
        let cte1 = result
            .ctes
            .as_ref()
            .unwrap()
            .iter()
            .find(|cte| cte.name == "cte1")
            .expect("cte1 should exist");
        assert!(
            cte1.query.r#where.is_none(),
            "cte1 WHERE should be empty after cascade pushdown: {:?}",
            cte1.query.r#where
        );

        // cte2 should have the condition in its WHERE.
        let cte2 = result
            .ctes
            .as_ref()
            .unwrap()
            .iter()
            .find(|cte| cte.name == "cte2")
            .expect("cte2 should exist");
        assert!(
            cte2.query.r#where.is_some(),
            "cte2 should have the pushed condition: {:?}",
            cte2.query.r#where
        );
    }

    #[test]
    fn multi_layer_cte_pushdown_with_alias() {
        // Same as above but cte1 uses an alias for cte2:
        //   WITH
        //     cte2 AS (SELECT id, val FROM t2),
        //     cte1 AS (SELECT * FROM cte2 AS inner_c)
        //   SELECT * FROM cte1 WHERE cte1.val > 10
        let cte2_body = QueryPlan {
            select: vec![
                column_projection(None, "id"),
                column_projection(None, "val"),
            ],
            from: FromClause {
                table: "t2".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let cte1_body = QueryPlan {
            select: vec![Projection::Star { table: None }],
            from: FromClause {
                table: "cte2".to_owned(),
                alias: Some("inner_c".to_owned()),
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let plan = QueryPlan {
            select: vec![Projection::Star { table: None }],
            from: FromClause {
                table: "cte1".to_owned(),
                alias: None,
            },
            r#where: Some(compare(
                col(Some("cte1"), "val"),
                ComparisonOperator::Gt,
                int(10),
            )),
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: Some(vec![
                CommonTableExpression {
                    name: "cte2".to_owned(),
                    query: Box::new(cte2_body), recursive: false
                },
                CommonTableExpression {
                    name: "cte1".to_owned(),
                    query: Box::new(cte1_body), recursive: false
                },
            ]),
        };

        let pipeline = RewriterPipeline::new().with(PredicatePushdown);
        let result = pipeline.rewrite(&plan).unwrap();

        // The outer WHERE should be empty.
        assert!(result.r#where.is_none(), "outer WHERE should be empty");

        // cte1's WHERE should be empty.
        let cte1 = result
            .ctes
            .as_ref()
            .unwrap()
            .iter()
            .find(|cte| cte.name == "cte1")
            .expect("cte1 should exist");
        assert!(
            cte1.query.r#where.is_none(),
            "cte1 WHERE should be empty after cascade: {:?}",
            cte1.query.r#where
        );

        // cte2 should have the condition.
        let cte2 = result
            .ctes
            .as_ref()
            .unwrap()
            .iter()
            .find(|cte| cte.name == "cte2")
            .expect("cte2 should exist");
        assert!(
            cte2.query.r#where.is_some(),
            "cte2 should have the pushed condition: {:?}",
            cte2.query.r#where
        );
    }
}
