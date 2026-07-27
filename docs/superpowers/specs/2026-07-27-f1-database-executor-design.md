# F1: DatabaseExecutor 统一执行层 — Design Spec

> **Date:** 2026-07-27
> **Branch:** `feat/0.4.0`
> **Status:** Approved

## Goal

将 `end_to_end_pg.rs`（2776 行）中的 PostgreSQL 执行样板抽象为统一的 `DatabaseExecutor` trait，使得用户只需提供数据库连接即可执行编译后的 SQL 查询。

## Architecture

新建 `crates/vlorql-core/src/execute/` 模块，定义 `DatabaseExecutor` trait 和 `QueryResult` 类型。PostgreSQL 实现放在 `vlorql` facade crate（`crates/vlorql/src/execute/`），feature-gated。

---

## Design

### Trait Definition

```rust
use async_trait::async_trait;
use crate::compile::CompiledQuery;

/// Unified database execution interface.
///
/// Implementations translate a [`CompiledQuery`] into a database-specific
/// execution and return the result as a structured [`QueryResult`].
#[async_trait]
pub trait DatabaseExecutor: Send + Sync {
    /// Executes the compiled query and returns the result set.
    async fn execute(&self, query: &CompiledQuery) -> Result<QueryResult, VlorQLError>;

    /// Optional: introspect the database and return a SchemaSnapshot.
    async fn fetch_schema(&self) -> Result<SchemaSnapshot, VlorQLError> {
        Err(VlorQLError::config(
            ConfigErrorKind::Unsupported,
            json!({"feature": "schema introspection"}),
        ))
    }
}

/// The result of executing a query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// Column names in order.
    pub columns: Vec<String>,
    /// Rows, each as a vector of JSON values.
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Number of rows affected (for INSERT/UPDATE/DELETE).
    pub rows_affected: u64,
}
```

### QueryResult Conversion (from tokio-postgres Row)

```rust
impl TryFrom<&tokio_postgres::Row> for Vec<serde_json::Value> { ... }
impl TryFrom<&tokio_postgres::Statement> for Vec<String> { ... }
```

### PostgreSQL Implementation

```rust
pub struct PgExecutor {
    client: tokio_postgres::Client,
}

#[async_trait]
impl DatabaseExecutor for PgExecutor {
    async fn execute(&self, query: &CompiledQuery) -> Result<QueryResult, VlorQLError> {
        let stmt = self.client.prepare(&query.sql).await.map_err(...)?;
        let params = convert_parameters(&query.parameters)?;
        let rows = self.client.query(&stmt, &params).await.map_err(...)?;
        let columns: Vec<String> = stmt.columns().iter().map(|c| c.name().to_owned()).collect();
        let rows: Vec<Vec<serde_json::Value>> = rows.iter().map(|r| r.into()).collect();
        Ok(QueryResult { columns, rows, rows_affected: 0 })
    }

    async fn execute_update(&self, query: &CompiledQuery) -> Result<u64, VlorQLError> {
        let stmt = self.client.prepare(&query.sql).await.map_err(...)?;
        let params = convert_parameters(&query.parameters)?;
        self.client.execute(&stmt, &params).await.map_err(...)
    }
}
```

### Feature Gating

```toml
# crates/vlorql/Cargo.toml
[features]
default = ["executor-postgres"]
executor-postgres = ["tokio-postgres", "tokio-postgres-rustls"]
executor-mysql = []    # future
executor-sqlite = []   # future
```

### Module Structure

```
crates/vlorql-core/src/execute/
  ├── mod.rs         — DatabaseExecutor trait + QueryResult
  └── conversion.rs  — Parameter/Row conversion helpers

crates/vlorql/src/
  ├── execute/
  │   ├── mod.rs     — re-exports
  │   └── pg.rs      — PgExecutor (cfg(feature = "executor-postgres"))
  └── lib.rs         — VlorQl 添加 with_executor()
```

### VlorQl Facade Integration

```rust
impl VlorQl {
    pub fn with_executor(mut self, executor: Arc<dyn DatabaseExecutor>) -> Self { ... }
    pub async fn run(&self, question: &str) -> Result<QueryResult, VlorQLError> {
        // 1. query → compile (existing)
        let compiled = self.query(question).await?;
        // 2. execute
        self.executor.as_ref().ok_or(...)?.execute(&compiled).await
    }
}
```

### Limits
- PostgreSQL 仅支持 tokio-postgres（暂不添加 sqlx 变体）
- 不支持流式结果（全部加载到内存，适合分析查询）
- 不支持事务控制（每个 `execute` 是独立的）

### Testing

| Test | Description |
|------|-------------|
| `pg_executor_round_trip` | Connect to PG, create table, insert, query, verify (integration test, requires PG) |
| `query_result_serialization` | QueryResult serializes/deserializes correctly |
| `parameter_conversion` | All CompiledQuery parameter types convert correctly |

---

## Non-goals

- MySQL/SQLite 实现（留给后续 PR）
- 连接池管理（用户自己管理 `tokio-postgres` 连接）
- 流式查询结果
- 事务/回滚支持
