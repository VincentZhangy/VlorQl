//! End-to-end validation pipeline orchestration.

use super::dialect::DialectValidator;
use super::operand::OperandValidator;
use super::schema::validate_schema;
use crate::errors::{ValidationErrors, VlorQLError};
use crate::optimizer::QueryOptimizer;
use crate::policy::PolicyEngine;
use crate::schema::{ArcSchemaSnapshot, DialectProfile, QueryPlan, SchemaSnapshot};
use std::ops::Deref;
use std::sync::Arc;

/// A query plan that has passed schema, policy, operand, and dialect validation.
///
/// # Examples
///
/// ```
/// use vlorql_core::validate::ValidatedPlan;
/// use vlorql_core::schema::{QueryPlan, Projection, FromClause};
/// use std::sync::Arc;
///
/// let plan = QueryPlan {
///     select: vec![Projection::Column {
///         table: None, column: "id".to_owned(), alias: None,
///     }],
///     from: FromClause { table: "users".to_owned(), alias: None },
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None,
///     joins: None, ctes: None,
/// };
/// let validated = ValidatedPlan(Arc::new(plan));
/// assert_eq!(validated.as_plan().from.table, "users");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedPlan(pub Arc<QueryPlan>);

impl ValidatedPlan {
    /// Borrows the validated query plan.
    pub fn as_plan(&self) -> &QueryPlan {
        self.0.as_ref()
    }

    /// Consumes the wrapper and returns the query plan.
    pub fn into_inner(self) -> Arc<QueryPlan> {
        self.0
    }
}

impl Deref for ValidatedPlan {
    type Target = QueryPlan;

    fn deref(&self) -> &Self::Target {
        self.as_plan()
    }
}

/// A query plan that has been validated **and** optimized.
///
/// Wraps a [`ValidatedPlan`] so that downstream consumers (notably the
/// SQL compiler) can distinguish optimised plans from merely-validated
/// ones.  `OptimizedPlan` derefs to `ValidatedPlan`, so it can be used
/// wherever a `&ValidatedPlan` is expected.
///
/// # Examples
///
/// ```
/// use vlorql_core::validate::{ValidatedPlan, OptimizedPlan};
/// use vlorql_core::schema::{QueryPlan, Projection, FromClause};
/// use std::sync::Arc;
///
/// let plan = QueryPlan {
///     select: vec![Projection::Column {
///         table: None, column: "id".to_owned(), alias: None,
///     }],
///     from: FromClause { table: "users".to_owned(), alias: None },
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None,
///     joins: None, ctes: None,
/// };
/// let validated = ValidatedPlan(Arc::new(plan));
/// let optimized = OptimizedPlan::from(validated);
/// assert_eq!(optimized.as_plan().from.table, "users");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizedPlan(ValidatedPlan);

impl OptimizedPlan {
    /// Borrows the underlying validated plan.
    pub fn as_validated(&self) -> &ValidatedPlan {
        &self.0
    }

    /// Consumes the wrapper and returns the validated plan.
    pub fn into_validated(self) -> ValidatedPlan {
        self.0
    }

    /// Consumes the wrapper and returns the query plan.
    pub fn into_inner(self) -> Arc<QueryPlan> {
        self.0.into_inner()
    }
}

impl From<ValidatedPlan> for OptimizedPlan {
    fn from(plan: ValidatedPlan) -> Self {
        Self(plan)
    }
}

impl Deref for OptimizedPlan {
    type Target = ValidatedPlan;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Runs all validation stages while sharing an immutable schema snapshot.
///
/// # Examples
///
/// ```
/// use vlorql_core::validate::ValidationPipeline;
/// use vlorql_core::schema::{SchemaSnapshot, DialectProfile, SqlDialect, QueryPlan, Projection, FromClause, TableSchema, ColumnSchema, DataType, SchemaMetadata};
/// use vlorql_core::policy::{PolicyConfig, PolicyEngine};
/// use std::sync::Arc;
///
/// let schema = Arc::new(SchemaSnapshot::new(
///     vec![TableSchema {
///         name: "users".to_owned(),
///         columns: vec![ColumnSchema {
///             name: "id".to_owned(), data_type: DataType::Int,
///             nullable: false, description: None,
///             is_primary_key: true, foreign_key: None,
///         }],
///         description: None, primary_key: Some(vec!["id".to_owned()]),
///     }],
///     SchemaMetadata::default(),
/// ));
/// let pipeline = ValidationPipeline::new(
///     schema,
///     DialectProfile::default(),
///     PolicyEngine::new(PolicyConfig::default()),
/// );
/// let plan = QueryPlan {
///     select: vec![Projection::Column {
///         table: None, column: "id".to_owned(), alias: None,
///     }],
///     from: FromClause { table: "users".to_owned(), alias: None },
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None,
///     joins: None, ctes: None,
/// };
/// assert!(pipeline.validate(&plan).is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct ValidationPipeline {
    schema: ArcSchemaSnapshot,
    dialect: DialectProfile,
    policy: PolicyEngine,
    optimizer: Option<QueryOptimizer>,
}

