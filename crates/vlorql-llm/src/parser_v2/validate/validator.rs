//! Validation orchestrator and error types.
//!
//! Defines the [`ValidationError`] type and the entry point for
//! running all validation stages.

use std::fmt;
use vlorql_core::schema::QueryPlan;

use super::semantic;

/// Kinds of validation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationErrorKind {
    /// SELECT list is empty.
    EmptySelect,
    /// FROM clause is missing or has an empty table name.
    MissingFrom,
    /// A projection item is invalid (e.g. empty column name).
    InvalidProjection,
    /// A JOIN is missing an ON condition.
    MissingJoinCondition,
    /// An expression is invalid (e.g. empty column name).
    InvalidExpression,
    /// A predicate is invalid (e.g. empty LIKE pattern).
    InvalidPredicate,
    /// LIMIT value is invalid (e.g. zero).
    InvalidLimit,
    /// CTE error (e.g. empty name).
    CteError,
    /// GROUP BY without aggregate functions in SELECT.
    MissingAggregate,
}

/// A validation error with a kind and a human-readable message.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// The error kind.
    pub kind: ValidationErrorKind,
    /// Human-readable error message.
    pub message: String,
}

impl ValidationError {
    /// Create a new validation error.
    pub fn new(kind: ValidationErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

/// The result of validating a [`QueryPlan`].
///
/// `Ok(())` when the plan is valid, `Err(Vec<ValidationError>)` when
/// one or more issues were found.
pub type ValidationResult = Result<(), Vec<ValidationError>>;

/// Run the full validation pipeline on a [`QueryPlan`].
///
/// Returns `Ok(())` when the plan is valid, or `Err(errors)` with all
/// discovered issues.
///
/// This is a pure structural/semantic validation — it does **not**
/// check against a schema snapshot or dialect profile.  For those,
/// use `vlorql_core::validate::pipeline::ValidationPipeline`.
pub fn validate_plan(plan: &QueryPlan) -> ValidationResult {
    let errors = semantic::validate(plan);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn valid_plan_ok() {
        let plan = valid_plan();
        assert!(validate_plan(&plan).is_ok());
    }

    #[test]
    fn empty_select_errors() {
        let mut plan = valid_plan();
        plan.select = vec![];
        let result = validate_plan(&plan);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::EmptySelect)
        );
    }

    #[test]
    fn multiple_errors_collected() {
        let mut plan = valid_plan();
        plan.select = vec![];
        plan.from.table = "".to_owned();
        let result = validate_plan(&plan);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::EmptySelect)
        );
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingFrom)
        );
    }

    #[test]
    fn validation_error_display() {
        let err = ValidationError::new(ValidationErrorKind::MissingFrom, "FROM clause is missing");
        let display = format!("{}", err);
        assert!(display.contains("MissingFrom"));
        assert!(display.contains("FROM clause is missing"));
    }

    #[test]
    fn validation_error_clone() {
        let err = ValidationError::new(ValidationErrorKind::EmptySelect, "SELECT is empty");
        let cloned = err.clone();
        assert_eq!(err.kind, cloned.kind);
        assert_eq!(err.message, cloned.message);
    }
}
