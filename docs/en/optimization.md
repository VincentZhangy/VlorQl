# VlorQl Query Optimizer

## Overview

VlorQl includes a built-in **logical query optimizer** that applies semantically equivalent rewrite rules to a validated `QueryPlan` after validation and before compilation, to reduce the execution cost of the generated SQL.

The optimizer consists of two phases:

1. **Synchronous Rewriter Pipeline** — applies three logical rules:
   - **Constant Folding**: statically evaluates constant sub-expressions, e.g. `20 + 5` → `25`.
   - **Predicate Pushdown**: moves `WHERE` conjuncts that reference only a single CTE into that CTE's body, enabling early data filtering.
   - **Column Pruning**: removes columns from CTE outputs that are not referenced by the outer query, reducing data transfer volume.
2. **Asynchronous Join Reorderer** — estimates row counts and column selectivity for each table based on statistics, and reorders inner join chains to minimize total cost.

## How It Works

```
┌─────────┐    ┌──────────────┐    ┌──────────────┐    ┌───────────┐
│  LLM    │ -> │  Validation  │ -> │  Optimizer   │ -> │  Compiler │
│  Plan   │    │  Pipeline    │    │  (optional)  │    │           │
└─────────┘    └──────────────┘    └──────────────┘    └───────────┘
                                      │
                                      ├─ Constant Folding
                                      ├─ Predicate Pushdown
                                      ├─ Column Pruning
                                      └─ Join Reordering (async, requires statistics)
```

The optimized plan is wrapped in the `OptimizedPlan` type, which implements `Deref<Target=ValidatedPlan>`, so it can be seamlessly passed to existing APIs like `compile_only()`.

## How to Use

### 1. Enable via VlorQlBuilder

```rust
use std::sync::Arc;
use vlorql::VlorQl;
use vlorql_core::statistics::DummyStatisticsProvider;

let stats = Arc::new(DummyStatisticsProvider::default());
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(policy)
    .with_statistics_provider(stats)  // ← Enables the optimizer
    .build()?;
```

When you call `vlorql.query("...")`, the optimizer runs automatically.

### 2. Use Rewrite Rules Only (No Statistics Required)

```rust
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(policy)
    // Don't call with_statistics_provider → only runs constant folding / predicate pushdown / column pruning
    .build()?;
```

### 3. Use QueryOptimizer Directly

```rust
use vlorql_core::optimizer::QueryOptimizer;
use vlorql_core::statistics::DummyStatisticsProvider;

let stats = Arc::new(DummyStatisticsProvider::default());
let optimizer = QueryOptimizer::new(stats);

// Synchronous rewrite
let optimized = optimizer.optimize(&plan)?;

// Async rewrite + join reordering
let optimized = optimizer.optimize_async(&plan).await?;
```

## Configuring a Statistics Provider

The statistics provider implements the `StatisticsProvider` trait and provides cost estimation data such as table row counts and column cardinality for the optimizer.

### Built-in Providers

| Provider | Description | Use Case |
|----------|-------------|----------|
| `DummyStatisticsProvider` | In-memory fixed dataset | Testing, development, fallback when no real statistics available |
| `ConfigFileStatisticsProvider` | Loads from JSON or YAML file | Manually maintained statistics snapshots |

### Using a JSON File

```json
{
  "tables": {
    "users": {
      "row_count": 1000000,
      "columns": {
        "id": { "distinct_count": 1000000, "null_fraction": 0.0 },
        "email": { "distinct_count": 950000, "null_fraction": 0.05 }
      }
    },
    "orders": {
      "row_count": 50000,
      "columns": {
        "id": { "distinct_count": 50000, "null_fraction": 0.0 },
        "user_id": { "distinct_count": 40000, "null_fraction": 0.0 }
      }
    }
  }
}
```

```rust
use vlorql_core::statistics::ConfigFileStatisticsProvider;

let provider = ConfigFileStatisticsProvider::load("stats.json")?;
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(policy)
    .with_statistics_provider(Arc::new(provider))
    .build()?;
```

### Using a YAML File

```yaml
tables:
  users:
    row_count: 1000000
    columns:
      id:
        distinct_count: 1000000
        null_fraction: 0.0
```

```rust
let provider = ConfigFileStatisticsProvider::load("stats.yaml")?;
```

### Custom Provider

Implement the `StatisticsProvider` trait:

```rust
use vlorql_core::statistics::{StatisticsProvider, TableStatistics, ColumnStatistics};

struct MyProvider {
    // Custom data source, e.g. a database connection
}

#[async_trait::async_trait]
impl StatisticsProvider for MyProvider {
    async fn get_table_stats(&self, table: &str) -> Result<Option<TableStatistics>, VlorQLError> {
        // Query from database or cache
    }
    async fn get_column_stats(&self, table: &str, column: &str) -> Result<Option<ColumnStatistics>, VlorQLError> {
        // ...
    }
    async fn get_catalog_stats(&self) -> Result<StatisticsCatalog, VlorQLError> {
        // ...
    }
}
```

## Disabling Specific Optimization Rules

The optimizer allows enabling/disabling specific features at runtime:

```rust
use vlorql_core::optimizer::QueryOptimizer;
use vlorql_core::statistics::DummyStatisticsProvider;

let stats = Arc::new(DummyStatisticsProvider::default());
let optimizer = QueryOptimizer::new(stats)
    .with_join_reorder(false);  // Disable join reordering
```

When `with_join_reorder(false)` is set, `optimize_async()` only runs the synchronous rewriter pipeline and does not invoke `JoinReorderer`.

## Safety Guarantees

The optimizer does not break policy validation:

- **Predicate pushdown only operates on user `WHERE` conditions** and does not move row-level filters appended by `PolicyEngine`.
- **Column pruning preserves primary key and foreign key columns** to ensure correctness of subsequent joins.
- **The optimized plan is re-validated by the policy engine** to ensure no unauthorized access is introduced.

## Cost Estimation

`QueryOptimizer::estimated_cost()` returns a three-axis `Cost` structure (CPU / IO / Memory), which can be used to compare plan costs before and after optimization:

```rust
let before = optimizer.estimated_cost(&plan).await;
let after = optimizer.estimated_cost(&optimized).await;
println!("Improvement: {:.1}%", (1.0 - after.total() / before.total()) * 100.0);
```

## Performance Benchmarks

| Scenario | Target | Observed |
|----------|--------|----------|
| Synchronous rewrite (10-table join) | < 5ms | See `cargo bench` |
| Async optimization (10-table join + reorder) | < 50ms | See `cargo bench` |
| Cost improvement | ≥ 30% | See `cargo bench` |

Run the benchmark:

```bash
cargo bench -p vlorql-core --bench optimizer_bench
```

## Model Limitations

- `FromClause` is a bare table name, not an inline subquery — pushdown and pruning only operate on CTEs.
- Join reordering only supports `INNER JOIN` chains, not `LEFT` / `RIGHT` / `FULL` / `CROSS JOIN`.
- When the number of joined tables exceeds `MAX_DP_RELATIONS` (currently 5), DP search falls back to a greedy algorithm.