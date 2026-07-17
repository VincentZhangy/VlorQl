//! Constant folding: statically evaluate constant sub-expressions.
//!
//! [`ConstantFolding`] walks every expression in a plan and replaces
//! any [`Expression::BinaryOp`] whose operands are both literals with
//! the computed [`Expression::Literal`]. Because predicates embed
//! expressions (`age > 20 + 5`), folding the operands also simplifies
//! the comparisons the planner and cost model see (`age > 25`).
//!
//! The rewrite is intentionally conservative: it folds only when it can
//! compute an exact, type-preserving result. Anything it is unsure
//! about (mixed types, division by zero, string operands, `NULL`) is
//! left untouched so the rewrite never changes query semantics.

use serde_json::{json, Value};

use crate::errors::VlorQLError;
use crate::schema::{
    BinaryOperator, CommonTableExpression, Expression, OrderByTerm, Predicate, Projection,
    QueryPlan,
};

use super::rules::PlanRewriter;

/// Folds constant expressions such as `1 + 2` into a single literal.
///
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::{ConstantFolding, PlanRewriter};
/// use vlorql_core::schema::{
///     BinaryOperator, DataType, Expression, FromClause, Projection, QueryPlan,
/// };
///
/// let plan = QueryPlan {
///     select: vec![Projection::Expr {
///         expression: Expression::BinaryOp {
///             left: Box::new(Expression::Literal { value: 1.into(), data_type: DataType::Int }),
///             op: BinaryOperator::Add,
///             right: Box::new(Expression::Literal { value: 2.into(), data_type: DataType::Int }),
///         },
///         alias: Some("three".to_owned()),
///     }],
///     from: FromClause { table: "t".to_owned(), alias: None },
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None, joins: None, ctes: None,
/// };
///
/// let folded = ConstantFolding.rewrite(&plan).unwrap();
/// assert_eq!(
///     folded.select[0],
///     Projection::Expr {
///         expression: Expression::Literal { value: 3.into(), data_type: DataType::Int },
///         alias: Some("three".to_owned()),
///     },
/// );
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ConstantFolding;

impl PlanRewriter for ConstantFolding {
    fn rewrite(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        Ok(fold_plan(plan))
    }
}

/// Recursively folds every expression and predicate in `plan`,
/// including nested CTE queries.
fn fold_plan(plan: &QueryPlan) -> QueryPlan {
    QueryPlan {
        select: plan.select.iter().map(fold_projection).collect(),
        from: plan.from.clone(),
        r#where: plan.r#where.as_ref().map(fold_predicate),
        group_by: plan
            .group_by
            .as_ref()
            .map(|exprs| exprs.iter().map(fold_expression).collect()),
        having: plan.having.as_ref().map(fold_predicate),
        order_by: plan.order_by.as_ref().map(|terms| {
            terms
                .iter()
                .map(|term| OrderByTerm {
                    expr: fold_expression(&term.expr),
                    descending: term.descending,
                })
                .collect()
        }),
        limit: plan.limit,
        offset: plan.offset,
        joins: plan.joins.as_ref().map(|joins| {
            joins
                .iter()
                .map(|join| crate::schema::JoinClause {
                    join_type: join.join_type,
                    right_table: join.right_table.clone(),
                    on: fold_predicate(&join.on),
                })
                .collect()
        }),
        ctes: plan.ctes.as_ref().map(|ctes| {
            ctes.iter()
                .map(|cte| CommonTableExpression {
                    name: cte.name.clone(),
                    query: Box::new(fold_plan(&cte.query)),
                })
                .collect()
        }),
    }
}

fn fold_projection(projection: &Projection) -> Projection {
    match projection {
        Projection::Expr { expression, alias } => Projection::Expr {
            expression: fold_expression(expression),
            alias: alias.clone(),
        },
        other => other.clone(),
    }
}

