//! Aggregating validation for controlled SQL dialect features.

use crate::errors::{ValidationErrorKind, VlorQLError};
use crate::schema::{
    DialectProfile, Expression, InTarget, Predicate, Projection, QueryPlan, WindowFrame,
    WindowFrameBound,
};
use serde_json::json;

/// Entry points for validating controlled SQL dialect features.
#[derive(Debug, Clone, Copy, Default)]
pub struct DialectValidator;

impl DialectValidator {
    /// Creates a validator bound to one dialect profile.
    pub fn bind(profile: &DialectProfile) -> BoundDialectValidator<'_> {
        BoundDialectValidator { profile }
    }

    /// Validates a plan directly against a dialect profile.
    pub fn validate(plan: &QueryPlan, profile: &DialectProfile) -> Result<(), Vec<VlorQLError>> {
        Self::bind(profile).validate(plan)
    }

    /// Alias for [`DialectValidator::validate`].
    pub fn validate_plan(
        plan: &QueryPlan,
        profile: &DialectProfile,
    ) -> Result<(), Vec<VlorQLError>> {
        Self::validate(plan, profile)
    }
}

/// A dialect validator borrowing one profile for repeated validation.
#[derive(Debug, Clone, Copy)]
pub struct BoundDialectValidator<'a> {
    profile: &'a DialectProfile,
}

