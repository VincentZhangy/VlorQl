//! Access-policy enforcement integration tests.
//!
//! These tests construct a strict [`PolicyConfig`] and verify that the
//! [`VlorQl`] facade refuses any plan that violates it. They use
//! [`VlorQl::validate_only`] for the pure validation checks and the
//! full [`VlorQl::query`] path for the row-filter compilation check.

use serde_json::json;
use std::sync::Arc;
use vlorql::VlorQl;
use vlorql_core::errors::{PolicyErrorKind, VlorQLError};
use vlorql_core::schema::{
    ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection, QueryPlan,
};
use vlorql_llm::MockLlmClient;

use super::common::{base_plan, facade_with, row_filter_policy, snapshot, strict_policy};

// ---------------------------------------------------------------------------
// Table-level enforcement
// ---------------------------------------------------------------------------

/// A plan that selects from the denied `orders` table must fail
/// validation with [`PolicyErrorKind::TableDenied`].
#[tokio::test]
async fn denied_table_surfaces_policy_error() {
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("orders".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "orders".to_owned(),
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
    };

    let facade = facade_with(base_plan(), strict_policy(), "sqlite");
    let errors = facade
        .validate_only(&plan)
        .expect_err("denied table should fail validation");

    let codes: std::collections::HashSet<_> = errors
        .as_slice()
        .iter()
        .map(VlorQLError::error_code)
        .collect();
    assert!(
        codes.contains("P001"),
        "expected P001 (TableDenied) in {codes:?}"
    );
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::TableDenied { table },
            ..
        } if table == "orders"
    )));
}

/// End-to-end via [`VlorQl::query`]: the LLM mock returns a plan that
/// targets `orders`, the facade must surface the policy error and must
/// not retry (policy errors are non-retryable).
#[tokio::test]
async fn query_does_not_compile_when_plan_targets_denied_table() {
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("orders".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "orders".to_owned(),
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
    };

    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(strict_policy())
        .with_llm_client(MockLlmClient::success(plan))
        .with_max_retries(2)
        .build()
        .expect("facade should build");

    let error = facade
        .query("list orders")
        .await
        .expect_err("policy error must surface immediately");
    assert!(matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::TableDenied { .. },
            ..
        }
    ));
    assert!(
        !error.is_retryable(),
        "policy errors must never be retryable"
    );
}

// ---------------------------------------------------------------------------
// Column-level enforcement
// ---------------------------------------------------------------------------

/// Selecting the globally-denied `email` column must fail with
/// [`PolicyErrorKind::ColumnDenied`].
#[tokio::test]
async fn globally_denied_column_surfaces_policy_error() {
    let mut plan = base_plan();
    plan.select.push(Projection::Column {
        table: Some("users".to_owned()),
        column: "email".to_owned(),
        alias: None,
    });

    let facade = facade_with(base_plan(), strict_policy(), "sqlite");
    let errors = facade
        .validate_only(&plan)
        .expect_err("globally denied column should fail validation");

    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::ColumnDenied { table, column },
            ..
        } if table == "users" && column == "email"
    )));
}

/// Selecting a column that is *not* on the per-table allow-list must
/// fail with [`PolicyErrorKind::ColumnDenied`] even if the column
/// exists in the schema snapshot.
#[tokio::test]
async fn column_outside_allowlist_surfaces_policy_error() {
    let mut plan = base_plan();
    plan.select.push(Projection::Column {
        table: Some("users".to_owned()),
        column: "tenant_id".to_owned(),
        alias: None,
    });

    let facade = facade_with(base_plan(), strict_policy(), "sqlite");
    let errors = facade
        .validate_only(&plan)
        .expect_err("column outside allow-list should fail validation");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::ColumnDenied { table, column },
            ..
        } if table == "users" && column == "tenant_id"
    )));
}

// ---------------------------------------------------------------------------
// Row-filter enforcement
// ---------------------------------------------------------------------------
//
// The `VlorQl::query` path does **not** automatically AND-combine a
// policy row filter into the compiled SQL. Callers who want the row
// filter to take effect must invoke `PolicyEngine::apply_row_filters`
// themselves, AND-merge the result with their `WHERE` predicate, and
// pass the augmented plan to the facade. These tests document that
// contract from both directions: the helper that exposes the row
// filter, and a manual merge that demonstrates the end-to-end SQL.

use vlorql_core::compile::{PostgresCompiler, SQLiteCompiler, SqlCompiler};
use vlorql_core::policy::PolicyEngine;
use vlorql_core::validate::ValidatedPlan;