/// Folds an expression bottom-up, collapsing constant `BinaryOp` nodes.
fn fold_expression(expr: &Expression) -> Expression {
    match expr {
        Expression::BinaryOp { left, op, right } => {
            let left = fold_expression(left);
            let right = fold_expression(right);

            if let (
                Expression::Literal {
                    value: left_value, ..
                },
                Expression::Literal {
                    value: right_value, ..
                },
            ) = (&left, &right)
            {
                if let Some(folded) = fold_binary(left_value, *op, right_value) {
                    return folded;
                }
            }

            Expression::BinaryOp {
                left: Box::new(left),
                op: *op,
                right: Box::new(right),
            }
        }
        Expression::FunctionCall {
            name,
            args,
            distinct,
        } => Expression::FunctionCall {
            name: name.clone(),
            args: args.iter().map(fold_expression).collect(),
            distinct: *distinct,
        },
        // Literals and column references have nothing to fold.
        other => other.clone(),
    }
}

/// Folds a predicate, folding the expressions it contains and recursing
/// through boolean structure. The predicate *shape* is preserved.
fn fold_predicate(pred: &Predicate) -> Predicate {
    match pred {
        Predicate::Comparison { left, op, right } => Predicate::Comparison {
            left: fold_expression(left),
            op: *op,
            right: fold_expression(right),
        },
        Predicate::And { left, right } => Predicate::And {
            left: Box::new(fold_predicate(left)),
            right: Box::new(fold_predicate(right)),
        },
        Predicate::Or { left, right } => Predicate::Or {
            left: Box::new(fold_predicate(left)),
            right: Box::new(fold_predicate(right)),
        },
        Predicate::Not { child } => Predicate::Not {
            child: Box::new(fold_predicate(child)),
        },
        Predicate::Between { expr, low, high } => Predicate::Between {
            expr: fold_expression(expr),
            low: fold_expression(low),
            high: fold_expression(high),
        },
        Predicate::In { expr, values } => Predicate::In {
            expr: fold_expression(expr),
            values: values.iter().map(fold_expression).collect(),
        },
        Predicate::Like { expr, pattern } => Predicate::Like {
            expr: fold_expression(expr),
            pattern: pattern.clone(),
        },
        Predicate::IsNull { expr } => Predicate::IsNull {
            expr: fold_expression(expr),
        },
    }
}

/// Evaluates a binary operation over two literal JSON values, returning
/// the folded literal or `None` when the result cannot be computed
/// exactly (and the original expression should be kept).
fn fold_binary(left: &Value, op: BinaryOperator, right: &Value) -> Option<Expression> {
    use BinaryOperator::*;

    match op {
        Add | Sub | Mul | Div | Mod => fold_arithmetic(left, op, right),
        And | Or => fold_boolean(left, op, right),
        Eq | Neq | Gt | Lt | Gte | Lte => fold_comparison(left, op, right),
        // Pattern-match operators are not folded (require SQL semantics).
        Like | ILike => None,
    }
}

fn fold_arithmetic(left: &Value, op: BinaryOperator, right: &Value) -> Option<Expression> {
    let (left, right) = (left.as_f64()?, right.as_f64()?);
    let (result, both_ints) = (
        match op {
            BinaryOperator::Add => left + right,
            BinaryOperator::Sub => left - right,
            BinaryOperator::Mul => left * right,
            // Guard against division by zero rather than emitting NaN/inf.
            BinaryOperator::Div if right != 0.0 => left / right,
            BinaryOperator::Mod if right != 0.0 => left % right,
            _ => return None,
        },
        is_integral(left) && is_integral(right),
    );

    // Preserve integer-ness: integer operands that yield an integral
    // result fold back to an integer literal, matching SQL expectations
    // for e.g. `1 + 2`. Division always stays floating unless exact.
    if both_ints && is_integral(result) {
        Some(int_literal(result as i64))
    } else {
        Some(float_literal(result))
    }
}

fn fold_boolean(left: &Value, op: BinaryOperator, right: &Value) -> Option<Expression> {
    let (left, right) = (left.as_bool()?, right.as_bool()?);
    let result = match op {
        BinaryOperator::And => left && right,
        BinaryOperator::Or => left || right,
        _ => return None,
    };
    Some(bool_literal(result))
}

fn fold_comparison(left: &Value, op: BinaryOperator, right: &Value) -> Option<Expression> {
    let ordering = compare_values(left, right)?;
    let result = match op {
        BinaryOperator::Eq => ordering.is_eq(),
        BinaryOperator::Neq => ordering.is_ne(),
        BinaryOperator::Gt => ordering.is_gt(),
        BinaryOperator::Lt => ordering.is_lt(),
        BinaryOperator::Gte => ordering.is_ge(),
        BinaryOperator::Lte => ordering.is_le(),
        _ => return None,
    };
    Some(bool_literal(result))
}

