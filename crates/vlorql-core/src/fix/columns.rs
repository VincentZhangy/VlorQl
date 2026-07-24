//! Fix column references that contain arithmetic expressions.
//!
//! Weak LLMs sometimes embed a calculation directly in a column name:
//!   `"column": "unit_price * quantity"`
//! instead of building a proper `binary_op` expression tree.
//!
//! This module detects such patterns and converts them to the canonical
//! `BinaryOp` form.

use crate::schema::expressions::Expression;
use crate::schema::{BinaryOperator, Predicate, Projection, QueryPlan};

/// Attempt to parse a simple arithmetic expression from a column name string.
///
/// Supports patterns like `"unit_price * quantity"`, `"price + tax"`,
/// `"total - discount"`, `"amount / count"`.
///
/// Returns `None` if the string doesn't look like a simple binary operation.
fn parse_arithmetic_column(col: &str) -> Option<(String, BinaryOperator, String)> {
    let col = col.trim();
    // Try each operator in order (longest first to avoid ambiguity).
    let patterns: &[(&str, BinaryOperator)] = &[
        (" * ", BinaryOperator::Mul),
        (" / ", BinaryOperator::Div),
        (" + ", BinaryOperator::Add),
        (" - ", BinaryOperator::Sub),
    ];
    for (sep, op) in patterns {
        if let Some(pos) = col.find(sep) {
            let left = col[..pos].trim().to_owned();
            let right = col[pos + sep.len()..].trim().to_owned();
            if !left.is_empty() && !right.is_empty() {
                return Some((left, op.clone(), right));
            }
        }
    }
    None
}

/// Attempt to parse a SQL function call from a column name string.
///
/// Weak LLMs sometimes embed `EXTRACT(MONTH FROM created_at)` or
/// `TO_CHAR(created_at, 'YYYY-MM')` as a single column name instead
/// of building a proper `function_call` expression tree.
///
/// Returns `None` if the string doesn't look like a function call.
fn parse_sql_function_column(col: &str) -> Option<(String, Vec<String>)> {
    let col = col.trim();
    // Find the first '(' and matching ')'.
    let open = col.find('(')?;
    let close = col.rfind(')')?;
    if open >= close {
        return None;
    }
    let name = col[..open].trim().to_owned();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        return None;
    }
    let args_str = &col[open + 1..close];
    // Split by comma, handling the EXTRACT-specific `x FROM y` pattern.
    // If the arguments contain " FROM ", the part before it is a keyword
    // (e.g. MONTH) and after it is a column reference.
    let mut args: Vec<String> = Vec::new();
    for part in args_str.split(',') {
        let part = part.trim();
        if !part.is_empty() {
            // EXTRACT: "MONTH FROM created_at" → two args: "month" and "created_at"
            if let Some(from_pos) = part.to_uppercase().find(" FROM ") {
                let keyword = part[..from_pos].trim().to_lowercase();
                let column = part[from_pos + 6..].trim().to_owned();
                if !keyword.is_empty() && !column.is_empty() {
                    args.push(keyword);
                    args.push(column);
                    continue;
                }
            }
            args.push(part.to_owned());
        }
    }
    if args.is_empty() {
        return None;
    }
    Some((name, args))
}

/// Create a ColumnRef expression.
fn col_ref(table: Option<String>, column: &str) -> Expression {
    Expression::ColumnRef {
        table,
        column: column.to_owned(),
    }
}

/// Convert a column name argument (from a SQL function string) into an
/// Expression.  If it looks like a bare column name, treat it as a
/// `ColumnRef`; otherwise treat it as a `Literal`.
fn arg_to_expr(table: Option<&str>, arg: &str) -> Expression {
    let arg = arg.trim();
    // If quoted with single quotes → literal string.
    if (arg.starts_with('\'') && arg.ends_with('\''))
        || (arg.starts_with('"') && arg.ends_with('"'))
    {
        let inner = &arg[1..arg.len() - 1];
        return Expression::Literal {
            value: serde_json::Value::String(inner.to_owned()),
            data_type: crate::schema::DataType::String,
        };
    }
    // If it looks like a SQL keyword (all uppercase or common lowercase
    // datepart like "month", "year", "day", "hour", etc.) → literal.
    if arg.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        || SQL_KEYWORDS.contains(&arg.to_lowercase().as_str())
    {
        return Expression::Literal {
            value: serde_json::Value::String(arg.to_lowercase()),
            data_type: crate::schema::DataType::String,
        };
    }
    // If it looks like a number → literal.
    if let Ok(n) = arg.parse::<i64>() {
        return Expression::Literal {
            value: serde_json::json!(n),
            data_type: crate::schema::DataType::Int,
        };
    }
    // Otherwise treat as a column reference.
    Expression::ColumnRef {
        table: table.map(|t| t.to_owned()),
        column: arg.to_owned(),
    }
}

/// Common SQL keywords that appear as arguments in EXTRACT(), DATE_TRUNC(),
/// TO_CHAR() and similar date/time functions.
const SQL_KEYWORDS: &[&str] = &[
    "month", "year", "day", "hour", "minute", "second",
    "quarter", "week", "dow", "doy", "epoch", "decade",
    "century", "millennium", "millisecond", "microsecond",
    "yyyy", "mm", "dd", "hh", "mi", "ss",
];

