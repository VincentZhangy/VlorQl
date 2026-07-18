//! Controlled SQL dialect capabilities.

use super::expressions::{Expression, InTarget, Predicate};
use super::query_plan::{CommonTableExpression, JoinClause, Projection, QueryPlan};
use super::types::{IdentifierQuoting, JoinType, SqlDialect};
use crate::errors::{ValidationErrorKind, VlorQLError};
use derive_builder::Builder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The SQL capabilities allowed for one query-planning context.
///
/// Each profile captures the dialect family, identifier quoting
/// convention, the set of allowed / denied features, and a few
/// quantitative limits that the validator and SQL compiler consult.
/// Use [`DialectProfile::builder`] to construct a customized profile
/// from the defaults.
///
/// # Examples
///
/// ```
/// use vlorql_core::schema::{DialectProfile, SqlDialect, JoinType};
///
/// let profile = DialectProfile::builder()
///     .dialect(SqlDialect::Sqlite)
///     .max_joins(5)
///     .build()
///     .expect("valid profile");
/// assert_eq!(profile.dialect, SqlDialect::Sqlite);
/// assert_eq!(profile.max_joins, Some(5));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Builder)]
#[builder(setter(into), default)]
#[serde(default, deny_unknown_fields)]
pub struct DialectProfile {
    /// The SQL dialect this profile emits.
    pub dialect: SqlDialect,
    /// How identifiers should be quoted in the generated SQL.
    pub quote_style: IdentifierQuoting,
    /// Whether `WITH <cte> ... SELECT ...` is allowed.
    pub supports_cte: bool,
    /// Whether window functions are allowed.
    pub supports_window_functions: bool,
    /// Whether JSON operators (`->`, `->>`, `@@`, …) are allowed.
    pub supports_json_operations: bool,
    /// Maximum number of joins permitted in a single plan. `None`
    /// means no limit.
    pub max_joins: Option<usize>,
    /// The join types the validator allows. An empty list means
    /// "all join types allowed".
    pub allowed_join_types: Vec<JoinType>,
    /// The function names the validator allows. An empty list
    /// means "any function not explicitly denied".
    pub allowed_functions: Vec<String>,
    /// The function names the validator always rejects.
    pub denied_functions: Vec<String>,
    /// Maximum number of `GROUP BY` expressions. `None` means no
    /// limit.
    pub max_group_by_columns: Option<usize>,
    /// Whether `DISTINCT` is permitted inside function calls.
    pub allow_distinct: bool,
    /// Whether `OFFSET n` is allowed.
    pub supports_offset: bool,
    /// Whether `FETCH FIRST n ROWS ONLY` is allowed.
    pub supports_fetch: bool,
}

impl Default for DialectProfile {
    fn default() -> Self {
        Self {
            dialect: SqlDialect::Postgres,
            quote_style: IdentifierQuoting::DoubleQuote,
            supports_cte: true,
            supports_window_functions: true,
            supports_json_operations: true,
            max_joins: None,
            allowed_join_types: vec![
                JoinType::Inner,
                JoinType::Left,
                JoinType::Right,
                JoinType::Full,
                JoinType::Cross,
            ],
            allowed_functions: Vec::new(),
            denied_functions: Vec::new(),
            max_group_by_columns: None,
            allow_distinct: true,
            supports_offset: true,
            supports_fetch: true,
        }
    }
}

impl DialectProfile {
    /// Returns a builder initialized with the profile defaults.
    pub fn builder() -> DialectProfileBuilder {
        DialectProfileBuilder::default()
    }

    /// Checks dialect-controlled features present in a query plan.
    ///
    /// This is intentionally a focused helper. The complete validation pipeline,
    /// including schema and policy checks, belongs in the `validate` module.
    ///
    /// # Errors
    ///
    /// Returns a [`VlorQLError::Validation`] when the plan exceeds
    /// `max_joins` / `max_group_by_columns`, references a disabled
    /// feature, or calls a denied function.
    pub fn validate_dialect_features(&self, plan: &QueryPlan) -> Result<(), VlorQLError> {
        let join_count = plan.joins.as_ref().map_or(0, Vec::len);
        if let Some(max_joins) = self.max_joins {
            if join_count > max_joins {
                return Err(VlorQLError::validation(
                    ValidationErrorKind::TooManyJoins {
                        actual: join_count,
                        max: max_joins,
                    },
                    json!({
                        "actual": join_count,
                        "max": max_joins,
                        "dialect": self.dialect,
                    }),
                ));
            }
        }

        if !self.supports_cte && has_ctes(plan.ctes.as_deref()) {
            return Err(self.feature_disabled("common_table_expressions"));
        }

        if !self.supports_offset && plan.offset.is_some() {
            return Err(self.feature_disabled("offset"));
        }

        if let Some(max_group_by_columns) = self.max_group_by_columns {
            let actual = plan.group_by.as_ref().map_or(0, Vec::len);
            if actual > max_group_by_columns {
                return Err(VlorQLError::validation(
                    ValidationErrorKind::AggregationMismatch {
                        message: format!(
                            "query groups by {actual} columns, but the maximum is {max_group_by_columns}"
                        ),
                    },
                    json!({
                        "actual": actual,
                        "max": max_group_by_columns,
                    }),
                ));
            }
        }

        for projection in &plan.select {
            self.validate_projection(projection)?;
        }
        if let Some(predicate) = &plan.r#where {
            self.validate_predicate(predicate)?;
        }
        if let Some(expressions) = &plan.group_by {
            for expression in expressions {
                self.validate_expression(expression)?;
            }
        }
        if let Some(predicate) = &plan.having {
            self.validate_predicate(predicate)?;
        }
        if let Some(order_by) = &plan.order_by {
            for term in order_by {
                self.validate_expression(&term.expr)?;
            }
        }
        if let Some(joins) = &plan.joins {
            for join in joins {
                self.validate_join(join)?;
            }
        }
        if let Some(ctes) = &plan.ctes {
            for cte in ctes {
                self.validate_cte(cte)?;
            }
        }

