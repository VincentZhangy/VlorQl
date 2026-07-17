//! End-to-end validation pipeline orchestration.

use super::dialect::DialectValidator;
use super::operand::OperandValidator;
use super::schema::validate_schema;
use crate::errors::{ValidationErrors, VlorQLError};
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
}

impl ValidationPipeline {
    /// Creates a complete validation pipeline.
    pub fn new(schema: Arc<SchemaSnapshot>, dialect: DialectProfile, policy: PolicyEngine) -> Self {
        Self {
            schema,
            dialect,
            policy,
        }
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
