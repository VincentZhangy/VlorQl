//! Fix missing JOIN clauses using the schema's foreign-key metadata.
//!
//! When a query references `table.column` in SELECT, WHERE, or ORDER BY
//! but `table` is not in FROM or any JOIN, this module looks up the
//! schema to find a valid FK relationship and inserts the correct JOIN.

use crate::schema::{
    FromClause, JoinClause, JoinType, Predicate, QueryPlan, SchemaSnapshot,
    expressions::Expression,
};
use std::collections::HashSet;

/// Collect all table names already referenced in the query scope
/// (FROM table + all JOIN right-hand sides).
fn scope_tables(plan: &QueryPlan) -> HashSet<String> {
    let mut tables = HashSet::new();
    tables.insert(plan.from.table.clone());
    if let Some(ref joins) = plan.joins {
        for join in joins {
            tables.insert(join.right_table.table.clone());
        }
    }
    tables
}

/// Collect all table-qualified column references from an expression tree.
fn collect_tables_in_expr(expr: &Expression, tables: &mut HashSet<String>) {
    match expr {
        Expression::ColumnRef { table: Some(t), .. } => {
            tables.insert(t.clone());
        }
        Expression::ColumnRef { table: None, .. } => {
            // Unqualified column reference — no table to add.
        }
        Expression::FunctionCall { args, .. } => {
            for arg in args {
                collect_tables_in_expr(arg, tables);
            }
        }
        Expression::BinaryOp { left, right, .. } => {
            collect_tables_in_expr(left, tables);
            collect_tables_in_expr(right, tables);
        }
        Expression::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_tables_in_expr(op, tables);
            }
            for wt in when_thens {
                collect_tables_in_expr(&wt.when, tables);
                collect_tables_in_expr(&wt.then, tables);
            }
            if let Some(els) = else_result {
                collect_tables_in_expr(els, tables);
            }
        }
        Expression::SubQuery { query } => {
            collect_tables_in_plan(query, tables);
        }
        Expression::WindowFunction { args, .. } => {
            for arg in args {
                collect_tables_in_expr(arg, tables);
            }
        }
        Expression::Literal { .. } | Expression::Star => {}
    }
}

/// Collect all table-qualified column references from a predicate.
fn collect_tables_in_pred(pred: &Predicate, tables: &mut HashSet<String>) {
    match pred {
        Predicate::Comparison { left, right, .. } => {
            collect_tables_in_expr(left, tables);
            collect_tables_in_expr(right, tables);
        }
        Predicate::And { left, right }
        | Predicate::Or { left, right } => {
            collect_tables_in_pred(left, tables);
            collect_tables_in_pred(right, tables);
        }
        Predicate::Not { child } => collect_tables_in_pred(child, tables),
        Predicate::Between { expr, low, high } => {
            collect_tables_in_expr(expr, tables);
            collect_tables_in_expr(low, tables);
            collect_tables_in_expr(high, tables);
        }
        Predicate::In { expr, target } => {
            collect_tables_in_expr(expr, tables);
            if let crate::schema::InTarget::SubQuery(query) = target {
                collect_tables_in_plan(query, tables);
            }
        }
        Predicate::Like { expr, .. } => {
            collect_tables_in_expr(expr, tables);
        }
        Predicate::IsNull { expr } => {
            collect_tables_in_expr(expr, tables);
        }
        Predicate::Exists { query } => {
            collect_tables_in_plan(query, tables);
        }
    }
}

/// Recursively collect all table references from an entire plan
/// (used for CTEs and subqueries).
fn collect_tables_in_plan(plan: &QueryPlan, tables: &mut HashSet<String>) {
    for proj in &plan.select {
        match proj {
            crate::schema::Projection::Column {
                table: Some(t), ..
            } => {
                tables.insert(t.clone());
            }
            crate::schema::Projection::Expr { expression, .. } => {
                collect_tables_in_expr(expression, tables);
            }
            crate::schema::Projection::Star { table: Some(t) } => {
                tables.insert(t.clone());
            }
            _ => {}
        }
    }
    if let Some(ref pred) = plan.r#where {
        collect_tables_in_pred(pred, tables);
    }
    if let Some(ref order_by) = plan.order_by {
        for term in order_by {
            collect_tables_in_expr(&term.expr, tables);
        }
    }
    if let Some(ref joins) = plan.joins {
        for join in joins {
            collect_tables_in_pred(&join.on, tables);
        }
    }
    // Recurse into CTEs and subqueries.
    if let Some(ref ctes) = plan.ctes {
        for cte in ctes {
            collect_tables_in_plan(&cte.query, tables);
        }
    }
}

