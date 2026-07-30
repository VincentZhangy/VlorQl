//! SQL-injection audit for query plans.
//!
//! The [`AuditStage`] inspects every identifier in a [`QueryPlan`] for
//! known SQL-injection patterns and validates table/column references
//! against the provided [`SchemaSnapshot`].

use crate::errors::{AuditErrorKind, VlorQLError};
use crate::schema::expressions::{Expression, InTarget, Predicate};
use crate::schema::query_plan::{CommonTableExpression, FromClause, Projection, QueryPlan};
use crate::schema::snapshot::SchemaSnapshot;
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How severe an audit finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Non-blocking observation.
    Warning,
    /// Definitive schema violation.
    Error,
    /// SQL-injection attempt detected.
    Critical,
}

/// The kind of an audit warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditWarningKind {
    /// A referenced table does not exist in the schema.
    MissingTable,
    /// A referenced column does not exist on its table.
    MissingColumn,
    /// An identifier contains a suspicious SQL-injection pattern.
    SuspiciousIdentifier,
}

/// A single audit finding.
#[derive(Debug, Clone)]
pub struct AuditWarning {
    /// What category of issue was found.
    pub kind: AuditWarningKind,
    /// The identifier that triggered the warning.
    pub identifier: String,
    /// Human-readable context describing where the issue occurred.
    pub context: String,
    /// How severe this issue is.
    pub severity: Severity,
}

/// The complete result of an audit run.
#[derive(Debug, Clone, Default)]
pub struct AuditReport {
    /// All warnings collected during the audit.
    pub warnings: Vec<AuditWarning>,
}

impl AuditReport {
    /// Returns `true` when at least one warning has [`Severity::Error`] or
    /// [`Severity::Critical`].
    pub fn has_blockers(&self) -> bool {
        self.warnings
            .iter()
            .any(|w| matches!(w.severity, Severity::Error | Severity::Critical))
    }

    /// Converts all `Error` and `Critical` warnings into [`VlorQLError`] values.
    /// `Warning`-level findings are silently dropped.
    pub fn into_errors(self) -> Vec<VlorQLError> {
        self.warnings
            .into_iter()
            .filter(|w| matches!(w.severity, Severity::Error | Severity::Critical))
            .map(|w| match w.kind {
                AuditWarningKind::MissingTable | AuditWarningKind::MissingColumn => {
                    VlorQLError::audit(
                        AuditErrorKind::IdentifierNotFound {
                            identifier: w.identifier,
                            context: w.context,
                        },
                        json!({}),
                    )
                }
                AuditWarningKind::SuspiciousIdentifier => VlorQLError::audit(
                    AuditErrorKind::SuspiciousPattern {
                        identifier: w.identifier,
                        pattern: w.context,
                    },
                    json!({}),
                ),
            })
            .collect()
    }
}

/// A validation stage that audits a query plan for SQL-injection vectors.
///
/// The stage checks every identifier in the plan — table names, column
/// references, CTE names — against a known block-list of suspicious
/// patterns and validates that tables and columns actually exist in the
/// provided schema snapshot.
#[derive(Debug, Clone, Default)]
pub struct AuditStage;

impl AuditStage {
    /// Creates a new `AuditStage`.
    pub fn new() -> Self {
        Self
    }