/// Orders two JSON scalars of the same kind. Returns `None` for mixed
/// or unorderable kinds so the caller leaves the expression alone.
fn compare_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Number(_), Value::Number(_)) => {
            left.as_f64()?.partial_cmp(&right.as_f64()?)
        }
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

fn is_integral(value: f64) -> bool {
    value.fract() == 0.0 && value.is_finite()
}

fn int_literal(value: i64) -> Expression {
    Expression::Literal {
        value: json!(value),
        data_type: crate::schema::DataType::Int,
    }
}

fn float_literal(value: f64) -> Expression {
    Expression::Literal {
        value: json!(value),
        data_type: crate::schema::DataType::Float,
    }
}

fn bool_literal(value: bool) -> Expression {
    Expression::Literal {
        value: json!(value),
        data_type: crate::schema::DataType::Boolean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ComparisonOperator, DataType, FromClause};

    fn lit_int(value: i64) -> Expression {
        Expression::Literal {
            value: json!(value),
            data_type: DataType::Int,
        }
    }

    fn column(column: &str) -> Expression {
        Expression::ColumnRef {
            table: None,
            column: column.to_owned(),
        }
    }

    fn binop(left: Expression, op: BinaryOperator, right: Expression) -> Expression {
        Expression::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }

    #[test]
    fn folds_integer_addition() {
        let folded = fold_expression(&binop(lit_int(1), BinaryOperator::Add, lit_int(2)));
        assert_eq!(folded, lit_int(3));
    }

    #[test]
    fn folds_nested_arithmetic() {
        // 2 * (3 + 4) => 14
        let inner = binop(lit_int(3), BinaryOperator::Add, lit_int(4));
        let folded = fold_expression(&binop(lit_int(2), BinaryOperator::Mul, inner));
        assert_eq!(folded, lit_int(14));
    }

    #[test]
    fn folds_constant_side_of_predicate() {
        // age > 20 + 5  =>  age > 25
        let pred = Predicate::Comparison {
            left: column("age"),
            op: ComparisonOperator::Gt,
            right: binop(lit_int(20), BinaryOperator::Add, lit_int(5)),
        };
        let folded = fold_predicate(&pred);
        assert_eq!(
            folded,
            Predicate::Comparison {
                left: column("age"),
                op: ComparisonOperator::Gt,
                right: lit_int(25),
            }
        );
    }

    #[test]
    fn does_not_fold_column_operands() {
        let expr = binop(column("a"), BinaryOperator::Add, lit_int(1));
        assert_eq!(fold_expression(&expr), expr);
    }

    #[test]
    fn does_not_fold_division_by_zero() {
        let expr = binop(lit_int(1), BinaryOperator::Div, lit_int(0));
        assert_eq!(fold_expression(&expr), expr);
    }

    #[test]
    fn division_stays_float_when_inexact() {
        // 1 / 2 => 0.5 (float literal, not truncated to 0)
        let folded = fold_expression(&binop(lit_int(1), BinaryOperator::Div, lit_int(2)));
        assert_eq!(
            folded,
            Expression::Literal {
                value: json!(0.5),
                data_type: DataType::Float,
            }
        );
    }

    #[test]
    fn folds_constant_boolean_comparison() {
        // 20 + 5 = 25 => true
        let expr = binop(
            binop(lit_int(20), BinaryOperator::Add, lit_int(5)),
            BinaryOperator::Eq,
            lit_int(25),
        );
        assert_eq!(fold_expression(&expr), bool_literal(true));
    }

    #[test]
    fn rewrite_folds_projection_and_leaves_plan_shape() {
        let plan = QueryPlan {
            select: vec![Projection::Expr {
                expression: binop(lit_int(6), BinaryOperator::Mul, lit_int(7)),
                alias: Some("answer".to_owned()),
            }],
            from: FromClause {
                table: "t".to_owned(),
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
        };
        let folded = ConstantFolding.rewrite(&plan).unwrap();
        assert_eq!(
            folded.select[0],
            Projection::Expr {
                expression: lit_int(42),
                alias: Some("answer".to_owned()),
            }
        );
    }
}
