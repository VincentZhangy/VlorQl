# VlorQl 查询优化器

## 概述

VlorQl 内置一个 **逻辑查询优化器**，它在验证（validation）之后、编译（compilation）之前对已验证的 `QueryPlan` 应用语义等价的重写规则，以降低生成的 SQL 执行成本。

优化器由两阶段组成：

1. **同步重写管道**（RewriterPipeline）—— 应用三个逻辑规则：
   - **常量折叠**（ConstantFolding）：静态计算常量子表达式，如 `20 + 5` → `25`，并简化代数恒等式（`x + 0` → `x`、`x * 1` → `x`、`true AND x` → `x`）。
   - **谓词下推**（PredicatePushdown）：将 `WHERE` 中仅引用单个 CTE 的合取项移到该 CTE 内部，使数据尽早过滤。支持**多层级联**：推入外层 CTE 的条件，如果该 CTE 的 FROM 引用了另一个 CTE，会自动翻译并继续推入内层 CTE。
   - **列剪枝**（ColumnPruning）：移除 CTE 输出中未被外层查询引用的列，减少数据传输量。聚合函数的参数列仅在该聚合结果被外部引用时才保留。
2. **异步连接重排序**（JoinReorderer）—— 基于统计信息估算每张表的行数和列的选择性，重新排列内连接的顺序以最小化总成本。

## 工作原理

```
┌─────────┐    ┌──────────────┐    ┌──────────────┐    ┌───────────┐
│  LLM    │ -> │  Validation  │ -> │  Optimizer   │ -> │  Compiler │
│  Plan   │    │  Pipeline    │    │  (optional)  │    │           │
└─────────┘    └──────────────┘    └──────────────┘    └───────────┘
                                      │
                                      ├─ 常量折叠
                                      ├─ 谓词下推
                                      ├─ 列剪枝
                                      └─ 连接重排序 (async, 需统计信息)
```

优化后的计划被包装为 `OptimizedPlan` 类型，它实现了 `Deref<Target=ValidatedPlan>`，因此可以无缝传递给 `compile_only()` 等现有 API。

## 如何使用

### 1. 通过 VlorQlBuilder 启用

```rust
use std::sync::Arc;
use vlorql::VlorQl;
use vlorql_core::statistics::DummyStatisticsProvider;

let stats = Arc::new(DummyStatisticsProvider::default());
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(policy)
    .with_statistics_provider(stats)  // ← 启用优化器
    .build()?;
```

当调用 `vlorql.query("...")` 时，优化器会自动运行。

### 2. 仅使用重写规则（无需统计信息）

```rust
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(policy)
    // 不调用 with_statistics_provider → 只运行常量折叠/谓词下推/列剪枝
    .build()?;
```

### 3. 直接使用 QueryOptimizer

```rust
use vlorql_core::optimizer::QueryOptimizer;
use vlorql_core::statistics::DummyStatisticsProvider;

let stats = Arc::new(DummyStatisticsProvider::default());
let optimizer = QueryOptimizer::new(stats);

// 同步重写（单轮）
let optimized = optimizer.optimize(&plan)?;

// 固定点迭代（最多 3 轮，直到稳定）
let optimized = optimizer.optimize_repeat(&plan, 3)?;

// 异步重写 + 连接重排序
let optimized = optimizer.optimize_async(&plan).await?;
```

### 4. 固定点迭代（Repeat Until Stable）

常量折叠可能产生新的常量，从而创造新的下推机会；下推后可能让更多列可被裁剪。`repeat_until_stable` 方法循环应用管道规则（最多配置轮数）直到计划不再变化：

```rust
use vlorql_core::optimizer::RewriterPipeline;

let pipeline = RewriterPipeline::new()
    .with(ConstantFolding)
    .with(PredicatePushdown)
    .with(ColumnPruning::new());

// 最多运行 3 轮，提前稳定则提前停止
let optimized = pipeline.repeat_until_stable(&plan, 3)?;
```

实践中 2-3 轮即可捕获所有级联效应。`QueryOptimizer::optimize_repeat()` 也暴露了该方法。

### 5. 多层 CTE 下推