impl BoundDialectValidator<'_> {
    /// Checks a plan and nested CTEs, collecting every dialect violation.
    pub fn validate(&self, plan: &QueryPlan) -> Result<(), Vec<VlorQLError>> {
        let mut errors = Vec::new();
        self.validate_inner(plan, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn validate_inner(&self, plan: &QueryPlan, errors: &mut Vec<VlorQLError>) {
        if plan.ctes.as_ref().is_some_and(|ctes| !ctes.is_empty()) && !self.profile.supports_cte {
            errors.push(self.feature_disabled("common_table_expressions"));
        }

        let join_count = plan.joins.as_ref().map_or(0, Vec::len);
        if let Some(max) = self.profile.max_joins
            && join_count > max
        {
            errors.push(VlorQLError::validation(
                ValidationErrorKind::TooManyJoins {
                    actual: join_count,
                    max,
                },
                json!({
                    "actual": join_count,
                    "max": max,
                    "dialect": self.profile.dialect,
                }),
            ));
        }

        if let Some(joins) = &plan.joins {
            for join in joins {
                if !self.profile.allowed_join_types.is_empty()
                    && !self.profile.allowed_join_types.contains(&join.join_type)
                {
                    errors.push(self.feature_disabled(format!(
                        "join_type:{}",
                        format!("{:?}", join.join_type).to_ascii_lowercase()
                    )));
                }
                self.validate_predicate(&join.on, errors);
            }
        }

        if !self.profile.supports_offset && plan.offset.is_some() {
            errors.push(self.feature_disabled("offset"));
        }

        if let Some(max) = self.profile.max_group_by_columns {
            let actual = plan.group_by.as_ref().map_or(0, Vec::len);
            if actual > max {
                errors.push(VlorQLError::validation(
                    ValidationErrorKind::AggregationMismatch {
                        message: format!(
                            "query groups by {actual} columns, but the maximum is {max}"
                        ),
                    },
                    json!({"actual": actual, "max": max}),
                ));
            }
        }

        if plan.distinct && !self.profile.allow_select_distinct {
            errors.push(self.feature_disabled("select_distinct"));
        }

        self.validate_select_group_by(plan, errors);

        for projection in &plan.select {
            if let Projection::Expr { expression, .. } = projection {
                self.validate_expression(expression, errors);
            }
        }
        if let Some(predicate) = &plan.r#where {
            self.validate_predicate(predicate, errors);
        }
        if let Some(expressions) = &plan.group_by {
            for expression in expressions {
                self.validate_expression(expression, errors);
            }
        }
        if let Some(predicate) = &plan.having {
            self.validate_predicate(predicate, errors);
        }
        if let Some(terms) = &plan.order_by {
            for term in terms {
                self.validate_expression(&term.expr, errors);
            }
        }
        if let Some(ctes) = &plan.ctes {
            for cte in ctes {
                self.validate_inner(&cte.query, errors);
            }
        }
    }

    fn validate_select_group_by(&self, plan: &QueryPlan, errors: &mut Vec<VlorQLError>) {
        let Some(group_by) = plan.group_by.as_ref().filter(|g| !g.is_empty()) else {
            return;
        };

        for projection in &plan.select {
            match projection {
                Projection::Star { .. } => {
                    errors.push(self.group_by_error(
                        "SELECT * is not allowed with GROUP BY: every column must be explicitly listed in GROUP BY or wrapped in an aggregate function",
                    ));
                }
                Projection::Column { table, column, .. } => {
                    if !is_column_in_group_by(group_by, table, column) {
                        errors.push(self.group_by_error(format!(
                            "column `{}.{}` must appear in GROUP BY or be wrapped in an aggregate function",
                            table.as_deref().unwrap_or("?"),
                            column,
                        )));
                    }
                }
                Projection::Expr { expression, .. } => {
                    match expression {
                        Expression::ColumnRef { table, column } => {
                            if !is_column_in_group_by(group_by, table, column) {
                                errors.push(self.group_by_error(format!(
                                    "column `{}.{}` must appear in GROUP BY or be wrapped in an aggregate function",
                                    table.as_deref().unwrap_or("?"),
                                    column,
                                )));
                            }
                        }
                        Expression::Star => {
                            errors.push(
                                self.group_by_error(
                                    "bare `*` is not allowed in SELECT with GROUP BY",
                                ),
                            );
                        }
                        // Aggregate functions (SUM, COUNT, etc.) are OK even if their
                        // arguments are not in GROUP BY.
                        Expression::FunctionCall { .. } => {}
                        // Other expression types (BinaryOp, Literal, SubQuery) are
                        // assumed valid — they don't introduce bare column references.
                        _ => {}
                    }
                }
            }
        }
    }

    fn group_by_error(&self, message: impl Into<String>) -> VlorQLError {
        VlorQLError::validation(
            ValidationErrorKind::AggregationMismatch {
                message: message.into(),
            },
            json!({"type": "select_group_by_mismatch"}),
        )
    }

    fn validate_expression(&self, expression: &Expression, errors: &mut Vec<VlorQLError>) {
        match expression {
            Expression::Literal { .. } | Expression::ColumnRef { .. } => {}
            Expression::FunctionCall {
                name,
                args,
                distinct,
            } => {
                let denied = self
                    .profile
                    .denied_functions
                    .iter()
                    .any(|function| function.eq_ignore_ascii_case(name));
                let allowed = self.profile.allowed_functions.is_empty()
                    || self
                        .profile
                        .allowed_functions
                        .iter()
                        .any(|function| function.eq_ignore_ascii_case(name));
                if denied || !allowed {
                    errors.push(VlorQLError::validation(
                        ValidationErrorKind::InvalidFunction {
                            function: name.clone(),
                            allowed_functions: self.profile.allowed_functions.clone(),
                        },
                        json!({
                            "function": name,
                            "denied": denied,
                            "allowed_functions": self.profile.allowed_functions,
                        }),
                    ));
                }
                if *distinct && !self.profile.allow_distinct {
                    errors.push(self.feature_disabled("distinct"));
                }
                for argument in args {
                    self.validate_expression(argument, errors);
                }
            }
            Expression::BinaryOp { left, right, .. } => {
                self.validate_expression(left, errors);
                self.validate_expression(right, errors);
            }
            Expression::Star => {} // `*` is always valid in function calls.
            Expression::SubQuery { query } => {
                self.validate_inner(query, errors);
            }
            Expression::Case {
                operand,
                when_thens,
                else_result,
            } => {
                if let Some(op) = operand {
                    self.validate_expression(op, errors);
                }
                for wt in when_thens {
                    self.validate_expression(&wt.when, errors);
                    self.validate_expression(&wt.then, errors);
                }
                if let Some(el) = else_result {
                    self.validate_expression(el, errors);
                }
            }
            Expression::WindowFunction {
                name,
                args,
                distinct,
                over,
            } => {
                if !self.profile.supports_window_functions {
                    errors.push(self.feature_disabled("window_functions"));
                }
                let denied = self
                    .profile
                    .denied_functions
                    .iter()
                    .any(|function| function.eq_ignore_ascii_case(name));
                let allowed = self.profile.allowed_functions.is_empty()
                    || self
                        .profile
                        .allowed_functions
                        .iter()
                        .any(|function| function.eq_ignore_ascii_case(name));
                if denied || !allowed {
                    errors.push(VlorQLError::validation(
                        ValidationErrorKind::InvalidFunction {
                            function: name.clone(),
                            allowed_functions: self.profile.allowed_functions.clone(),
                        },
                        json!({
                            "function": name,
                            "denied": denied,
                            "allowed_functions": self.profile.allowed_functions,
                        }),
                    ));
                }
                if *distinct && !self.profile.allow_distinct {
                    errors.push(self.feature_disabled("distinct"));
                }
                for argument in args {
                    self.validate_expression(argument, errors);
                }
                // Validate expressions inside the window spec.
                if let Some(partition_by) = &over.partition_by {
                    for expr in partition_by {
                        self.validate_expression(expr, errors);
                    }
                }
                if let Some(order_by) = &over.order_by {
                    for term in order_by {
                        self.validate_expression(&term.expr, errors);
                    }
                }
                if let Some(frame) = &over.frame {
                    self.validate_window_frame_bounds(frame, errors);
                }
            }
        }
    }

    fn validate_predicate(&self, predicate: &Predicate, errors: &mut Vec<VlorQLError>) {
        match predicate {
            Predicate::Comparison { left, right, .. } => {
                self.validate_expression(left, errors);
                self.validate_expression(right, errors);
            }
            Predicate::And { left, right } | Predicate::Or { left, right } => {
                self.validate_predicate(left, errors);
                self.validate_predicate(right, errors);
            }
            Predicate::Not { child } => self.validate_predicate(child, errors),
            Predicate::Between { expr, low, high } => {
                self.validate_expression(expr, errors);
                self.validate_expression(low, errors);
                self.validate_expression(high, errors);
            }
            Predicate::In { expr, target } => {
                self.validate_expression(expr, errors);
                match target {
                    InTarget::Values(values) => {
                        for value in values {
                            self.validate_expression(value, errors);
                        }
                    }
                    InTarget::SubQuery(query) => {
                        self.validate_inner(query, errors);
                    }
                }
            }
            Predicate::Exists { query } => {
                self.validate_inner(query, errors);
            }
            Predicate::Like { expr, .. } | Predicate::IsNull { expr } => {
                self.validate_expression(expr, errors);
            }
        }
    }

    fn feature_disabled(&self, feature: impl Into<String>) -> VlorQLError {
        let feature = feature.into();
        VlorQLError::validation(
            ValidationErrorKind::DialectFeatureDisabled {
                feature: feature.clone(),
            },
            json!({
                "feature": feature,
                "dialect": self.profile.dialect,
            }),
        )
    }

    fn validate_window_frame_bounds(
        &self,
        frame: &WindowFrame,
        errors: &mut Vec<VlorQLError>,
    ) {
        // Validate frame boundary expressions (PRECEDING / FOLLOWING values).
        Self::validate_frame_bound(&frame.start, errors);
        if let Some(end) = &frame.end {
            Self::validate_frame_bound(end, errors);
        }
    }

    fn validate_frame_bound(bound: &WindowFrameBound, errors: &mut Vec<VlorQLError>) {
        match bound {
            WindowFrameBound::Preceding(expr) | WindowFrameBound::Following(expr) => {
                // The expression must be a literal or column ref (no subqueries).
                if matches!(expr.as_ref(), Expression::SubQuery { .. }) {
                    errors.push(VlorQLError::validation(
                        ValidationErrorKind::TypeMismatch {
                            expected: "literal or column".to_owned(),
                            found: "subquery".to_owned(),
                            expr: "window_frame_bound".to_owned(),
                        },
                        json!({
                            "expected": "literal or column",
                            "found": "subquery",
                            "bound": format!("{bound:?}"),
                        }),
                    ));
                }
            }
            _ => {}
        }
    }
}

/// Checks whether a `(table, column)` reference is covered by the GROUP BY expressions.
///
/// A column is considered covered if any GROUP BY expression is a `ColumnRef`
/// whose `(table, column)` pair matches (comparing qualified names if both are
/// qualified, or just column names if either side is unqualified).
fn is_column_in_group_by(group_by: &[Expression], table: &Option<String>, column: &str) -> bool {
    group_by.iter().any(|expr| match expr {
        Expression::ColumnRef {
            table: gb_table,
            column: gb_column,
        } => match (gb_table, table) {
            (Some(t1), Some(t2)) => t1 == t2 && gb_column == column,
            _ => gb_column == column,
        },
        _ => false,
    })
}
