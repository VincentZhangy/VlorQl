//! Fix column references that point to the wrong table via FK relationships.
//!
//! When the LLM writes `order_items.name` but `name` does not exist on
//! `order_items`, this module checks FK relationships to find the correct
//! table.  If `order_items.product_id → products.id` and `products.name`
//! exists, the reference is rewritten to `products.name`.
//!
//! This must run BEFORE `fix_missing_joins` so the rewritten table reference
//! triggers a join to the corrected table.

use crate::schema::{
    FromClause, JoinClause, JoinType, Predicate, Projection, QueryPlan, SchemaSnapshot,
    expressions::Expression,
};

/// Try to find `column` on a table related to `table_name` via FK.
///
/// Returns `Some(replacement_table_name)` if found.
fn find_column_via_fk(
    table_name: &str,
    column: &str,
    schema: &SchemaSnapshot,
) -> Option<String> {
    let table = schema.get_table(table_name)?;
    // Collect all FK columns from this table.
    let fks: Vec<(&str, &str)> = table
        .columns
        .iter()
        .filter_map(|col| {
            col.foreign_key
                .as_ref()
                .map(|fk| (col.name.as_str(), fk.foreign_table.as_str()))
        })
        .collect();
    // For each FK, check if the target table has the desired column.
    for (_local_fk_col, foreign_table) in &fks {
        if let Some(ft) = schema.get_table(foreign_table) {
            if ft.columns.iter().any(|c| c.name == column) {
                return Some(ft.name.clone());
            }
        }
    }
    // Reverse FK lookup: check if any other table has a FK pointing TO this
    // table AND has the desired column.  This handles LLM mistakes where a
    // column from the FK-referencing table is incorrectly qualified with
    // the FK-target table name (e.g. `users.user_id` instead of `orders.user_id`
    // when `orders.user_id → users.id`).
    for t in &schema.tables {
        for col in &t.columns {
            if let Some(fk) = &col.foreign_key {
                if fk.foreign_table == table_name && t.columns.iter().any(|c| c.name == column) {
                    return Some(t.name.clone());
                }
            }
        }
    }
    None
}

/// Recursively fix column references in an expression tree.
fn fix_expr(expr: &mut Expression, schema: &SchemaSnapshot) -> bool {
    match expr {
        Expression::ColumnRef {
            table: Some(table_name),
            column,
        } => {
            // Check if the column actually exists on this table.
            let exists = schema
                .get_table(table_name)
                .is_some_and(|t| t.columns.iter().any(|c| c.name == *column));
            if !exists {
                if let Some(new_table) = find_column_via_fk(table_name, column, schema) {
                    *table_name = new_table;
                    return true;
                }
            }
            false
        }
        Expression::ColumnRef { .. } => false,
        Expression::FunctionCall { args, .. } => {
            let mut changed = false;
            for arg in args.iter_mut() {
                changed |= fix_expr(arg, schema);
            }
            changed
        }
        Expression::BinaryOp { left, right, .. } => {
            fix_expr(left, schema) | fix_expr(right, schema)
        }
        Expression::Case {
            operand,
            when_thens,
            else_result,
        } => {
            let mut changed = false;
            if let Some(op) = operand {
                changed |= fix_expr(op, schema);
            }
            for wt in when_thens.iter_mut() {
                changed |= fix_expr(&mut wt.when, schema);
                changed |= fix_expr(&mut wt.then, schema);
            }
            if let Some(els) = else_result {
                changed |= fix_expr(els, schema);
            }
            changed
        }
        Expression::SubQuery { query } => fix_plan(query, schema),
        Expression::WindowFunction { args, .. } => {
            let mut changed = false;
            for arg in args.iter_mut() {
                changed |= fix_expr(arg, schema);
            }
            changed
        }
        Expression::Literal { .. } | Expression::Star => false,
    }
}

/// Recursively fix column references in a predicate.
fn fix_pred(pred: &mut Predicate, schema: &SchemaSnapshot) -> bool {
    match pred {
        Predicate::Comparison { left, right, .. } => {
            fix_expr(left, schema) | fix_expr(right, schema)
        }
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            fix_pred(left, schema) | fix_pred(right, schema)
        }
        Predicate::Not { child } => fix_pred(child, schema),
        Predicate::Between { expr, low, high } => {
            fix_expr(expr, schema) | fix_expr(low, schema) | fix_expr(high, schema)
        }
        Predicate::In { expr, target } => {
            let mut changed = fix_expr(expr, schema);
            if let crate::schema::InTarget::SubQuery(query) = target {
                changed |= fix_plan(query, schema);
            }
            changed
        }
        Predicate::Like { expr, .. } => fix_expr(expr, schema),
        Predicate::IsNull { expr } => fix_expr(expr, schema),
        Predicate::Exists { query } => fix_plan(query, schema),
    }
}