/// Build a reverse FK index: for each table X, list all (local_table, local_column, fk)
/// where `local_table` has a FK pointing TO X.
///
/// This lets us answer: "which tables reference `users` via their FK?"
fn build_reverse_fk_index(
    schema: &SchemaSnapshot,
) -> std::collections::HashMap<String, Vec<(String, String, String)>> {
    let mut index: std::collections::HashMap<String, Vec<(String, String, String)>> =
        std::collections::HashMap::new();
    for table in &schema.tables {
        for col in &table.columns {
            if let Some(ref fk) = col.foreign_key {
                index
                    .entry(fk.foreign_table.clone())
                    .or_default()
                    .push((table.name.clone(), col.name.clone(), fk.foreign_column.clone()));
            }
        }
    }
    index
}

/// Find the best way to join `missing_table` into the query using schema FK metadata.
///
/// Strategy (in order of preference):
/// 1. Does the `from` table have a FK column pointing to `missing_table`?
///    → JOIN missing_table ON from_table.fk_column = missing_table.id
/// 2. Does `missing_table` have a FK column pointing to the `from` table?
///    → JOIN missing_table ON from_table.id = missing_table.fk_column
/// 3. Does any already-joined table have a FK pointing to `missing_table`?
///    → JOIN missing_table ON joined_table.fk_column = missing_table.id
/// 4. Does `missing_table` have a FK pointing to any already-joined table?
///    → JOIN missing_table ON joined_table.id = missing_table.fk_column
fn find_join_for(
    missing_table: &str,
    scope: &HashSet<String>,
    schema: &SchemaSnapshot,
) -> Option<JoinClause> {
    let reverse_fk = build_reverse_fk_index(schema);

    // 1. FK from an in-scope table → missing_table (e.g. orders.user_id → users.id)
    for scope_table in scope {
        if let Some(table) = schema.get_table(scope_table) {
            for col in &table.columns {
                if let Some(ref fk) = col.foreign_key {
                    if fk.foreign_table == missing_table {
                        return Some(JoinClause {
                            join_type: JoinType::Inner,
                            right_table: FromClause {
                                table: missing_table.to_owned(),
                                alias: None,
                            },
                            on: Predicate::Comparison {
                                left: Expression::ColumnRef {
                                    table: Some(scope_table.clone()),
                                    column: col.name.clone(),
                                },
                                op: crate::schema::ComparisonOperator::Eq,
                                right: Expression::ColumnRef {
                                    table: Some(missing_table.to_owned()),
                                    column: fk.foreign_column.clone(),
                                },
                            },
                        });
                    }
                }
            }
        }
    }

    // 2. FK from missing_table → an in-scope table (e.g. order_items.order_id → orders.id)
    if let Some(missing_schema) = schema.get_table(missing_table) {
        for col in &missing_schema.columns {
            if let Some(ref fk) = col.foreign_key {
                if scope.contains(&fk.foreign_table) {
                    return Some(JoinClause {
                        join_type: JoinType::Inner,
                        right_table: FromClause {
                            table: missing_table.to_owned(),
                            alias: None,
                        },
                        on: Predicate::Comparison {
                            left: Expression::ColumnRef {
                                table: Some(fk.foreign_table.clone()),
                                column: fk.foreign_column.clone(),
                            },
                            op: crate::schema::ComparisonOperator::Eq,
                            right: Expression::ColumnRef {
                                table: Some(missing_table.to_owned()),
                                column: col.name.clone(),
                            },
                        },
                    });
                }
            }
        }
    }

    // 3. Reverse FK: another table's FK → missing_table (referenced FROM outside)
    if let Some(refs) = reverse_fk.get(missing_table) {
        for (local_table, local_column, fk_column) in refs {
            if scope.contains(local_table) {
                return Some(JoinClause {
                    join_type: JoinType::Inner,
                    right_table: FromClause {
                        table: missing_table.to_owned(),
                        alias: None,
                    },
                    on: Predicate::Comparison {
                        left: Expression::ColumnRef {
                            table: Some(local_table.clone()),
                            column: local_column.clone(),
                        },
                        op: crate::schema::ComparisonOperator::Eq,
                        right: Expression::ColumnRef {
                            table: Some(missing_table.to_owned()),
                            column: fk_column.clone(),
                        },
                    },
                });
            }
        }
    }

    None
}

