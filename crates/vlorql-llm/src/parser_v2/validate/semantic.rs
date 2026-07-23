//! Semantic validation rules for [`QueryPlan`] AST.
//!
//! Checks that the plan is structurally and semantically valid before
//! it is passed to the SQL compiler.  This layer does **no** repair —
//! it only reports errors.

use vlorql_core::schema::{
    Expression, InTarget, JoinClause, JoinType, Predicate, Projection, QueryPlan,
};

use super::validator::{ValidationError, ValidationErrorKind};

/// Returns `true` when `select` contains at least one aggregate function
/// call, making a `GROUP BY` meaningful.
fn has_aggregate_in_select(select: &[Projection]) -> bool {
    select.iter().any(|p| match p {
        Projection::Expr { expression, .. } => is_aggregate_expr(expression),
        _ => false,
    })
}

/// Returns `true` when `expr` is (or contains) an aggregate function call.
fn is_aggregate_expr(expr: &Expression) -> bool {
    match expr {
        Expression::FunctionCall { name, .. } => vlorql_core::function::is_aggregate(name),
        Expression::BinaryOp { left, right, .. } => {
            is_aggregate_expr(left) || is_aggregate_expr(right)
        }
        Expression::SubQuery { query } => query.group_by.is_some(),
        _ => false,
    }
}

/// Run all semantic checks on a [`QueryPlan`].
///
/// Returns a list of [`ValidationError`]s.  The list is empty when the
/// plan is valid.
pub fn validate(plan: &QueryPlan) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    validate_plan(plan, &mut errors);
    errors
}

fn validate_plan(plan: &QueryPlan, errors: &mut Vec<ValidationError>) {
    // 1. Validate SELECT
    validate_select(&plan.select, errors);

    // 2. Validate FROM
    validate_from(&plan.from.table, errors);

    // 3. Validate WHERE
    if let Some(ref predicate) = plan.r#where {
        validate_predicate(predicate, errors);
    }

    // 4. Validate GROUP BY
    if let Some(ref expressions) = plan.group_by {
        for expr in expressions {
            validate_expression(expr, errors);
        }
        // 4a. GROUP BY without aggregate functions → meaningless grouping.
        if !has_aggregate_in_select(&plan.select) {
            errors.push(ValidationError::new(
                ValidationErrorKind::MissingAggregate,
                "GROUP BY requires at least one aggregate function (SUM/COUNT/AVG/MIN/MAX) in SELECT; bare columns alone do not produce meaningful grouping",
            ));
        }
    }

    // 5. Validate HAVING
    if let Some(ref predicate) = plan.having {
        validate_predicate(predicate, errors);
    }

    // 6. Validate ORDER BY
    if let Some(ref terms) = plan.order_by {
        for term in terms {
            validate_expression(&term.expr, errors);
        }
    }

    // 7. Validate LIMIT / OFFSET
    validate_limit_offset(plan.limit, plan.offset, errors);

    // 8. Validate JOINs
    if let Some(ref joins) = plan.joins {
        for join in joins {
            validate_join(join, errors);
        }
    }

    // 9. Validate CTEs
    if let Some(ref ctes) = plan.ctes {
        for cte in ctes {
            if cte.name.is_empty() {
                errors.push(ValidationError::new(
                    ValidationErrorKind::CteError,
                    "CTE has an empty name",
                ));
            }
            validate_plan(&cte.query, errors);
        }
    }
}

/// Validate the SELECT clause.
fn validate_select(select: &[Projection], errors: &mut Vec<ValidationError>) {
    if select.is_empty() {
        errors.push(ValidationError::new(
            ValidationErrorKind::EmptySelect,
            "SELECT list is empty — at least one projection is required",
        ));
        return;
    }

    for (i, projection) in select.iter().enumerate() {
        match projection {
            Projection::Column { column, .. } => {
                if column.is_empty() {
                    errors.push(ValidationError::new(
                        ValidationErrorKind::InvalidProjection,
                        format!("select[{i}]: column_ref has empty column name"),
                    ));
                }
            }
            Projection::Expr { expression, .. } => {
                validate_expression(expression, errors);
            }
            Projection::Star { .. } => {} // Always valid.
        }
    }
}

