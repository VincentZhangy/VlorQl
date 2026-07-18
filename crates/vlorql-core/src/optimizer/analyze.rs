//! Shared read-only analysis helpers for the rewrite rules.
//!
//! These functions never mutate a plan; they extract the facts the
//! rewriters need — which columns an expression touches, how a `WHERE`
//! tree splits into independent conjuncts, and how to recombine
//! predicates with `AND`.

use std::collections::BTreeSet;

use crate::schema::{Expression, InTarget, OrderByTerm, Predicate, Projection, QueryPlan};

/// A column reference as it appears in a plan: an optional table
/// qualifier plus the column name.
pub type ColumnRef = (Option<String>, String);

/// Splits a predicate into its top-level `AND` conjuncts.
///
/// `a AND (b AND c)` flattens to `[a, b, c]`. Any non-`And` predicate
/// yields a single-element vector. This is the canonical form the
/// pushdown rule reasons about, since each conjunct can be relocated
/// independently.
pub fn split_conjuncts(pred: &Predicate) -> Vec<Predicate> {
    let mut out = Vec::new();
    collect_conjuncts(pred, &mut out);
    out
}

fn collect_conjuncts(pred: &Predicate, out: &mut Vec<Predicate>) {
    match pred {
        Predicate::And { left, right } => {
            collect_conjuncts(left, out);
            collect_conjuncts(right, out);
        }
        other => out.push(other.clone()),
    }
}

/// Recombines conjuncts into a single `AND` tree, or `None` when empty.
pub fn combine_conjuncts(mut conjuncts: Vec<Predicate>) -> Option<Predicate> {
    let first = conjuncts.first().cloned()?;
    let rest = conjuncts.split_off(1);
    Some(rest.into_iter().fold(first, |acc, next| Predicate::And {
        left: Box::new(acc),
        right: Box::new(next),
    }))
}

/// Collects every column referenced anywhere in a predicate.
pub fn columns_in_predicate(pred: &Predicate) -> BTreeSet<ColumnRef> {
    let mut set = BTreeSet::new();
    walk_predicate(pred, &mut set);
    set
}

/// Collects every column referenced anywhere in an expression.
pub fn columns_in_expression(expr: &Expression) -> BTreeSet<ColumnRef> {
    let mut set = BTreeSet::new();
    walk_expression(expr, &mut set);
    set
}

/// Collects the columns a projection reads from its input relations.
///
/// A `Star` projection returns `None`, signalling "every column" — the
/// caller must treat that as "cannot prune".
pub fn columns_in_projection(projection: &Projection) -> Option<BTreeSet<ColumnRef>> {
    match projection {
        Projection::Column { table, column, .. } => {
            Some(BTreeSet::from([(table.clone(), column.clone())]))
        }
        Projection::Expr { expression, .. } => Some(columns_in_expression(expression)),
        Projection::Star { .. } => None,
    }
}

/// Collects the columns referenced by an `ORDER BY` term list.
pub fn columns_in_order_by(terms: &[OrderByTerm]) -> BTreeSet<ColumnRef> {
    let mut set = BTreeSet::new();
    for term in terms {
        walk_expression(&term.expr, &mut set);
    }
    set
}

/// Returns the distinct table qualifiers referenced by a predicate.
///
/// A qualifier of `None` (an unqualified column) is represented by the
/// `None` entry in the set, so a caller can detect "this conjunct has
/// an unqualified column and therefore cannot be safely attributed to a
/// single relation".
pub fn referenced_tables(pred: &Predicate) -> BTreeSet<Option<String>> {
    columns_in_predicate(pred)
        .into_iter()
        .map(|(table, _)| table)
        .collect()
}