impl ValidationPipeline {
    /// Creates a complete validation pipeline without an optimizer.
    pub fn new(schema: Arc<SchemaSnapshot>, dialect: DialectProfile, policy: PolicyEngine) -> Self {
        Self {
            schema,
            dialect,
            policy,
            optimizer: None,
        }
    }

    /// Attaches an optional [`QueryOptimizer`] to the pipeline.
    ///
    /// When set, [`Self::validate_and_optimize`] will run the optimizer
    /// after validation succeeds and re-validate policy constraints on
    /// the optimised plan.
    #[must_use]
    pub fn with_optimizer(mut self, optimizer: QueryOptimizer) -> Self {
        self.optimizer = Some(optimizer);
        self
    }

    /// Executes every validation stage and aggregates all returned errors.
    pub fn validate(&self, plan: &QueryPlan) -> Result<ValidatedPlan, ValidationErrors> {
        let mut errors = Vec::new();

        if let Err(stage_errors) = self.validate_schema(plan) {
            extend_unique(&mut errors, stage_errors);
        }
        if let Err(stage_errors) = self.policy.validate(plan, &self.schema) {
            extend_unique(&mut errors, stage_errors);
        }
        if let Err(stage_errors) = OperandValidator::validate(plan, &self.schema) {
            extend_unique(&mut errors, stage_errors);
        }
        if let Err(stage_errors) = DialectValidator::validate(plan, &self.dialect) {
            extend_unique(&mut errors, stage_errors);
        }

        if errors.is_empty() {
            Ok(ValidatedPlan(Arc::new(plan.clone())))
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// Validates a plan and, when an optimizer is configured, applies
    /// optimisation passes to the validated result.
    ///
    /// After optimisation the plan is **re-validated against the policy**
    /// engine to ensure the rewrite rules did not introduce any
    /// unauthorised table or column access.  Schema, operand, and dialect
    /// checks are *not* re-run because the optimiser never introduces
    /// new tables, columns, or expressions that those stages would reject.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationErrors`] when any stage (including the
    /// post-optimisation policy check) fails.
    pub async fn validate_and_optimize(
        &self,
        plan: &QueryPlan,
    ) -> Result<OptimizedPlan, ValidationErrors> {
        let validated = self.validate(plan)?;

        let Some(ref optimizer) = self.optimizer else {
            return Ok(OptimizedPlan::from(validated));
        };

        let optimized_plan = match optimizer.optimize_async(validated.as_plan()).await {
            Ok(p) => p,
            Err(e) => {
                return Err(ValidationErrors::new(vec![e]));
            }
        };

        // Re-validate policy on the optimised plan — the rewrite rules
        // are conservative, but a reorder could, in theory, expose a
        // column that was previously pruned.
        if let Err(stage_errors) = self.policy.validate(&optimized_plan, &self.schema) {
            return Err(ValidationErrors(stage_errors));
        }

        Ok(OptimizedPlan::from(ValidatedPlan(Arc::new(optimized_plan))))
    }

    /// Runs only schema table and column existence validation.
    pub fn validate_schema(&self, plan: &QueryPlan) -> Result<(), Vec<VlorQLError>> {
        validate_schema(plan, &self.schema)
    }

    /// Returns the shared schema snapshot.
    pub fn schema(&self) -> &ArcSchemaSnapshot {
        &self.schema
    }

    /// Returns the dialect profile.
    pub fn dialect(&self) -> &DialectProfile {
        &self.dialect
    }

    /// Returns the policy engine.
    pub fn policy(&self) -> &PolicyEngine {
        &self.policy
    }
}

fn extend_unique(errors: &mut Vec<VlorQLError>, stage_errors: Vec<VlorQLError>) {
    for error in stage_errors {
        if !errors.contains(&error) {
            errors.push(error);
        }
    }
}
