//! Predicate simplification rules.
//!
//! Simplifies boolean predicate trees by applying algebraic rules:
//!
//! - `AND TRUE` / `TRUE AND` → remove the TRUE operand
//! - `OR FALSE` / `FALSE OR` → remove the FALSE operand
//! - `NOT NOT` → eliminate double negation
//! - Constant comparison folding: `1 = 1` → `TRUE`, `1 != 1` → `FALSE`
//! - `AND FALSE` → `FALSE` (short-circuit)
//! - `OR TRUE` → `TRUE` (short-circuit)

use serde_json::Value;
use vlorql_core::schema::{ComparisonOperator, DataType, Expression, Predicate};

/// Run all predicate simplification rules on a predicate tree.
///
/// Returns `true` if any simplification was applied.
#[must_use]
pub fn simplify(predicate: &mut Predicate) -> bool {
    simplify_recursive(predicate)
}

fn simplify_recursive(predicate: &mut Predicate) -> bool {
    let mut changed = false;

    match predicate {
        Predicate::And { left, right } => {
            changed |= simplify_recursive(left);
            changed |= simplify_recursive(right);

            // AND TRUE → remove TRUE side
            changed |= simplify_and_true(predicate);
            // AND FALSE → FALSE
            changed |= simplify_and_false(predicate);
            // Same predicate on both sides → keep one
            changed |= simplify_duplicate_and(predicate);
        }
        Predicate::Or { left, right } => {
            changed |= simplify_recursive(left);
            changed |= simplify_recursive(right);

            // OR FALSE → remove FALSE side
            changed |= simplify_or_false(predicate);
            // OR TRUE → TRUE
            changed |= simplify_or_true(predicate);
            // Same predicate on both sides → keep one
            changed |= simplify_duplicate_or(predicate);
        }
        Predicate::Not { child } => {
            changed |= simplify_recursive(child);
            // NOT NOT → eliminate double negation
            changed |= simplify_not_not(predicate);
        }
        Predicate::Comparison { .. } => {
            // Fold constant comparisons: 1 = 1 → TRUE, 1 != 1 → FALSE
            changed |= fold_constant_comparison(predicate);
            // Simplify trivial comparisons: column = column → TRUE
            changed |= simplify_trivial_comparison(predicate);
            // Simplify expressions inside
            if let Predicate::Comparison { left, right, .. } = predicate {
                changed |= simplify_expression(left);
                changed |= simplify_expression(right);
            }
        }
        Predicate::Between { expr, low, high } => {
            changed |= simplify_expression(expr);
            changed |= simplify_expression(low);
            changed |= simplify_expression(high);
        }
        Predicate::In { expr, target } => {
            changed |= simplify_expression(expr);
            if let vlorql_core::schema::InTarget::Values(values) = target {
                for v in values.iter_mut() {
                    changed |= simplify_expression(v);
                }
            }
        }
        Predicate::Like { expr, .. } | Predicate::IsNull { expr } => {
            changed |= simplify_expression(expr);
        }
        Predicate::Exists { .. } => {
            // Subquery optimization is not yet implemented.
        }
    }

    changed
}

/// Simplify expressions (currently a no-op, reserved for future).
fn simplify_expression(_expr: &mut Expression) -> bool {
    false
}

/// `predicate AND TRUE` → `predicate`, `TRUE AND predicate` → `predicate`
fn simplify_and_true(predicate: &mut Predicate) -> bool {
    if let Predicate::And { left, right } = predicate {
        if is_true_predicate(left) {
            *predicate = std::mem::replace(
                right,
                Predicate::And {
                    left: Box::new(Predicate::Comparison {
                        left: Expression::Literal {
                            value: Value::Bool(true),
                            data_type: DataType::Boolean,
                        },
                        op: ComparisonOperator::Eq,
                        right: Expression::Literal {
                            value: Value::Bool(true),
                            data_type: DataType::Boolean,
                        },
                    }),
                    right: Box::new(Predicate::Comparison {
                        left: Expression::Literal {
                            value: Value::Bool(true),
                            data_type: DataType::Boolean,
                        },
                        op: ComparisonOperator::Eq,
                        right: Expression::Literal {
                            value: Value::Bool(true),
                            data_type: DataType::Boolean,
                        },
                    }),
                },
            );
            return true;
        }
        if is_true_predicate(right) {
            let replacement = std::mem::replace(
                left,
                Box::new(Predicate::Comparison {
                    left: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                }),
            );
            *predicate = *replacement;
            return true;
        }
    }
    false
}