/// Recursively fix table references in a whole plan.
fn fix_plan(plan: &mut QueryPlan, schema: &SchemaSnapshot) -> bool {
    let mut changed = false;
    for proj in plan.select.iter_mut() {
        changed |= match proj {
            Projection::Expr { expression, .. } => fix_expr(expression, schema),
            Projection::Column {
                table,
                column,
                alias: _,
            } => {
                if let Some(table_name) = table {
                    let exists = schema
                        .get_table(table_name)
                        .is_some_and(|t| t.columns.iter().any(|c| c.name == *column));
                    if !exists {
                        if let Some(new_table) =
                            find_column_via_fk(table_name, column, schema)
                        {
                            *table_name = new_table;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            Projection::Star { .. } => false,
        };
    }
    if let Some(ref mut pred) = plan.r#where {
        changed |= fix_pred(pred, schema);
    }
    if let Some(ref mut having) = plan.having {
        changed |= fix_pred(having, schema);
    }
    if let Some(ref mut group_by) = plan.group_by {
        for expr in group_by.iter_mut() {
            changed |= fix_expr(expr, schema);
        }
        // After fixing table qualifiers, remove any GroupBy expressions that
        // are still qualified ColumnRefs whose column doesn't exist in the
        // schema at all — these are LLM hallucinations (e.g. `orders.category`
        // when `category` is only a SELECT alias, not a real column).
        let before = group_by.len();
        group_by.retain(|expr| {
            if let Expression::ColumnRef {
                table: Some(table_name),
                column,
            } = expr
            {
                let exists = schema
                    .get_table(table_name)
                    .is_some_and(|t| t.columns.iter().any(|c| c.name == *column));
                if exists {
                    return true;
                }
                // Also check reverse FK — maybe the column exists on a
                // related table (redundant with fix_expr above, but keeps
                // the guard self-contained).
                let via_fk = find_column_via_fk(table_name, column, schema).is_some();
                via_fk
            } else {
                true
            }
        });
        if group_by.len() != before {
            changed = true;
        }
    }
    if let Some(ref mut order_by) = plan.order_by {
        for term in order_by.iter_mut() {
            changed |= fix_expr(&mut term.expr, schema);
        }
    }
    if let Some(ref mut joins) = plan.joins {
        for join in joins.iter_mut() {
            changed |= fix_pred(&mut join.on, schema);
        }
    }
    if let Some(ref mut ctes) = plan.ctes {
        for cte in ctes.iter_mut() {
            changed |= fix_plan(&mut cte.query, schema);
        }
    }
    if let Some(ref mut set_op) = plan.set_operation {
        changed |= fix_plan(&mut set_op.right, schema);
    }
    changed
}

/// Fix column references that point to the wrong table via FK lookup.
///
/// When a plan references `order_items.name` but `name` only exists on
/// `products` (reachable via `order_items.product_id → products.id`),
/// rewrites the reference to `products.name`.
pub fn fix_column_table(plan: &mut QueryPlan, schema: &SchemaSnapshot) -> bool {
    fix_plan(plan, schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        ColumnSchema, DataType, ForeignKey, SchemaMetadata, TableSchema,
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
    fn fixes_order_items_name_to_products_name() {
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
                        name: "string_agg".to_owned(),
                        args: vec![Expression::ColumnRef {
                            table: Some("order_items".to_owned()),
                            column: "name".to_owned(),
                        }],
                        distinct: false,
                    },
                    alias: Some("product_names".to_owned()),
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

        assert!(fix_column_table(&mut plan, &schema));

        // The string_agg arg should now reference products.name
        if let Projection::Expr { expression, .. } = &plan.select[1] {
            if let Expression::FunctionCall { args, .. } = expression {
                assert_eq!(args.len(), 1);
                match &args[0] {
                    Expression::ColumnRef {
                        table: Some(t),
                        column: c,
                    } => {
                        assert_eq!(t, "products");
                        assert_eq!(c, "name");
                    }
                    _ => panic!("expected ColumnRef after fix"),
                }
            } else {
                panic!("expected FunctionCall");
            }
        } else {
            panic!("expected Expr projection");
        }
    }

    #[test]
    fn leaves_valid_column_refs_unchanged() {
        let schema = test_schema();
        let mut plan = QueryPlan {
            select: vec![Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            }],
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

        assert!(!fix_column_table(&mut plan, &schema));
    }

    #[test]
    fn no_change_when_column_not_found_anywhere() {
        let schema = test_schema();
        let mut plan = QueryPlan {
            select: vec![Projection::Column {
                table: Some("order_items".to_owned()),
                column: "nonexistent".to_owned(),
                alias: None,
            }],
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

        assert!(!fix_column_table(&mut plan, &schema));
    }
}