当 CTE 的 FROM 子句引用了另一个 CTE 时，推入外层 CTE 的条件会被自动翻译并继续推入内层 CTE。例如：

```sql
WITH
  cte2 AS (SELECT id, val FROM t2),
  cte1 AS (SELECT * FROM cte2)
SELECT * FROM cte1 WHERE cte1.val > 10
```

优化器会将 `cte1.val > 10` 推入 `cte1`，然后级联推入 `cte2`，使得过滤条件尽早执行。内层 CTE 使用别名（`FROM cte2 AS alias`）时也能正确处理。

## 配置统计信息提供者

统计信息提供者实现了 `StatisticsProvider` trait，负责为优化器提供表行数和列基数等成本估算数据。

### 内置提供者

| 提供者 | 说明 | 使用场景 |
|--------|------|----------|
| `DummyStatisticsProvider` | 内存中的固定数据集 | 测试、开发、无真实统计信息时降级 |
| `ConfigFileStatisticsProvider` | 从 JSON 或 YAML 文件加载 | 手动维护的统计信息快照 |

### 使用 JSON 文件

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

### 使用 YAML 文件

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

### 自定义提供者

实现 `StatisticsProvider` trait：

```rust
use vlorql_core::statistics::{StatisticsProvider, TableStatistics, ColumnStatistics};

struct MyProvider {
    // 自定义数据源，例如数据库连接
}

#[async_trait::async_trait]
impl StatisticsProvider for MyProvider {
    async fn get_table_stats(&self, table: &str) -> Result<Option<TableStatistics>, VlorQLError> {
        // 从数据库或缓存中查询
    }
    async fn get_column_stats(&self, table: &str, column: &str) -> Result<Option<ColumnStatistics>, VlorQLError> {
        // ...
    }
    async fn get_catalog_stats(&self) -> Result<StatisticsCatalog, VlorQLError> {
        // ...
    }
}
```

## 禁用特定优化规则

密优化器允许在运行时开启/关闭特定功能：

```rust
use vlorql_core::optimizer::QueryOptimizer;
use vlorql_core::statistics::DummyStatisticsProvider;

let stats = Arc::new(DummyStatisticsProvider::default());
let optimizer = QueryOptimizer::new(stats)
    .with_join_reorder(false);  // 禁用连接重排序
```

当 `with_join_reorder(false)` 时，`optimize_async()` 仅执行同步重写管道，不会调用 `JoinReorderer`。

## 安全保证

优化器不会破坏策略验证：

- **谓词下推仅作用于用户 `WHERE` 条件**，不会移动由 `PolicyEngine` 附加的行级过滤条件。
- **列剪枝保留主键和外键列**，以确保后续连接的正确性。
- 优化后的计划会**重新通过策略引擎验证**，确保没有引入未授权访问。

## 成本估算

`QueryOptimizer::estimated_cost()` 返回一个三轴 `Cost` 结构（CPU / IO / 内存），可用于比较优化前后的计划成本：

```rust
let before = optimizer.estimated_cost(&plan).await;
let after = optimizer.estimated_cost(&optimized).await;
println!("改善: {:.1}%", (1.0 - after.total() / before.total()) * 100.0);
```

## 性能基准

| 场景 | 目标 | 实测 |
|------|------|------|
| 同步重写（10表连接） | < 5ms | 见 `cargo bench` |
| 异步优化（10表连接 + 重排序） | < 50ms | 见 `cargo bench` |
| 成本改善 | ≥ 30% | 见 `cargo bench` |

运行基准测试：

```bash
cargo bench -p vlorql-core --bench optimizer_bench
```

## 模型限制

- `FromClause` 是一个裸表名，没有内联子查询 —— 下推和剪枝仅作用于 CTE。
- 连接重排序仅支持 `INNER JOIN` 链，不支持 `LEFT` / `RIGHT` / `FULL` / `CROSS JOIN`。
- 当连接的表数超过 `MAX_DP_RELATIONS`（当前为 5）时，DP 搜索退化为贪心算法。
- 多层 CTE 下推仅在同一个 CTE 定义层级内级联，不会跨不同查询块定义的作用域。
- 非等值连接条件（`a.x > b.y`）可重排序，但缺乏列统计信息时会退化为默认选择度。