/// Validate the FROM clause.
fn validate_from(table: &str, errors: &mut Vec<ValidationError>) {
    if table.is_empty() {
        errors.push(ValidationError::new(
            ValidationErrorKind::MissingFrom,
            "FROM clause has an empty table name",
        ));
    }
}

/// Validate a JOIN clause.
fn validate_join(join: &JoinClause, errors: &mut Vec<ValidationError>) {
    // Check that non-cross joins have ON conditions.
    if join.join_type != JoinType::Cross {
        // Check if the ON predicate is a dummy (auto-injected by builder).
        let is_dummy = matches!(
            &join.on,
            Predicate::Comparison {
                left: Expression::Literal { value, .. },
                right: Expression::Literal { .. },
                ..
            } if value.as_bool() == Some(true)
        );

        if is_dummy {
            errors.push(ValidationError::new(
                ValidationErrorKind::MissingJoinCondition,
                format!(
                    "{:?} JOIN on table `{}` is missing an ON condition",
                    join.join_type, join.right_table.table
                ),
            ));
        } else {
            validate_predicate(&join.on, errors);
        }
    }

    // Check that right_table has a table name.
    if join.right_table.table.is_empty() {
        errors.push(ValidationError::new(
            ValidationErrorKind::MissingJoinCondition,
            "JOIN has an empty right_table name",
        ));
    }
}

/// Validate LIMIT / OFFSET values.
fn validate_limit_offset(
    limit: Option<u64>,
    offset: Option<u64>,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(limit) = limit {
        if limit == 0 {
            errors.push(ValidationError::new(
                ValidationErrorKind::InvalidLimit,
                "LIMIT must be greater than 0",
            ));
        }
    }

    if let Some(offset) = offset {
        if offset == 0 {
            // offset=0 is valid (no-op), but warn about it.
            // Actually it's fine, just skip.
        }
    }
}

/// Validate a predicate tree.
fn validate_predicate(predicate: &Predicate, errors: &mut Vec<ValidationError>) {
    match predicate {
        Predicate::Comparison { left, op: _, right } => {
            validate_expression(left, errors);
            validate_expression(right, errors);
        }
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            validate_predicate(left, errors);
            validate_predicate(right, errors);
        }
        Predicate::Not { child } => {
            validate_predicate(child, errors);
        }
        Predicate::Between { expr, low, high } => {
            validate_expression(expr, errors);
            validate_expression(low, errors);
            validate_expression(high, errors);
        }
        Predicate::In { expr, target } => {
            validate_expression(expr, errors);
            match target {
                InTarget::Values(values) => {
                    for value in values {
                        validate_expression(value, errors);
                    }
                }
                InTarget::SubQuery(query) => {
                    validate_plan(query, errors);
                }
            }
        }
        Predicate::Like { expr, pattern } => {
            validate_expression(expr, errors);
            if pattern.is_empty() {
                errors.push(ValidationError::new(
                    ValidationErrorKind::InvalidPredicate,
                    "LIKE pattern is empty",
                ));
            }
        }
        Predicate::IsNull { expr } => {
            validate_expression(expr, errors);
        }
        Predicate::Exists { query } => {
            validate_plan(query, errors);
        }
    }
}

