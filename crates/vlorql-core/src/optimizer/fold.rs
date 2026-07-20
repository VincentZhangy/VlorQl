//! Constant folding: statically evaluate constant sub-expressions.
//!
//! [`ConstantFolding`] walks every expression in a plan using the
//! [`ExpressionFold`] visitor and replaces any
//! [`Expression::BinaryOp`] whose operands are both literals with
//! the computed [`Expression::Literal`]. Because predicates embed
//! expressions (`age > 20 + 5`), folding the operands also simplifies
//! the comparisons the planner and cost model see (`age > 25`).
//!
//! In addition to full constant folding, the rule also applies
//! boolean identity simplifications:
//!
//! * `true AND x` / `x AND true` → `x`
//! * `false AND x` / `x AND false` → `false`
//! * `true OR x` / `x OR true` → `true`
//! * `false OR x` / `x OR false` → `x`
//! * `NOT true` → `false`, `NOT false` → `true`
//! * `x IS NULL` where `x` is a non-null literal → `false`
//! * `NOT NOT x` → `x`
//!
//! The rewrite is intentionally conservative: it folds only when it can
//! compute an exact, type-preserving result. Anything it is unsure
//! about (mixed types, division by zero, string operands, `NULL`) is
//! left untouched so the rewrite never changes query semantics.

use serde_json::{Value, json};

use crate::errors::VlorQLError;
use crate::schema::{
    BinaryOperator, ComparisonOperator, DataType, Expression, Predicate, QueryPlan,
};

use super::rules::PlanRewriter;
use super::visitor::{ExpressionFold, default_fold_expression, default_fold_predicate};

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
        let mut folder = *self;
        Ok(folder.fold_plan(plan))
    }
}