/// `predicate AND FALSE` → `FALSE`
fn simplify_and_false(predicate: &mut Predicate) -> bool {
    if let Predicate::And { left, right } = predicate {
        if is_false_predicate(left) || is_false_predicate(right) {
            *predicate = Predicate::Comparison {
                left: Expression::Literal {
                    value: Value::Bool(false),
                    data_type: DataType::Boolean,
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: Value::Bool(true),
                    data_type: DataType::Boolean,
                },
            };
            return true;
        }
    }
    false
}

/// `predicate OR FALSE` → `predicate`, `FALSE OR predicate` → `predicate`
fn simplify_or_false(predicate: &mut Predicate) -> bool {
    if let Predicate::Or { left, right } = predicate {
        if is_false_predicate(left) {
            let replacement = std::mem::replace(
                right,
                Box::new(Predicate::Comparison {
                    left: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                }),
            );
            *predicate = *replacement;
            return true;
        }
        if is_false_predicate(right) {
            let replacement = std::mem::replace(
                left,
                Box::new(Predicate::Comparison {
                    left: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                }),
            );
            *predicate = *replacement;
            return true;
        }
    }
    false
}

/// `predicate OR TRUE` → `TRUE`, `TRUE OR predicate` → `TRUE`
fn simplify_or_true(predicate: &mut Predicate) -> bool {
    if let Predicate::Or { left, right } = predicate {
        if is_true_predicate(left) || is_true_predicate(right) {
            *predicate = Predicate::Comparison {
                left: Expression::Literal {
                    value: Value::Bool(true),
                    data_type: DataType::Boolean,
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: Value::Bool(true),
                    data_type: DataType::Boolean,
                },
            };
            return true;
        }
    }
    false
}

/// `NOT NOT predicate` → `predicate`
fn simplify_not_not(predicate: &mut Predicate) -> bool {
    if let Predicate::Not { child } = predicate {
        if let Predicate::Not { child: inner_child } = child.as_ref() {
            let replacement = inner_child.clone();
            *predicate = *replacement;
            return true;
        }
    }
    false
}

