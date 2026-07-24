//! Fix GROUP BY clauses that are missing non-aggregated columns or contain
//! garbage entries (e.g. `literal null`).

use crate::schema::expressions::Expression;
use crate::schema::{Projection, QueryPlan};

/// Check whether an expression tree contains any aggregate function call.
fn has_aggregate(expr: &Expression) -> bool {
    match expr {
        Expression::FunctionCall { name, args, .. } => {
            let name_lower = name.to_lowercase();
            if matches!(name_lower.as_str(), "sum" | "count" | "avg" | "min" | "max") {
                return true;
            }
            args.iter().any(|a| has_aggregate(a))
        }
        Expression::BinaryOp { left, right, .. } => {
            has_aggregate(left) || has_aggregate(right)
        }
        Expression::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                if has_aggregate(op) {
                    return true;
                }
            }
            for wt in when_thens {
                if has_aggregate(&wt.when) || has_aggregate(&wt.then) {
                    return true;
                }
            }
            if let Some(els) = else_result {
                if has_aggregate(els) {
                    return true;
                }
            }
            false
        }
        Expression::SubQuery { .. } | Expression::WindowFunction { .. } => false,
        Expression::Literal { .. } | Expression::ColumnRef { .. } | Expression::Star => false,
    }
}

/// Check if a projection is a non-aggregated expression (should be in GROUP BY).
fn is_non_aggregated(proj: &Projection) -> bool {
    match proj {
        Projection::Column { .. } => {
            // A simple column reference is never aggregated.
            true
        }
        Projection::Expr { expression, .. } => !has_aggregate(expression),
        Projection::Star { .. } => false,
    }
}

/// Collect all non-aggregated column references from the SELECT list that
/// must appear in GROUP BY.
fn collect_non_aggregated_columns(plan: &QueryPlan) -> Vec<Expression> {
    let mut cols = Vec::new();
    for proj in &plan.select {
        if is_non_aggregated(proj) {
            match proj {
                Projection::Column {
                    table, column, alias: _,
                } => {
                    let expr = Expression::ColumnRef {
                        table: table.clone(),
                        column: column.clone(),
                    };
                    if !cols.contains(&expr) {
                        cols.push(expr);
                    }
                }
                Projection::Expr { expression, .. } => {
                    if !cols.contains(expression) {
                        cols.push(expression.clone());
                    }
                }
                Projection::Star { .. } => {}
            }
        }
    }
    cols
}

/// Fix GROUP BY in a query plan.
///
/// 1. Remove any `Literal` entries from GROUP BY (garbage from weak LLMs).
/// 2. Add any non-aggregated SELECT columns that are missing from GROUP BY.
///
/// Does nothing if the plan has no aggregate functions (no GROUP BY needed),
/// or if GROUP BY is already complete.
pub fn fix_group_by(plan: &mut QueryPlan) -> bool {
    // If there's no aggregate function anywhere, GROUP BY is not needed.
    let has_agg = plan.select.iter().any(|proj| match proj {
        Projection::Expr { expression, .. } => has_aggregate(expression),
        _ => false,
    }) || plan
        .having
        .as_ref()
        .is_some_and(|having| has_aggregate_in_pred(having));

    if !has_agg {
        // No aggregates → clear GROUP BY entirely if it exists.
        if plan.group_by.is_some() {
            plan.group_by = None;
            return true;
        }
        return false;
    }

    let mut changed = false;

    // Step 1: Remove garbage entries (Literal type).
    if let Some(ref mut group_by) = plan.group_by {
        let before = group_by.len();
        group_by.retain(|expr| !matches!(expr, Expression::Literal { .. }));
        if group_by.len() != before {
            changed = true;
        }
    }

    // Step 2: Add missing non-aggregated columns.
    let required = collect_non_aggregated_columns(plan);
    if required.is_empty() {
        return changed;
    }

    if plan.group_by.is_none() {
        plan.group_by = Some(Vec::new());
        changed = true;
    }

    {
        let group_by = plan.group_by.as_mut().unwrap();
        for col in required {
            if !group_by.contains(&col) {
                group_by.push(col);
                changed = true;
            }
        }
    }

    // Step 3: If GROUP BY is empty after cleaning, remove it.
    if let Some(ref group_by) = plan.group_by {
        if group_by.is_empty() {
            plan.group_by = None;
            changed = true;
        }
    }

    // Recurse into CTE subqueries.
    if let Some(ref mut ctes) = plan.ctes {
        for cte in ctes.iter_mut() {
            changed |= fix_group_by(&mut cte.query);
        }
    }

    changed
}

