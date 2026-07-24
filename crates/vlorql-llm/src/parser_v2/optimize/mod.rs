//! Optimize layer: AST optimization for [`QueryPlan`].
//!
//! Applies semantic-preserving transformations to produce a more
//! efficient query plan before SQL compilation.
//!
//! # Sub-modules
//!
//! - **predicate** — predicate simplification (AND TRUE, OR FALSE, NOT NOT, etc.)
//! - **projection** — projection pruning (duplicate column removal)
//! - **rewrite** — SQL rewrite rules (placeholder for future work)

pub mod predicate;
pub mod projection;
pub mod rewrite;

use vlorql_core::schema::QueryPlan;

/// Run all optimization passes on a [`QueryPlan`].
///
/// Returns `true` if any optimization was applied.
#[must_use]
pub fn optimize(plan: &mut QueryPlan) -> bool {
    let mut changed = false;

    // 1. Simplify predicates (WHERE, HAVING, JOIN ON).
    if let Some(ref mut predicate) = plan.r#where {
        changed |= predicate::simplify(predicate);
    }
    if let Some(ref mut predicate) = plan.having {
        changed |= predicate::simplify(predicate);
    }
    if let Some(ref mut joins) = plan.joins {
        for join in joins.iter_mut() {
            changed |= predicate::simplify(&mut join.on);
        }
    }

    // 2. Optimize projections.
    changed |= projection::optimize(plan);

    // 3. Apply rewrite rules (future).
    changed |= rewrite::rewrite(plan);

    // 4. Recursively optimize CTE subqueries.
    if let Some(ref mut ctes) = plan.ctes {
        for cte in ctes.iter_mut() {
            changed |= optimize(&mut cte.query);
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vlorql_core::schema::*;

    fn col(name: &str) -> Expression {
        Expression::ColumnRef {
            table: None,
            column: name.to_owned(),
        }
    }

    fn lit_int(v: i64) -> Expression {
        Expression::Literal {
            value: json!(v),
            data_type: DataType::Int,
        }
    }

    fn lit_bool(v: bool) -> Expression {
        Expression::Literal {
            value: json!(v),
            data_type: DataType::Boolean,
        }
    }

    fn true_pred() -> Predicate {
        Predicate::Comparison {
            left: lit_bool(true),
            op: ComparisonOperator::Eq,
            right: lit_bool(true),
        }
    }

    fn base_plan() -> QueryPlan {
        QueryPlan {
            select: vec![Projection::Star { table: None }],
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
            distinct: false,
            distinct_on: None,
            set_operation: None,
        }
    }

    #[test]
    fn simplifies_where_predicate() {
        let mut plan = base_plan();
        plan.r#where = Some(Predicate::And {
            left: Box::new(Predicate::Comparison {
                left: col("age"),
                op: ComparisonOperator::Gt,
                right: lit_int(18),
            }),
            right: Box::new(true_pred()),
        });
        assert!(optimize(&mut plan));
        // AND TRUE removed
        assert!(matches!(
            plan.r#where.unwrap(),
            Predicate::Comparison { .. }
        ));
    }

    #[test]
    fn simplifies_join_predicate() {
        let mut plan = base_plan();
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: None,
            },
            on: Predicate::And {
                left: Box::new(Predicate::Comparison {
                    left: col("user_id"),
                    op: ComparisonOperator::Eq,
                    right: col("id"),
                }),
                right: Box::new(true_pred()),
            },
        }]);
        assert!(optimize(&mut plan));
        let join = &plan.joins.unwrap()[0];
        // AND TRUE removed from ON
        assert!(matches!(join.on, Predicate::Comparison { .. }));
    }

    #[test]
    fn removes_duplicate_projections() {
        let mut plan = base_plan();
        plan.select = vec![
            Projection::Column {
                table: None,
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: None,
                column: "id".to_owned(),
                alias: None,
            },
        ];
        assert!(optimize(&mut plan));
        assert_eq!(plan.select.len(), 1);
    }

    #[test]
    fn no_change_for_canonical() {
        let mut plan = base_plan();
        plan.r#where = Some(Predicate::Comparison {
            left: col("age"),
            op: ComparisonOperator::Gt,
            right: lit_int(18),
        });
        assert!(!optimize(&mut plan));
    }

    #[test]
    fn recursive_cte_optimization() {
        let mut plan = base_plan();
        plan.ctes = Some(vec![CommonTableExpression {
            name: "active".to_owned(),
            recursive: false,
            query: Box::new(QueryPlan {
                select: vec![Projection::Star { table: None }],
                from: FromClause {
                    table: "users".to_owned(),
                    alias: None,
                },
                r#where: Some(Predicate::And {
                    left: Box::new(Predicate::Comparison {
                        left: col("status"),
                        op: ComparisonOperator::Eq,
                        right: Expression::Literal {
                            value: json!("active"),
                            data_type: DataType::String,
                        },
                    }),
                    right: Box::new(true_pred()),
                }),
                group_by: None,
                having: None,
                order_by: None,
                limit: None,
                offset: None,
                joins: None,
                ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,            }),
        }]);
        assert!(optimize(&mut plan));
        // CTE subquery should have its AND TRUE simplified.
        let cte = &plan.ctes.unwrap()[0];
        assert!(matches!(
            cte.query.r#where.as_ref().unwrap(),
            Predicate::Comparison { .. }
        ));
    }
}