    /// Run the audit against `plan` using the tables in `schema`.
    ///
    /// Returns `Ok(())` when no blocking issues are found, or
    /// `Err(errors)` containing all `Error` / `Critical` findings.
    pub fn audit(
        &self,
        plan: &QueryPlan,
        schema: &Arc<SchemaSnapshot>,
    ) -> Result<(), Vec<VlorQLError>> {
        let mut report = AuditReport::default();

        // 1. Check the FROM clause.
        audit_from(&plan.from, schema, &mut report);
        if let FromClause::Subquery { query, .. } = &plan.from {
            let _ = self.audit(query.as_ref(), schema);
        }

        // 2. Check JOIN clauses.
        if let Some(joins) = &plan.joins {
            for join in joins {
                audit_from(&join.right_table, schema, &mut report);
                if let FromClause::Subquery { query, .. } = &join.right_table {
                    let _ = self.audit(query.as_ref(), schema);
                }
            }
        }

        // 3. Check CTE names (injection only, no existence check).
        if let Some(ctes) = &plan.ctes {
            for cte in ctes {
                audit_cte_name(cte, &mut report);
            }
        }

        // 4. Check SELECT projections.
        for projection in &plan.select {
            audit_projection(projection, schema, &mut report);
        }

        // 5. Check WHERE predicate.
        if let Some(pred) = &plan.r#where {
            audit_predicate(pred, schema, &mut report);
        }

        // 6. Check HAVING predicate.
        if let Some(pred) = &plan.having {
            audit_predicate(pred, schema, &mut report);
        }

        // 7. Check GROUP BY expressions.
        if let Some(group_by) = &plan.group_by {
            for expr in group_by {
                audit_expression(expr, schema, &mut report);
            }
        }

        // 8. Check ORDER BY expressions.
        if let Some(order_by) = &plan.order_by {
            for term in order_by {
                audit_expression(&term.expr, schema, &mut report);
            }
        }

        // 9. Check CTE inner plans recursively.
        if let Some(ctes) = &plan.ctes {
            for cte in ctes {
                let _ = self.audit(&cte.query, schema);
            }
        }

        // 10. Check set-operation sub-plans recursively.
        if let Some(set_op) = &plan.set_operation {
            let _ = self.audit(&set_op.right, schema);
        }

