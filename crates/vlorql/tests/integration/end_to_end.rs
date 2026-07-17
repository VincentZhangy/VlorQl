//! End-to-end natural-language → SQL integration tests.
//!
//! These tests exercise the full [`VlorQl`] facade: schema → policy →
//! LLM-driven plan generation → validation → compilation. They use the
//! [`MockLlmClient`] from `vlorql-llm` and the richer sequence /
//! streaming clients defined in [`super::common`], so no real network
//! traffic occurs.

use futures::StreamExt;
use rstest::rstest;
use serde_json::json;
use vlorql::VlorQl;
use vlorql_core::errors::{LlmErrorKind, VlorQLError};
use vlorql_core::schema::{
    ComparisonOperator, DataType, Expression, Predicate, QueryPlan, SqlDialect,
};
use vlorql_llm::MockLlmClient;

use super::common::{
    base_plan, chunked_plan_payload, facade_with, facade_with_streaming_chunks, open_policy,
    sequence_client, snapshot,
};

/// `query()` should ask the LLM for a plan, validate it, and compile
/// it. The resulting [`vlorql::CompiledQuery`] must carry the SQL the
/// compiler produced for the supplied plan.
#[tokio::test]
async fn query_runs_prompt_validation_and_compilation() {
    let facade = facade_with(base_plan(), open_policy(), "sqlite");

    let compiled = facade
        .query("show user ids")
        .await
        .expect("valid mock plan should compile");

    assert_eq!(compiled.dialect, SqlDialect::Sqlite);
    assert_eq!(compiled.sql, "SELECT \"users\".\"id\" FROM \"users\"");
    assert!(compiled.parameters.is_empty());
}

/// A non-default plan that exercises `WHERE` and parameter binding
/// should still flow through `query()` end-to-end. The literal value
/// must end up as a bind parameter (never inline SQL).
#[tokio::test]
async fn query_emits_parameterized_where_clause() {
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
        },
        op: ComparisonOperator::Gt,
        right: Expression::Literal {
            value: json!(5),
            data_type: DataType::Int,
        },
    });

    let facade = facade_with(plan, open_policy(), "postgres");
    let compiled = facade
        .query("users with id > 5")
        .await
        .expect("where clause should compile");

    assert_eq!(compiled.dialect, SqlDialect::Postgres);
    assert_eq!(
        compiled.sql,
        "SELECT \"users\".\"id\" FROM \"users\" WHERE \"users\".\"id\" > $1"
    );
    assert_eq!(compiled.parameters.len(), 1);
    assert_eq!(compiled.parameters[0].value, json!(5));
}

/// `query_stream()` should yield one [`StreamEvent::TextChunk`] per LLM
/// delta and then a final [`StreamEvent::PlanComplete`] carrying the
/// assembled plan.
#[tokio::test]
async fn query_stream_emits_chunks_then_plan_complete() {
    let plan = base_plan();
    let chunks = chunked_plan_payload(&plan);
    let expected_chunk_count = chunks.len();
    let facade = facade_with_streaming_chunks(chunks, open_policy(), "sqlite");

    let mut stream = facade
        .query_stream("list users")
        .await
        .expect("query_stream should succeed");

    let mut chunk_count = 0usize;
    let mut final_plan = None;
    let mut saw_error = None;
    while let Some(item) = stream.next().await {
        match item.expect("event should be Ok") {
            vlorql::StreamEvent::TextChunk(text) => {
                assert!(!text.is_empty(), "chunks should be non-empty");
                chunk_count += 1;
            }
            vlorql::StreamEvent::PlanComplete(plan) => final_plan = Some(*plan),
            vlorql::StreamEvent::Error(error) => saw_error = Some(error),
        }
    }

    assert!(
        saw_error.is_none(),
        "did not expect an error event: {saw_error:?}"
    );
    assert_eq!(
        chunk_count, expected_chunk_count,
        "should receive one chunk per streamed delta"
    );
    assert_eq!(final_plan, Some(plan));
}

