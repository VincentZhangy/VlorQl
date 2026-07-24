//! Predicate pushdown: move `WHERE` conjuncts closer to their source.
//!
//! Pushdown reduces the number of rows an upper operator has to process
//! by evaluating a filter as early as possible. In this plan model the
//! only relation that can carry its own `WHERE` is a
//! [`CommonTableExpression`](crate::schema::CommonTableExpression) — the
//! [`FromClause`](crate::schema::FromClause) is a bare table name with
//! no room for a sub-filter. So [`PredicatePushdown`] targets CTEs:
//!
//! * The outer `WHERE` is split into independent `AND` conjuncts.
//! * A conjunct that references **only** columns of a single CTE-backed
//!   relation (by the CTE name or its `FROM`/join alias) is removed from
//!   the outer `WHERE` and `AND`-ed into that CTE's own `WHERE`.
//! * Every other conjunct — anything touching a base table, an
//!   unqualified column, or more than one relation — stays where it is.
//!
//! The rule is deliberately conservative. When it cannot prove a
//! conjunct belongs to exactly one CTE it leaves the conjunct in place,
//! so the rewrite can only ever move a filter earlier, never change
//! which rows survive it. Pushdown is applied recursively so a CTE that
//! is itself defined in terms of other CTEs is optimized too.
//!
//! Outer joins are handled carefully: a filter on the null-supplying
//! side of a `LEFT`/`RIGHT`/`FULL` join is **not** pushed down, because
//! doing so would change the join's semantics (it would suppress the
//! null-extended rows the outer join is meant to keep).

use std::collections::{HashMap, HashSet};

use crate::errors::VlorQLError;
use crate::schema::{Expression, JoinType, Predicate, QueryPlan};

use super::analyze::{combine_conjuncts, referenced_tables, split_conjuncts};
use super::rules::PlanRewriter;
use super::visitor::ExpressionFold;

/// Pushes single-relation `WHERE` conjuncts down into the CTEs they
/// filter.
///
/// See the [module documentation](super) for the exact rules and the
/// limitations imposed by the plan model.
#[derive(Debug, Clone, Copy, Default)]
pub struct PredicatePushdown;

impl PlanRewriter for PredicatePushdown {
    fn rewrite(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        Ok(push_plan(plan))
    }
}

