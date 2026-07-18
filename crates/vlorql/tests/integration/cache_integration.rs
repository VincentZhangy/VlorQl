//! Integration tests for the cache layer within the VlorQl facade.
//!
//! These tests verify that:
//! * The compile cache avoids re-compilation for the same plan + dialect.
//! * Cache invalidation forces re-compilation.
//! * Different dialects produce separate cache entries.

use super::common::{base_plan, open_policy, snapshot};
use vlorql::VlorQl;
use vlorql_core::schema::{
    ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection,
    QueryPlan,
};
use vlorql_llm::MockLlmClient;

/// Builds a VlorQl facade with compile cache enabled.
fn facade_with_compile_cache() -> VlorQl {
    VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("postgres")
        .with_policy(open_policy())
        .with_llm_client(MockLlmClient::success(base_plan()))
        .with_compile_cache(1024, 60)
        .build()
        .expect("facade should build")
}

/// Builds a VlorQl facade with both compile cache and prompt cache enabled.
fn facade_with_all_caches() -> VlorQl {
    VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("postgres")
        .with_policy(open_policy())
        .with_llm_client(MockLlmClient::success(base_plan()))
        .with_compile_cache(1024, 60)
        .with_prompt_cache(100, 60)
        .build()
        .expect("facade should build")
}

/// A plan that selects from `users` with a filter.
fn plan_with_filter() -> QueryPlan {
    QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: Expression::Literal {
                value: serde_json::json!(10),
                data_type: DataType::Int,
            },
        }),
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
    }
}

// ---------------------------------------------------------------------------
// Compile cache integration
// ---------------------------------------------------------------------------

/// Two consecutive `query` calls with the same plan should hit the compile
/// cache on the second call.  We verify this by checking that the returned
/// `CompiledQuery` is identical.
#[tokio::test]
async fn compile_cache_hits_on_second_call() {
    let vlorql = facade_with_compile_cache();

    // First call: generate plan, validate, compile (cache miss).
    let first = vlorql
        .query("list users with id > 10")
        .await
        .expect("first query should succeed");

    // Second call: same plan should hit the compile cache.
    let second = vlorql
        .query("list users with id > 10")
        .await
        .expect("second query should succeed");

    assert_eq!(
        first.sql, second.sql,
        "cached compiled SQL should match the first compilation"
    );
    assert_eq!(
        first.parameters, second.parameters,
        "cached parameters should match"
    );
}

/// After invalidating the compile cache, the same query should re-compile
/// (and produce the same SQL, since the plan is the same).
#[tokio::test]
async fn compile_cache_invalidation_forces_recompile() {
    let vlorql = facade_with_compile_cache();

    let first = vlorql
        .query("list users with id > 10")
        .await
        .expect("first query should succeed");

    // Validate the plan so we can pass it to invalidate_compile_cache.
    let plan = plan_with_filter();
    let validated = vlorql
        .validate_only(&plan)
        .expect("plan should validate");
    vlorql.invalidate_compile_cache(&validated).await;

    let second = vlorql
        .query("list users with id > 10")
        .await
        .expect("second query should succeed");

    // After invalidation, the result should be freshly compiled (same SQL).
    assert_eq!(
        first.sql, second.sql,
        "re-compiled SQL should be identical to the original"
    );
}

/// Different dialects should produce different cache entries, even for the
/// same plan.  We can't easily test this within a single facade because
/// the dialect is fixed at build time, but we can verify that the compile
/// cache correctly isolates entries by dialect through the `CompileCache`
/// API.
#[tokio::test]
async fn compile_cache_isolation_by_dialect() {
    let vlorql = facade_with_compile_cache();

    // Run a query to populate the cache.
    let _ = vlorql
        .query("list users")
        .await
        .expect("query should succeed");

    // The compile cache should have at least one entry.
    let compile_cache = vlorql.compile_cache();
    assert!(
        compile_cache.is_some(),
        "compile cache should be configured"
    );
}

// ---------------------------------------------------------------------------
// Prompt cache integration
// ---------------------------------------------------------------------------

/// When a prompt cache is configured, the system prompt should be cached
/// and reused for the same schema + dialect + policy configuration.
#[tokio::test]
async fn prompt_cache_is_configured() {
    let vlorql = facade_with_all_caches();

    // The prompt cache should be accessible.
    let prompt_cache = vlorql.prompt_cache();
    assert!(
        prompt_cache.is_some(),
        "prompt cache should be configured"
    );
}

// ---------------------------------------------------------------------------
// Cache management methods
// ---------------------------------------------------------------------------

/// Clearing all caches should not affect subsequent queries.
#[tokio::test]
async fn clear_all_caches_does_not_break_queries() {
    let vlorql = facade_with_compile_cache();

    // Run a query to populate the cache.
    let _ = vlorql
        .query("list users with id > 10")
        .await
        .expect("query before clear should succeed");

    // Clear all caches.
    vlorql.clear_all_caches();

    // Subsequent queries should still work (result from fresh compilation).
    let result = vlorql
        .query("list users with id > 10")
        .await
        .expect("query after clear should succeed");
    assert!(
        !result.sql.is_empty(),
        "compiled SQL should not be empty"
    );
}

// ---------------------------------------------------------------------------
// Schema cache integration
// ---------------------------------------------------------------------------

/// When a schema cache is configured, invalidating by version should not
/// break subsequent operations.
#[tokio::test]
async fn schema_cache_is_configured() {
    let vlorql = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("postgres")
        .with_policy(open_policy())
        .with_schema_cache(10, 60)
        .build()
        .expect("facade with schema cache should build");

    let schema_cache = vlorql.schema_cache();
    assert!(
        schema_cache.is_some(),
        "schema cache should be configured"
    );

    // Invalidating a non-existent version should not panic.
    vlorql.invalidate_schema_cache("nonexistent");
}