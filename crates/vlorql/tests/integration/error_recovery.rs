//! Error recovery and retry-policy integration tests.
//!
//! These tests verify the framework's behaviour around validation
//! error aggregation, retry classification, and short-circuiting on
//! non-retryable categories.

use serde_json::json;
use std::collections::HashMap;
use vlorql::VlorQl;
use vlorql_core::errors::{
    LlmErrorKind, PolicyErrorKind, SchemaErrorKind, ValidationErrorKind, VlorQLError,
};
use vlorql_core::policy::{PolicyConfig, TablePolicy};
use vlorql_core::schema::{
    ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection, QueryPlan,
};
use vlorql_llm::MockLlmClient;

use super::common::{base_plan, facade_with, sequence_client, snapshot, streaming_client};

// ---------------------------------------------------------------------------
// Validation error aggregation
// ---------------------------------------------------------------------------

/// A single plan that simultaneously references a missing table *and*
/// a missing column should produce a [`ValidationErrors`] collection
/// with **two** errors — one for each kind of mistake.
///
/// The plan joins `users` (which exists) with `missing_table` and
/// projects a column that does not exist on `users`. The schema
/// validator surfaces both issues independently.
#[tokio::test]
async fn validation_aggregates_multiple_schema_errors() {
    let mut plan = base_plan();
    plan.select.push(Projection::Column {
        table: Some("users".to_owned()),
        column: "nonexistent".to_owned(),
        alias: None,
    });
    plan.joins = Some(vec![vlorql_core::schema::JoinClause {
        join_type: vlorql_core::schema::JoinType::Inner,
        right_table: FromClause {
            table: "missing_table".to_owned(),
            alias: None,
        },
        on: Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Eq,
            right: Expression::ColumnRef {
                table: Some("missing_table".to_owned()),
                column: "id".to_owned(),
            },
        },
    }]);

    let facade = facade_with(base_plan(), PolicyConfig::default(), "sqlite");
    let errors = facade
        .validate_only(&plan)
        .expect_err("plan with two schema issues should fail validation");

    assert_eq!(
        errors.len(),
        2,
        "expected two aggregated errors, got {}: {errors:?}",
        errors.len()
    );
    let codes: std::collections::HashSet<_> = errors
        .as_slice()
        .iter()
        .map(VlorQLError::error_code)
        .collect();
    assert!(
        codes.contains("S001"),
        "expected TableNotFound in {codes:?}"
    );
    assert!(
        codes.contains("S002"),
        "expected ColumnNotFound in {codes:?}"
    );

    // Each error should carry its own kind-specific data.
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Schema {
            kind: SchemaErrorKind::TableNotFound { table },
            ..
        } if table == "missing_table"
    )));
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Schema {
            kind: SchemaErrorKind::ColumnNotFound { table, column },
            ..
        } if table == "users" && column == "nonexistent"
    )));
}

/// A plan with two operand-type mismatches should produce two
/// independent `TypeMismatch` errors.
#[tokio::test]
async fn validation_aggregates_multiple_type_mismatches() {
    // First mismatch: `name > 1` (string > int).
    let string_gt_int = Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
        },
        op: ComparisonOperator::Gt,
        right: Expression::Literal {
            value: json!(1),
            data_type: DataType::Int,
        },
    };
    // Second mismatch: `id LIKE 'x'` (int LIKE string).
    let int_like_string = Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
        },
        op: ComparisonOperator::Like,
        right: Expression::Literal {
            value: json!("x"),
            data_type: DataType::String,
        },
    };
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::And {
        left: Box::new(string_gt_int),
        right: Box::new(int_like_string),
    });

    let facade = facade_with(base_plan(), PolicyConfig::default(), "sqlite");
    let errors = facade
        .validate_only(&plan)
        .expect_err("two type mismatches should be aggregated");

    let type_errors: Vec<_> = errors
        .as_slice()
        .iter()
        .filter(|error| {
            matches!(
                error,
                VlorQLError::Validation {
                    kind: ValidationErrorKind::TypeMismatch { .. },
                    ..
                }
            )
        })
        .collect();
    assert!(
        type_errors.len() >= 2,
        "expected at least two type-mismatch errors, got {type_errors:?}"
    );
}

// ---------------------------------------------------------------------------
// Retry-classification
// ---------------------------------------------------------------------------