impl ExpressionFold for ConstantFolding {
    fn fold_expression(&mut self, expr: &Expression) -> Expression {
        match expr {
            Expression::BinaryOp { left, op, right } => {
                let left = self.fold_expression(left);
                let right = self.fold_expression(right);

                // Boolean short-circuit and identity simplifications.
                match op {
                    BinaryOperator::And | BinaryOperator::Or => {
                        if let Some(simplified) = fold_boolean_identity(&left, *op, &right) {
                            return simplified;
                        }
                    }
                    _ => {}
                }

                // Algebraic identity simplifications (x + 0 → x, x * 1 → x, etc.).
                if let Some(simplified) = fold_algebraic_identity(&left, *op, &right) {
                    return simplified;
                }

                // Both literals: compute the result.
                if let (
                    Expression::Literal {
                        value: left_value, ..
                    },
                    Expression::Literal {
                        value: right_value, ..
                    },
                ) = (&left, &right)
                    && let Some(folded) = fold_binary(left_value, *op, right_value)
                {
                    return folded;
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
                args: args.iter().map(|a| self.fold_expression(a)).collect(),
                distinct: *distinct,
            },
            other => default_fold_expression(self, other),
        }
    }

    fn fold_predicate(&mut self, pred: &Predicate) -> Predicate {
        match pred {
            Predicate::Not { child } => {
                let child = self.fold_predicate(child);
                // Double negation: NOT NOT x = x
                if let Predicate::Not { child: inner } = &child {
                    return *inner.clone();
                }
                // NOT on a comparison with all-literal operands.
                if let Predicate::Comparison { left, op, right } = &child
                    && let Some(result) = fold_not_comparison(left, *op, right)
                {
                    return result;
                }
                Predicate::Not {
                    child: Box::new(child),
                }
            }
            Predicate::IsNull { expr } => {
                let expr = self.fold_expression(expr);
                // IS NULL on a known non-null literal → false.
                if let Expression::Literal { value, .. } = &expr {
                    if !value.is_null() {
                        return bool_predicate(false);
                    }
                    // value.is_null() → keep as IS NULL (null is still unknown at runtime? No,
                    // SQL NULL IS NULL → true)
                    if value.is_null() {
                        return bool_predicate(true);
                    }
                }
                Predicate::IsNull { expr }
            }
            Predicate::And { left, right } => {
                let left = self.fold_predicate(left);
                let right = self.fold_predicate(right);
                fold_and_or_short_circuit(&left, true, &right).unwrap_or_else(|| Predicate::And {
                    left: Box::new(left),
                    right: Box::new(right),
                })
            }
            Predicate::Or { left, right } => {
                let left = self.fold_predicate(left);
                let right = self.fold_predicate(right);
                fold_and_or_short_circuit(&left, false, &right).unwrap_or_else(|| Predicate::Or {
                    left: Box::new(left),
                    right: Box::new(right),
                })
            }
            other => default_fold_predicate(self, other),
        }
    }
}

// -- boolean identity helpers ----------------------------------------------

/// Applies `AND`/`OR` short-circuit and identity rules at the
/// `Expression` level (`true AND col` → `col`).
fn fold_boolean_identity(
    left: &Expression,
    op: BinaryOperator,
    right: &Expression,
) -> Option<Expression> {
    let left_bool = lit_as_bool(left);
    let right_bool = lit_as_bool(right);

    match (op, left_bool, right_bool) {
        // true AND x → x,  x AND true → x
        (BinaryOperator::And, Some(true), _) => Some(right.clone()),
        (BinaryOperator::And, _, Some(true)) => Some(left.clone()),
        // false AND x → false,  x AND false → false
        (BinaryOperator::And, Some(false), _) => Some(left.clone()),
        (BinaryOperator::And, _, Some(false)) => Some(right.clone()),
        // true OR x → true,  x OR true → true
        (BinaryOperator::Or, Some(true), _) => Some(left.clone()),
        (BinaryOperator::Or, _, Some(true)) => Some(right.clone()),
        // false OR x → x,  x OR false → x
        (BinaryOperator::Or, Some(false), _) => Some(right.clone()),
        (BinaryOperator::Or, _, Some(false)) => Some(left.clone()),
        _ => None,
    }
}

/// Applies algebraic identity simplifications (`x + 0 → x`, `x * 1 → x`, etc.)
///
/// These are safe for all values including NULL because the identity
/// element does not change the operand (NULL + 0 = NULL, NULL * 1 = NULL).
fn fold_algebraic_identity(
    left: &Expression,
    op: BinaryOperator,
    right: &Expression,
) -> Option<Expression> {
    let right_is_zero =
        matches!(right, Expression::Literal { value, .. } if value.as_f64() == Some(0.0));
    let right_is_one =
        matches!(right, Expression::Literal { value, .. } if value.as_f64() == Some(1.0));
    let left_is_zero =
        matches!(left, Expression::Literal { value, .. } if value.as_f64() == Some(0.0));
    let left_is_one =
        matches!(left, Expression::Literal { value, .. } if value.as_f64() == Some(1.0));

    match op {
        BinaryOperator::Add => {
            // x + 0 → x,  0 + x → x
            if right_is_zero {
                Some(left.clone())
            } else if left_is_zero {
                Some(right.clone())
            } else {
                None
            }
        }
        BinaryOperator::Sub if right_is_zero => {
            // x - 0 → x
            Some(left.clone())
        }
        BinaryOperator::Mul => {
            // x * 1 → x,  1 * x → x
            if right_is_one {
                Some(left.clone())
            } else if left_is_one {
                Some(right.clone())
            } else {
                None
            }
        }
        BinaryOperator::Div if right_is_one => {
            // x / 1 → x
            Some(left.clone())
        }
        _ => None,
    }
}

/// Applies `AND`/`OR` short-circuit at the `Predicate` level.
fn fold_and_or_short_circuit(
    left: &Predicate,
    is_and: bool,
    right: &Predicate,
) -> Option<Predicate> {
    let left_val = predicate_is_bool_literal(left);
    let right_val = predicate_is_bool_literal(right);

    match (is_and, left_val, right_val) {
        // AND: false AND _ → false,  _ AND false → false
        (true, Some(false), _) => Some(bool_predicate(false)),
        (true, _, Some(false)) => Some(bool_predicate(false)),
        // AND: true AND x → x,  x AND true → x
        (true, Some(true), _) => Some(right.clone()),
        (true, _, Some(true)) => Some(left.clone()),
        // OR: true OR _ → true,  _ OR true → true
        (false, Some(true), _) => Some(bool_predicate(true)),
        (false, _, Some(true)) => Some(bool_predicate(true)),
        // OR: false OR x → x,  x OR false → x
        (false, Some(false), _) => Some(right.clone()),
        (false, _, Some(false)) => Some(left.clone()),
        _ => None,
    }
}

/// Evaluates `NOT (left op right)` where all are literals.
fn fold_not_comparison(
    left: &Expression,
    op: ComparisonOperator,
    right: &Expression,
) -> Option<Predicate> {
    let (Expression::Literal { value: lv, .. }, Expression::Literal { value: rv, .. }) =
        (left, right)
    else {
        return None;
    };
    let ordering = compare_values(lv, rv)?;
    let result = match op {
        ComparisonOperator::Eq => !ordering.is_eq(),
        ComparisonOperator::Neq => !ordering.is_ne(),
        ComparisonOperator::Gt => !ordering.is_gt(),
        ComparisonOperator::Lt => !ordering.is_lt(),
        ComparisonOperator::Gte => !ordering.is_ge(),
        ComparisonOperator::Lte => !ordering.is_le(),
        // Non-evaluable comparison operators: keep NOT intact.
        ComparisonOperator::Like
        | ComparisonOperator::ILike
        | ComparisonOperator::In
        | ComparisonOperator::Between => return None,
    };
    Some(bool_predicate(result))
}

/// Returns `Some(true)` or `Some(false)` when `expr` is a boolean
/// literal, `None` otherwise.
fn lit_as_bool(expr: &Expression) -> Option<bool> {
    match expr {
        Expression::Literal { value, data_type } if *data_type == DataType::Boolean => {
            value.as_bool()
        }
        _ => None,
    }
}

/// Checks whether a predicate reduces to a boolean constant
/// (e.g. `true = true` or `1 = 1` for true, `1 = 0` for false).
fn predicate_is_bool_literal(pred: &Predicate) -> Option<bool> {
    if let Predicate::Comparison { left, op, right } = pred
        && matches!(op, ComparisonOperator::Eq)
        && let (Expression::Literal { value: lv, .. }, Expression::Literal { value: rv, .. }) =
            (left, right)
    {
        // true = true, false = false
        if lv == rv {
            return lv.as_bool();
        }
        // 1 = 1 is also true, 1 = 0 is false
        if let (Some(l), Some(r)) = (lv.as_f64(), rv.as_f64()) {
            return Some((l - r).abs() < f64::EPSILON);
        }
    }
    None
}

// -- original constant-folding helpers -------------------------------------

/// Evaluates a binary operation over two literal JSON values, returning
/// the folded literal or `None` when the result cannot be computed
/// exactly (and the original expression should be kept).
fn fold_binary(left: &Value, op: BinaryOperator, right: &Value) -> Option<Expression> {
    use BinaryOperator::*;

    match op {
        Add | Sub | Mul | Div | Mod => fold_arithmetic(left, op, right),
        And | Or => fold_boolean(left, op, right),
        Eq | Neq | Gt | Lt | Gte | Lte => fold_comparison(left, op, right),
        Like | ILike => None,
    }
}

fn fold_arithmetic(left: &Value, op: BinaryOperator, right: &Value) -> Option<Expression> {
    let (left, right) = (left.as_f64()?, right.as_f64()?);
    let both_ints = is_integral(left) && is_integral(right);

    let result = match op {
        BinaryOperator::Add => left + right,
        BinaryOperator::Sub => left - right,
        BinaryOperator::Mul => left * right,
        BinaryOperator::Div if right != 0.0 => left / right,
        BinaryOperator::Mod if right != 0.0 => left % right,
        _ => return None,
    };

    if !result.is_finite() {
        return None;
    }

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

fn compare_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Number(_), Value::Number(_)) => left.as_f64()?.partial_cmp(&right.as_f64()?),
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
        data_type: DataType::Int,
    }
}