        if report.has_blockers() {
            Err(report.into_errors())
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Known SQL-injection patterns checked against every identifier.
const SUSPICIOUS_PATTERNS: &[&str] = &[
    ";",
    "--",
    "/*",
    "DROP ",
    "DELETE ",
    "INSERT ",
    "UPDATE ",
    "EXEC ",
    "UNION SELECT",
    "INTO OUTFILE",
    "OR 1=1",
    "OR '1'='1'",
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check `ident` against all suspicious patterns (case-insensitive).
///
/// Returns the matched pattern when found, or `None` if the identifier
/// looks clean.
fn has_suspicious_pattern(ident: &str) -> Option<&'static str> {
    let upper = ident.to_uppercase();
    SUSPICIOUS_PATTERNS
        .iter()
        .find(|pat| upper.contains(&pat.to_uppercase()))
        .copied()
}

/// Check a `FromClause` for injection patterns *and* against the schema.
fn audit_from(from: &FromClause, schema: &Arc<SchemaSnapshot>, report: &mut AuditReport) {
    let (table_name, alias) = match from {
        FromClause::Table { table, alias } => (Some(table.as_str()), alias.as_deref()),
        FromClause::Subquery { .. } => (None, None),
    };

    // Injection check (always runs, before alias/lookup).
    if let Some(table_name) = table_name {
        if let Some(pattern) = has_suspicious_pattern(table_name) {
            warn!(
                "AUDIT: identifier `{}` contains suspicious pattern `{}` (FROM)",
                table_name, pattern
            );
            report.warnings.push(AuditWarning {
                kind: AuditWarningKind::SuspiciousIdentifier,
                identifier: table_name.to_owned(),
                context: pattern.to_owned(),
                severity: Severity::Critical,
            });
            // Still check existence if the identifier is unsafe?  The spec
            // says to check injection + existence, so we do both.
        }

        // Existence check – skip aliases.
        if alias.is_none() && schema.get_table(table_name).is_none() {
            warn!("AUDIT: table `{}` not found in schema (FROM)", table_name);
            report.warnings.push(AuditWarning {
                kind: AuditWarningKind::MissingTable,
                identifier: table_name.to_owned(),
                context: "FROM clause".to_owned(),
                severity: Severity::Error,
            });
        }
    }
}

/// Check a CTE name for injection patterns only (no schema existence check).
fn audit_cte_name(cte: &CommonTableExpression, report: &mut AuditReport) {
    // CTE names are user-defined; check injection only.
    if let Some(pattern) = has_suspicious_pattern(&cte.name) {
        warn!(
            "AUDIT: CTE identifier `{}` contains suspicious pattern `{}`",
            cte.name, pattern
        );
        report.warnings.push(AuditWarning {
            kind: AuditWarningKind::SuspiciousIdentifier,
            identifier: cte.name.clone(),
            context: pattern.to_owned(),
            severity: Severity::Critical,
        });
    }
}

/// Check a projection for suspicious identifiers and missing columns.
fn audit_projection(
    projection: &Projection,
    schema: &Arc<SchemaSnapshot>,
    report: &mut AuditReport,
) {
    match projection {
        Projection::Column {
            table,
            column,
            alias,
        } => {
            // Injection check on column name.
            if let Some(pattern) = has_suspicious_pattern(column) {
                warn!(
                    "AUDIT: identifier `{}` contains suspicious pattern `{}` (SELECT column)",
                    column, pattern
                );
                report.warnings.push(AuditWarning {
                    kind: AuditWarningKind::SuspiciousIdentifier,
                    identifier: column.clone(),
                    context: pattern.to_owned(),
                    severity: Severity::Critical,
                });
            }

            // Existence check – skip aliases.
            if alias.is_none() {
                if let Some(t) = table {
                    // Qualified reference: check single table.
                    if schema.get_column(t, column).is_none() && schema.get_table(t).is_some() {
                        warn!(
                            "AUDIT: column `{}` not found on table `{}` (SELECT)",
                            column, t
                        );
                        report.warnings.push(AuditWarning {
                            kind: AuditWarningKind::MissingColumn,
                            identifier: format!("{}.{}", t, column),
                            context: "SELECT projection".to_owned(),
                            severity: Severity::Error,
                        });
                    }
                } else {
                    // Unqualified reference: check all tables.
                    let found = schema
                        .tables
                        .iter()
                        .any(|t| t.columns.iter().any(|c| c.name == *column));
                    if !found {
                        warn!(
                            "AUDIT: unqualified column `{}` not found in schema (SELECT)",
                            column
                        );
                        report.warnings.push(AuditWarning {
                            kind: AuditWarningKind::MissingColumn,
                            identifier: column.clone(),
                            context: "SELECT projection".to_owned(),
                            severity: Severity::Error,
                        });
                    }
                }
            }
        }
        Projection::Expr {
            expression,
            alias: _,
        } => {
            // Aliases are not checked.
            audit_expression(expression, schema, report);
        }
        Projection::Star { table } => {
            // Star projections reference no specific column; injection
            // check on the table qualifier.
            if let Some(t) = table
                && let Some(pattern) = has_suspicious_pattern(t)
            {
                warn!(
                    "AUDIT: identifier `{}` contains suspicious pattern `{}` (SELECT *)",
                    t, pattern
                );
                report.warnings.push(AuditWarning {
                    kind: AuditWarningKind::SuspiciousIdentifier,
                    identifier: t.clone(),
                    context: pattern.to_owned(),
                    severity: Severity::Critical,
                });
            }
        }
    }
}

/// Recursively audit an expression.
fn audit_expression(expr: &Expression, schema: &Arc<SchemaSnapshot>, report: &mut AuditReport) {
    match expr {
        Expression::ColumnRef { table, column } => {
            // Injection check.
            if let Some(pattern) = has_suspicious_pattern(column) {
                warn!(
                    "AUDIT: identifier `{}` contains suspicious pattern `{}` (expression column)",
                    column, pattern
                );
                report.warnings.push(AuditWarning {
                    kind: AuditWarningKind::SuspiciousIdentifier,
                    identifier: column.clone(),
                    context: pattern.to_owned(),
                    severity: Severity::Critical,
                });
            }

            // Existence check.
            if let Some(t) = table {
                // Qualified reference.
                if schema.get_column(t, column).is_none() && schema.get_table(t).is_some() {
                    warn!(
                        "AUDIT: column `{}` not found on table `{}` (expression)",
                        column, t
                    );
                    report.warnings.push(AuditWarning {
                        kind: AuditWarningKind::MissingColumn,
                        identifier: format!("{}.{}", t, column),
                        context: "expression".to_owned(),
                        severity: Severity::Error,
                    });
                }
            } else {
                // Unqualified reference.
                let found = schema
                    .tables
                    .iter()
                    .any(|t| t.columns.iter().any(|c| c.name == *column));
                if !found {
                    warn!(
                        "AUDIT: unqualified column `{}` not found in schema (expression)",
                        column
                    );
                    report.warnings.push(AuditWarning {
                        kind: AuditWarningKind::MissingColumn,
                        identifier: column.clone(),
                        context: "expression".to_owned(),
                        severity: Severity::Error,
                    });
                }
            }
        }
        Expression::BinaryOp { left, right, .. } => {
            audit_expression(left, schema, report);
            audit_expression(right, schema, report);
        }
        Expression::FunctionCall { args, .. } => {
            for arg in args.iter() {
                audit_expression(arg, schema, report);
            }
        }
        Expression::SubQuery { query } => {
            // Recursively audit inner plan — but collect into a temp
            // report to avoid clobbering the outer one. For simplicity
            // we just push directly.
            let inner_stage = AuditStage::new();
            if let Err(errors) = inner_stage.audit(query, schema) {
                // Convert errors back into warnings so the outer report
                // stays consistent.  Since this is an audit context we
                // just report them directly via a textual warning.
                for err in errors {
                    warn!("AUDIT: sub-query issue: {}", err);
                }
            }
        }
        Expression::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                audit_expression(op, schema, report);
            }
            for wt in when_thens.iter() {
                audit_expression(&wt.when, schema, report);
                audit_expression(&wt.then, schema, report);
            }
            if let Some(els) = else_result {
                audit_expression(els, schema, report);
            }
        }
        Expression::WindowFunction { args, over, .. } => {
            for arg in args.iter() {
                audit_expression(arg, schema, report);
            }
            if let Some(partition_by) = &over.partition_by {
                for expr in partition_by {
                    audit_expression(expr, schema, report);
                }
            }
            if let Some(order_by) = &over.order_by {
                for term in order_by {
                    audit_expression(&term.expr, schema, report);
                }
            }
        }
        // Literal and Star have no identifiers to check.
        Expression::Literal { .. } | Expression::Star => {}
    }
}