/// `query_stream()` must preserve the original chunk ordering: every
/// emitted `TextChunk` arrives before the terminal `PlanComplete`.
#[tokio::test]
async fn query_stream_preserves_chunk_ordering() {
    let plan = base_plan();
    let chunks = chunked_plan_payload(&plan);
    let facade = facade_with_streaming_chunks(chunks.clone(), open_policy(), "sqlite");

    let mut stream = facade
        .query_stream("list users")
        .await
        .expect("query_stream should succeed");

    let mut observed = Vec::new();
    while let Some(item) = stream.next().await {
        match item.expect("event should be Ok") {
            vlorql::StreamEvent::TextChunk(text) => observed.push(text),
            vlorql::StreamEvent::PlanComplete(_) => {
                observed.push(String::from("__PLAN_COMPLETE__"));
            }
            vlorql::StreamEvent::Error(error) => panic!("unexpected error event: {error}"),
        }
    }

    // Every chunk from the LLM must appear, and the PlanComplete marker
    // must be the very last item.
    let mut chunk_iter = chunks.into_iter();
    for observed_chunk in observed.iter().take(observed.len() - 1) {
        assert_eq!(
            observed_chunk,
            chunk_iter
                .next()
                .as_ref()
                .expect("chunk iterator must match observation")
        );
    }
    assert_eq!(
        observed.last().map(String::as_str),
        Some("__PLAN_COMPLETE__"),
        "PlanComplete must be the terminal event"
    );
}

/// `query()` must retry when the first plan is rejected by validation.
/// The retry stops at the first successful plan.
#[tokio::test]
async fn query_retries_after_retryable_validation_error() {
    let invalid = plan_with_bad_where();
    let valid = base_plan();
    // The sequence client pops from the back of the queue, so the last
    // element is returned first. Pre-populate it accordingly.
    let client = sequence_client(vec![valid.clone(), invalid]);
    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(open_policy())
        .with_llm_client(client)
        .with_max_retries(2)
        .build()
        .expect("facade should build");

    let compiled = facade
        .query("show user ids")
        .await
        .expect("second valid plan should be used after retry");
    assert_eq!(compiled.sql, "SELECT \"users\".\"id\" FROM \"users\"");
}

/// `query()` retries the LLM up to the configured `max_retries` value
/// when every attempt returns a retryable validation error. After the
/// budget is exhausted, the framework must surface the last error.
#[tokio::test]
async fn query_exhausts_retries_then_returns_last_error() {
    let invalid = plan_with_bad_where();
    // Three identical invalid plans means the facade must consume two
    // retries (initial + 2 = 3 attempts) before giving up.
    let client = sequence_client(vec![invalid.clone(), invalid.clone(), invalid]);
    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(open_policy())
        .with_llm_client(client)
        .with_max_retries(2)
        .build()
        .expect("facade should build");

    let error = facade
        .query("show user ids")
        .await
        .expect_err("exhausting retries should surface the last error");
    // The exact kind depends on the inner validator, but it must be a
    // validation error (the only retryable category).
    assert!(
        matches!(error, VlorQLError::Validation { .. }),
        "expected validation error, got {error:?}"
    );
    assert!(error.is_retryable());
}

/// `MockLlmClient::failure()` produces a non-retryable LLM error and
/// `query()` must surface it directly without retrying.
#[tokio::test]
async fn query_does_not_retry_non_retryable_llm_errors() {
    let facade = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("sqlite")
        .with_policy(open_policy())
        .with_llm_client(MockLlmClient::failure())
        .with_max_retries(3)
        .build()
        .expect("facade should build");

    let error = facade
        .query("anything")
        .await
        .expect_err("failure mock should bubble up an error");
    assert!(matches!(
        error,
        VlorQLError::Llm {
            kind: LlmErrorKind::ApiError { status: 500, .. },
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// rstest fixtures
// ---------------------------------------------------------------------------

/// rstest parameters for the dialect smoke checks. The full
/// parameterized matrix lives in [`crate::dialect_compilation`]; this
/// rstest block demonstrates the same pattern locally and ensures the
/// `rstest` dependency is exercised by `cargo test --test integration`.
#[rstest]
#[case::sqlite("sqlite", SqlDialect::Sqlite, "\"users\".\"id\"")]
#[case::postgres("postgres", SqlDialect::Postgres, "\"users\".\"id\"")]
#[tokio::test]
async fn dialect_smoke(
    #[case] dialect_name: &str,
    #[case] expected_dialect: SqlDialect,
    #[case] expected_identifier: &str,
) {
    let facade = facade_with(base_plan(), open_policy(), dialect_name);
    let compiled = facade
        .query("list users")
        .await
        .expect("dialect should compile");
    assert_eq!(compiled.dialect, expected_dialect);
    assert!(
        compiled.sql.contains(expected_identifier),
        "expected `{expected_identifier}` in `{}`",
        compiled.sql
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn plan_with_bad_where() -> QueryPlan {
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