/// Validation errors are retryable. A retryable error from the first
/// plan must trigger another LLM call, and the second valid plan must
/// ultimately be compiled.
#[tokio::test]
async fn validation_errors_are_retryable_and_do_trigger_retry() {
    let invalid = plan_with_invalid_where();
    let valid = base_plan();
    // SequenceClient pops from the back, so the order here means:
    // first call returns `invalid`, second call returns `valid`.
    let client = sequence_client(vec![valid.clone(), invalid]);
    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(PolicyConfig::default())
        .with_llm_client(client)
        .with_max_retries(2)
        .build()
        .expect("facade should build");

    let compiled = facade
        .query("list users")
        .await
        .expect("retryable error should trigger another LLM attempt");
    assert_eq!(compiled.sql, "SELECT \"t1\".\"id\" FROM \"users\" AS \"t1\"");
}

/// When the LLM returns JSON that fails to deserialize as a
/// [`QueryPlan`] (for example, missing the required `from` field), the
/// framework surfaces a retryable [`VlorQLError::Llm`] parse error.
/// This documents the contract that the retry loop will retry such
/// errors and only escalate them once the retry budget is exhausted.
#[tokio::test]
async fn malformed_plan_json_is_classified_as_retryable() {
    // Intentionally malformed payload: `from` is missing, so
    // `serde_json` rejects it before any validation stage runs.
    let partial = r#"{"select":[{"type":"column","table":"users","column":"id"}]}"#;
    let client = streaming_client(vec![partial.to_owned()]);

    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(PolicyConfig::default())
        .with_llm_client(client)
        .with_max_retries(0)
        .build()
        .expect("facade should build");

    let error = facade
        .query("list users")
        .await
        .expect_err("malformed plan JSON should fail parsing");
    assert!(
        matches!(
            error,
            VlorQLError::Llm {
                kind: LlmErrorKind::ParseError { .. },
                ..
            }
        ),
        "expected Llm::ParseError, got {error:?}"
    );
    assert!(error.is_retryable(), "LLM parse errors must be retryable");
}

/// Policy violations are **not** retryable. A plan that selects from a
/// denied table must be surfaced immediately without another LLM call.
#[tokio::test]
async fn policy_violations_short_circuit_retry_loop() {
    let bad_plan = QueryPlan {
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

    let policy = PolicyConfig {
        table_policies: HashMap::from([(
            "orders".to_owned(),
            TablePolicy {
                allowed: false,
                ..TablePolicy::default()
            },
        )]),
        ..PolicyConfig::default()
    };

    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(policy)
        .with_llm_client(MockLlmClient::success(bad_plan))
        .with_max_retries(5)
        .build()
        .expect("facade should build");

    let error = facade
        .query("list orders")
        .await
        .expect_err("policy violation must not be retried");
    assert!(matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::TableDenied { .. },
            ..
        }
    ));
    assert!(
        !error.is_retryable(),
        "policy violations must be classified as non-retryable"
    );
}

/// LLM API errors are retryable but, when the LLM is hard-wired to fail
/// via [`MockLlmClient::failure`], the facade must surface the failure
/// without panicking. The error code must be the documented one.
#[tokio::test]
async fn llm_api_errors_bubble_up_with_documented_code() {
    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(PolicyConfig::default())
        .with_llm_client(MockLlmClient::failure())
        .with_max_retries(3)
        .build()
        .expect("facade should build");

    let error = facade
        .query("anything")
        .await
        .expect_err("mock failure should bubble up");
    assert!(matches!(
        error,
        VlorQLError::Llm {
            kind: LlmErrorKind::ApiError { status: 500, .. },
            ..
        }
    ));
    assert!(error.is_retryable(), "LLM errors are retryable");
}

// ---------------------------------------------------------------------------
// Aggregated retry exhaustion
// ---------------------------------------------------------------------------

/// When the LLM only ever produces plans with the same retryable
/// error, the framework must surface that error after the configured
/// retry budget is exhausted.
#[tokio::test]
async fn retry_loop_exhausts_on_persistent_retryable_error() {
    let invalid = plan_with_invalid_where();
    // Three identical plans: initial attempt + 2 retries = 3 attempts.
    let client = sequence_client(vec![invalid.clone(), invalid.clone(), invalid]);
    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(PolicyConfig::default())
        .with_llm_client(client)
        .with_max_retries(2)
        .build()
        .expect("facade should build");

    let error = facade
        .query("show user ids")
        .await
        .expect_err("exhausted retries should surface the last error");
    assert!(
        matches!(error, VlorQLError::Validation { .. }),
        "expected validation error after retry exhaustion, got {error:?}"
    );
    assert!(error.is_retryable());
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn plan_with_invalid_where() -> QueryPlan {
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
        },
        op: ComparisonOperator::Gt,
        right: Expression::Literal {
            value: json!(1),
            data_type: DataType::Int,
        },
    });
    plan
}