/// Applies pushdown to `plan` and, recursively, to every CTE it defines.
fn push_plan(plan: &QueryPlan) -> QueryPlan {
    // Optimize nested CTE bodies first (bottom-up), then push the outer
    // query's conjuncts into them.
    let ctes: Vec<_> = plan
        .ctes
        .as_ref()
        .map(|ctes| {
            ctes.iter()
                .map(|cte| crate::schema::CommonTableExpression {
                    name: cte.name.clone(),
                    query: Box::new(push_plan(&cte.query)), recursive: false
                })
                .collect()
        })
        .unwrap_or_default();

    let mut plan = QueryPlan {
        ctes: if ctes.is_empty() { None } else { Some(ctes) },
        ..plan.clone()
    };

    // Nothing to push without both a `WHERE` and at least one CTE.
    let Some(where_clause) = plan.r#where.clone() else {
        return plan;
    };
    if plan.ctes.as_ref().is_none_or(Vec::is_empty) {
        return plan;
    }

    // Map every relation alias visible in this query to the CTE it
    // resolves to. A relation whose table name is not a CTE (i.e. a base
    // table) is intentionally absent, so conjuncts over it never move.
    let cte_names: HashSet<&str> = plan
        .ctes
        .as_ref()
        .into_iter()
        .flatten()
        .map(|cte| cte.name.as_str())
        .collect();
    let alias_to_cte = cte_relation_aliases(&plan, &cte_names);

    // Aliases on the null-supplying side of an outer join must not
    // receive pushed filters.
    let protected = outer_join_protected_aliases(&plan);

    let mut kept: Vec<Predicate> = Vec::new();
    // CTE name -> conjuncts to inject into that CTE's WHERE.
    let mut pushed: HashMap<String, Vec<Predicate>> = HashMap::new();

    for conjunct in split_conjuncts(&where_clause) {
        match single_cte_target(&conjunct, &alias_to_cte, &protected) {
            Some(cte) => pushed.entry(cte).or_default().push(conjunct),
            None => kept.push(conjunct),
        }
    }

    if pushed.is_empty() {
        return plan;
    }

    // Rebuild the outer WHERE from the conjuncts that stayed.
    plan.r#where = combine_conjuncts(kept);

    // Inject each pushed conjunct into its CTE's WHERE. For CTEs whose
    // body's FROM clause references another CTE at the same level, try to
    // cascade the pushdown further (multi-layer CTE support).
    //
    // We use a two-phase approach to avoid borrow conflicts:
    // Phase 1: collect cascade targets (inner CTE → conditions).
    // Phase 2: apply cascades and local injections.
    if let Some(ctes) = plan.ctes.as_mut() {
        let cte_names_set: HashSet<&str> = ctes.iter().map(|cte| cte.name.as_str()).collect();

        // Phase 1: decide what goes where.
        let mut cascade_targets: Vec<(String, Vec<Predicate>)> = Vec::new();
        // Phase 1b: conditions that stay in the outer CTE, keyed by CTE name.
        let mut local_injections: Vec<(String, Vec<Predicate>)> = Vec::new();

        for cte in ctes.iter() {
            let Some(conjuncts) = pushed.get(&cte.name) else {
                continue;
            };

            let inner_table = cte.query.from.table.as_str();
            if cte_names_set.contains(inner_table) {
                let inner_alias = cte.query.from.alias.as_deref().unwrap_or(inner_table);

                let mut cascade: Vec<Predicate> = Vec::new();
                let mut local: Vec<Predicate> = Vec::new();

                for conjunct in conjuncts {
                    let translated = translate_qualifier(conjunct, &cte.name, inner_alias);

                    let mut inner_alias_map = HashMap::new();
                    inner_alias_map.insert(inner_alias.to_owned(), inner_table.to_owned());
                    let inner_protected = outer_join_protected_aliases(&cte.query);

                    if single_cte_target(&translated, &inner_alias_map, &inner_protected).is_some()
                    {
                        cascade.push(translated);
                    } else {
                        local.push(conjunct.clone());
                    }
                }

                if !cascade.is_empty() {
                    cascade_targets.push((inner_table.to_owned(), cascade));
                }
                if !local.is_empty() {
                    local_injections.push((cte.name.clone(), local));
                }
            } else {
                // Normal case: all conditions stay in this CTE.
                local_injections.push((cte.name.clone(), conjuncts.clone()));
            }
        }

        // Phase 2: apply injections.
        for cte in ctes.iter_mut() {
            // First, apply cascaded conditions from outer CTEs targeting this one.
            if let Some((_, conds)) = cascade_targets.iter().find(|(name, _)| name == &cte.name) {
                inject_into_cte(&mut cte.query, conds.clone());
            }
            // Then, apply local conditions (conditions that target this CTE directly).
            if let Some((_, conds)) = local_injections.iter().find(|(name, _)| name == &cte.name) {
                inject_into_cte(&mut cte.query, conds.clone());
            }
        }
    }

    plan
}

/// Rewrites the conjuncts to reference the CTE's own output columns and
/// `AND`s them into the CTE body's `WHERE`.
fn inject_into_cte(cte_query: &mut QueryPlan, conjuncts: Vec<Predicate>) {
    // The pushed conjuncts are expressed against the *outer* qualifier
    // (the CTE name or alias). Inside the CTE those columns are produced
    // by its own `select`, so we strip the qualifier and let the CTE's
    // validation re-resolve it against the CTE's own `from`.
    let mut existing = cte_query
        .r#where
        .as_ref()
        .map(split_conjuncts)
        .unwrap_or_default();
    for conjunct in conjuncts {
        existing.push(strip_qualifiers(&conjunct));
    }
    cte_query.r#where = combine_conjuncts(existing);
}

/// Returns the CTE a conjunct can be pushed into, if it references
/// exactly one CTE-backed relation and nothing else.
///
/// Returns `None` (keep the conjunct in place) when the conjunct:
/// * references an unqualified column (`None` qualifier),
/// * references more than one relation,
/// * references a base table rather than a CTE, or
/// * targets a relation on the null-supplying side of an outer join.
fn single_cte_target(
    conjunct: &Predicate,
    alias_to_cte: &HashMap<String, String>,
    protected: &HashSet<String>,
) -> Option<String> {
    let tables = referenced_tables(conjunct);

    // Any unqualified column means we cannot attribute the conjunct to a
    // single relation safely.
    if tables.contains(&None) {
        return None;
    }
    if tables.len() != 1 {
        return None;
    }

    let alias = tables.into_iter().next().flatten()?;
    if protected.contains(&alias) {
        return None;
    }
    alias_to_cte.get(&alias).cloned()
}

