//! Benchmarks for the [`QueryOptimizer`] pipeline.
//!
//! Measures:
//!   1. Synchronous rewrite cost (constant folding → pushdown → pruning)
//!      on a plan with a 10-table join chain.
//!   2. Async end-to-end cost including join reordering with injected
//!      statistics.
//!   3. Estimated cost difference between the original and optimised
//!      plan.
//!
//! Target: the sync rewrite pipeline should complete in well under 5 ms
//! for a 10-join plan.

use criterion::{criterion_group, criterion_main, Criterion};
use std::sync::Arc;
use vlorql_core::optimizer::{
    ColumnPruning, ConstantFolding, PlanRewriter, PredicatePushdown, QueryOptimizer,
    RewriterPipeline,
};
use vlorql_core::schema::{
    ComparisonOperator, Expression, FromClause, JoinClause, JoinType, Predicate, Projection,
    QueryPlan,
};
use vlorql_core::statistics::{
    ColumnStatistics, DummyStatisticsProvider, StatisticsCatalog, TableStatistics,
};

/// Builds a plan with `n` inner joins forming a chain.
/// `SELECT * FROM t0 JOIN t1 ON t0.id = t1.fk0 JOIN t2 ON t1.id = t2.fk1 …`
fn build_chain_join_plan(n: usize) -> QueryPlan {
    let mut joins = Vec::with_capacity(n.saturating_sub(1));
    for i in 1..n {
        let prev = format!("t{}", i - 1);
        let cur = format!("t{}", i);
        joins.push(JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: cur.clone(),
                alias: None,
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some(prev),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some(cur),
                    column: format!("fk{}", i - 1),
                },
            },
        });
    }

    QueryPlan {
        select: vec![Projection::Star { table: None }],
        from: FromClause {
            table: "t0".to_owned(),
            alias: None,
        },
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: Some(joins),
        ctes: None,
    }
}

/// Builds a statistics catalog with `n` tables, each having `row_count`
/// rows scaling linearly with the index so the reorderer has a clear
/// winner (t0 = 1 row, t1 = 2 rows, …).
fn build_catalog(n: usize) -> StatisticsCatalog {
    let mut catalog = StatisticsCatalog::default();
    for i in 0..n {
        let row_count = (i + 1) as u64;
        let mut table = TableStatistics::default();
        table.row_count = row_count;
        table.columns.insert(
            "id".to_owned(),
            ColumnStatistics {
                distinct_count: row_count,
                null_fraction: 0.0,
                ..ColumnStatistics::default()
            },
        );
        if i > 0 {
            table.columns.insert(
                format!("fk{}", i - 1),
                ColumnStatistics {
                    distinct_count: row_count,
                    null_fraction: 0.0,
                    ..ColumnStatistics::default()
                },
            );
        }
        catalog.tables.insert(format!("t{i}"), table);
    }
    catalog
}

// ---------------------------------------------------------------------------
// Rewrite pipeline only (no join reordering)
// ---------------------------------------------------------------------------

fn bench_sync_rewrite(c: &mut Criterion) {
    let plan = build_chain_join_plan(10);
    let pipeline = RewriterPipeline::new()
        .with(ConstantFolding)
        .with(PredicatePushdown)
        .with(ColumnPruning { schema: None });

    c.bench_function("optimizer/sync_rewrite_10_joins", |bencher| {
        bencher.iter(|| {
            let result = pipeline.rewrite(criterion::black_box(&plan));
            criterion::black_box(result.unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Async end-to-end (rewrite + join reorder)
// ---------------------------------------------------------------------------

fn bench_async_optimize(c: &mut Criterion) {
    let plan = build_chain_join_plan(10);
    let catalog = build_catalog(10);
    let stats_provider = Arc::new(DummyStatisticsProvider::new(catalog));
    let optimizer = QueryOptimizer::new(stats_provider);

    // Use a synchronous wrapper because criterion's async runtime is
    // not available in the simple bench_function API.
    c.bench_function("optimizer/async_optimize_10_joins", |bencher| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        bencher.iter(|| {
            let result = rt.block_on(optimizer.optimize_async(criterion::black_box(&plan)));
            criterion::black_box(result.unwrap());
        });
    });
}

// ---------------------------------------------------------------------------
// Cost comparison: original vs optimised
// ---------------------------------------------------------------------------

fn bench_cost_comparison(c: &mut Criterion) {
    let plan = build_chain_join_plan(10);
    let catalog = build_catalog(10);
    let stats_provider = Arc::new(DummyStatisticsProvider::new(catalog));
    let optimizer = QueryOptimizer::new(stats_provider);
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Pre-compute the original cost.
    let original_cost = rt
        .block_on(optimizer.estimated_cost(&plan))
        .expect("cost should be computable");

    // Optimise and get the new cost.
    let optimized = rt
        .block_on(optimizer.optimize_async(&plan))
        .expect("optimisation should succeed");
    let optimized_cost = rt
        .block_on(optimizer.estimated_cost(&optimized))
        .expect("cost should be computable");

    eprintln!(
        "Cost: original={:.2}, optimised={:.2}, improvement={:.1}%",
        original_cost.total(),
        optimized_cost.total(),
        (1.0 - optimized_cost.total() / original_cost.total()) * 100.0,
    );

    c.bench_function("optimizer/cost_comparison", |bencher| {
        bencher.iter(|| {
            let _ = criterion::black_box(&original_cost);
            let _ = criterion::black_box(&optimized_cost);
        });
    });
}

criterion_group!(
    benches,
    bench_sync_rewrite,
    bench_async_optimize,
    bench_cost_comparison,
);
criterion_main!(benches);