/// `1 = 1` → `TRUE`, `1 != 1` → `FALSE`
fn fold_constant_comparison(predicate: &mut Predicate) -> bool {
    if let Predicate::Comparison { left, op, right } = predicate {
        if let (Some(lv), Some(rv)) = (as_constant_value(left), as_constant_value(right)) {
            // Only fold comparisons of the same scalar type.
            let result = match (lv, rv) {
                (serde_json::Value::Number(a), serde_json::Value::Number(b)) => {
                    match (a.as_i64(), b.as_i64()) {
                        (Some(ai), Some(bi)) => Some(match op {
                            ComparisonOperator::Eq => ai == bi,
                            ComparisonOperator::Neq => ai != bi,
                            ComparisonOperator::Gt => ai > bi,
                            ComparisonOperator::Gte => ai >= bi,
                            ComparisonOperator::Lt => ai < bi,
                            ComparisonOperator::Lte => ai <= bi,
                            _ => return false,
                        }),
                        (Some(ai), None) => b.as_f64().map(|bf| {
                            let af = ai as f64;
                            match op {
                                ComparisonOperator::Eq => af == bf,
                                ComparisonOperator::Neq => af != bf,
                                ComparisonOperator::Gt => af > bf,
                                ComparisonOperator::Gte => af >= bf,
                                ComparisonOperator::Lt => af < bf,
                                ComparisonOperator::Lte => af <= bf,
                                _ => return false,
                            }
                        }),
                        _ => None,
                    }
                }
                (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => Some(match op {
                    ComparisonOperator::Eq => a == b,
                    ComparisonOperator::Neq => a != b,
                    _ => return false,
                }),
                (serde_json::Value::String(a), serde_json::Value::String(b)) => Some(match op {
                    ComparisonOperator::Eq => a == b,
                    ComparisonOperator::Neq => a != b,
                    _ => return false,
                }),
                _ => None,
            };
            if let Some(result) = result {
                *predicate = Predicate::Comparison {
                    left: Expression::Literal {
                        value: Value::Bool(result),
                        data_type: DataType::Boolean,
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                };
                return true;
            }
        }
    }
    false
}

/// `column = column` → `TRUE`, `column != column` → `FALSE`
fn simplify_trivial_comparison(predicate: &mut Predicate) -> bool {
    if let Predicate::Comparison { left, op, right } = predicate {
        if let (Some(lc), Some(rc)) = (as_column_name(left), as_column_name(right)) {
            if lc == rc {
                let is_eq = matches!(
                    op,
                    ComparisonOperator::Eq | ComparisonOperator::Gte | ComparisonOperator::Lte
                );
                *predicate = Predicate::Comparison {
                    left: Expression::Literal {
                        value: Value::Bool(is_eq),
                        data_type: DataType::Boolean,
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                };
                return true;
            }
        }
    }
    false
}

/// `A AND A` → `A` (duplicate elimination)
fn simplify_duplicate_and(predicate: &mut Predicate) -> bool {
    if let Predicate::And { left, right } = predicate {
        if predicates_equal(left, right) {
            let replacement = std::mem::replace(
                left,
                Box::new(Predicate::Comparison {
                    left: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                }),
            );
            *predicate = *replacement;
            return true;
        }
    }
    false
}

/// `A OR A` → `A` (duplicate elimination)
fn simplify_duplicate_or(predicate: &mut Predicate) -> bool {
    if let Predicate::Or { left, right } = predicate {
        if predicates_equal(left, right) {
            let replacement = std::mem::replace(
                left,
                Box::new(Predicate::Comparison {
                    left: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: Value::Bool(true),
                        data_type: DataType::Boolean,
                    },
                }),
            );
            *predicate = *replacement;
            return true;
        }
    }
    false
}

// ── Helper functions ──────────────────────────────────────────────

/// Check if a predicate is a constant TRUE.
fn is_true_predicate(pred: &Predicate) -> bool {
    matches!(pred, Predicate::Comparison {
        left: Expression::Literal { value: lv, .. },
        op: ComparisonOperator::Eq,
        right: Expression::Literal { value: rv, .. },
    } if lv.as_bool() == Some(true) && rv.as_bool() == Some(true))
}

/// Check if a predicate is a constant FALSE.
fn is_false_predicate(pred: &Predicate) -> bool {
    matches!(pred, Predicate::Comparison {
        left: Expression::Literal { value: lv, .. },
        op: ComparisonOperator::Eq,
        right: Expression::Literal { value: rv, .. },
    } if lv.as_bool() == Some(false) && rv.as_bool() == Some(true))
}

/// Extract a constant value from an expression, if possible.
fn as_constant_value(expr: &Expression) -> Option<serde_json::Value> {
    match expr {
        Expression::Literal { value, .. } => Some(value.clone()),
        _ => None,
    }
}

/// Extract a column name from an expression, if it's a ColumnRef.
fn as_column_name(expr: &Expression) -> Option<&str> {
    match expr {
        Expression::ColumnRef { column, .. } => Some(column.as_str()),
        _ => None,
    }
}

/// Check if two predicates are structurally equal (recursive).
fn predicates_equal(a: &Predicate, b: &Predicate) -> bool {
    use vlorql_core::schema::InTarget;
    match (a, b) {
        (
            Predicate::Comparison {
                left: la,
                op: oa,
                right: ra,
            },
            Predicate::Comparison {
                left: lb,
                op: ob,
                right: rb,
            },
        ) => oa == ob && expressions_equal(la, lb) && expressions_equal(ra, rb),
        (
            Predicate::And {
                left: la,
                right: ra,
            },
            Predicate::And {
                left: lb,
                right: rb,
            },
        ) => predicates_equal(la, lb) && predicates_equal(ra, rb),
        (
            Predicate::Or {
                left: la,
                right: ra,
            },
            Predicate::Or {
                left: lb,
                right: rb,
            },
        ) => predicates_equal(la, lb) && predicates_equal(ra, rb),
        (Predicate::Not { child: ca }, Predicate::Not { child: cb }) => predicates_equal(ca, cb),
        (
            Predicate::Between {
                expr: ea,
                low: loa,
                high: hia,
            },
            Predicate::Between {
                expr: eb,
                low: lob,
                high: hib,
            },
        ) => {
            expressions_equal(ea, eb) && expressions_equal(loa, lob) && expressions_equal(hia, hib)
        }
        (
            Predicate::In {
                expr: ea,
                target: ta,
            },
            Predicate::In {
                expr: eb,
                target: tb,
            },
        ) => {
            expressions_equal(ea, eb)
                && match (ta, tb) {
                    (InTarget::Values(va), InTarget::Values(vb)) => {
                        va.len() == vb.len()
                            && va.iter().zip(vb).all(|(a, b)| expressions_equal(a, b))
                    }
                    _ => false,
                }
        }
        (
            Predicate::Like {
                expr: ea,
                pattern: pa,
            },
            Predicate::Like {
                expr: eb,
                pattern: pb,
            },
        ) => expressions_equal(ea, eb) && pa == pb,
        (Predicate::IsNull { expr: ea }, Predicate::IsNull { expr: eb }) => {
            expressions_equal(ea, eb)
        }
        (Predicate::Exists { .. }, Predicate::Exists { .. }) => {
            false // Subquery plans are compared by pointer, not structurally
        }
        _ => false,
    }
}

/// Check if two expressions are structurally equal.
fn expressions_equal(a: &Expression, b: &Expression) -> bool {
    match (a, b) {
        (
            Expression::Literal {
                value: va,
                data_type: dta,
            },
            Expression::Literal {
                value: vb,
                data_type: dtb,
            },
        ) => va == vb && dta == dtb,
        (
            Expression::ColumnRef {
                table: ta,
                column: ca,
            },
            Expression::ColumnRef {
                table: tb,
                column: cb,
            },
        ) => ta == tb && ca == cb,
        (
            Expression::FunctionCall {
                name: na,
                args: aa,
                distinct: da,
            },
            Expression::FunctionCall {
                name: nb,
                args: ab,
                distinct: db,
            },
        ) => {
            na == nb
                && da == db
                && aa.len() == ab.len()
                && aa.iter().zip(ab).all(|(a, b)| expressions_equal(a, b))
        }
        (
            Expression::BinaryOp {
                left: la,
                op: oa,
                right: ra,
            },
            Expression::BinaryOp {
                left: lb,
                op: ob,
                right: rb,
            },
        ) => oa == ob && expressions_equal(la, lb) && expressions_equal(ra, rb),
        (Expression::Star, Expression::Star) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vlorql_core::schema::*;

    fn lit_bool(v: bool) -> Expression {
        Expression::Literal {
            value: json!(v),
            data_type: DataType::Boolean,
        }
    }

    fn lit_int(v: i64) -> Expression {
        Expression::Literal {
            value: json!(v),
            data_type: DataType::Int,
        }
    }

    fn col(name: &str) -> Expression {
        Expression::ColumnRef {
            table: None,
            column: name.to_owned(),
        }
    }

    fn true_pred() -> Predicate {
        Predicate::Comparison {
            left: lit_bool(true),
            op: ComparisonOperator::Eq,
            right: lit_bool(true),
        }
    }

    fn false_pred() -> Predicate {
        Predicate::Comparison {
            left: lit_bool(false),
            op: ComparisonOperator::Eq,
            right: lit_bool(true),
        }
    }

    // ── AND TRUE simplification ──────────────────────────────────

    #[test]
    fn and_true_right() {
        let mut pred = Predicate::And {
            left: Box::new(Predicate::Comparison {
                left: col("age"),
                op: ComparisonOperator::Gt,
                right: lit_int(18),
            }),
            right: Box::new(true_pred()),
        };
        assert!(simplify(&mut pred));
        assert!(matches!(pred, Predicate::Comparison { .. }));
    }

    #[test]
    fn and_true_left() {
        let mut pred = Predicate::And {
            left: Box::new(true_pred()),
            right: Box::new(Predicate::Comparison {
                left: col("age"),
                op: ComparisonOperator::Gt,
                right: lit_int(18),
            }),
        };
        assert!(simplify(&mut pred));
        assert!(matches!(pred, Predicate::Comparison { .. }));
    }

    // ── AND FALSE simplification ─────────────────────────────────

    #[test]
    fn and_false_right() {
        let mut pred = Predicate::And {
            left: Box::new(Predicate::Comparison {
                left: col("age"),
                op: ComparisonOperator::Gt,
                right: lit_int(18),
            }),
            right: Box::new(false_pred()),
        };
        assert!(simplify(&mut pred));
        assert!(is_false_predicate(&pred));
    }

    // ── OR FALSE simplification ──────────────────────────────────

    #[test]
    fn or_false_right() {
        let mut pred = Predicate::Or {
            left: Box::new(Predicate::Comparison {
                left: col("age"),
                op: ComparisonOperator::Gt,
                right: lit_int(18),
            }),
            right: Box::new(false_pred()),
        };
        assert!(simplify(&mut pred));
        assert!(matches!(pred, Predicate::Comparison { .. }));
    }

    #[test]
    fn or_false_left() {
        let mut pred = Predicate::Or {
            left: Box::new(false_pred()),
            right: Box::new(Predicate::Comparison {
                left: col("age"),
                op: ComparisonOperator::Gt,
                right: lit_int(18),
            }),
        };
        assert!(simplify(&mut pred));
        assert!(matches!(pred, Predicate::Comparison { .. }));
    }

    // ── OR TRUE simplification ───────────────────────────────────

    #[test]
    fn or_true_right() {
        let mut pred = Predicate::Or {
            left: Box::new(Predicate::Comparison {
                left: col("age"),
                op: ComparisonOperator::Gt,
                right: lit_int(18),
            }),
            right: Box::new(true_pred()),
        };
        assert!(simplify(&mut pred));
        assert!(is_true_predicate(&pred));
    }

    // ── NOT NOT simplification ───────────────────────────────────

    #[test]
    fn not_not_elimination() {
        let mut pred = Predicate::Not {
            child: Box::new(Predicate::Not {
                child: Box::new(Predicate::Comparison {
                    left: col("age"),
                    op: ComparisonOperator::Gt,
                    right: lit_int(18),
                }),
            }),
        };
        assert!(simplify(&mut pred));
        assert!(matches!(pred, Predicate::Comparison { .. }));
    }

    // ── Constant folding ─────────────────────────────────────────

    #[test]
    fn fold_eq_true() {
        let mut pred = Predicate::Comparison {
            left: lit_int(1),
            op: ComparisonOperator::Eq,
            right: lit_int(1),
        };
        assert!(simplify(&mut pred));
        assert!(is_true_predicate(&pred));
    }

    #[test]
    fn fold_eq_false() {
        let mut pred = Predicate::Comparison {
            left: lit_int(1),
            op: ComparisonOperator::Eq,
            right: lit_int(2),
        };
        assert!(simplify(&mut pred));
        assert!(is_false_predicate(&pred));
    }

    #[test]
    fn fold_gt_true() {
        let mut pred = Predicate::Comparison {
            left: lit_int(2),
            op: ComparisonOperator::Gt,
            right: lit_int(1),
        };
        assert!(simplify(&mut pred));
        assert!(is_true_predicate(&pred));
    }

    // ── Trivial comparison ───────────────────────────────────────

    #[test]
    fn column_eq_self() {
        let mut pred = Predicate::Comparison {
            left: col("id"),
            op: ComparisonOperator::Eq,
            right: col("id"),
        };
        assert!(simplify(&mut pred));
        assert!(is_true_predicate(&pred));
    }

    #[test]
    fn column_ne_self() {
        let mut pred = Predicate::Comparison {
            left: col("id"),
            op: ComparisonOperator::Neq,
            right: col("id"),
        };
        assert!(simplify(&mut pred));
        assert!(is_false_predicate(&pred));
    }

    // ── Duplicate elimination ────────────────────────────────────

    #[test]
    fn duplicate_and() {
        let cmp = Predicate::Comparison {
            left: col("age"),
            op: ComparisonOperator::Gt,
            right: lit_int(18),
        };
        let mut pred = Predicate::And {
            left: Box::new(cmp.clone()),
            right: Box::new(cmp),
        };
        assert!(simplify(&mut pred));
        assert!(matches!(pred, Predicate::Comparison { .. }));
    }

    #[test]
    fn duplicate_or() {
        let cmp = Predicate::Comparison {
            left: col("age"),
            op: ComparisonOperator::Gt,
            right: lit_int(18),
        };
        let mut pred = Predicate::Or {
            left: Box::new(cmp.clone()),
            right: Box::new(cmp),
        };
        assert!(simplify(&mut pred));
        assert!(matches!(pred, Predicate::Comparison { .. }));
    }

    // ── Nested simplification ────────────────────────────────────

    #[test]
    fn nested_and_or_simplification() {
        // (age > 18 AND TRUE) OR (status = 'active' AND FALSE)
        let mut pred = Predicate::Or {
            left: Box::new(Predicate::And {
                left: Box::new(Predicate::Comparison {
                    left: col("age"),
                    op: ComparisonOperator::Gt,
                    right: lit_int(18),
                }),
                right: Box::new(true_pred()),
            }),
            right: Box::new(Predicate::And {
                left: Box::new(Predicate::Comparison {
                    left: col("status"),
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: json!("active"),
                        data_type: DataType::String,
                    },
                }),
                right: Box::new(false_pred()),
            }),
        };
        assert!(simplify(&mut pred));
        // After simplification: (age > 18) OR FALSE → age > 18
        assert!(matches!(pred, Predicate::Comparison { .. }));
    }

    // ── No-op for canonical input ────────────────────────────────

    #[test]
    fn no_simplification_needed() {
        let mut pred = Predicate::Comparison {
            left: col("age"),
            op: ComparisonOperator::Gt,
            right: lit_int(18),
        };
        assert!(!simplify(&mut pred));
    }
}