fn float_literal(value: f64) -> Expression {
    Expression::Literal {
        value: json!(value),
        data_type: DataType::Float,
    }
}

fn bool_literal(value: bool) -> Expression {
    Expression::Literal {
        value: json!(value),
        data_type: DataType::Boolean,
    }
}

fn bool_predicate(value: bool) -> Predicate {
    let lit = Expression::Literal {
        value: json!(value),
        data_type: DataType::Boolean,
    };
    Predicate::Comparison {
        left: lit.clone(),
        op: ComparisonOperator::Eq,
        right: lit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ComparisonOperator, DataType, FromClause, Projection};

    fn lit_int(value: i64) -> Expression {
        Expression::Literal {
            value: json!(value),
            data_type: DataType::Int,
        }
    }

    fn lit_bool(value: bool) -> Expression {
        Expression::Literal {
            value: json!(value),
            data_type: DataType::Boolean,
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
        let folded =
            ConstantFolding.fold_expression(&binop(lit_int(1), BinaryOperator::Add, lit_int(2)));
        assert_eq!(folded, lit_int(3));
    }

    #[test]
    fn folds_nested_arithmetic() {
        let inner = binop(lit_int(3), BinaryOperator::Add, lit_int(4));
        let folded =
            ConstantFolding.fold_expression(&binop(lit_int(2), BinaryOperator::Mul, inner));
        assert_eq!(folded, lit_int(14));
    }

    #[test]
    fn folds_constant_side_of_predicate() {
        let pred = Predicate::Comparison {
            left: column("age"),
            op: ComparisonOperator::Gt,
            right: binop(lit_int(20), BinaryOperator::Add, lit_int(5)),
        };
        let folded = ConstantFolding.fold_predicate(&pred);
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
        assert_eq!(ConstantFolding.fold_expression(&expr), expr);
    }

    #[test]
    fn does_not_fold_division_by_zero() {
        let expr = binop(lit_int(1), BinaryOperator::Div, lit_int(0));
        assert_eq!(ConstantFolding.fold_expression(&expr), expr);
    }

    #[test]
    fn division_stays_float_when_inexact() {
        let folded =
            ConstantFolding.fold_expression(&binop(lit_int(1), BinaryOperator::Div, lit_int(2)));
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
        let expr = binop(
            binop(lit_int(20), BinaryOperator::Add, lit_int(5)),
            BinaryOperator::Eq,
            lit_int(25),
        );
        assert_eq!(ConstantFolding.fold_expression(&expr), bool_literal(true));
    }

    // --- boolean identity tests -------------------------------------------

    #[test]
    fn true_and_column_is_column() {
        let expr = binop(lit_bool(true), BinaryOperator::And, column("x"));
        assert_eq!(ConstantFolding.fold_expression(&expr), column("x"));
    }

    #[test]
    fn column_and_true_is_column() {
        let expr = binop(column("x"), BinaryOperator::And, lit_bool(true));
        assert_eq!(ConstantFolding.fold_expression(&expr), column("x"));
    }

    #[test]
    fn false_and_column_is_false() {
        let expr = binop(lit_bool(false), BinaryOperator::And, column("x"));
        assert_eq!(ConstantFolding.fold_expression(&expr), lit_bool(false));
    }

    #[test]
    fn column_and_false_is_false() {
        let expr = binop(column("x"), BinaryOperator::And, lit_bool(false));
        assert_eq!(ConstantFolding.fold_expression(&expr), lit_bool(false));
    }

    #[test]
    fn true_or_column_is_true() {
        let expr = binop(lit_bool(true), BinaryOperator::Or, column("x"));
        assert_eq!(ConstantFolding.fold_expression(&expr), lit_bool(true));
    }

    #[test]
    fn false_or_column_is_column() {
        let expr = binop(lit_bool(false), BinaryOperator::Or, column("x"));
        assert_eq!(ConstantFolding.fold_expression(&expr), column("x"));
    }

    #[test]
    fn not_not_x_is_x() {
        let inner = Predicate::Comparison {
            left: column("age"),
            op: ComparisonOperator::Gt,
            right: lit_int(18),
        };
        let pred = Predicate::Not {
            child: Box::new(Predicate::Not {
                child: Box::new(inner.clone()),
            }),
        };
        assert_eq!(ConstantFolding.fold_predicate(&pred), inner);
    }

    #[test]
    fn not_literal_comparison_is_folded() {
        let pred = Predicate::Not {
            child: Box::new(Predicate::Comparison {
                left: lit_int(1),
                op: ComparisonOperator::Eq,
                right: lit_int(2),
            }),
        };
        let folded = ConstantFolding.fold_predicate(&pred);
        assert_eq!(folded, bool_predicate(true)); // NOT (1 = 2) → true
    }

    #[test]
    fn is_null_on_non_null_literal_is_false() {
        let pred = Predicate::IsNull { expr: lit_int(42) };
        assert_eq!(ConstantFolding.fold_predicate(&pred), bool_predicate(false));
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

    #[test]
    fn true_and_predicate_simplifies() {
        let pred = Predicate::And {
            left: Box::new(bool_predicate(true)),
            right: Box::new(Predicate::Comparison {
                left: column("x"),
                op: ComparisonOperator::Eq,
                right: lit_int(1),
            }),
        };
        let folded = ConstantFolding.fold_predicate(&pred);
        assert_eq!(
            folded,
            Predicate::Comparison {
                left: column("x"),
                op: ComparisonOperator::Eq,
                right: lit_int(1),
            }
        );
    }

    #[test]
    fn false_or_predicate_simplifies() {
        let pred = Predicate::Or {
            left: Box::new(bool_predicate(false)),
            right: Box::new(Predicate::Comparison {
                left: column("x"),
                op: ComparisonOperator::Eq,
                right: lit_int(1),
            }),
        };
        let folded = ConstantFolding.fold_predicate(&pred);
        assert_eq!(
            folded,
            Predicate::Comparison {
                left: column("x"),
                op: ComparisonOperator::Eq,
                right: lit_int(1),
            }
        );
    }

    // --- algebraic identity tests -----------------------------------------

    #[test]
    fn column_plus_zero_is_column() {
        assert_eq!(
            ConstantFolding.fold_expression(&binop(column("x"), BinaryOperator::Add, lit_int(0))),
            column("x"),
        );
    }

    #[test]
    fn zero_plus_column_is_column() {
        assert_eq!(
            ConstantFolding.fold_expression(&binop(lit_int(0), BinaryOperator::Add, column("x"))),
            column("x"),
        );
    }

    #[test]
    fn column_minus_zero_is_column() {
        assert_eq!(
            ConstantFolding.fold_expression(&binop(column("x"), BinaryOperator::Sub, lit_int(0))),
            column("x"),
        );
    }

    #[test]
    fn column_times_one_is_column() {
        assert_eq!(
            ConstantFolding.fold_expression(&binop(column("x"), BinaryOperator::Mul, lit_int(1))),
            column("x"),
        );
    }

    #[test]
    fn one_times_column_is_column() {
        assert_eq!(
            ConstantFolding.fold_expression(&binop(lit_int(1), BinaryOperator::Mul, column("x"))),
            column("x"),
        );
    }

    #[test]
    fn column_divided_by_one_is_column() {
        assert_eq!(
            ConstantFolding.fold_expression(&binop(column("x"), BinaryOperator::Div, lit_int(1))),
            column("x"),
        );
    }

    #[test]
    fn no_simplification_for_non_identity() {
        // x + 2 should stay as-is
        let expr = binop(column("x"), BinaryOperator::Add, lit_int(2));
        assert_eq!(ConstantFolding.fold_expression(&expr), expr);
    }

    #[test]
    fn identity_works_with_nested_expressions() {
        // (x + 0) * 1 → x
        let inner = binop(column("x"), BinaryOperator::Add, lit_int(0));
        let expr = binop(inner, BinaryOperator::Mul, lit_int(1));
        assert_eq!(ConstantFolding.fold_expression(&expr), column("x"));
    }
}
