//! Table-, column-, and row-level policy evaluation.

use super::config::{PolicyConfig, RowFilter, TablePolicy};
use crate::errors::{PolicyErrorKind, SchemaErrorKind, VlorQLError};
use crate::query::{
    ColumnReference, QueryScope, QuerySource, collect_plan_references, collect_predicate_references,
};
use crate::schema::{
    Expression, InTarget, Predicate, Projection, QueryPlan, SchemaSnapshot, TableSchema,
};
use serde_json::json;
use std::collections::HashSet;

/// Evaluates query plans against a fixed policy configuration.
///
/// # Examples
///
/// ```
/// use vlorql_core::policy::{PolicyConfig, PolicyEngine};
///
/// let engine = PolicyEngine::new(PolicyConfig::default());
/// assert!(engine.config().table_policies.is_empty());
/// ```
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    config: PolicyConfig,
}

impl PolicyEngine {
    /// Creates an engine from an immutable policy configuration.
    pub fn new(config: PolicyConfig) -> Self {
        Self { config }
    }

    /// Returns the configuration used by this engine.
    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    /// Validates all base-table and column references and collects every violation.
    pub fn validate(
        &self,
        plan: &QueryPlan,
        schema: &SchemaSnapshot,
    ) -> Result<(), Vec<VlorQLError>> {
        let mut errors = Vec::new();
        self.validate_plan(plan, schema, &mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Combines every applicable row filter into one left-associated `AND` predicate.
    ///
    /// The returned predicate contains policy filters only. The caller is responsible
    /// for combining it with the query plan's existing `WHERE` predicate.
    pub fn apply_row_filters(&self, plan: &QueryPlan) -> Option<Predicate> {
        let sources = QueryScope::from_plan(plan).sources;
        let mut filters = Vec::new();

        for source in &sources {
            if let Some(filter) = self
                .get_policy_for_table(&source.table)
                .and_then(|policy| policy.row_filter.as_ref())
            {
                filters.push(filter.condition.clone());
            }
        }

        for filter in &self.config.row_filters {
            if row_filter_matches_sources(filter, &sources) {
                filters.push(filter.condition.clone());
            }
        }

        combine_with_and(filters)
    }

    /// Returns whether a table and column pass the configured policy rules.
    pub fn is_column_allowed(&self, table: &str, column: &str) -> bool {
        if self.is_globally_denied(table, column) {
            return false;
        }

        let Some(policy) = self.get_policy_for_table(table) else {
            return true;
        };
        if !policy.allowed || contains_name(&policy.denied_columns, column) {
            return false;
        }

        match &policy.allowed_columns {
            Some(allowed) => contains_name(allowed, column),
            None => true,
        }
    }

    /// Returns the table-specific row filter, if configured.
    pub fn get_row_filter(&self, table: &str) -> Option<&RowFilter> {
        self.get_policy_for_table(table)?.row_filter.as_ref()
    }

    /// Returns the policy configured for an exact schema table name.
    pub fn get_policy_for_table(&self, table: &str) -> Option<&TablePolicy> {
        self.config.table_policies.get(table)
    }

    fn validate_plan(
        &self,
        plan: &QueryPlan,
        schema: &SchemaSnapshot,
        errors: &mut Vec<VlorQLError>,
    ) {
        self.validate_plan_inner(plan, schema, errors, None);
    }

    fn validate_plan_inner(
        &self,
        plan: &QueryPlan,
        schema: &SchemaSnapshot,
        errors: &mut Vec<VlorQLError>,
        outer_scope: Option<&QueryScope>,
    ) {
        // A CTE's body is an independent query scope and must be checked recursively.
        if let Some(ctes) = &plan.ctes {
            for cte in ctes {
                self.validate_plan_inner(&cte.query, schema, errors, None);
            }
        }

        let mut scope = QueryScope::from_plan(plan);
        if let Some(outer) = outer_scope {
            scope.extend_with_outer(outer);
        }
        let mut reported_tables = HashSet::new();
        let mut reported_columns = HashSet::new();

        for source in &scope.sources {
            if scope.cte_names.contains(&source.table) {
                continue;
            }
            let Some(table_schema) = schema.get_table(&source.table) else {
                if reported_tables.insert(("schema", source.table.clone())) {
                    errors.push(table_not_found_error(&source.table, schema));
                }
                continue;
            };
            self.validate_table_access(table_schema, errors, &mut reported_tables);
        }

        let references = collect_plan_references(plan);
        for star in &references.stars {
            self.validate_star(
                star.as_deref(),
                &scope,
                schema,
                errors,
                &mut reported_tables,
                &mut reported_columns,
            );
        }
        for column in &references.columns {
            self.validate_column_reference(
                column,
                &scope,
                schema,
                errors,
                &mut reported_tables,
                &mut reported_columns,
            );
        }

        self.validate_subqueries_in_plan(plan, schema, errors, &scope);
    }

    fn validate_subqueries_in_plan(
        &self,
        plan: &QueryPlan,
        schema: &SchemaSnapshot,
        errors: &mut Vec<VlorQLError>,
        outer_scope: &QueryScope,
    ) {
        for projection in &plan.select {
            if let Projection::Expr { expression, .. } = projection {
                self.validate_subqueries_in_expression(expression, schema, errors, outer_scope);
            }
        }
        if let Some(predicate) = &plan.r#where {
            self.validate_subqueries_in_predicate(predicate, schema, errors, outer_scope);
        }
        if let Some(expressions) = &plan.group_by {
            for expression in expressions {
                self.validate_subqueries_in_expression(expression, schema, errors, outer_scope);
            }
        }
        if let Some(predicate) = &plan.having {
            self.validate_subqueries_in_predicate(predicate, schema, errors, outer_scope);
        }
        if let Some(terms) = &plan.order_by {
            for term in terms {
                self.validate_subqueries_in_expression(&term.expr, schema, errors, outer_scope);
            }
        }
        if let Some(joins) = &plan.joins {
            for join in joins {
                self.validate_subqueries_in_predicate(&join.on, schema, errors, outer_scope);
            }
        }
    }

    fn validate_subqueries_in_expression(
        &self,
        expression: &Expression,
        schema: &SchemaSnapshot,
        errors: &mut Vec<VlorQLError>,
        outer_scope: &QueryScope,
    ) {
        match expression {
            Expression::SubQuery { query } => {
                self.validate_plan_inner(query, schema, errors, Some(outer_scope));
            }
            Expression::FunctionCall { args, .. } => {
                for argument in args {
                    self.validate_subqueries_in_expression(argument, schema, errors, outer_scope);
                }
            }
            Expression::BinaryOp { left, right, .. } => {
                self.validate_subqueries_in_expression(left, schema, errors, outer_scope);
                self.validate_subqueries_in_expression(right, schema, errors, outer_scope);
            }
            _ => {}
        }
    }

    fn validate_subqueries_in_predicate(
        &self,
        predicate: &Predicate,
        schema: &SchemaSnapshot,
        errors: &mut Vec<VlorQLError>,
        outer_scope: &QueryScope,
    ) {
        match predicate {
            Predicate::Comparison { left, right, .. } => {
                self.validate_subqueries_in_expression(left, schema, errors, outer_scope);
                self.validate_subqueries_in_expression(right, schema, errors, outer_scope);
            }
            Predicate::And { left, right } | Predicate::Or { left, right } => {
                self.validate_subqueries_in_predicate(left, schema, errors, outer_scope);
                self.validate_subqueries_in_predicate(right, schema, errors, outer_scope);
            }
            Predicate::Not { child } => {
                self.validate_subqueries_in_predicate(child, schema, errors, outer_scope);
            }
            Predicate::Between { expr, low, high } => {
                self.validate_subqueries_in_expression(expr, schema, errors, outer_scope);
                self.validate_subqueries_in_expression(low, schema, errors, outer_scope);
                self.validate_subqueries_in_expression(high, schema, errors, outer_scope);
            }
            Predicate::In { expr, target } => {
                self.validate_subqueries_in_expression(expr, schema, errors, outer_scope);
                if let InTarget::SubQuery(query) = target {
                    self.validate_plan_inner(query, schema, errors, Some(outer_scope));
                }
            }
            Predicate::Exists { query } => {
                self.validate_plan_inner(query, schema, errors, Some(outer_scope));
            }
            Predicate::Like { expr, .. } | Predicate::IsNull { expr } => {
                self.validate_subqueries_in_expression(expr, schema, errors, outer_scope);
            }
        }
    }

    fn validate_table_access(
        &self,
        table: &TableSchema,
        errors: &mut Vec<VlorQLError>,
        reported_tables: &mut HashSet<(&'static str, String)>,
    ) {
        let denied = self
            .get_policy_for_table(&table.name)
            .is_some_and(|policy| !policy.allowed);
        if denied && reported_tables.insert(("policy", table.name.clone())) {
            errors.push(VlorQLError::policy(
                PolicyErrorKind::TableDenied {
                    table: table.name.clone(),
                },
                json!({
                    "table": table.name,
                    "reason": "table_policy_denied",
                }),
            ));
        }
    }

    fn validate_star(
        &self,
        qualifier: Option<&str>,
        scope: &QueryScope,
        schema: &SchemaSnapshot,
        errors: &mut Vec<VlorQLError>,
        reported_tables: &mut HashSet<(&'static str, String)>,
        reported_columns: &mut HashSet<(String, String, &'static str)>,
    ) {
        let sources = match qualifier {
            Some(qualifier) => match scope.resolve_source(qualifier) {
                Some(source) => vec![source],
                None => {
                    if reported_tables.insert(("schema", qualifier.to_owned())) {
                        errors.push(table_not_in_scope_or_not_found(qualifier, schema));
                    }
                    return;
                }
            },
            None => scope.sources.iter().collect(),
        };

        for source in sources {
            if scope.cte_names.contains(&source.table) {
                continue;
            }
            let Some(table) = schema.get_table(&source.table) else {
                continue;
            };
            for column in &table.columns {
                self.validate_column_policy(table, &column.name, errors, reported_columns);
            }
        }
    }

    fn validate_column_reference(
        &self,
        reference: &ColumnReference,
        scope: &QueryScope,
        schema: &SchemaSnapshot,
        errors: &mut Vec<VlorQLError>,
        reported_tables: &mut HashSet<(&'static str, String)>,
        reported_columns: &mut HashSet<(String, String, &'static str)>,
    ) {
        if let Some(qualifier) = &reference.table {
            let Some(source) = scope.resolve_source(qualifier) else {
                if reported_tables.insert(("schema", qualifier.clone())) {
                    errors.push(table_not_in_scope_or_not_found(qualifier, schema));
                }
                return;
            };
            if scope.cte_names.contains(&source.table) {
                return;
            }
            let Some(table) = schema.get_table(&source.table) else {
                return;
            };
            self.validate_existing_column(table, &reference.column, errors, reported_columns);
            return;
        }

        let candidates: Vec<_> = scope
            .sources
            .iter()
            .filter(|source| !scope.cte_names.contains(&source.table))
            .filter_map(|source| schema.get_table(&source.table))
            .filter(|table| {
                table
                    .columns
                    .iter()
                    .any(|column| column.name == reference.column)
            })
            .collect();

        if candidates.is_empty() {
            // An unqualified CTE column cannot be resolved without deriving the CTE output
            // schema, which belongs to schema validation rather than policy evaluation.
            if scope
                .sources
                .iter()
                .any(|source| scope.cte_names.contains(&source.table))
            {
                return;
            }
            let table = scope
                .sources
                .first()
                .map_or("<unknown>", |source| source.table.as_str());
            self.push_missing_column(table, &reference.column, schema, errors, reported_columns);
            return;
        }

        // Conservatively enforce policy for every matching table. An ambiguous SQL
        // reference will be rejected later by schema validation, but must not bypass policy.
        for table in candidates {
            self.validate_column_policy(table, &reference.column, errors, reported_columns);
        }
    }

    fn validate_existing_column(
        &self,
        table: &TableSchema,
        column: &str,
        errors: &mut Vec<VlorQLError>,
        reported_columns: &mut HashSet<(String, String, &'static str)>,
    ) {
        if table
            .columns
            .iter()
            .any(|candidate| candidate.name == column)
        {
            self.validate_column_policy(table, column, errors, reported_columns);
        } else {
            self.push_missing_column(
                &table.name,
                column,
                &SchemaView::from_table(table),
                errors,
                reported_columns,
            );
        }
    }

    fn push_missing_column<S: AvailableColumns>(
        &self,
        table: &str,
        column: &str,
        schema: &S,
        errors: &mut Vec<VlorQLError>,
        reported_columns: &mut HashSet<(String, String, &'static str)>,
    ) {
        if reported_columns.insert((table.to_owned(), column.to_owned(), "schema")) {
            errors.push(VlorQLError::schema(
                SchemaErrorKind::ColumnNotFound {
                    table: table.to_owned(),
                    column: column.to_owned(),
                },
                json!({
                    "table": table,
                    "column": column,
                    "available_columns": schema.available_columns(table),
                }),
            ));
        }
    }

    fn validate_column_policy(
        &self,
        table: &TableSchema,
        column: &str,
        errors: &mut Vec<VlorQLError>,
        reported_columns: &mut HashSet<(String, String, &'static str)>,
    ) {
        let reason = self.column_denial_reason(&table.name, column);
        if let Some(reason) = reason
            && reported_columns.insert((table.name.clone(), column.to_owned(), reason))
        {
            errors.push(VlorQLError::policy(
                PolicyErrorKind::ColumnDenied {
                    table: table.name.clone(),
                    column: column.to_owned(),
                },
                json!({
                    "table": table.name,
                    "column": column,
                    "reason": reason,
                }),
            ));
        }
    }

    fn column_denial_reason(&self, table: &str, column: &str) -> Option<&'static str> {
        if self.is_globally_denied(table, column) {
            return Some("global_denied_column");
        }

        let policy = self.get_policy_for_table(table)?;
        if !policy.allowed {
            return None;
        }
        if contains_name(&policy.denied_columns, column) {
            return Some("table_denied_column");
        }
        if policy
            .allowed_columns
            .as_ref()
            .is_some_and(|allowed| !contains_name(allowed, column))
        {
            return Some("column_not_in_allowlist");
        }
        None
    }

    fn is_globally_denied(&self, table: &str, column: &str) -> bool {
        self.config.global_denied_columns.iter().any(|denied| {
            denied.len() == column.len() && denied == column
                || denied.len() > table.len() + 1
                    && denied.starts_with(table)
                    && denied.as_bytes().get(table.len()) == Some(&b'.')
                    && &denied[table.len() + 1..] == column
        })
    }
}

fn table_not_found_error(table: &str, schema: &SchemaSnapshot) -> VlorQLError {
    VlorQLError::schema(
        SchemaErrorKind::TableNotFound {
            table: table.to_owned(),
        },
        json!({
            "table": table,
            "available_tables": schema
                .tables
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
        }),
    )
}

fn table_not_in_scope_or_not_found(table: &str, schema: &SchemaSnapshot) -> VlorQLError {
    let context = json!({
        "table": table,
        "available_tables": schema
            .tables
            .iter()
            .map(|candidate| candidate.name.as_str())
            .collect::<Vec<_>>(),
    });
    if schema.get_table(table).is_some() {
        VlorQLError::schema(SchemaErrorKind::TableNotInScope { table: table.to_owned() }, context)
    } else {
        VlorQLError::schema(SchemaErrorKind::TableNotFound { table: table.to_owned() }, context)
    }
}

trait AvailableColumns {
    fn available_columns(&self, table: &str) -> Vec<&str>;
}

impl AvailableColumns for SchemaSnapshot {
    fn available_columns(&self, table: &str) -> Vec<&str> {
        self.get_table(table)
            .map(|table| {
                table
                    .columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct SchemaView<'a> {
    table: &'a TableSchema,
}

impl<'a> SchemaView<'a> {
    fn from_table(table: &'a TableSchema) -> Self {
        Self { table }
    }
}

impl AvailableColumns for SchemaView<'_> {
    fn available_columns(&self, table: &str) -> Vec<&str> {
        if self.table.name == table {
            self.table
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect()
        } else {
            Vec::new()
        }
    }
}

fn contains_name(names: &[String], candidate: &str) -> bool {
    names.iter().any(|name| name == candidate)
}

fn combine_with_and(filters: Vec<Predicate>) -> Option<Predicate> {
    filters.into_iter().reduce(|left, right| Predicate::And {
        left: Box::new(left),
        right: Box::new(right),
    })
}

fn row_filter_matches_sources(filter: &RowFilter, sources: &[QuerySource]) -> bool {
    let mut references = Vec::new();
    collect_predicate_references(&filter.condition, &mut references);
    let qualified_tables: HashSet<_> = references
        .iter()
        .filter_map(|reference| reference.table.as_deref())
        .collect();

    if qualified_tables.is_empty() {
        return !sources.is_empty();
    }

    qualified_tables.iter().all(|qualifier| {
        sources.iter().any(|source| {
            source.table == **qualifier || source.alias.as_deref() == Some(*qualifier)
        })
    })
}