/// Recursively fix column references in an expression tree.
/// Tries both arithmetic and SQL function patterns.
fn fix_expr(expr: &mut Expression) -> bool {
    match expr {
        Expression::ColumnRef {
            table,
            column,
        } if !column.is_empty() && parse_arithmetic_column(column).is_some() => {
            let (left, op, right) = parse_arithmetic_column(column).unwrap();
            let new_expr = Expression::BinaryOp {
                left: Box::new(col_ref(table.clone(), &left)),
                op,
                right: Box::new(col_ref(table.clone(), &right)),
            };
            *expr = new_expr;
            true
        }
        Expression::ColumnRef {
            table,
            column,
        } if !column.is_empty() && parse_sql_function_column(column).is_some() => {
            let table_str = table.as_deref();
            let (name, args) = parse_sql_function_column(column).unwrap();
            let expr_args: Vec<Expression> = args
                .iter()
                .map(|a| arg_to_expr(table_str, a))
                .collect();
            let new_expr = Expression::FunctionCall {
                name: name.to_lowercase(),
                args: expr_args,
                distinct: false,
            };
            *expr = new_expr;
            true
        }
        Expression::ColumnRef { .. } => false,
        Expression::FunctionCall { args, .. } => {
            let mut changed = false;
            for arg in args.iter_mut() {
                changed |= fix_expr(arg);
            }
            changed
        }
        Expression::BinaryOp { left, right, .. } => {
            fix_expr(left) | fix_expr(right)
        }
        Expression::Case {
            operand,
            when_thens,
            else_result,
        } => {
            let mut changed = false;
            if let Some(op) = operand {
                changed |= fix_expr(op);
            }
            for wt in when_thens.iter_mut() {
                changed |= fix_expr(&mut wt.when);
                changed |= fix_expr(&mut wt.then);
            }
            if let Some(els) = else_result {
                changed |= fix_expr(els);
            }
            changed
        }
        Expression::SubQuery { query } => fix_plan(query),
        Expression::WindowFunction { args, .. } => {
            let mut changed = false;
            for arg in args.iter_mut() {
                changed |= fix_expr(arg);
            }
            changed
        }
        Expression::Literal { .. } | Expression::Star => false,
    }
}

/// Recursively fix arithmetic column references in a predicate.
fn fix_pred(pred: &mut Predicate) -> bool {
    match pred {
        Predicate::Comparison { left, right, .. } => fix_expr(left) | fix_expr(right),
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            fix_pred(left) | fix_pred(right)
        }
        Predicate::Not { child } => fix_pred(child),
        Predicate::Between { expr, low, high } => {
            fix_expr(expr) | fix_expr(low) | fix_expr(high)
        }
        Predicate::In { expr, target } => {
            let mut changed = fix_expr(expr);
            if let crate::schema::InTarget::SubQuery(query) = target {
                changed |= fix_plan(query);
            }
            changed
        }
        Predicate::Like { expr, .. } => fix_expr(expr),
        Predicate::IsNull { expr } => fix_expr(expr),
        Predicate::Exists { query } => fix_plan(query),
    }
}

/// Fix arithmetic column references in a projection.
fn fix_proj(proj: &mut Projection) -> bool {
    match proj {
        Projection::Expr { expression, .. } => fix_expr(expression),
        Projection::Column {
            table,
            column,
            alias,
        } => {
            if column.is_empty() {
                return false;
            }
            // Try arithmetic pattern first (e.g. "quantity * unit_price").
            if let Some((left, op, right)) = parse_arithmetic_column(column) {
                let new_expr = Expression::BinaryOp {
                    left: Box::new(col_ref(table.clone(), &left)),
                    op,
                    right: Box::new(col_ref(table.clone(), &right)),
                };
                let old_alias = alias.take();
                *proj = Projection::Expr {
                    expression: new_expr,
                    alias: old_alias,
                };
                return true;
            }
            // Try SQL function pattern (e.g. "EXTRACT(MONTH FROM created_at)").
            if let Some((name, args)) = parse_sql_function_column(column) {
                let table_str = table.as_deref();
                let expr_args: Vec<Expression> = args
                    .iter()
                    .map(|a| arg_to_expr(table_str, a))
                    .collect();
                let new_expr = Expression::FunctionCall {
                    name: name.to_lowercase(),
                    args: expr_args,
                    distinct: false,
                };
                let old_alias = alias.take();
                *proj = Projection::Expr {
                    expression: new_expr,
                    alias: old_alias,
                };
                return true;
            }
            false
        }
        Projection::Star { .. } => false,
    }
}

