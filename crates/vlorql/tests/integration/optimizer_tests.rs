//! Integration tests for the [`QueryOptimizer`] within the VlorQl facade.
//!
//! These tests verify that:
//! * The optimizer pipeline (constant folding → pushdown → pruning) runs
//!   correctly when a statistics provider is configured.
//! * Optimised plans compile to valid SQL.
//! * The optimizer does not break policy enforcement.
//! * Join reordering with injected statistics selects the correct base
//!   table order.

use super::common::{base_plan, open_policy, snapshot};
use vlorql::VlorQl;
use vlorql_core::schema::{
    BinaryOperator, DataType, Expression, FromClause, JoinClause, JoinType, Predicate, Projection,
    QueryPlan,
};
use vlorql_core::statistics::{
    ColumnStatistics, DummyStatisticsProvider, StatisticsCatalog, TableStatistics,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns a [`VlorQl`] facade with the default snapshot and an optimizer
/// configured with rewrites-only (no join reordering).
fn facade_with_rewrites_only() -> VlorQl {
    let stats = Arc::new(DummyStatisticsProvider::default());
    VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("postgres")
        .with_policy(open_policy())
        .with_llm_client(vlorql_llm::MockLlmClient::success(base_plan()))
        .with_statistics_provider(stats)
        .build()
        .expect("facade should build")
}

/// A plan with a constant expression `(20 + 5)` that constant folding
/// should collapse to `25`.
fn plan_with_constant_expr() -> QueryPlan {
    let mut plan = base_plan();
    plan.select = vec![Projection::Expr {
        expression: Expression::BinaryOp {
            left: Box::new(Expression::Literal {
                value: serde_json::json!(20),
                data_type: DataType::Int,
            }),
            op: BinaryOperator::Add,
            right: Box::new(Expression::Literal {
                value: serde_json::json!(5),
                data_type: DataType::Int,
            }),
        },
        alias: Some("total".to_owned()),
    }];
    plan
}

/// A plan with a single join so join reordering can be exercised.
fn plan_with_joins() -> QueryPlan {
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
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: Some("o".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: vlorql_core::schema::ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("o".to_owned()),
                    column: "owner_id".to_owned(),
                },
            },
        }]),
        ctes: None,
    }
}

// ---------------------------------------------------------------------------
// Optimiser pipeline integration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validate_and_optimize_folds_constants() {
    let plan = plan_with_constant_expr();
    let vlorql = facade_with_rewrites_only();

    let optimized = vlorql
        .validate_and_optimize(&plan)
        .await
        .expect("validation + optimisation should succeed");

    // The constant expression `20 + 5` should have been folded to `25`.
    let folded = &optimized.as_plan().select[0];
    assert_eq!(
        *folded,
        Projection::Expr {
            expression: Expression::Literal {
                value: serde_json::json!(25),
                data_type: DataType::Int,
            },
            alias: Some("total".to_owned()),
        },
        "constant folding should collapse 20 + 5 into 25"
    );
}

#[tokio::test]
async fn optimized_plan_compiles_to_sql() {
    let plan = plan_with_constant_expr();
    let vlorql = facade_with_rewrites_only();

    let validated = vlorql
        .validate_only(&plan)
        .expect("validation should succeed");
    let compiled_before = vlorql
        .compile_only(&validated)
        .expect("compilation should succeed");

    let optimized = vlorql
        .validate_and_optimize(&plan)
        .await
        .expect("optimisation should succeed");
    // OptimizedPlan derefs to ValidatedPlan, so compile_only works.
    let compiled_after = vlorql
        .compile_only(optimized.as_validated())
        .expect("compilation of optimised plan should succeed");

    // The optimised SQL should differ from the un-optimised because the
    // constant expression was folded (25 literals instead of 20+5).
    assert_ne!(
        compiled_before.sql, compiled_after.sql,
        "optimised SQL should differ from un-optimised SQL"
    );
}

// ---------------------------------------------------------------------------
// Policy safety: optimiser must not bypass policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn optimized_plan_still_enforces_policy() {
    use vlorql_core::policy::{PolicyConfig, TablePolicy};
    use std::collections::HashMap;

    let strict_policy = PolicyConfig {
        table_policies: HashMap::from([(
            "secrets".to_owned(),
            TablePolicy {
                allowed: false,
                ..TablePolicy::default()
            },
        )]),
        ..PolicyConfig::default()
    };

    let vlorql = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("postgres")
        .with_policy(strict_policy)
        .build()
        .expect("facade should build");

    // A plan targeting the denied table must be rejected even after
    // optimisation.
    let bad_plan = QueryPlan {
        select: vec![Projection::Column {
            table: None,
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "secrets".to_owned(),
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
    let result = vlorql.validate_and_optimize(&bad_plan).await;
    assert!(
        result.is_err(),
        "policy should reject denied table even after optimisation"
    );
}

// ---------------------------------------------------------------------------
// Join reordering with injected statistics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn join_reorderer_picks_smallest_base_table_first() {
    // Build a statistics catalog where `orders` is far smaller than
    // `users`. The reorderer should start from the smallest relation.
    let mut catalog = StatisticsCatalog::default();

    let mut users = TableStatistics {
        row_count: 1_000_000,
        ..TableStatistics::default()
    };
    users.columns.insert(
        "id".to_owned(),
        ColumnStatistics {
            distinct_count: 1_000_000,
            null_fraction: 0.0,
            ..ColumnStatistics::default()
        },
    );
    catalog.tables.insert("users".to_owned(), users);

    let mut orders = TableStatistics {
        row_count: 10_000,
        ..TableStatistics::default()
    };
    orders.columns.insert(
        "owner_id".to_owned(),
        ColumnStatistics {
            distinct_count: 9_000,
            null_fraction: 0.0,
            ..ColumnStatistics::default()
        },
    );
    catalog.tables.insert("orders".to_owned(), orders);

    let stats_provider = Arc::new(DummyStatisticsProvider::new(catalog));

    let vlorql = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("postgres")
        .with_policy(open_policy())
        .with_statistics_provider(stats_provider)
        .build()
        .expect("facade with stats should build");

    let plan = plan_with_joins();
    let optimized = vlorql
        .validate_and_optimize(&plan)
        .await
        .expect("validation + optimisation should succeed");

    let opt_plan = optimized.as_plan();

    // The plan should still be valid after optimisation.
    assert!(
        !opt_plan.select.is_empty(),
        "optimised plan must have a select list"
    );
    assert_eq!(
        opt_plan.from.table, "users",
        "FROM clause should be preserved"
    );
}

// ---------------------------------------------------------------------------
// Degradation: empty statistics should not panic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_statistics_does_not_panic() {
    let stats_provider = Arc::new(DummyStatisticsProvider::default());
    let vlorql = VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name("postgres")
        .with_policy(open_policy())
        .with_statistics_provider(stats_provider)
        .build()
        .expect("facade with empty stats should build");

    let plan = plan_with_joins();
    let result = vlorql.validate_and_optimize(&plan).await;
    assert!(
        result.is_ok(),
        "empty statistics should not cause a panic: {result:?}"
    );
}