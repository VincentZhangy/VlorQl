//! Schema existence validation for query plans.

use crate::errors::{SchemaErrorKind, VlorQLError};
use crate::query::{ColumnReference, QueryScope, collect_plan_references};
use crate::schema::{Expression, InTarget, Predicate, Projection, QueryPlan, SchemaSnapshot};
use serde_json::json;
use std::collections::HashSet;
use tracing::debug;

/// Checks every base table and column reference against a schema snapshot.
pub fn validate_schema(plan: &QueryPlan, schema: &SchemaSnapshot) -> Result<(), Vec<VlorQLError>> {
    let mut errors = Vec::new();
    validate_plan(plan, schema, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_plan(plan: &QueryPlan, schema: &SchemaSnapshot, errors: &mut Vec<VlorQLError>) {
    validate_plan_with_outer(plan, schema, errors, None);
}

fn validate_plan_with_outer(
    plan: &QueryPlan,
    schema: &SchemaSnapshot,
    errors: &mut Vec<VlorQLError>,
    outer_scope: Option<&QueryScope>,
) {
    if let Some(ctes) = &plan.ctes {
        for cte in ctes {
            validate_plan_with_outer(&cte.query, schema, errors, None);
        }
    }

    let mut scope = QueryScope::from_plan(plan);
    if let Some(outer) = outer_scope {
        debug!(
            "extending scope with outer: {:?}",
            outer.sources.iter().map(|s| &s.table).collect::<Vec<_>>()
        );
        scope.extend_with_outer(outer);
    }
    let mut reported_tables = HashSet::new();
    let mut reported_columns = HashSet::new();

    for source in &scope.sources {
        if !scope.cte_names.contains(&source.table)
            && schema.get_table(&source.table).is_none()
            && reported_tables.insert(source.table.clone())
        {
            errors.push(table_not_found_error(&source.table, schema));
        }
    }

    let references = collect_plan_references(plan);
    for qualifier in references.stars.into_iter().flatten() {
        if scope.resolve_source(&qualifier).is_none() && reported_tables.insert(qualifier.clone()) {
            errors.push(table_not_in_scope_or_not_found(&qualifier, schema));
        }
    }

    for reference in references.columns {
        validate_column_reference(
            &reference,
            &scope,
            schema,
            errors,
            &mut reported_tables,
            &mut reported_columns,
        );
    }

    validate_subqueries_in_plan(plan, schema, errors, &scope);
}

fn validate_column_reference(
    reference: &ColumnReference,
    scope: &QueryScope,
    schema: &SchemaSnapshot,
    errors: &mut Vec<VlorQLError>,
    reported_tables: &mut HashSet<String>,
    reported_columns: &mut HashSet<(String, String)>,
) {
    if let Some(qualifier) = &reference.table {
        let Some(source) = scope.resolve_source(qualifier) else {
            if reported_tables.insert(qualifier.clone()) {
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
        if !table
            .columns
            .iter()
            .any(|column| column.name == reference.column)
        {
            push_column_not_found(
                &table.name,
                &reference.column,
                schema,
                errors,
                reported_columns,
            );
        }
        return;
    }

    let found = scope
        .sources
        .iter()
        .filter(|source| !scope.cte_names.contains(&source.table))
        .filter_map(|source| schema.get_table(&source.table))
        .any(|table| {
            table
                .columns
                .iter()
                .any(|column| column.name == reference.column)
        });
    if found
        || scope
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
    push_column_not_found(table, &reference.column, schema, errors, reported_columns);
}

fn push_column_not_found(
    table: &str,
    column: &str,
    schema: &SchemaSnapshot,
    errors: &mut Vec<VlorQLError>,
    reported_columns: &mut HashSet<(String, String)>,
) {
    if !reported_columns.insert((table.to_owned(), column.to_owned())) {
        return;
    }
    let available_columns = schema
        .get_table(table)
        .map(|table| {
            table
                .columns
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    errors.push(VlorQLError::schema(
        SchemaErrorKind::ColumnNotFound {
            table: table.to_owned(),
            column: column.to_owned(),
        },
        json!({
            "table": table,
            "column": column,
            "available_columns": available_columns,
        }),
    ));
}

/// Returns a `TableNotInScope` error when `table` exists in the schema,
/// or a `TableNotFound` error when `table` is genuinely missing.
fn table_not_in_scope_or_not_found(table: &str, schema: &SchemaSnapshot) -> VlorQLError {
    debug!("table_not_in_scope_or_not_found: {}", table);
    let context = json!({
        "table": table,
        "available_tables": schema
            .tables
            .iter()
            .map(|candidate| candidate.name.as_str())
            .collect::<Vec<_>>(),
    });
    if schema.get_table(table).is_some() {
        VlorQLError::schema(
            SchemaErrorKind::TableNotInScope {
                table: table.to_owned(),
            },
            context,
        )
    } else {
        VlorQLError::schema(
            SchemaErrorKind::TableNotFound {
                table: table.to_owned(),
            },
            context,
        )
    }
}

/// Recursively validate subqueries found in predicates and expressions.
fn validate_subqueries_in_plan(
    plan: &QueryPlan,
    schema: &SchemaSnapshot,
    errors: &mut Vec<VlorQLError>,
    outer_scope: &QueryScope,
) {
    for projection in &plan.select {
        if let Projection::Expr { expression, .. } = projection {
            validate_subqueries_in_expression(expression, schema, errors, outer_scope);
        }
    }
    if let Some(predicate) = &plan.r#where {
        validate_subqueries_in_predicate(predicate, schema, errors, outer_scope);
    }
    if let Some(expressions) = &plan.group_by {
        for expression in expressions {
            validate_subqueries_in_expression(expression, schema, errors, outer_scope);
        }
    }
    if let Some(predicate) = &plan.having {
        validate_subqueries_in_predicate(predicate, schema, errors, outer_scope);
    }
    if let Some(terms) = &plan.order_by {
        for term in terms {
            validate_subqueries_in_expression(&term.expr, schema, errors, outer_scope);
        }
    }
    if let Some(joins) = &plan.joins {
        for join in joins {
            validate_subqueries_in_predicate(&join.on, schema, errors, outer_scope);
        }
    }
}

fn validate_subqueries_in_expression(
    expression: &Expression,
    schema: &SchemaSnapshot,
    errors: &mut Vec<VlorQLError>,
    outer_scope: &QueryScope,
) {
    match expression {
        Expression::SubQuery { query } => {
            validate_plan_with_outer(query, schema, errors, Some(outer_scope));
        }
        Expression::FunctionCall { args, .. } => {
            for argument in args {
                validate_subqueries_in_expression(argument, schema, errors, outer_scope);
            }
        }
        Expression::BinaryOp { left, right, .. } => {
            validate_subqueries_in_expression(left, schema, errors, outer_scope);
            validate_subqueries_in_expression(right, schema, errors, outer_scope);
        }
        _ => {}
    }
}

fn validate_subqueries_in_predicate(
    predicate: &Predicate,
    schema: &SchemaSnapshot,
    errors: &mut Vec<VlorQLError>,
    outer_scope: &QueryScope,
) {
    match predicate {
        Predicate::Comparison { left, right, .. } => {
            validate_subqueries_in_expression(left, schema, errors, outer_scope);
            validate_subqueries_in_expression(right, schema, errors, outer_scope);
        }
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            validate_subqueries_in_predicate(left, schema, errors, outer_scope);
            validate_subqueries_in_predicate(right, schema, errors, outer_scope);
        }
        Predicate::Not { child } => {
            validate_subqueries_in_predicate(child, schema, errors, outer_scope);
        }
        Predicate::Between { expr, low, high } => {
            validate_subqueries_in_expression(expr, schema, errors, outer_scope);
            validate_subqueries_in_expression(low, schema, errors, outer_scope);
            validate_subqueries_in_expression(high, schema, errors, outer_scope);
        }
        Predicate::In { expr, target } => {
            validate_subqueries_in_expression(expr, schema, errors, outer_scope);
            if let InTarget::SubQuery(query) = target {
                validate_plan_with_outer(query, schema, errors, Some(outer_scope));
            }
        }
        Predicate::Exists { query } => {
            validate_plan_with_outer(query, schema, errors, Some(outer_scope));
        }
        Predicate::Like { expr, .. } | Predicate::IsNull { expr } => {
            validate_subqueries_in_expression(expr, schema, errors, outer_scope);
        }
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