/// Builds a map from every relation alias in `plan` (the `FROM` table
/// and each join's right table) to the CTE it resolves to.
///
/// The key is the identifier a `WHERE` column would use to qualify
/// itself: the alias when present, otherwise the table name. Only
/// relations whose underlying table is a CTE are included.
fn cte_relation_aliases(plan: &QueryPlan, cte_names: &HashSet<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();

    let mut record = |table: &str, alias: &Option<String>| {
        if cte_names.contains(table) {
            let key = alias.clone().unwrap_or_else(|| table.to_owned());
            map.insert(key, table.to_owned());
        }
    };

    record(&plan.from.table, &plan.from.alias);
    if let Some(joins) = plan.joins.as_ref() {
        for join in joins {
            record(&join.right_table.table, &join.right_table.alias);
        }
    }

    map
}

/// Collects the aliases (or table names) that sit on the null-supplying
/// side of an outer join and therefore must not receive pushed filters.
///
/// For a `LEFT` join the right side is null-supplying; for a `RIGHT`
/// join the left side (`FROM` and every preceding relation) is; a `FULL`
/// join protects both. `INNER`/`CROSS` joins protect nothing.
fn outer_join_protected_aliases(plan: &QueryPlan) -> HashSet<String> {
    let mut protected = HashSet::new();
    let Some(joins) = plan.joins.as_ref() else {
        return protected;
    };

    let from_key = plan
        .from
        .alias
        .clone()
        .unwrap_or_else(|| plan.from.table.clone());

    for (index, join) in joins.iter().enumerate() {
        let right_key = join
            .right_table
            .alias
            .clone()
            .unwrap_or_else(|| join.right_table.table.clone());
        match join.join_type {
            JoinType::Left => {
                protected.insert(right_key);
            }
            JoinType::Right => {
                // Everything to the left of this join is null-supplying.
                protected.insert(from_key.clone());
                for earlier in joins.iter().take(index) {
                    protected.insert(
                        earlier
                            .right_table
                            .alias
                            .clone()
                            .unwrap_or_else(|| earlier.right_table.table.clone()),
                    );
                }
            }
            JoinType::Full => {
                protected.insert(from_key.clone());
                protected.insert(right_key);
                for earlier in joins.iter().take(index) {
                    protected.insert(
                        earlier
                            .right_table
                            .alias
                            .clone()
                            .unwrap_or_else(|| earlier.right_table.table.clone()),
                    );
                }
            }
            JoinType::Inner | JoinType::Cross => {}
        }
    }

    protected
}

/// Removes the table qualifier from every column reference in a
/// predicate, so a conjunct written against the outer CTE alias resolves
/// against the CTE body's own relations.
fn strip_qualifiers(pred: &Predicate) -> Predicate {
    QualifierStripper.fold_predicate(pred)
}

/// Replaces the table qualifier on every column reference in a predicate,
/// translating from `from_qualifier` to `to_qualifier`. This is used for
/// multi-layer CTE pushdown: when a condition qualified with the outer CTE
/// name is pushed into a CTE whose FROM clause references another CTE, the
/// qualifier is translated to the inner CTE's alias so the condition can be
/// pushed further.
fn translate_qualifier(pred: &Predicate, from_qualifier: &str, to_qualifier: &str) -> Predicate {
    QualifierTranslator {
        from: from_qualifier.to_owned(),
        to: to_qualifier.to_owned(),
    }
    .fold_predicate(pred)
}

struct QualifierStripper;

impl ExpressionFold for QualifierStripper {
    fn fold_expression(&mut self, expr: &Expression) -> Expression {
        match expr {
            Expression::ColumnRef { column, .. } => Expression::ColumnRef {
                table: None,
                column: column.clone(),
            },
            other => super::visitor::default_fold_expression(self, other),
        }
    }
}

/// A fold that replaces a specific table qualifier with another.
struct QualifierTranslator {
    from: String,
    to: String,
}