        Ok(())
    }

    fn validate_projection(&self, projection: &Projection) -> Result<(), VlorQLError> {
        match projection {
            Projection::Column { .. } | Projection::Star { .. } => Ok(()),
            Projection::Expr { expression, .. } => self.validate_expression(expression),
        }
    }

    fn validate_join(&self, join: &JoinClause) -> Result<(), VlorQLError> {
        if !self.allowed_join_types.is_empty() && !self.allowed_join_types.contains(&join.join_type)
        {
            return Err(self.feature_disabled(format!("join_type:{:?}", join.join_type)));
        }
        self.validate_predicate(&join.on)
    }

    fn validate_cte(&self, cte: &CommonTableExpression) -> Result<(), VlorQLError> {
        self.validate_dialect_features(&cte.query)
    }

    fn validate_expression(&self, expression: &Expression) -> Result<(), VlorQLError> {
        match expression {
            Expression::Literal { .. } | Expression::ColumnRef { .. } => Ok(()),
            Expression::FunctionCall {
                name,
                args,
                distinct,
            } => {
                self.validate_function(name)?;
                if *distinct && !self.allow_distinct {
                    return Err(self.feature_disabled("distinct"));
                }
                for argument in args {
                    self.validate_expression(argument)?;
                }
                Ok(())
            }
            Expression::BinaryOp { left, right, .. } => {
                self.validate_expression(left)?;
                self.validate_expression(right)
            }
            Expression::Star => Ok(()),
            Expression::SubQuery { query } => self.validate_dialect_features(query),
        }
    }

    fn validate_predicate(&self, predicate: &Predicate) -> Result<(), VlorQLError> {
        match predicate {
            Predicate::Comparison { left, right, .. } => {
                self.validate_expression(left)?;
                self.validate_expression(right)
            }
            Predicate::And { left, right } | Predicate::Or { left, right } => {
                self.validate_predicate(left)?;
                self.validate_predicate(right)
            }
            Predicate::Not { child } => self.validate_predicate(child),
            Predicate::Between { expr, low, high } => {
                self.validate_expression(expr)?;
                self.validate_expression(low)?;
                self.validate_expression(high)
            }
            Predicate::In { expr, target } => {
                self.validate_expression(expr)?;
                match target {
                    InTarget::Values(values) => {
                        for value in values {
                            self.validate_expression(value)?;
                        }
                        Ok(())
                    }
                    InTarget::SubQuery(query) => self.validate_dialect_features(query),
                }
            }
            Predicate::Exists { query } => self.validate_dialect_features(query),
            Predicate::Like { expr, .. } | Predicate::IsNull { expr } => {
                self.validate_expression(expr)
            }
        }
    }

    fn validate_function(&self, function: &str) -> Result<(), VlorQLError> {
        let denied = self
            .denied_functions
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(function));
        let allowed = self.allowed_functions.is_empty()
            || self
                .allowed_functions
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(function));

        if denied || !allowed {
            return Err(VlorQLError::validation(
                ValidationErrorKind::InvalidFunction {
                    function: function.to_owned(),
                    allowed_functions: self.allowed_functions.clone(),
                },
                json!({
                    "function": function,
                    "denied": denied,
                    "allowed_functions": self.allowed_functions,
                }),
            ));
        }
        Ok(())
    }

    fn feature_disabled(&self, feature: impl Into<String>) -> VlorQLError {
        let feature = feature.into();
        VlorQLError::validation(
            ValidationErrorKind::DialectFeatureDisabled {
                feature: feature.clone(),
            },
            json!({
                "feature": feature,
                "dialect": self.dialect,
            }),
        )
    }
}

fn has_ctes(ctes: Option<&[CommonTableExpression]>) -> bool {
    ctes.is_some_and(|items| !items.is_empty())
}