/// Validate an expression.
fn validate_expression(expression: &Expression, errors: &mut Vec<ValidationError>) {
    match expression {
        Expression::Literal {
            value,
            data_type: _,
        } => {
            if value.is_null() {
                // NULL literal is always valid.
            }
        }
        Expression::ColumnRef { table: _, column } => {
            if column.is_empty() {
                errors.push(ValidationError::new(
                    ValidationErrorKind::InvalidExpression,
                    "ColumnRef has an empty column name",
                ));
            }
        }
        Expression::FunctionCall {
            name,
            args,
            distinct: _,
        } => {
            if name.is_empty() {
                errors.push(ValidationError::new(
                    ValidationErrorKind::InvalidExpression,
                    "FunctionCall has an empty function name",
                ));
            }
            for arg in args {
                validate_expression(arg, errors);
            }
        }
        Expression::BinaryOp { left, op: _, right } => {
            validate_expression(left, errors);
            validate_expression(right, errors);
        }
        Expression::Star => {
            errors.push(ValidationError::new(
                ValidationErrorKind::InvalidExpression,
                "`*` is only valid in SELECT, not in WHERE/ON/HAVING or other expression positions",
            ));
        }
        Expression::SubQuery { query } => {
            validate_plan(query, errors);
        }
        Expression::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                validate_expression(op, errors);
            }
            for wt in when_thens {
                validate_expression(&wt.when, errors);
                validate_expression(&wt.then, errors);
            }
            if let Some(el) = else_result {
                validate_expression(el, errors);
            }
        }
        Expression::WindowFunction {
            name, args, ..
        } => {
            if name.is_empty() {
                errors.push(ValidationError::new(
                    ValidationErrorKind::InvalidExpression,
                    "WindowFunction has an empty function name",
                ));
            }
            for arg in args {
                validate_expression(arg, errors);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vlorql_core::schema::*;

    fn valid_plan() -> QueryPlan {
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

    #[test]
    fn valid_plan_passes() {
        let plan = valid_plan();
        let errors = validate(&plan);
        assert!(
            errors.is_empty(),
            "valid plan should have no errors: {:?}",
            errors
        );
    }

    #[test]
    fn detects_empty_select() {
        let mut plan = valid_plan();
        plan.select = vec![];
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::EmptySelect)
        );
    }

    #[test]
    fn detects_empty_column_ref() {
        let mut plan = valid_plan();
        plan.select = vec![Projection::Column {
            table: None,
            column: "".to_owned(),
            alias: None,
        }];
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidProjection)
        );
    }

    #[test]
    fn detects_empty_from_table() {
        let mut plan = valid_plan();
        plan.from.table = "".to_owned();
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingFrom)
        );
    }

    #[test]
    fn detects_missing_join_condition() {
        let mut plan = valid_plan();
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: None,
            },
            on: Predicate::Comparison {
                left: Expression::Literal {
                    value: json!(true),
                    data_type: DataType::Boolean,
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: json!(true),
                    data_type: DataType::Boolean,
                },
            },
        }]);
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingJoinCondition)
        );
    }

    #[test]
    fn cross_join_without_on_is_valid() {
        let mut plan = valid_plan();
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Cross,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: None,
            },
            on: Predicate::Comparison {
                left: Expression::Literal {
                    value: json!(true),
                    data_type: DataType::Boolean,
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: json!(true),
                    data_type: DataType::Boolean,
                },
            },
        }]);
        let errors = validate(&plan);
        assert!(
            !errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingJoinCondition)
        );
    }

    #[test]
    fn detects_empty_column_in_column_ref() {
        let mut plan = valid_plan();
        plan.r#where = Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: None,
                column: "".to_owned(),
            },
            op: ComparisonOperator::Eq,
            right: Expression::Literal {
                value: json!(1),
                data_type: DataType::Int,
            },
        });
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidExpression)
        );
    }

    #[test]
    fn detects_empty_function_name() {
        let mut plan = valid_plan();
        plan.select = vec![Projection::Expr {
            expression: Expression::FunctionCall {
                name: "".to_owned(),
                args: vec![],
                distinct: false,
            },
            alias: None,
        }];
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidExpression)
        );
    }

    #[test]
    fn detects_empty_like_pattern() {
        let mut plan = valid_plan();
        plan.r#where = Some(Predicate::Like {
            expr: Expression::ColumnRef {
                table: None,
                column: "name".to_owned(),
            },
            pattern: "".to_owned(),
        });
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidPredicate)
        );
    }

    #[test]
    fn detects_limit_zero() {
        let mut plan = valid_plan();
        plan.limit = Some(0);
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidLimit)
        );
    }

    #[test]
    fn validates_nested_cte() {
        let mut plan = valid_plan();
        plan.ctes = Some(vec![CommonTableExpression {
            name: "active".to_owned(),
            query: Box::new(QueryPlan {
                select: vec![Projection::Star { table: None, recursive: false }],
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
            set_operation: None,            }),
        }]);
        let errors = validate(&plan);
        assert!(
            errors.is_empty(),
            "valid CTE should have no errors: {:?}",
            errors
        );
    }

    #[test]
    fn detects_cte_with_empty_name() {
        let mut plan = valid_plan();
        plan.ctes = Some(vec![CommonTableExpression {
            name: "".to_owned(),
            query: Box::new(valid_plan()),, recursive: false
        }]);
        let errors = validate(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::CteError)
        );
    }
}