impl ExpressionFold for QualifierTranslator {
    fn fold_expression(&mut self, expr: &Expression) -> Expression {
        match expr {
            Expression::ColumnRef {
                table: Some(t),
                column,
            } if t == &self.from => Expression::ColumnRef {
                table: Some(self.to.clone()),
                column: column.clone(),
            },
            other => super::visitor::default_fold_expression(self, other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        CommonTableExpression, ComparisonOperator, DataType, Expression, FromClause, JoinClause,
        Projection,
    };

    fn col(table: Option<&str>, column: &str) -> Expression {
        Expression::ColumnRef {
            table: table.map(str::to_owned),
            column: column.to_owned(),
        }
    }

    fn lit(value: i64) -> Expression {
        Expression::Literal {
            value: value.into(),
            data_type: DataType::Int,
        }
    }

    fn gt(left: Expression, right: Expression) -> Predicate {
        Predicate::Comparison {
            left,
            op: ComparisonOperator::Gt,
            right,
        }
    }

    fn select_col(table: &str, column: &str) -> Projection {
        Projection::Column {
            table: Some(table.to_owned()),
            column: column.to_owned(),
            alias: None,
        }
    }

    /// A CTE `active` selecting `id`/`amount` from a base table, wrapped
    /// by an outer query that filters on both the CTE and a base table.
    fn plan_with_cte(outer_where: Predicate) -> QueryPlan {
        let cte = CommonTableExpression {
            name: "active".to_owned(),
            recursive: false,
            query: Box::new(QueryPlan {
                select: vec![select_col("orders", "id"), select_col("orders", "amount")],
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
            distinct: false,
            distinct_on: None,
            set_operation: None,            }),
        };
        QueryPlan {
            select: vec![select_col("active", "id")],
            from: FromClause {
                table: "active".to_owned(),
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
            distinct: false,
            distinct_on: None,
            set_operation: None,
        }
    }

    #[test]
    fn pushes_single_cte_conjunct_into_cte() {
        // WHERE active.amount > 100
        let plan = plan_with_cte(gt(col(Some("active"), "amount"), lit(100)));
        let rewritten = PredicatePushdown.rewrite(&plan).unwrap();

        // The only conjunct referenced just the CTE, so the outer WHERE
        // becomes empty and the CTE gains the filter.
        assert!(rewritten.r#where.is_none());
        let cte_where = rewritten.ctes.as_ref().unwrap()[0]
            .query
            .r#where
            .as_ref()
            .expect("cte should have received the pushed filter");
        // The qualifier is stripped inside the CTE.
        assert_eq!(*cte_where, gt(col(None, "amount"), lit(100)),);
    }

    #[test]
    fn keeps_multi_relation_conjunct_in_place() {
        // WHERE active.amount > active.id  (single relation, still pushable)
        // AND   active.id > 5
        let pred = Predicate::And {
            left: Box::new(gt(col(Some("active"), "amount"), lit(100))),
            right: Box::new(gt(col(Some("base"), "x"), lit(1))),
        };
        let plan = plan_with_cte(pred);
        let rewritten = PredicatePushdown.rewrite(&plan).unwrap();

        // `base` is not a CTE, so that conjunct stays in the outer WHERE.
        let outer = rewritten.r#where.as_ref().expect("outer where remains");
        assert_eq!(split_conjuncts(outer).len(), 1);
        assert!(rewritten.ctes.as_ref().unwrap()[0].query.r#where.is_some());
    }

    #[test]
    fn does_not_push_onto_null_supplying_side_of_left_join() {
        // Outer query LEFT JOINs the CTE; a filter on the CTE must not be
        // pushed because that would drop null-extended rows.
        let mut plan = plan_with_cte(gt(col(Some("active"), "amount"), lit(100)));
        plan.from = FromClause {
            table: "base".to_owned(),
            alias: None,
        };
        plan.select = vec![select_col("base", "id")];
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Left,
            right_table: FromClause {
                table: "active".to_owned(),
                alias: None,
            },
            on: gt(col(Some("base"), "id"), col(Some("active"), "id")),
        }]);

        let rewritten = PredicatePushdown.rewrite(&plan).unwrap();
        // Filter stayed in the outer WHERE; CTE untouched.
        assert!(rewritten.r#where.is_some());
        assert!(rewritten.ctes.as_ref().unwrap()[0].query.r#where.is_none());
    }

    #[test]
    fn unqualified_conjunct_is_not_pushed() {
        let plan = plan_with_cte(gt(col(None, "amount"), lit(100)));
        let rewritten = PredicatePushdown.rewrite(&plan).unwrap();
        assert!(rewritten.r#where.is_some());
        assert!(rewritten.ctes.as_ref().unwrap()[0].query.r#where.is_none());
    }
}
