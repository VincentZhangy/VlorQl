//! SQL rewrite rules.
//!
//! This module is a placeholder for future SQL rewrite rules that
//! transform the QueryPlan AST before SQL compilation.
//!
//! Planned rules:
//!
//! - **Limit pushdown** — push LIMIT into subqueries where safe
//! - **Predicate pushdown** — push WHERE filters closer to table scans
//! - **Join reordering** — reorder join order for better performance
//! - **Subquery flattening** — convert correlated subqueries to JOINs
//!
//! For now, this module returns the plan unchanged.

use vlorql_core::schema::QueryPlan;

/// Run all SQL rewrite rules on a [`QueryPlan`].
///
/// Returns `true` if any rewrite was applied.
///
/// Currently a no-op — reserved for future implementation.
#[must_use]
pub fn rewrite(_plan: &mut QueryPlan) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use vlorql_core::schema::*;

    #[test]
    fn rewrite_is_currently_noop() {
        let mut plan = QueryPlan {
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
        };
        assert!(!rewrite(&mut plan));
    }
}
