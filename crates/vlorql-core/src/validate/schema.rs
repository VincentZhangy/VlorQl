//! Schema existence validation for query plans.

use crate::errors::{SchemaErrorKind, VlorQLError};
use crate::query::{ColumnReference, QueryScope, collect_plan_references};
use crate::schema::{QueryPlan, SchemaSnapshot};
use serde_json::json;
use std::collections::HashSet;

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
    if let Some(ctes) = &plan.ctes {
        for cte in ctes {
            validate_plan(&cte.query, schema, errors);
        }
    }

    let scope = QueryScope::from_plan(plan);
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
            errors.push(table_not_found_error(&qualifier, schema));
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
                errors.push(table_not_found_error(qualifier, schema));
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
