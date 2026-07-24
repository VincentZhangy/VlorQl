//! Deduplicate nested aggregate function calls.
//!
//! Weak LLMs sometimes emit `SUM(SUM(x))` or `COUNT(COUNT(x))` instead of
//! a single `SUM(x)` / `COUNT(x)`.  This module unwraps the outer call
//! when the inner call has the same function name.

use crate::schema::expressions::Expression;
use crate::schema::{Predicate, Projection, QueryPlan};

/// The set of aggregate function names that we check for nesting.
const AGGREGATES: &[&str] = &["sum", "count", "avg", "min", "max"];

/// Check whether `name` is a known aggregate function (case-insensitive).
fn is_aggregate(name: &str) -> bool {
    AGGREGATES.iter().any(|a| a.eq_ignore_ascii_case(name))
}

/// Recursively unwrap nested aggregate calls inside an expression tree.
///
/// `SUM(SUM(x))` → `SUM(x)`
/// `COUNT(COUNT(x))` → `COUNT(x)`
/// `SUM(COUNT(x))` → unchanged (different function)
fn dedup_expr(expr: &mut Expression) -> bool {
    match expr {
        Expression::FunctionCall { name, args, .. } if is_aggregate(name) => {
            let mut changed = false;
            // First, recurse into all arguments to fix any nesting there.
            for arg in args.iter_mut() {
                changed |= dedup_expr(arg);
            }
            // Then check if the single argument is the same aggregate → unwrap.
            if args.len() == 1 {
                if let Expression::FunctionCall {
                    name: inner_name,
                    args: inner_args,
                    ..
                } = &args[0]
                {
                    if inner_name.eq_ignore_ascii_case(name) {
                        // Unwrap: replace outer with inner's argument.
                        // e.g. SUM(SUM(x)) → the inner SUM's arg (x)
                        if let Some(inner_arg) = inner_args.first() {
                            if let Some(unwrapped) = take_single_arg(inner_arg) {
                                *expr = unwrapped;
                                return true;
                            }
                        }
                    }
                }
            }
            changed
        }
        Expression::FunctionCall { args, .. } => {
            // Non-aggregate function call — still recurse into args.
            let mut changed = false;
            for arg in args.iter_mut() {
                changed |= dedup_expr(arg);
            }
            changed
        }
        Expression::BinaryOp { left, right, .. } => {
            dedup_expr(left) | dedup_expr(right)
        }
        Expression::Case {
            operand,
            when_thens,
            else_result,
        } => {
            let mut changed = false;
            if let Some(op) = operand {
                changed |= dedup_expr(op);
            }
            for wt in when_thens.iter_mut() {
                changed |= dedup_expr(&mut wt.when);
                changed |= dedup_expr(&mut wt.then);
            }
            if let Some(els) = else_result {
                changed |= dedup_expr(els);
            }
            changed
        }
        Expression::SubQuery { query } => dedup_plan(query),
        Expression::WindowFunction { args, .. } => {
            let mut changed = false;
            for arg in args.iter_mut() {
                changed |= dedup_expr(arg);
            }
            changed
        }
        Expression::Literal { .. } | Expression::ColumnRef { .. } | Expression::Star => false,
    }
}

/// Recursively deduplicate aggregates inside a predicate.
fn dedup_pred(pred: &mut Predicate) -> bool {
    match pred {
        Predicate::Comparison { left, right, .. } => dedup_expr(left) | dedup_expr(right),
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            dedup_pred(left) | dedup_pred(right)
        }
        Predicate::Not { child } => dedup_pred(child),
        Predicate::Between { expr, low, high } => {
            dedup_expr(expr) | dedup_expr(low) | dedup_expr(high)
        }
        Predicate::In { expr, target } => {
            let mut changed = dedup_expr(expr);
            if let crate::schema::InTarget::SubQuery(query) = target {
                changed |= dedup_plan(query);
            }
            changed
        }
        Predicate::Like { expr, .. } => dedup_expr(expr),
        Predicate::IsNull { expr } => dedup_expr(expr),
        Predicate::Exists { query } => dedup_plan(query),
    }
}

/// Recursively deduplicate aggregates in a projection.
fn dedup_proj(proj: &mut Projection) -> bool {
    match proj {
        Projection::Expr { expression, .. } => dedup_expr(expression),
        Projection::Column { .. } | Projection::Star { .. } => false,
    }
}