/// Fix arithmetic column references in a whole plan.
fn fix_plan(plan: &mut QueryPlan) -> bool {
    let mut changed = false;
    for proj in plan.select.iter_mut() {
        changed |= fix_proj(proj);
    }
    if let Some(ref mut pred) = plan.r#where {
        changed |= fix_pred(pred);
    }
    if let Some(ref mut having) = plan.having {
        changed |= fix_pred(having);
    }
    if let Some(ref mut group_by) = plan.group_by {
        for expr in group_by.iter_mut() {
            changed |= fix_expr(expr);
        }
    }
    if let Some(ref mut order_by) = plan.order_by {
        for term in order_by.iter_mut() {
            changed |= fix_expr(&mut term.expr);
        }
    }
    if let Some(ref mut joins) = plan.joins {
        for join in joins.iter_mut() {
            changed |= fix_pred(&mut join.on);
        }
    }
    if let Some(ref mut ctes) = plan.ctes {
        for cte in ctes.iter_mut() {
            changed |= fix_plan(&mut cte.query);
        }
    }
    if let Some(ref mut set_op) = plan.set_operation {
        changed |= fix_plan(&mut set_op.right);
    }
    changed
}

/// Detect and fix column references that contain arithmetic expressions.
///
/// Transforms `ColumnRef { column: "unit_price * quantity" }` into
/// `BinaryOp { left: ColumnRef("unit_price"), op: Mul, right: ColumnRef("quantity") }`.
///
/// This runs BEFORE the schema-aware fix (join/aggregate/group_by fixes)
/// so the resulting BinaryOp is available for downstream analysis.
pub fn fix_arithmetic_column_refs(plan: &mut QueryPlan) -> bool {
    fix_plan(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mul() {
        let (left, op, right) = parse_arithmetic_column("unit_price * quantity").unwrap();
        assert_eq!(left, "unit_price");
        assert_eq!(op, BinaryOperator::Mul);
        assert_eq!(right, "quantity");
    }

    #[test]
    fn parses_div() {
        let (left, op, right) = parse_arithmetic_column("total / count").unwrap();
        assert_eq!(left, "total");
        assert_eq!(op, BinaryOperator::Div);
        assert_eq!(right, "count");
    }

    #[test]
    fn parses_add() {
        let (left, op, right) = parse_arithmetic_column("price + tax").unwrap();
        assert_eq!(left, "price");
        assert_eq!(op, BinaryOperator::Add);
        assert_eq!(right, "tax");
    }

    #[test]
    fn parses_sub() {
        let (left, op, right) = parse_arithmetic_column("total - discount").unwrap();
        assert_eq!(left, "total");
        assert_eq!(op, BinaryOperator::Sub);
        assert_eq!(right, "discount");
    }

    #[test]
    fn no_match_without_spaces() {
        assert!(parse_arithmetic_column("unit_price*quantity").is_none());
    }

    #[test]
    fn no_match_single_word() {
        assert!(parse_arithmetic_column("unit_price").is_none());
    }

    #[test]
    fn no_match_empty() {
        assert!(parse_arithmetic_column("").is_none());
    }

    #[test]
    fn fixes_column_ref_in_select() {
        let mut plan = QueryPlan {
            select: vec![
                Projection::Column {
                    table: Some("products".to_owned()),
                    column: "name".to_owned(),
                    alias: Some("product".to_owned()),
                },
                Projection::Expr {
                    expression: Expression::FunctionCall {
                        name: "sum".to_owned(),
                        args: vec![Expression::ColumnRef {
                            table: Some("order_items".to_owned()),
                            column: "unit_price * quantity".to_owned(),
                        }],
                        distinct: false,
                    },
                    alias: Some("total_sold".to_owned()),
                },
            ],
            distinct: false,
            distinct_on: None,
            from: crate::schema::FromClause {
                table: "products".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: Some(vec![Expression::ColumnRef {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
            }]),
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
            set_operation: None,
        };

        assert!(fix_arithmetic_column_refs(&mut plan));

        // The sum's arg should now be a BinaryOp, not a ColumnRef.
        if let Projection::Expr { expression, .. } = &plan.select[1] {
            if let Expression::FunctionCall { args, .. } = expression {
                assert_eq!(args.len(), 1);
                match &args[0] {
                    Expression::BinaryOp { left, op, right } => {
                        assert_eq!(*op, BinaryOperator::Mul);
                        if let Expression::ColumnRef {
                            table: Some(t),
                            column: c,
                        } = left.as_ref()
                        {
                            assert_eq!(t, "order_items");
                            assert_eq!(c, "unit_price");
                        } else {
                            panic!("left should be ColumnRef");
                        }
                        if let Expression::ColumnRef {
                            table: Some(t),
                            column: c,
                        } = right.as_ref()
                        {
                            assert_eq!(t, "order_items");
                            assert_eq!(c, "quantity");
                        } else {
                            panic!("right should be ColumnRef");
                        }
                    }
                    _ => panic!("expected BinaryOp"),
                }
            } else {
                panic!("expected FunctionCall");
            }
        } else {
            panic!("expected Expr projection");
        }
    }

    #[test]
    fn no_change_when_column_name_valid() {
        let mut plan = QueryPlan {
            select: vec![Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: None,
            }],
            distinct: false,
            distinct_on: None,
            from: crate::schema::FromClause {
                table: "products".to_owned(),
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

        assert!(!fix_arithmetic_column_refs(&mut plan));
    }
}