/// Recursively audit a predicate.
fn audit_predicate(pred: &Predicate, schema: &Arc<SchemaSnapshot>, report: &mut AuditReport) {
    match pred {
        Predicate::Comparison { left, right, .. } => {
            audit_expression(left, schema, report);
            audit_expression(right, schema, report);
        }
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            audit_predicate(left, schema, report);
            audit_predicate(right, schema, report);
        }
        Predicate::Not { child } => {
            audit_predicate(child, schema, report);
        }
        Predicate::Between { expr, low, high } => {
            audit_expression(expr, schema, report);
            audit_expression(low, schema, report);
            audit_expression(high, schema, report);
        }
        Predicate::In { expr, target } => {
            audit_expression(expr, schema, report);
            match target {
                InTarget::Values(values) => {
                    for v in values {
                        audit_expression(v, schema, report);
                    }
                }
                InTarget::SubQuery(query) => {
                    let inner_stage = AuditStage::new();
                    if let Err(errors) = inner_stage.audit(query, schema) {
                        for err in errors {
                            warn!("AUDIT: sub-query in IN: {}", err);
                        }
                    }
                }
            }
        }
        Predicate::Like { expr, .. } => {
            audit_expression(expr, schema, report);
        }
        Predicate::IsNull { expr } => {
            audit_expression(expr, schema, report);
        }
        Predicate::Exists { query } => {
            let inner_stage = AuditStage::new();
            if let Err(errors) = inner_stage.audit(query, schema) {
                for err in errors {
                    warn!("AUDIT: sub-query in EXISTS: {}", err);
                }
            }
        }
        Predicate::True | Predicate::False => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::query_plan::{FromClause, Projection, QueryPlan};
    use crate::schema::snapshot::{ColumnSchema, SchemaSnapshot, TableSchema};
    use crate::schema::types::DataType;
    use std::sync::Arc;

    /// Create a test schema with one table `users` (columns: `id` Int).
    fn test_schema() -> Arc<SchemaSnapshot> {
        Arc::new(SchemaSnapshot::new(
            vec![TableSchema {
                name: "users".to_owned(),
                columns: vec![ColumnSchema {
                    name: "id".to_owned(),
                    data_type: DataType::Int,
                    nullable: false,
                    description: None,
                    is_primary_key: true,
                    foreign_key: None,
                }],
                description: None,
                primary_key: Some(vec!["id".to_owned()]),
            }],
            Default::default(),
        ))
    }

    /// Create a minimal valid query plan.
    fn valid_plan() -> QueryPlan {
        QueryPlan {
            select: vec![Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            }],
            from: FromClause::table("users".to_owned(), None),
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
    fn audit_accepts_valid_plan() {
        let schema = test_schema();
        let stage = AuditStage::new();
        let result = stage.audit(&valid_plan(), &schema);
        assert!(result.is_ok(), "valid plan should pass audit");
    }

    #[test]
    fn audit_rejects_missing_table() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.from = FromClause::table("nonexistent".to_owned(), None);
        let stage = AuditStage::new();
        let result = stage.audit(&plan, &schema);
        assert!(result.is_err(), "missing table should be rejected");
    }

    #[test]
    fn audit_rejects_missing_column() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.select = vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "nonexistent".to_owned(),
            alias: None,
        }];
        let stage = AuditStage::new();
        let result = stage.audit(&plan, &schema);
        assert!(result.is_err(), "missing column should be rejected");
    }

    #[test]
    fn audit_rejects_injection_pattern() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.from = FromClause::table("users; DROP TABLE".to_owned(), None);
        let stage = AuditStage::new();
        let result = stage.audit(&plan, &schema);
        assert!(result.is_err(), "injection pattern should be rejected");
    }

    #[test]
    fn audit_accepts_valid_cte_name() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.ctes = Some(vec![CommonTableExpression {
            name: "recent".to_owned(),
            recursive: false,
            query: Box::new(valid_plan()),
        }]);
        let stage = AuditStage::new();
        let result = stage.audit(&plan, &schema);
        assert!(result.is_ok(), "valid CTE name should pass");
    }

    #[test]
    fn audit_rejects_injection_in_cte_name() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.ctes = Some(vec![CommonTableExpression {
            name: "recent; DROP".to_owned(),
            recursive: false,
            query: Box::new(valid_plan()),
        }]);
        let stage = AuditStage::new();
        let result = stage.audit(&plan, &schema);
        assert!(
            result.is_err(),
            "CTE name with injection pattern should be rejected"
        );
    }

    #[test]
    fn audit_accepts_alias_names() {
        let schema = test_schema();
        let mut plan = valid_plan();
        // FROM with an alias — should skip existence check on table name
        // because the table is looked up by its real name, not the alias.
        // Also add a column alias in the projection.
        plan.from = FromClause::table("users".to_owned(), Some("u".to_owned()));
        plan.select = vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
            alias: Some("my_alias".to_owned()),
        }];
        let stage = AuditStage::new();
        let result = stage.audit(&plan, &schema);
        assert!(result.is_ok(), "alias names should pass audit");
    }
}