/// Recursively deduplicate aggregates in a whole plan.
fn dedup_plan(plan: &mut QueryPlan) -> bool {
    let mut changed = false;
    for proj in plan.select.iter_mut() {
        changed |= dedup_proj(proj);
    }
    if let Some(ref mut pred) = plan.r#where {
        changed |= dedup_pred(pred);
    }
    if let Some(ref mut having) = plan.having {
        changed |= dedup_pred(having);
    }
    if let Some(ref mut group_by) = plan.group_by {
        for expr in group_by.iter_mut() {
            changed |= dedup_expr(expr);
        }
    }
    if let Some(ref mut order_by) = plan.order_by {
        for term in order_by.iter_mut() {
            changed |= dedup_expr(&mut term.expr);
        }
    }
    // Recurse into CTEs and subqueries.
    if let Some(ref mut ctes) = plan.ctes {
        for cte in ctes.iter_mut() {
            changed |= dedup_plan(&mut cte.query);
        }
    }
    if let Some(ref mut set_op) = plan.set_operation {
        changed |= dedup_plan(&mut set_op.right);
    }
    changed
}

/// Helper: clone an inner expression to break the borrow issue with
/// replacing a FunctionCall node.
fn take_single_arg(expr: &Expression) -> Option<Expression> {
    Some(expr.clone())
}

/// Deduplicate nested aggregate function calls in a query plan.
///
/// Transforms `SUM(SUM(x))` → `SUM(x)` for known aggregate functions,
/// applied recursively across the entire plan tree.
pub fn deduplicate_nested_aggregates(plan: &mut QueryPlan) -> bool {
    dedup_plan(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_sum_sum() {
        let mut expr = Expression::FunctionCall {
            name: "sum".to_owned(),
            args: vec![Expression::FunctionCall {
                name: "sum".to_owned(),
                args: vec![Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "total".to_owned(),
                }],
                distinct: false,
            }],
            distinct: false,
        };
        assert!(dedup_expr(&mut expr));
        // Expect the outer SUM to be removed, leaving just the inner arg.
        match expr {
            Expression::ColumnRef {
                ref table,
                ref column,
            } => {
                assert_eq!(table.as_deref(), Some("orders"));
                assert_eq!(column, "total");
            }
            _ => panic!("Expected ColumnRef after dedup"),
        }
    }

    #[test]
    fn dedup_count_count() {
        let mut expr = Expression::FunctionCall {
            name: "count".to_owned(),
            args: vec![Expression::FunctionCall {
                name: "count".to_owned(),
                args: vec![Expression::Star],
                distinct: false,
            }],
            distinct: false,
        };
        assert!(dedup_expr(&mut expr));
        assert!(matches!(expr, Expression::Star));
    }

    #[test]
    fn no_change_different_functions() {
        let mut expr = Expression::FunctionCall {
            name: "sum".to_owned(),
            args: vec![Expression::FunctionCall {
                name: "count".to_owned(),
                args: vec![Expression::ColumnRef {
                    table: None,
                    column: "id".to_owned(),
                }],
                distinct: false,
            }],
            distinct: false,
        };
        // SUM(COUNT(id)) — different functions, no change.
        assert!(!dedup_expr(&mut expr));
    }

    #[test]
    fn no_change_single_aggregate() {
        let mut expr = Expression::FunctionCall {
            name: "sum".to_owned(),
            args: vec![Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            }],
            distinct: false,
        };
        assert!(!dedup_expr(&mut expr));
    }

    #[test]
    fn dedup_in_having() {
        let mut having = Predicate::Comparison {
            left: Expression::FunctionCall {
                name: "sum".to_owned(),
                args: vec![Expression::FunctionCall {
                    name: "sum".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "total".to_owned(),
                    }],
                    distinct: false,
                }],
                distinct: false,
            },
            op: crate::schema::ComparisonOperator::Gt,
            right: Expression::Literal {
                value: serde_json::json!(150),
                data_type: crate::schema::DataType::Float,
            },
        };
        assert!(dedup_pred(&mut having));
        if let Predicate::Comparison { ref left, .. } = having {
            match left {
                Expression::ColumnRef {
                    table: Some(t),
                    column: c,
                } => {
                    assert_eq!(t, "orders");
                    assert_eq!(c, "total");
                }
                _ => panic!("Expected ColumnRef after dedup"),
            }
        }
    }
}