/// `PolicyEngine::apply_row_filters` must surface the `users.id > 10`
/// predicate when a query targets the `users` table.
#[test]
fn apply_row_filters_surfaces_users_filter() {
    let policy = PolicyEngine::new(row_filter_policy());
    let filter = policy
        .apply_row_filters(&base_plan())
        .expect("expected a row filter on `users`");

    // The serialized form should include the `users`.`id` reference and
    // the `>` comparison with the literal `10`.
    let serialized = serde_json::to_string(&filter).expect("filter should serialize");
    assert!(
        serialized.contains(r#""users""#),
        "filter should reference users table, got `{serialized}`"
    );
    assert!(
        serialized.contains(r#""id""#),
        "filter should reference the id column, got `{serialized}`"
    );
    assert!(
        serialized.contains(r#""gt""#),
        "filter should use the `>` operator, got `{serialized}`"
    );
    assert!(
        serialized.contains("10"),
        "filter should bind the literal 10, got `{serialized}`"
    );
}

/// Manually AND-merging a row filter with a plan whose `WHERE` is
/// absent produces a SQL statement that contains the row filter
/// predicate on the PostgreSQL compiler.
#[test]
fn row_filter_combined_with_empty_where_compiles_to_filtered_sql() {
    let policy = PolicyEngine::new(row_filter_policy());
    let mut plan = base_plan();
    let row_filter = policy
        .apply_row_filters(&plan)
        .expect("policy should provide a row filter for the users table");
    plan.r#where = Some(row_filter);

    let compiled = PostgresCompiler
        .compile(&ValidatedPlan(Arc::new(plan)))
        .expect("plan with row filter should compile");
    assert!(
        compiled.sql.contains("WHERE"),
        "compiled SQL should carry the row filter, got `{}`",
        compiled.sql
    );
    assert!(
        compiled.sql.contains(r#""users"."id" > $1"#),
        "compiled SQL should reference the row-filter column, got `{}`",
        compiled.sql
    );
    assert_eq!(compiled.parameters.len(), 1);
    assert_eq!(compiled.parameters[0].value, json!(10));
}

/// Manually AND-merging a row filter with a plan whose `WHERE` is
/// already populated yields a SQL statement that contains **both**
/// predicates on the SQLite compiler (parameterized as `?`).
#[test]
fn row_filter_combines_with_user_supplied_where() {
    let policy = PolicyEngine::new(row_filter_policy());
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
        },
        op: ComparisonOperator::Eq,
        right: Expression::Literal {
            value: json!("alice"),
            data_type: DataType::String,
        },
    });

    let row_filter = policy
        .apply_row_filters(&plan)
        .expect("policy should provide a row filter");
    plan.r#where = Some(Predicate::And {
        left: Box::new(plan.r#where.unwrap()),
        right: Box::new(row_filter),
    });

    let compiled = SQLiteCompiler
        .compile(&ValidatedPlan(Arc::new(plan)))
        .expect("filtered query should compile");
    assert!(
        compiled.sql.contains(r#""users"."id" > ?"#),
        "expected parameterized row-filter predicate in `{}`",
        compiled.sql
    );
    assert!(
        compiled.sql.contains(r#""users"."name" = ?"#),
        "expected parameterized user predicate in `{}`",
        compiled.sql
    );
    assert_eq!(compiled.parameters.len(), 2);
}

// ---------------------------------------------------------------------------
// Combined enforcement
// ---------------------------------------------------------------------------

/// A plan that simultaneously selects an unknown column *and* a denied
/// column should aggregate **both** policy errors.
#[tokio::test]
async fn multiple_policy_violations_are_aggregated() {
    let mut plan = base_plan();
    plan.select.push(Projection::Column {
        table: Some("users".to_owned()),
        column: "email".to_owned(),
        alias: None,
    });
    plan.select.push(Projection::Column {
        table: Some("users".to_owned()),
        column: "tenant_id".to_owned(),
        alias: None,
    });

    let facade = facade_with(base_plan(), strict_policy(), "sqlite");
    let errors = facade
        .validate_only(&plan)
        .expect_err("two policy violations should fail validation");

    let codes: std::collections::HashSet<_> = errors
        .as_slice()
        .iter()
        .map(VlorQLError::error_code)
        .collect();
    // Both `email` (global deny) and `tenant_id` (allow-list) should be
    // reported.
    let column_denials: Vec<_> = errors
        .as_slice()
        .iter()
        .filter_map(|error| match error {
            VlorQLError::Policy {
                kind: PolicyErrorKind::ColumnDenied { table, column },
                ..
            } => Some((table.clone(), column.clone())),
            _ => None,
        })
        .collect();
    assert!(
        column_denials.contains(&("users".to_owned(), "email".to_owned())),
        "expected email denial in {column_denials:?}"
    );
    assert!(
        column_denials.contains(&("users".to_owned(), "tenant_id".to_owned())),
        "expected tenant_id denial in {column_denials:?}"
    );
    assert!(codes.contains("P002"));
}