/// Fix missing JOIN clauses for tables referenced but not joined.
///
/// Scans all table-qualified column references in the plan, finds those
/// whose table is missing from the query scope, and inserts the correct
/// INNER JOIN using actual schema FK metadata.
pub fn fix_missing_joins(plan: &mut QueryPlan, schema: &SchemaSnapshot) -> bool {
    // Collect all referenced tables.
    let mut referenced: HashSet<String> = HashSet::new();
    for proj in &plan.select {
        match proj {
            crate::schema::Projection::Column {
                table: Some(t), ..
            } => {
                referenced.insert(t.clone());
            }
            crate::schema::Projection::Expr { expression, .. } => {
                collect_tables_in_expr(expression, &mut referenced);
            }
            crate::schema::Projection::Star { table: Some(t) } => {
                referenced.insert(t.clone());
            }
            _ => {}
        }
    }
    if let Some(ref pred) = plan.r#where {
        collect_tables_in_pred(pred, &mut referenced);
    }
    if let Some(ref having) = plan.having {
        collect_tables_in_pred(having, &mut referenced);
    }
    if let Some(ref order_by) = plan.order_by {
        for term in order_by {
            collect_tables_in_expr(&term.expr, &mut referenced);
        }
    }
    if let Some(ref group_by) = plan.group_by {
        for expr in group_by {
            collect_tables_in_expr(expr, &mut referenced);
        }
    }

    // Iteratively add joins until no more can be found.
    // This handles bridge-table scenarios: orders → order_items → products.
    let mut changed = false;
    loop {
        let mut scope = scope_tables(plan);
        let missing: Vec<String> = referenced
            .iter()
            .filter(|t| !scope.contains(*t))
            .cloned()
            .collect();

        if missing.is_empty() {
            break;
        }

        let mut found_any = false;
        for table in &missing {
            if let Some(join) = find_join_for(table, &scope, schema) {
                if plan.joins.is_none() {
                    plan.joins = Some(Vec::new());
                }
                plan.joins.as_mut().unwrap().push(join);
                scope.insert(table.clone());
                found_any = true;
                changed = true;
            }
        }

        if !found_any {
            // No more tables can be joined via FK relationships.
            break;
        }
    }

    // Recurse into CTE subqueries.
    if let Some(ref mut ctes) = plan.ctes {
        for cte in ctes.iter_mut() {
            changed |= fix_missing_joins(&mut cte.query, schema);
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        ColumnSchema, DataType, ForeignKey, Projection, SchemaMetadata, TableSchema,
    };

    fn test_schema() -> SchemaSnapshot {
        SchemaSnapshot::new(
            vec![
                TableSchema {
                    name: "orders".to_owned(),
                    columns: vec![
                        ColumnSchema {
                            name: "id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: true,
                            foreign_key: None,
                        },
                        ColumnSchema {
                            name: "user_id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: false,
                            foreign_key: Some(ForeignKey {
                                foreign_table: "users".to_owned(),
                                foreign_column: "id".to_owned(),
                            }),
                        },
                        ColumnSchema {
                            name: "total".to_owned(),
                            data_type: DataType::Float,
                            nullable: false,
                            description: None,
                            is_primary_key: false,
                            foreign_key: None,
                        },
                    ],
                    description: None,
                    primary_key: Some(vec!["id".to_owned()]),
                },
                TableSchema {
                    name: "users".to_owned(),
                    columns: vec![
                        ColumnSchema {
                            name: "id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: true,
                            foreign_key: None,
                        },
                        ColumnSchema {
                            name: "name".to_owned(),
                            data_type: DataType::String,
                            nullable: false,
                            description: None,
                            is_primary_key: false,
                            foreign_key: None,
                        },
                    ],
                    description: None,
                    primary_key: Some(vec!["id".to_owned()]),
                },
                TableSchema {
                    name: "order_items".to_owned(),
                    columns: vec![
                        ColumnSchema {
                            name: "id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: true,
                            foreign_key: None,
                        },
                        ColumnSchema {
                            name: "order_id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: false,
                            foreign_key: Some(ForeignKey {
                                foreign_table: "orders".to_owned(),
                                foreign_column: "id".to_owned(),
                            }),
                        },
                        ColumnSchema {
                            name: "product_id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: false,
                            foreign_key: Some(ForeignKey {
                                foreign_table: "products".to_owned(),
                                foreign_column: "id".to_owned(),
                            }),
                        },
                        ColumnSchema {
                            name: "quantity".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: false,
                            foreign_key: None,
                        },
                    ],
                    description: None,
                    primary_key: Some(vec!["id".to_owned()]),
                },
                TableSchema {
                    name: "products".to_owned(),
                    columns: vec![
                        ColumnSchema {
                            name: "id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: true,
                            foreign_key: None,
                        },
                        ColumnSchema {
                            name: "name".to_owned(),
                            data_type: DataType::String,
                            nullable: false,
                            description: None,
                            is_primary_key: false,
                            foreign_key: None,
                        },
                    ],
                    description: None,
                    primary_key: Some(vec!["id".to_owned()]),
                },
            ],
            SchemaMetadata {
                version: Some("1.0".to_owned()),
                source: Some("test".to_owned()),
                generated_at: None,
            },
        )
    }

    #[test]
    fn adds_missing_users_join() {
        let schema = test_schema();
        let mut plan = QueryPlan {
            select: vec![
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
            ],
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
        };

        assert!(fix_missing_joins(&mut plan, &schema));
        assert!(plan.joins.is_some());
        let joins = plan.joins.as_ref().unwrap();
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].right_table.table, "users");
        // Verify the ON condition uses the correct FK: orders.user_id = users.id
        if let Predicate::Comparison {
            ref left,
            op: _,
            ref right,
        } = joins[0].on
        {
            if let Expression::ColumnRef {
                table: Some(t),
                column: c,
            } = left
            {
                assert_eq!(t, "orders");
                assert_eq!(c, "user_id");
            } else {
                panic!("left should be ColumnRef");
            }
            if let Expression::ColumnRef {
                table: Some(t),
                column: c,
            } = right
            {
                assert_eq!(t, "users");
                assert_eq!(c, "id");
            } else {
                panic!("right should be ColumnRef");
            }
        } else {
            panic!("on should be Comparison");
        }
    }

    #[test]
    fn no_op_when_all_tables_joined() {
        let schema = test_schema();
        let mut plan = QueryPlan {
            select: vec![
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
            ],
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
            joins: Some(vec![JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "users".to_owned(),
                    alias: None,
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "user_id".to_owned(),
                    },
                    op: crate::schema::ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("users".to_owned()),
                        column: "id".to_owned(),
                    },
                },
            }]),
            ctes: None,
            set_operation: None,
        };

        assert!(!fix_missing_joins(&mut plan, &schema));
    }

    #[test]
    fn adds_order_items_join_via_reverse_fk() {
        let schema = test_schema();
        let mut plan = QueryPlan {
            select: vec![
                Projection::Column {
                    table: Some("orders".to_owned()),
                    column: "id".to_owned(),
                    alias: None,
                },
                Projection::Expr {
                    expression: Expression::FunctionCall {
                        name: "sum".to_owned(),
                        args: vec![Expression::ColumnRef {
                            table: Some("order_items".to_owned()),
                            column: "quantity".to_owned(),
                        }],
                        distinct: false,
                    },
                    alias: Some("total_qty".to_owned()),
                },
            ],
            distinct: false,
            distinct_on: None,
            from: FromClause {
                table: "orders".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: Some(vec![Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
            }]),
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
            set_operation: None,
        };

        assert!(fix_missing_joins(&mut plan, &schema));
        let joins = plan.joins.as_ref().unwrap();
        // order_items has FK order_id → orders.id, so the join should be:
        // orders.id = order_items.order_id
        let found = joins.iter().any(|j| j.right_table.table == "order_items");
        assert!(found, "order_items should be joined");
    }
}
