//! Shared read-only analysis helpers for the rewrite rules.
//!
//! These functions never mutate a plan; they extract the facts the
//! rewriters need — which columns an expression touches, how a `WHERE`
//! tree splits into independent conjuncts, and how to recombine
//! predicates with `AND`.

use std::collections::HashSet;

use crate::schema::{Expression, OrderByTerm, Predicate, Projection};

use super::visitor::ExpressionVisit;

/// A column reference as it appears in a plan: an optional table
/// qualifier plus the column name.
pub type ColumnRef = (Option<String>, String);

/// Splits a predicate into its top-level `AND` conjuncts.
///
/// `a AND (b AND c)` flattens to `[a, b, c]`. Any non-`And` predicate
/// yields a single-element vector.
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
pub fn columns_in_predicate(pred: &Predicate) -> HashSet<ColumnRef> {
    let mut set = HashSet::new();
    let mut visitor = ColumnCollector;
    visitor.visit_predicate(pred, &mut set);
    set
}

/// Collects every column referenced anywhere in an expression.
pub fn columns_in_expression(expr: &Expression) -> HashSet<ColumnRef> {
    let mut set = HashSet::new();
    let mut visitor = ColumnCollector;
    visitor.visit_expression(expr, &mut set);
    set
}

/// Collects the columns a projection reads from its input relations.
///
/// A `Star` projection returns `None`, signalling "every column" — the
/// caller must treat that as "cannot prune".
pub fn columns_in_projection(projection: &Projection) -> Option<HashSet<ColumnRef>> {
    match projection {
        Projection::Column { table, column, .. } => {
            Some(HashSet::from([(table.clone(), column.clone())]))
        }
        Projection::Expr { expression, .. } => Some(columns_in_expression(expression)),
        Projection::Star { .. } => None,
    }
}

/// Collects the columns referenced by an `ORDER BY` term list.
pub fn columns_in_order_by(terms: &[OrderByTerm]) -> HashSet<ColumnRef> {
    let mut set = HashSet::new();
    let mut visitor = ColumnCollector;
    for term in terms {
        visitor.visit_expression(&term.expr, &mut set);
    }
    set
}

/// Returns the distinct table qualifiers referenced by a predicate.
///
/// A qualifier of `None` (an unqualified column) is represented by the
/// `None` entry in the set, so a caller can detect "this conjunct has
/// an unqualified column and therefore cannot be safely attributed to a
/// single relation".
pub fn referenced_tables(pred: &Predicate) -> HashSet<Option<String>> {
    columns_in_predicate(pred)
        .into_iter()
        .map(|(table, _)| table)
        .collect()
}

// -- visitor implementation ------------------------------------------------

struct ColumnCollector;

impl ExpressionVisit for ColumnCollector {
    type Ctx = HashSet<ColumnRef>;

    fn visit_expression(&mut self, expr: &Expression, ctx: &mut Self::Ctx) {
        if let Expression::ColumnRef { table, column } = expr {
            ctx.insert((table.clone(), column.clone()));
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