fn walk_predicate(pred: &Predicate, out: &mut BTreeSet<ColumnRef>) {
    match pred {
        Predicate::Comparison { left, right, .. } => {
            walk_expression(left, out);
            walk_expression(right, out);
        }
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            walk_predicate(left, out);
            walk_predicate(right, out);
        }
        Predicate::Not { child } => walk_predicate(child, out),
        Predicate::Between {
            expr, low, high, ..
        } => {
            walk_expression(expr, out);
            walk_expression(low, out);
            walk_expression(high, out);
        }
        Predicate::In { expr, target } => {
            walk_expression(expr, out);
            match target {
                InTarget::Values(values) => {
                    for value in values {
                        walk_expression(value, out);
                    }
                }
                InTarget::SubQuery(query) => {
                    walk_plan(query, out);
                }
            }
        }
        Predicate::Exists { query } => {
            walk_plan(query, out);
        }
        Predicate::Like { expr, .. } | Predicate::IsNull { expr } => walk_expression(expr, out),
    }
}

fn walk_expression(expr: &Expression, out: &mut BTreeSet<ColumnRef>) {
    match expr {
        Expression::ColumnRef { table, column } => {
            out.insert((table.clone(), column.clone()));
        }
        Expression::BinaryOp { left, right, .. } => {
            walk_expression(left, out);
            walk_expression(right, out);
        }
        Expression::FunctionCall { args, .. } => {
            for arg in args {
                walk_expression(arg, out);
            }
        }
        Expression::Literal { .. } | Expression::Star => {}
        Expression::SubQuery { query } => walk_plan(query, out),
    }
}

fn walk_plan(plan: &QueryPlan, out: &mut BTreeSet<ColumnRef>) {
    for projection in &plan.select {
        match projection {
            Projection::Column { table, column, .. } => {
                out.insert((table.clone(), column.clone()));
            }
            Projection::Expr { expression, .. } => {
                walk_expression(expression, out);
            }
            Projection::Star { .. } => {}
        }
    }
    if let Some(predicate) = &plan.r#where {
        walk_predicate(predicate, out);
    }
    if let Some(expressions) = &plan.group_by {
        for expression in expressions {
            walk_expression(expression, out);
        }
    }
    if let Some(predicate) = &plan.having {
        walk_predicate(predicate, out);
    }
    if let Some(terms) = &plan.order_by {
        for term in terms {
            walk_expression(&term.expr, out);
        }
    }
    if let Some(joins) = &plan.joins {
        for join in joins {
            walk_predicate(&join.on, out);
        }
    }
    if let Some(ctes) = &plan.ctes {
        for cte in ctes {
            walk_plan(&cte.query, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ComparisonOperator, DataType};

    fn col(table: Option<&str>, column: &str) -> Expression {
        Expression::ColumnRef {
            table: table.map(str::to_owned),
            column: column.to_owned(),
        }
    }

    fn cmp(left: Expression, right: Expression) -> Predicate {
        Predicate::Comparison {
            left,
            op: ComparisonOperator::Eq,
            right,
        }
    }

    fn lit(value: i64) -> Expression {
        Expression::Literal {
            value: value.into(),
            data_type: DataType::Int,
        }
    }

    #[test]
    fn splits_and_recombines_conjuncts() {
        let pred = Predicate::And {
            left: Box::new(cmp(col(Some("a"), "x"), lit(1))),
            right: Box::new(Predicate::And {
                left: Box::new(cmp(col(Some("a"), "y"), lit(2))),
                right: Box::new(cmp(col(Some("b"), "z"), lit(3))),
            }),
        };
        let conjuncts = split_conjuncts(&pred);
        assert_eq!(conjuncts.len(), 3);

        let recombined = combine_conjuncts(conjuncts).unwrap();
        // Recombination is left-associated but semantically equivalent;
        // splitting it again yields the same three conjuncts.
        assert_eq!(split_conjuncts(&recombined).len(), 3);
    }

    #[test]
    fn collects_referenced_tables() {
        let pred = cmp(col(Some("users"), "id"), col(Some("orders"), "user_id"));
        let tables = referenced_tables(&pred);
        assert!(tables.contains(&Some("users".to_owned())));
        assert!(tables.contains(&Some("orders".to_owned())));
    }

    #[test]
    fn star_projection_cannot_be_pruned() {
        assert!(columns_in_projection(&Projection::Star { table: None }).is_none());
    }
}
