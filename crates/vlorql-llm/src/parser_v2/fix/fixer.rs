//! Auto Fix Engine: opinionated, safe fixes for [`QueryPlan`] AST.
//!
//! This layer runs after the builder and before the validator.  It
//! applies universally safe defaults to fix common issues that small
//! LLMs produce:
//!
//! - **Missing alias** → auto-generate a unique alias for tables
//! - **Limit zero** → remove (default to unlimited)
//! - **Empty select** → inject `[{"type": "star"}]`
//!
//! This layer is **opinionated** — it makes reasonable assumptions so
//! that the validator can focus on real errors.  When in doubt, it
//! does nothing and lets the validator report the issue.

use vlorql_core::schema::{
    Expression, JoinClause, Projection, QueryPlan,
};

/// Run all auto-fix rules on a [`QueryPlan`] recursively
/// (including CTE subqueries).
///
/// Returns `true` if any fix was applied.
#[must_use]
pub fn fix_plan(plan: &mut QueryPlan) -> bool {
    let mut changed = false;
    changed |= fix_limit_zero(plan);
    changed |= fix_empty_select(plan);
    changed |= fix_star_with_group_by(plan);
    // fix_missing_aliases must run last because it adds new aliases.
    changed |= fix_missing_aliases(plan);
    // Recursively fix CTE subqueries.
    if let Some(ref mut ctes) = plan.ctes {
        for cte in ctes.iter_mut() {
            changed |= fix_plan(&mut cte.query);
        }
    }
    changed
}

/// Remove `LIMIT 0` (set to `None`).
///
/// `LIMIT 0` returns no rows and is almost certainly a mistake from
/// the LLM.  Removing it lets the query proceed normally.
#[must_use]
fn fix_limit_zero(plan: &mut QueryPlan) -> bool {
    if plan.limit == Some(0) {
        plan.limit = None;
        true
    } else {
        false
    }
}

/// Inject a default `[{"type": "star"}]` select when the select list
/// is empty.
///
/// The normalize layer already injects a default select when the
/// field is missing entirely, but the builder may produce an empty
/// vector if all items were invalid.
#[must_use]
fn fix_empty_select(plan: &mut QueryPlan) -> bool {
    if plan.select.is_empty() {
        plan.select = vec![Projection::Star { table: None }];
        true
    } else {
        false
    }
}

/// When `SELECT *` is used with `GROUP BY`, replace `*` with the group-by
/// columns as explicit projections.  `SELECT *` with `GROUP BY` is invalid
/// SQL; converting to explicit columns makes the plan valid (the LLM can
/// add aggregates on retry if needed).
fn fix_star_with_group_by(plan: &mut QueryPlan) -> bool {
    let group_by = match &plan.group_by {
        Some(g) if !g.is_empty() => g,
        _ => return false,
    };
    if !plan.select.iter().any(|p| matches!(p, Projection::Star { .. })) {
        return false;
    }

    let mut new_select: Vec<Projection> = Vec::with_capacity(group_by.len());
    for expr in group_by {
        match expr {
            Expression::ColumnRef { table, column } => {
                new_select.push(Projection::Column {
                    table: table.clone(),
                    column: column.clone(),
                    alias: Some(column.clone()),
                });
            }
            other => {
                new_select.push(Projection::Expr {
                    expression: other.clone(),
                    alias: None,
                });
            }
        }
    }
    plan.select = new_select;
    true
}

/// Auto-generate missing aliases for tables in FROM and JOIN clauses.
///
/// Uses a simple counter-based scheme: `t1`, `t2`, `t3`, etc.
/// Duplicate aliases are avoided by tracking used names.
#[must_use]
fn fix_missing_aliases(plan: &mut QueryPlan) -> bool {
    let mut changed = false;
    let mut alias_counter: u32 = 0;
    let mut used_aliases: Vec<String> = Vec::new();

    // Collect existing aliases.
    collect_existing_aliases(plan, &mut used_aliases);

    // Fix FROM alias.
    changed |= fix_from_alias(plan, &mut alias_counter, &mut used_aliases);

    // Fix JOIN aliases.
    if let Some(joins) = &mut plan.joins {
        for join in joins.iter_mut() {
            changed |= fix_join_alias(join, &mut alias_counter, &mut used_aliases);
        }
    }

    // Fix CTE aliases.
    if let Some(ctes) = &mut plan.ctes {
        for cte in ctes.iter_mut() {
            changed |= fix_missing_aliases(&mut cte.query);
        }
    }

    changed
}

/// Collect all existing aliases from the plan tree.
fn collect_existing_aliases(plan: &QueryPlan, aliases: &mut Vec<String>) {
    if let Some(ref alias) = plan.from.alias {
        aliases.push(alias.clone());
    }
    if let Some(ref joins) = plan.joins {
        for join in joins {
            if let Some(ref alias) = join.right_table.alias {
                aliases.push(alias.clone());
            }
        }
    }
    if let Some(ref ctes) = plan.ctes {
        for cte in ctes {
            aliases.push(cte.name.clone());
            collect_existing_aliases(&cte.query, aliases);
        }
    }
}

/// Generate a unique alias name.
fn generate_alias(counter: &mut u32, used: &mut Vec<String>) -> String {
    loop {
        *counter += 1;
        let alias = format!("t{}", counter);
        if !used.contains(&alias) {
            used.push(alias.clone());
            return alias;
        }
    }
}

/// Fix missing alias on FROM clause.
fn fix_from_alias(
    plan: &mut QueryPlan,
    counter: &mut u32,
    used: &mut Vec<String>,
) -> bool {
    if plan.from.alias.is_none() {
        let alias = generate_alias(counter, used);
        plan.from.alias = Some(alias);
        true
    } else {
        false
    }
}

