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

use vlorql_core::schema::{Expression, InTarget, JoinClause, Predicate, Projection, QueryPlan};

/// Run all auto-fix rules on a [`QueryPlan`] recursively
/// (including CTE subqueries).
///
/// Returns `true` if any fix was applied.
#[must_use]
pub fn fix_plan(plan: &mut QueryPlan) -> bool {
    let mut changed = false;
    changed |= fix_limit_zero(plan);
    changed |= fix_empty_select(plan);
    changed |= fix_missing_aggregate(plan);
    changed |= fix_missing_group_by(plan);
    // fix_missing_aliases must run last because it adds new aliases.
    changed |= fix_missing_aliases(plan);
    // Replace ORDER BY alias references with the original SELECT expressions.
    changed |= fix_order_by_aliases(plan);
    // Move aggregate conditions from WHERE to HAVING.
    changed |= fix_where_aggregates(plan);
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

/// When `GROUP BY` is present but SELECT has no aggregate function
/// (SUM/COUNT/AVG/MIN/MAX), prepend a suitable aggregate as fallback.
///
/// Small LLMs (7B) often generate `SELECT col GROUP BY col` without
/// any aggregate — valid SQL but meaningless for the user's question.
///
/// Heuristic for choosing the aggregate:
/// - If a non-grouped SELECT column has a numeric-suggesting name
///   (`price`, `total`, `amount`, `quantity`, `cost`, `revenue`, etc.),
///   wrap it with `SUM()`.
/// - Otherwise fall back to `COUNT(*)`.
#[must_use]
fn fix_missing_aggregate(plan: &mut QueryPlan) -> bool {
    // Only applies when GROUP BY is present and non-empty.
    if plan.group_by.as_ref().is_none_or(|g| g.is_empty()) {
        return false;
    }
    // Check if SELECT already has an aggregate function.
    let has_agg = plan.select.iter().any(|p| match p {
        Projection::Expr { expression, .. } => is_aggregate_expr(expression),
        _ => false,
    });
    if has_agg {
        return false;
    }

    // Collect GROUP BY column names (table.column or bare column).
    let group_cols: Vec<(&str, &str)> = plan.group_by.as_ref().map_or_else(Vec::new, |g| {
        g.iter()
            .filter_map(|e| match e {
                Expression::ColumnRef { table, column } => {
                    Some((table.as_deref().unwrap_or(""), column.as_str()))
                }
                _ => None,
            })
            .collect()
    });

    // Find the first non-grouped column with a numeric-suggesting name.
    let agg = plan.select.iter().find_map(|p| match p {
        Projection::Column { table, column, .. } => {
            if group_cols.iter().any(|(t, c)| {
                let t_match = table.as_deref().unwrap_or("") == *t;
                t_match && *c == column.as_str()
            }) {
                return None; // grouped column, skip
            }
            infer_numeric_aggregate(column)
        }
        _ => None,
    });

    match agg {
        Some((name, col)) => {
            // Prepend SUM(col) as the first projection.
            plan.select.insert(
                0,
                Projection::Expr {
                    expression: Expression::FunctionCall {
                        name: name.to_owned(),
                        args: vec![Expression::ColumnRef {
                            table: None,
                            column: col.to_owned(),
                        }],
                        distinct: false,
                    },
                    alias: Some(format!("{}_{}", name, col)),
                },
            );
        }
        None => {
            // Fallback: COUNT(*) as the first projection.
            plan.select.insert(
                0,
                Projection::Expr {
                    expression: Expression::FunctionCall {
                        name: "count".to_owned(),
                        args: vec![Expression::Star],
                        distinct: false,
                    },
                    alias: Some("count".to_owned()),
                },
            );
        }
    }
    true
}

/// Column names that suggest a SUM aggregate is appropriate.
const SUM_COLUMNS: &[&str] = &[
    "price", "total", "amount", "quantity", "cost", "revenue", "salary", "budget", "fee", "rate",
    "score", "value", "count",
];

/// If `column` has a numeric-suggesting name, return `("sum", column)`.
/// Otherwise return `None` (caller falls back to `COUNT(*)`).
fn infer_numeric_aggregate(column: &str) -> Option<(&'static str, &str)> {
    let lower = column.to_lowercase();
    if SUM_COLUMNS.iter().any(|&s| lower.contains(s)) {
        Some(("sum", column))
    } else {
        None
    }
}

/// Returns `true` when `expr` is (or contains) an aggregate function call.
fn is_aggregate_expr(expr: &Expression) -> bool {
    match expr {
        Expression::FunctionCall { name, .. } => vlorql_core::function::is_aggregate(name),
        Expression::BinaryOp { left, right, .. } => {
            is_aggregate_expr(left) || is_aggregate_expr(right)
        }
        _ => false,
    }
}

/// When SELECT has both aggregate functions and bare column references
/// but no GROUP BY, add GROUP BY based on the non-aggregated columns.
///
/// Small LLMs sometimes generate `SELECT name, count(qty) FROM ...`
/// without `GROUP BY name` — valid SQL but semantically wrong for
/// "each/every/per" questions.
#[must_use]
fn fix_missing_group_by(plan: &mut QueryPlan) -> bool {
    // Skip if GROUP BY already exists.
    if plan.group_by.as_ref().is_some_and(|g| !g.is_empty()) {
        return false;
    }
    // Check: SELECT has at least one aggregate AND at least one bare column.
    let has_agg = plan.select.iter().any(|p| match p {
        Projection::Expr { expression, .. } => is_aggregate_expr(expression),
        _ => false,
    });
    if !has_agg {
        return false;
    }
    let bare_cols: Vec<Expression> = plan
        .select
        .iter()
        .filter_map(|p| match p {
            Projection::Column { table, column, .. } => Some(Expression::ColumnRef {
                table: table.clone(),
                column: column.clone(),
            }),
            _ => None,
        })
        .collect();
    if bare_cols.is_empty() {
        return false;
    }
    plan.group_by = Some(bare_cols);
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
fn fix_from_alias(plan: &mut QueryPlan, counter: &mut u32, used: &mut Vec<String>) -> bool {
    if plan.from.alias.is_none() {
        let alias = generate_alias(counter, used);
        plan.from.alias = Some(alias);
        true
    } else {
        false
    }
}

/// Fix missing alias on a JOIN clause's right_table.
fn fix_join_alias(join: &mut JoinClause, counter: &mut u32, used: &mut Vec<String>) -> bool {
    if join.right_table.alias.is_none() {
        let alias = generate_alias(counter, used);
        join.right_table.alias = Some(alias);
        true
    } else {
        false
    }
}

/// Move aggregate conditions from WHERE to HAVING.
///
/// LLMs frequently put aggregate filter conditions (e.g. `COUNT(*) > 5`) in
/// the WHERE clause instead of HAVING.  This is invalid SQL because aggregate
/// functions cannot appear in WHERE.  This function detects such conditions
/// and moves them to HAVING.
///
/// When WHERE contains any expression with an aggregate function, the entire
/// WHERE predicate is promoted to HAVING (since WHERE cannot contain aggregates,
/// this is safe — the LLM intended these as HAVING conditions).
#[must_use]
fn fix_where_aggregates(plan: &mut QueryPlan) -> bool {
    let Some(ref where_pred) = plan.r#where else {
        return false;
    };

    // Check if WHERE contains any aggregate function call.
    if !contains_aggregate(where_pred) {
        return false;
    }

    // Move the entire WHERE to HAVING.
    // If HAVING already exists, AND the two together.
    let new_having = if let Some(ref having_pred) = plan.having {
        Predicate::And {
            left: Box::new(where_pred.clone()),
            right: Box::new(having_pred.clone()),
        }
    } else {
        where_pred.clone()
    };

    plan.having = Some(new_having);
    plan.r#where = None;
    true
}

/// Recursively check if a predicate tree contains any aggregate function call.
fn contains_aggregate(pred: &Predicate) -> bool {
    match pred {
        Predicate::Comparison { left, right, .. } => {
            is_aggregate_expr(left) || is_aggregate_expr(right)
        }
        Predicate::Between { expr, low, high } => {
            is_aggregate_expr(expr) || is_aggregate_expr(low) || is_aggregate_expr(high)
        }
        Predicate::In { expr, target, .. } => {
            if is_aggregate_expr(expr) {
                return true;
            }
            match target {
                InTarget::Values(values) => values.iter().any(|v| is_aggregate_expr(v)),
                InTarget::SubQuery(_) => false,
            }
        }
        Predicate::Like { expr, .. } => {
            is_aggregate_expr(expr)
        }
        Predicate::IsNull { expr } => is_aggregate_expr(expr),
        Predicate::And { left, right } => contains_aggregate(left) || contains_aggregate(right),
        Predicate::Or { left, right } => contains_aggregate(left) || contains_aggregate(right),
        Predicate::Not { child } => contains_aggregate(child),
        Predicate::Exists { .. } => false,
    }
}

/// Replace ORDER BY expressions that reference SELECT aliases with the
/// original SELECT expression.
///
/// LLMs frequently emit order_by terms that reference an alias from the
/// SELECT list (e.g. `ORDER BY total_amount DESC`), but in the canonical
/// JSON the ORDER BY `expr` must be the actual expression (not an alias).
/// This function builds an alias → expression map from the SELECT list
/// and replaces any ORDER BY `column_ref` that resolves to an alias with
/// the original SELECT expression.
#[must_use]
fn fix_order_by_aliases(plan: &mut QueryPlan) -> bool {
    let Some(ref mut order_by) = plan.order_by else {
        return false;
    };
    if order_by.is_empty() {
        return false;
    }

    // Build alias → expression map from SELECT projections.
    let mut alias_map: Vec<(String, Expression)> = Vec::new();
    for proj in &plan.select {
        match proj {
            Projection::Column { column, alias: Some(a), .. } => {
                alias_map.push((a.clone(), Expression::ColumnRef {
                    table: None,
                    column: column.clone(),
                }));
            }
            Projection::Expr { expression, alias: Some(a), .. } => {
                alias_map.push((a.clone(), expression.clone()));
            }
            _ => {}
        }
    }

    if alias_map.is_empty() {
        return false;
    }

    let mut changed = false;
    for term in order_by.iter_mut() {
        // Case 1: Direct column_ref matching an alias.
        if let Expression::ColumnRef { ref column, .. } = term.expr {
            if let Some((_, original_expr)) = alias_map.iter().find(|(alias, _)| alias == column) {
                term.expr = original_expr.clone();
                changed = true;
                continue;
            }
        }
        // Case 2: desc(...) function call wrapping a column_ref matching an alias.
        if let Expression::FunctionCall { ref name, ref args, .. } = term.expr {
            if name == "desc" && args.len() == 1 {
                if let Expression::ColumnRef { ref column, .. } = args[0] {
                    if let Some((_, original_expr)) = alias_map.iter().find(|(alias, _)| alias == column) {
                        term.expr = original_expr.clone();
                        term.descending = true;
                        changed = true;
                        continue;
                    }
                }
            }
        }
    }

    changed
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
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: None,
            },
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
        }]);
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
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: None,
            },
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
        }]);
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
        plan.ctes = Some(vec![CommonTableExpression {
            name: "active".to_owned(),
            recursive: false,
            query: Box::new(QueryPlan {
                select: vec![Projection::Star { table: None }],
                from: FromClause {
                    table: "users".to_owned(),
                    alias: None,
                },
                r#where: None,
                group_by: None,
                having: None,
                order_by: None,
                limit: Some(0),
                offset: None,
                joins: None,
                ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,            }),
        }]);
        assert!(fix_plan(&mut plan));
        // CTE subquery should also be fixed.
        let cte = &plan.ctes.unwrap()[0];
        assert_eq!(cte.query.limit, None);
        assert_eq!(cte.query.from.alias, Some("t1".to_owned()));
    }
}