fn has_aggregate_in_pred(pred: &crate::schema::Predicate) -> bool {
    match pred {
        crate::schema::Predicate::Comparison { left, right, .. } => {
            has_aggregate(left) || has_aggregate(right)
        }
        crate::schema::Predicate::And { left, right }
        | crate::schema::Predicate::Or { left, right } => {
            has_aggregate_in_pred(left) || has_aggregate_in_pred(right)
        }
        crate::schema::Predicate::Not { child } => has_aggregate_in_pred(child),
        crate::schema::Predicate::Between { expr, low, high } => {
            has_aggregate(expr) || has_aggregate(low) || has_aggregate(high)
        }
        crate::schema::Predicate::In { expr, target } => {
            let mut found = has_aggregate(expr);
            if let crate::schema::InTarget::SubQuery(query) = target {
                found |= plan_has_aggregates(query);
            }
            found
        }
        crate::schema::Predicate::Like { expr, .. } => has_aggregate(expr),
        crate::schema::Predicate::IsNull { expr } => has_aggregate(expr),
        crate::schema::Predicate::Exists { query } => plan_has_aggregates(query),
    }
}

fn plan_has_aggregates(plan: &QueryPlan) -> bool {
    plan.select.iter().any(|proj| match proj {
        Projection::Expr { expression, .. } => has_aggregate(expression),
        _ => false,
    }) || plan.having.as_ref().is_some_and(|h| has_aggregate_in_pred(h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FromClause, QueryPlan};

    fn make_order_id() -> Expression {
        Expression::ColumnRef {
            table: Some("orders".to_owned()),
            column: "id".to_owned(),
        }
    }

    fn make_user_name() -> Expression {
        Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
        }
    }

    fn make_sum_total() -> Expression {
        Expression::FunctionCall {
            name: "sum".to_owned(),
            args: vec![Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            }],
            distinct: false,
        }
    }

    fn base_plan(select: Vec<Projection>) -> QueryPlan {
        QueryPlan {
            select,
            distinct: false,
            distinct_on: None,
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
            set_operation: None,
        }
    }

    #[test]
    fn removes_literal_from_group_by() {
        let mut plan = base_plan(vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: make_sum_total(),
                alias: Some("total".to_owned()),
            },
        ]);
        plan.group_by = Some(vec![
            Expression::Literal {
                value: serde_json::Value::Null,
                data_type: crate::schema::DataType::Null,
            },
            make_order_id(),
        ]);

        assert!(fix_group_by(&mut plan));
        let gb = plan.group_by.as_ref().unwrap();
        assert_eq!(gb.len(), 1);
        assert!(matches!(gb[0], Expression::ColumnRef { .. }));
    }

    #[test]
    fn adds_missing_non_aggregated_columns() {
        let mut plan = base_plan(vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: make_sum_total(),
                alias: Some("total".to_owned()),
            },
        ]);
        plan.group_by = Some(vec![make_order_id()]);

        assert!(fix_group_by(&mut plan));
        let gb = plan.group_by.as_ref().unwrap();
        assert!(gb.contains(&make_order_id()), "should contain orders.id");
        assert!(
            gb.contains(&make_user_name()),
            "should contain users.name"
        );
    }

    #[test]
    fn removes_group_by_when_no_aggregates() {
        let mut plan = base_plan(vec![Projection::Column {
            table: Some("orders".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }]);
        plan.group_by = Some(vec![make_order_id()]);

        assert!(fix_group_by(&mut plan));
        assert!(plan.group_by.is_none(), "GROUP BY should be removed");
    }

    #[test]
    fn no_change_when_group_by_correct() {
        let mut plan = base_plan(vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: make_sum_total(),
                alias: Some("total".to_owned()),
            },
        ]);
        plan.group_by = Some(vec![make_order_id()]);

        assert!(!fix_group_by(&mut plan));
    }
}