/// Fix missing alias on a JOIN clause's right_table.
fn fix_join_alias(
    join: &mut JoinClause,
    counter: &mut u32,
    used: &mut Vec<String>,
) -> bool {
    if join.right_table.alias.is_none() {
        let alias = generate_alias(counter, used);
        join.right_table.alias = Some(alias);
        true
    } else {
        false
    }
}

/// Convenience: create a fix pipeline function that returns a new
/// [`QueryPlan`] with all fixes applied.
#[must_use]
pub fn apply_fixes(mut plan: QueryPlan) -> QueryPlan {
    let _ = fix_plan(&mut plan);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use vlorql_core::schema::*;

    fn base_plan() -> QueryPlan {
        QueryPlan {
            select: vec![Projection::Star { table: None }],
            from: FromClause { table: "users".to_owned(), alias: None },
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

    // ── fix_limit_zero ───────────────────────────────────────────

    #[test]
    fn removes_limit_zero() {
        let mut plan = base_plan();
        plan.limit = Some(0);
        assert!(fix_limit_zero(&mut plan));
        assert_eq!(plan.limit, None);
    }

    #[test]
    fn keeps_valid_limit() {
        let mut plan = base_plan();
        plan.limit = Some(10);
        assert!(!fix_limit_zero(&mut plan));
        assert_eq!(plan.limit, Some(10));
    }

    #[test]
    fn keeps_none_limit() {
        let mut plan = base_plan();
        assert!(!fix_limit_zero(&mut plan));
        assert_eq!(plan.limit, None);
    }

    // ── fix_empty_select ─────────────────────────────────────────

    #[test]
    fn injects_star_for_empty_select() {
        let mut plan = base_plan();
        plan.select = vec![];
        assert!(fix_empty_select(&mut plan));
        assert_eq!(plan.select.len(), 1);
        assert!(matches!(plan.select[0], Projection::Star { .. }));
    }

    #[test]
    fn keeps_valid_select() {
        let mut plan = base_plan();
        assert!(!fix_empty_select(&mut plan));
        assert_eq!(plan.select.len(), 1);
    }

    // ── fix_missing_aliases ──────────────────────────────────────

    #[test]
    fn adds_alias_to_from() {
        let mut plan = base_plan();
        assert!(fix_missing_aliases(&mut plan));
        assert_eq!(plan.from.alias, Some("t1".to_owned()));
    }

    #[test]
    fn adds_alias_to_join() {
        let mut plan = base_plan();
        plan.joins = Some(vec![
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause { table: "orders".to_owned(), alias: None },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: None,
                        column: "user_id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: None,
                        column: "id".to_owned(),
                    },
                },
            },
        ]);
        assert!(fix_missing_aliases(&mut plan));
        assert_eq!(plan.from.alias, Some("t1".to_owned()));
        let join = &plan.joins.unwrap()[0];
        assert_eq!(join.right_table.alias, Some("t2".to_owned()));
    }

    #[test]
    fn skips_if_alias_already_exists() {
        let mut plan = base_plan();
        plan.from.alias = Some("u".to_owned());
        assert!(!fix_missing_aliases(&mut plan));
        assert_eq!(plan.from.alias, Some("u".to_owned()));
    }

    #[test]
    fn generates_unique_aliases() {
        let mut plan = base_plan();
        plan.from.alias = Some("t1".to_owned());
        plan.joins = Some(vec![
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause { table: "orders".to_owned(), alias: None },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: None,
                        column: "user_id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: None,
                        column: "id".to_owned(),
                    },
                },
            },
        ]);
        assert!(fix_missing_aliases(&mut plan));
        // "t1" is already used, so the join should get "t2"
        let join = &plan.joins.unwrap()[0];
        assert_eq!(join.right_table.alias, Some("t2".to_owned()));
    }

    // ── fix_plan (full pipeline) ─────────────────────────────────

    #[test]
    fn full_fix_pipeline() {
        let mut plan = base_plan();
        plan.limit = Some(0);
        plan.select = vec![];
        assert!(fix_plan(&mut plan));
        // Limit zero removed.
        assert_eq!(plan.limit, None);
        // Empty select fixed.
        assert_eq!(plan.select.len(), 1);
        // Missing alias added.
        assert_eq!(plan.from.alias, Some("t1".to_owned()));
    }

    #[test]
    fn no_change_for_valid_plan() {
        let mut plan = base_plan();
        plan.from.alias = Some("u".to_owned());
        // A valid plan should have no changes.
        assert!(!fix_plan(&mut plan));
    }

    #[test]
    fn apply_fixes_returns_new_plan() {
        let mut plan = base_plan();
        plan.limit = Some(0);
        let fixed = apply_fixes(plan);
        assert_eq!(fixed.limit, None);
        assert_eq!(fixed.from.alias, Some("t1".to_owned()));
    }

    #[test]
    fn fixes_cte_subquery() {
        let mut plan = base_plan();
        plan.ctes = Some(vec![
            CommonTableExpression {
                name: "active".to_owned(),
                query: Box::new(QueryPlan {
                    select: vec![Projection::Star { table: None }],
                    from: FromClause { table: "users".to_owned(), alias: None },
                    r#where: None,
                    group_by: None,
                    having: None,
                    order_by: None,
                    limit: Some(0),
                    offset: None,
                    joins: None,
                    ctes: None,
                }),
            },
        ]);
        assert!(fix_plan(&mut plan));
        // CTE subquery should also be fixed.
        let cte = &plan.ctes.unwrap()[0];
        assert_eq!(cte.query.limit, None);
        assert_eq!(cte.query.from.alias, Some("t1".to_owned()));
    }
}