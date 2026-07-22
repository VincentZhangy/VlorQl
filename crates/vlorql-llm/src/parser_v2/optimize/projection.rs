//! Projection pruning and optimization.
//!
//! Optimizes the SELECT list by:
//!
//! - Removing duplicate column references
//! - (Future) Removing unused columns (requires schema analysis)
//! - (Future) Pushing down predicates to reduce rows earlier

use std::collections::HashSet;
use vlorql_core::schema::{Projection, QueryPlan};

/// Run all projection optimization rules on a [`QueryPlan`].
///
/// Returns `true` if any optimization was applied.
#[must_use]
pub fn optimize(plan: &mut QueryPlan) -> bool {
    let mut changed = false;
    changed |= remove_duplicate_columns(plan);
    changed
}

/// Remove duplicate column references from the SELECT list.
///
/// If the same column appears twice (e.g., `SELECT id, id`), the
/// duplicate is removed.  Star projections and expression projections
/// are not deduplicated.
#[must_use]
fn remove_duplicate_columns(plan: &mut QueryPlan) -> bool {
    let mut seen: HashSet<(Option<String>, String)> = HashSet::new();
    let mut deduped: Vec<Projection> = Vec::with_capacity(plan.select.len());
    let mut changed = false;

    for proj in plan.select.drain(..) {
        match &proj {
            Projection::Column {
                table,
                column,
                alias: _,
            } => {
                let key = (table.clone(), column.clone());
                if !seen.insert(key) {
                    changed = true;
                    continue; // Skip duplicate
                }
                deduped.push(proj);
            }
            // Star and Expr projections are always kept.
            _ => {
                deduped.push(proj);
            }
        }
    }

    plan.select = deduped;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use vlorql_core::schema::*;

    fn base_plan() -> QueryPlan {
        QueryPlan {
            select: vec![],
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
        }
    }

    #[test]
    fn removes_duplicate_columns() {
        let mut plan = base_plan();
        plan.select = vec![
            Projection::Column {
                table: None,
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: None,
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: None,
                column: "id".to_owned(),
                alias: None,
            },
        ];
        assert!(optimize(&mut plan));
        assert_eq!(plan.select.len(), 2);
    }

    #[test]
    fn keeps_unique_columns() {
        let mut plan = base_plan();
        plan.select = vec![
            Projection::Column {
                table: None,
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: None,
                column: "name".to_owned(),
                alias: None,
            },
        ];
        assert!(!optimize(&mut plan));
        assert_eq!(plan.select.len(), 2);
    }

    #[test]
    fn keeps_star_projections() {
        let mut plan = base_plan();
        plan.select = vec![
            Projection::Star { table: None },
            Projection::Star { table: None },
        ];
        // Star projections are not deduplicated (they're always kept).
        assert!(!optimize(&mut plan));
        assert_eq!(plan.select.len(), 2);
    }

    #[test]
    fn keeps_expr_projections() {
        let mut plan = base_plan();
        plan.select = vec![
            Projection::Expr {
                expression: vlorql_core::schema::Expression::Literal {
                    value: serde_json::json!(42),
                    data_type: vlorql_core::schema::DataType::Int,
                },
                alias: None,
            },
            Projection::Expr {
                expression: vlorql_core::schema::Expression::Literal {
                    value: serde_json::json!(42),
                    data_type: vlorql_core::schema::DataType::Int,
                },
                alias: None,
            },
        ];
        // Expr projections are not deduplicated (they may have side effects).
        assert!(!optimize(&mut plan));
        assert_eq!(plan.select.len(), 2);
    }

    #[test]
    fn deduplicates_with_qualified_columns() {
        let mut plan = base_plan();
        plan.select = vec![
            Projection::Column {
                table: Some("u".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("u".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("o".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
        ];
        assert!(optimize(&mut plan));
        assert_eq!(plan.select.len(), 2); // u.id appears once, o.id appears once
    }
}
