# 6-项代码优化实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 对 VlorQl 项目进行 6 项独立的代码质量优化，消除重复、改进类型安全、减少不必要的克隆。

**架构:** 每项优化针对一个独立子系统，互不依赖，可并行执行。T1-T4 为去重/类型改进，T5 为性能优化，T6 为管道重构。

**Tech Stack:** Rust, async (tokio), sqlx 0.8, moka, reqwest, serde

## Global Constraints

- 所有现有测试必须通过（`cargo test --workspace` 或各 crate 级别）。
- `cargo check --workspace` 不得有警告（现有警告维持，不增加新警告）。
- `deny_unknown_fields` 属性在现有 `#[serde(deny_unknown_fields)]` struct 和 `#[serde(tag = "type", deny_unknown_fields)]` enum 上保持。
- 不破坏 JSON 序列化兼容性 — 新引入的序列化变体需做 `#[serde(skip_serializing)]` / `#[serde(skip_deserializing)]` 或默认值处理。
- 代码修改严格限制在每项任务指定的文件范围内。

---

### Task 1: Provider 代码去重 — 提取 RetryableHttpClient trait

**Files:**
- Create: `crates/vlorql-llm/src/retry_client.rs`
- Modify: `crates/vlorql-llm/src/anthropic.rs`, `crates/vlorql-llm/src/deepseek.rs`, `crates/vlorql-llm/src/zhipu.rs`, `crates/vlorql-llm/src/local.rs`, `crates/vlorql-llm/src/lib.rs`

**Interfaces:**
- Consumes: `LlmClient trait (lib.rs:243)`, `is_retryable (lib.rs:1107)`, `retry_backoff (lib.rs:1246)`, `transport_error (lib.rs:1090)`, `response_message (lib.rs:1128)`, `truncate (lib.rs:1141)`, `sse_lines (lib.rs:1262)`, `drive_sse_consumer (lib.rs:1149)`, `drive_sse_consumer_with (lib.rs:1168)`, `extract_delta_text` (各 provider 自定义)
- Produces:
  - `pub trait RetryableHttpClient: Send + Sync` — 2 个默认方法：`async fn execute_with_retry(...)`（生成模式）和 `async fn execute_stream(...)`（流模式）
  - `pub fn build_http_request(...)` — 统一 HTTP 请求构造

**分析:** 四个 provider 的 `generate_plan` 和 `stream_plan` 有大量重复。
- `generate_plan` 重复模式：Anthropic(L153-193) / DeepSeek(L194-234) / Zhipu(L236-276) — 三者几乎相同；Local(L360-418) 带有额外的 fallback 逻辑。
- `stream_plan` 重复模式：Anthropic(L195-248) / DeepSeek(L236-281) / Zhipu(L278-323) — HTTP 请求 + SSE 消费 + mpsc channel 几乎完全相同；Local(L420-480) 带有 vLLM/Ollama 变体。

**设计决策:** 
- `RetryableHttpClient` trait 提供 `generate_with_retry` 和 `stream_with_sse` 两个默认方法。
- 各 provider 只需实现 `send_request(&self, endpoint, body)` 和 `parse_stream_delta(&self, value) -> Option<String>`。
- `send_request` 返回 `reqwest::Response`，统一处理 transport error。
- `stream_with_sse` 接受 `extract_delta: fn(&Value) -> Option<String>` 作为参数。
- LocalClient 的 `generate_plan` fallback 逻辑保持 override。

- [ ] **Step 1: 创建并实现 `retry_client.rs`**

创建 `crates/vlorql-llm/src/retry_client.rs`：

```rust
//! Retryable HTTP client trait — unified retry logic, error handling,
//! and SSE streaming for all LLM providers.

use crate::{
    is_retryable, response_message, retry_backoff, sse_lines, transport_error, truncate,
    drive_sse_consumer, drive_sse_consumer_with,
};
use async_trait::async_trait;
use futures::stream::Stream;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::warn;
use vlorql_core::errors::{LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;

/// Default retry delay base.
pub(crate) const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(1);

/// A trait for LLM HTTP clients that provides built-in retry and SSE
/// streaming logic.
///
/// Implementors only need to provide:
/// - `max_attempts()` — how many times to retry
/// - `send_request(endpoint, body)` — the actual HTTP call
///
/// The trait provides default implementations for:
/// - `generate_with_retry(...)` — retry loop around `send_request`
/// - `stream_with_sse(...)` — SSE streaming pipeline
#[async_trait]
pub(crate) trait RetryableHttpClient: Send + Sync {
    /// Maximum number of retry attempts.
    fn max_attempts(&self) -> usize;

    /// Provider label for log messages.
    fn provider_label(&self) -> &'static str;

    /// Send an HTTP request and return the raw response.
    /// The implementor is responsible for setting headers and auth.
    async fn send_request(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<reqwest::Response, VlorQLError>;

    /// Execute with retry: call `send_request`, parse the response as a
    /// `QueryPlan`, and retry on retryable errors.
    async fn generate_with_retry(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<QueryPlan, VlorQLError> {
        let max_attempts = self.max_attempts();
        let label = self.provider_label();
        let mut last_error: Option<VlorQLError> = None;

        for attempt in 0..max_attempts {
            let response = self.send_request(endpoint, body).await;
            let result = match response {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        let body_text = resp.text().await.unwrap_or_default();
                        Err(VlorQLError::llm(
                            LlmErrorKind::ApiError {
                                status: status.as_u16(),
                                message: response_message(&body_text),
                            },
                            json!({
                                "status": status.as_u16(),
                                "body": truncate(&body_text, 2048),
                            }),
                        ))
                    } else {
                        let body_text = resp.text().await.unwrap_or_default();
                        crate::parse_llm_response(&body_text)
                    }
                }
                Err(e) => Err(e),
            };

            match result {
                Ok(plan) => return Ok(plan),
                Err(error) => {
                    let can_retry = is_retryable(&error) && attempt + 1 < max_attempts;
                    if !can_retry {
                        return Err(error);
                    }
                    let delay = retry_backoff(DEFAULT_RETRY_DELAY, attempt);
                    warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        ?delay,
                        provider = label,
                        "{label} request failed; retrying"
                    );
                    last_error = Some(error);
                    sleep(delay).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: 0,
                    message: format!("{label} request did not produce a result"),
                },
                json!({"source": label}),
            )
        }))
    }

    /// Start an SSE streaming session from a POST request.
    /// Returns a `Stream<Item = Result<String, VlorQLError>>` of text deltas.
    async fn stream_with_sse(
        &self,
        endpoint: &str,
        body: &Value,
        extract_delta: fn(&Value) -> Option<String>,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        let response = self.send_request(endpoint, body).await?;
        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: status.as_u16(),
                    message: response_message(&body_text),
                },
                json!({
                    "status": status.as_u16(),
                    "body": truncate(&body_text, 2048),
                }),
            ));
        }

        let byte_stream = response.bytes_stream();
        let (tx, rx) = mpsc::unbounded_channel::<Result<String, VlorQLError>>();
        let line_stream = sse_lines(byte_stream);
        let max_attempts = self.max_attempts();
        let retry_base = DEFAULT_RETRY_DELAY;

        tokio::spawn(async move {
            if !drive_sse_consumer_with(line_stream, tx, max_attempts, retry_base, extract_delta).await
            {
                warn!("{} SSE consumer ended before producing content", self.provider_label());
            }
        });

        let output = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        Ok(Box::new(Box::pin(output)))
    }
}
```

- [ ] **Step 2: 重构 AnthropicClient 使用 RetryableHttpClient**

修改 `crates/vlorql-llm/src/anthropic.rs`：
1. 添加 `mod retry_client;` 到 `lib.rs`
2. 为 `AnthropicClient` 实现 `RetryableHttpClient`
3. `send_request` 实现：设置 `x-api-key` + `anthropic-version` 头，POST JSON
4. 重写 `generate_plan`：调用 `self.generate_with_retry(&endpoint, &body)` 
5. 重写 `stream_plan`：调用 `self.stream_with_sse(&endpoint, &body, extract_delta_text)`
6. 导出 `extract_delta_text` 为 `pub(crate)` 函数引用
7. 移除 `send_once`、`max_attempts`（若不再单独需要）、`DEFAULT_RETRY_DELAY`（已移到 retry_client.rs）

```rust
#[async_trait]
impl RetryableHttpClient for AnthropicClient {
    fn max_attempts(&self) -> usize {
        self.config.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    fn provider_label(&self) -> &'static str {
        "anthropic"
    }

    async fn send_request(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<reqwest::Response, VlorQLError> {
        self.client
            .post(endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version, ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|error| transport_error(&error))
    }
}

impl LlmClient for AnthropicClient {
    async fn generate_plan(&self, question: &str, system_prompt: &str, temperature: Option<f32>) -> Result<QueryPlan, VlorQLError> {
        let endpoint = self.endpoint();
        let body = self.build_request_body(question, system_prompt, false, temperature);
        self.generate_with_retry(&endpoint, &body).await
    }

    async fn stream_plan(&self, question: String, system_prompt: String) -> Result<Box<dyn Stream<...>>, VlorQLError> {
        let endpoint = self.endpoint();
        let body = self.build_request_body(&question, &system_prompt, true, None);
        self.stream_with_sse(&endpoint, &body, extract_delta_text).await
    }
    // ...
}
```

- [ ] **Step 3: 重构 DeepSeekClient 使用 RetryableHttpClient**

为 `DeepSeekClient` 实现 `RetryableHttpClient`：
1. `provider_label()` → `"deepseek"`
2. `send_request`：POST + `bearer_auth` + `accept: text/event-stream` 头
3. 同样移除 `send_once`、`max_attempts` 等方法
4. `generate_plan` / `stream_plan` 改为代理到 trait 默认方法
5. 已有 `extract_delta_text` → 改名或保留

- [ ] **Step 4: 重构 ZhipuClient 使用 RetryableHttpClient**

为 `ZhipuClient` 实现 `RetryableHttpClient`：
1. `provider_label()` → `"zhipu"`
2. `send_request`：POST + `bearer_auth` + `accept: text/event-stream` 头
3. `generate_plan` / `stream_plan` 改为代理到 trait 默认方法
4. 移除 `send_once` 等。

- [ ] **Step 5: 重构 LocalClient 使用 RetryableHttpClient**

为 `LocalClient` 实现 `RetryableHttpClient`（注意：LocalClient 的 `generate_plan` 需要 override，因为含 fallback 逻辑）：
1. `provider_label()` → `"local"`
2. `send_request`：POST + 条件 bearer_auth (vLLM) + accept 头
3. `generate_plan`：保持 fallback 逻辑作为 override，但用 `self.generate_with_retry(endpoint, body)` 替代手动 retry 循环
4. `stream_plan`：使用 `self.stream_with_sse` 处理 vLLM，保持 Ollama 的 `drive_ollama_ndjson_consumer`

- [ ] **Step 6: 编译验证并运行测试**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-llm 2>&1
cargo test -p vlorql-llm 2>&1 | tail -20
```

---

### Task 2: Sqlx 执行器统一 — 泛型 SqlxExecutor<P: SqlxPool>

**Files:**
- Create: `crates/vlorql/src/execute/sqlx_executor.rs`
- Modify: `crates/vlorql/src/execute/mysql.rs`, `crates/vlorql/src/execute/sqlite.rs`, `crates/vlorql/src/execute/mod.rs`

**Interfaces:**
- Consumes: `sqlx::MySqlPool`, `sqlx::SqlitePool`, `sqlx::Database::Row`, `DatabaseExecutor trait`, `QueryResult`
- Produces: `SqlxExecutor<P>` where `P: SqlxPool` trait; `pub use SqlxExecutor as MysqlExecutor / SqliteExecutor`

**设计决策:**
- 创建 `SqlxPool` trait，统一 `MySqlPool` 和 `SqlitePool` 的行为
- `SqlxExecutor<P: SqlxPool>` 泛型 struct，`new(url)` 连接 + `execute()` 统一实现
- `row_to_values` 通过关联类型或 trait 方法处理不同 Row 类型
- MySQL 和 SQLite 的 `row_to_values` 签名相同（只是 Row 类型不同）

- [ ] **Step 1: 新建 `sqlx_executor.rs`**

创建 `crates/vlorql/src/execute/sqlx_executor.rs`：

```rust
//! Generic sqlx-based executor for MySQL and SQLite.
//!
//! Wraps an `SqlxPool` and implements the [`DatabaseExecutor`] trait.

use async_trait::async_trait;
use sqlx::{Column, Row, Database};
use vlorql_core::compile::CompiledQuery;
use vlorql_core::errors::{ConfigErrorKind, VlorQLError};
use vlorql_core::execute::{DatabaseExecutor, QueryResult};

/// A pool type that can be used with [`SqlxExecutor`].
pub trait SqlxPool: Sized + Send + Sync {
    /// The corresponding database driver.
    type DB: Database;
    /// The options type for creating a pool.
    type Options: Default + Send;
    /// The row type for this database.
    type DbRow: Row;

    /// Connect to a database URL.
    fn connect(url: &str, opts: &Self::Options) -> impl std::future::Future<Output = Result<Self, sqlx::Error>> + Send;

    /// Fetch all rows for a SQL query.
    fn fetch_all(pool: &Self, sql: &str) -> impl std::future::Future<Output = Result<Vec<Self::DbRow>, sqlx::Error>> + Send;
}

impl SqlxPool for sqlx::MySqlPool {
    type DB = sqlx::MySql;
    type Options = sqlx::mysql::MySqlPoolOptions;
    type DbRow = sqlx::mysql::MySqlRow;

    async fn connect(url: &str, opts: &Self::Options) -> Result<Self, sqlx::Error> {
        opts.connect(url).await
    }

    async fn fetch_all(pool: &Self, sql: &str) -> Result<Vec<Self::DbRow>, sqlx::Error> {
        sqlx::query(sql).fetch_all(pool).await
    }
}

impl SqlxPool for sqlx::SqlitePool {
    type DB = sqlx::Sqlite;
    type Options = sqlx::sqlite::SqlitePoolOptions;
    type DbRow = sqlx::sqlite::SqliteRow;

    async fn connect(url: &str, opts: &Self::Options) -> Result<Self, sqlx::Error> {
        opts.connect(url).await
    }

    async fn fetch_all(pool: &Self, sql: &str) -> Result<Vec<Self::DbRow>, sqlx::Error> {
        sqlx::query(sql).fetch_all(pool).await
    }
}

/// Generic sqlx-backed executor.
pub struct SqlxExecutor<P: SqlxPool> {
    pool: P,
}

impl<P: SqlxPool> SqlxExecutor<P> {
    /// Creates a new executor by connecting to the given database URL.
    pub async fn new(database_url: &str) -> Result<Self, VlorQLError> {
        let pool = P::connect(database_url, &P::Options::default())
            .await
            .map_err(|e| {
                VlorQLError::config(
                    ConfigErrorKind::ConfigFileError {
                        path: "database".into(),
                        reason: format!("failed to connect: {e}"),
                    },
                    serde_json::json!({}),
                )
            })?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl<P: SqlxPool + 'static> DatabaseExecutor for SqlxExecutor<P> {
    async fn execute(&self, query: &CompiledQuery) -> Result<QueryResult, VlorQLError> {
        let rows = P::fetch_all(&self.pool, &query.sql)
            .await
            .map_err(|e| {
                VlorQLError::config(
                    ConfigErrorKind::ConfigFileError {
                        path: "database".into(),
                        reason: format!("query failed: {e}"),
                    },
                    serde_json::json!({}),
                )
            })?;

        let columns: Vec<String> = if rows.is_empty() {
            Vec::new()
        } else {
            rows[0]
                .columns()
                .iter()
                .map(|c| c.name().to_owned())
                .collect()
        };

        let rows_affected = rows.len() as u64;

        let values: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .enumerate()
                    .map(|(i, _)| row_to_values(row, i))
                    .collect()
            })
            .collect();

        Ok(QueryResult {
            columns,
            rows: values,
            rows_affected,
        })
    }
}

/// Converts a row cell at the given index into a `serde_json::Value`.
fn row_to_values<R: Row>(row: &R, i: usize) -> serde_json::Value {
    if let Ok(v) = row.try_get::<i32>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<i64>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<f64>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<String>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<bool>(i) {
        return serde_json::json!(v);
    }
    serde_json::Value::Null
}
```

- [ ] **Step 2: 简化 mysql.rs**

`crates/vlorql/src/execute/mysql.rs` 改为：

```rust
//! MySQL executor (type alias for the generic sqlx executor).

pub type MysqlExecutor = crate::execute::sqlx_executor::SqlxExecutor<sqlx::MySqlPool>;
```

- [ ] **Step 3: 简化 sqlite.rs**

`crates/vlorql/src/execute/sqlite.rs` 改为：

```rust
//! SQLite executor (type alias for the generic sqlx executor).

pub type SqliteExecutor = crate::execute::sqlx_executor::SqlxExecutor<sqlx::SqlitePool>;
```

- [ ] **Step 4: 修改 mod.rs 导出新模块**

`crates/vlorql/src/execute/mod.rs` 添加：
```rust
#[cfg(any(feature = "executor-mysql", feature = "executor-sqlite"))]
mod sqlx_executor;
// 保持原有 feature-gated pub mod mysql/sqlite，但内容改为 type alias
```

- [ ] **Step 5: 编译验证**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql --features "executor-mysql,executor-sqlite" 2>&1
```

---

### Task 3: Predicate 枚举优化 — 添加 True/False 变体

**Files:**
- Modify: `crates/vlorql-core/src/schema/expressions.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/optimize/predicate.rs`
- Modify（可能）: `crates/vlorql-llm/src/parser_v2/builder/expr_builder.rs`（build_predicate 新增 True/False 分支）
- Modify（可能）: `crates/vlorql-llm/src/parser_v2/normalize/pipeline.rs`（若有用到 is_true_predicate）

**分析:**
- 当前 `Predicate` enum 没有 `True`/`False` 变体，用 `Predicate::Comparison { left: Literal(true), op: Eq, right: Literal(true) }` 作为 TRUE 占位，用 `{ left: Literal(false), op: Eq, right: Literal(true) }` 作为 FALSE
- `is_true_predicate` / `is_false_predicate` 通过模式匹配检测这些特殊形式
- `simplify_and_true` / `simplify_and_false` / `simplify_or_true` / `simplify_or_false` / `simplify_or_false` / `simplify_duplicate_and` / `simplify_duplicate_or` 等函数使用 `mem::replace` 交换 Box 内容时，需要构造复杂的 dummy `Predicate::Comparison` 来"占位"
- 添加 `Predicate::True` / `Predicate::False` 变体后：
  - `is_true_predicate` / `is_false_predicate` 简化为直接匹配变体
  - 所有 `mem::replace` 调用点可以消除 — 因为新变更可以作为占位符，或者直接用 `std::mem::take` + Predicate::True 作为默认值
  - `simplify_and_true` 和 `simplify_or_false` 等函数的 `*predicate = std::mem::replace(right, Predicate::True { ... })` 改为 `*predicate = std::mem::take(right)` （配合 `Default` 实现返回 `True`）

- [ ] **Step 1: 为 Predicate 添加 True/False 变体并实现 Default**

在 `expressions.rs` 的 `Predicate` enum 末尾（在 `Exists` 之后）添加：

```rust
/// A constant `true` value (used internally by the optimizer;
/// serialized only when present in a plan).
#[serde(skip_serializing)]
True,
/// A constant `false` value (used internally by the optimizer;
/// serialized only when present in a plan).
#[serde(skip_serializing)]
False,
```

并为 Predicate 实现 `Default`（返回 `Predicate::True`）：

```rust
impl Default for Predicate {
    fn default() -> Self {
        Predicate::True
    }
}
```

- [ ] **Step 2: 简化 `is_true_predicate` / `is_false_predicate`**

在 `predicate.rs` 中：

```rust
fn is_true_predicate(pred: &Predicate) -> bool {
    matches!(pred, Predicate::True)
}

fn is_false_predicate(pred: &Predicate) -> bool {
    matches!(pred, Predicate::False)
}
```

- [ ] **Step 3: 消除所有 `mem::replace` 的 dummy Comparison 构造**

对于 `simplify_and_true`（predicate.rs:95-147）：

```rust
fn simplify_and_true(predicate: &mut Predicate) -> bool {
    if let Predicate::And { left, right } = predicate {
        if matches!(left.as_ref(), Predicate::True) {
            // AND TRUE → keep right
            *predicate = std::mem::take(right);
            return true;
        }
        if matches!(right.as_ref(), Predicate::True) {
            // TRUE AND → keep left
            *predicate = std::mem::take(left);
            return true;
        }
    }
    false
}
```

同样简化：
- `simplify_and_false` — 直接赋值为 `Predicate::False`
- `simplify_or_false` — 使用 `std::mem::take`
- `simplify_or_true` — 直接赋值为 `Predicate::True`
- `simplify_duplicate_and` — 使用 `std::mem::take`
- `simplify_duplicate_or` — 使用 `std::mem::take`
- `fold_constant_comparison` — 赋值为 `Predicate::True` / `Predicate::False`
- `simplify_trivial_comparison` — 赋值为 `Predicate::True` / `Predicate::False`

- [ ] **Step 4: 更新 build_predicate（expr_builder.rs）支持 True/False**

在 `build_predicate` 的 match 中添加分支：

```rust
"true" => Ok(Predicate::True),
"false" => Ok(Predicate::False),
```

- [ ] **Step 5: 编译验证并运行测试**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-core -p vlorql-llm 2>&1
cargo test -p vlorql-llm -- parser_v2::optimize 2>&1 | tail -30
```

---

### Task 4: 配置加载改进 — LlmOverrides 命名结构体

**Files:**
- Modify: `crates/vlorql-cli/src/main.rs`

**分析:**
- 当前 `type LlmOverrides = (Option<String>, Option<String>, Option<String>, Option<String>, usize);` (L18-24) — 5 元组难以阅读
- `build_facade` 中解构为 `let Some((provider, cli_api_key, cli_model, cli_api_base, max_retries)) = llm_overrides` (L195) — 顺序容易出错
- 改为命名 struct 后，字段名自文档化，顺序无关紧要

- [ ] **Step 1: 定义 `LlmOverrides` 结构体**

替换 `type LlmOverrides = (...)` 为：

```rust
/// CLI overrides for LLM configuration.
struct LlmOverrides {
    /// Provider (e.g. "openai", "anthropic", "deepseek", "zhipu").
    provider: LlmProvider,
    /// Optional API key override.
    api_key: Option<String>,
    /// Optional model name override.
    model: Option<String>,
    /// Optional API base URL override.
    api_base: Option<String>,
    /// Maximum retry attempts for LLM calls.
    max_retries: usize,
}
```

- [ ] **Step 2: 更新 CLI 参数解析**

在 clap 参数中，保持不变（仍是 5 个独立参数），但在 `build_facade` 调用处构造结构体：

```rust
// 在生成 llm_overrides 的地方：
Some(LlmOverrides {
    provider,
    api_key: cli_api_key,
    model: cli_model,
    api_base: cli_api_base,
    max_retries,
})
```

- [ ] **Step 3: 更新 `build_facade` 函数签名和内部解构**

```rust
fn build_facade(
    config: FileConfig,
    dialect_override: Option<&str>,
    llm_overrides: Option<LlmOverrides>,
) -> Result<VlorQl> {
    // ...
    if let Some(overrides) = llm_overrides {
        let api_key_env = llm.api_key_env.as_deref().unwrap_or("LLM_API_KEY");
        let api_key = overrides.api_key
            .or_else(|| env::var(api_key_env).ok())
            .filter(|key| !key.trim().is_empty());
        let model = overrides.model.or(llm.model);
        let api_base = overrides.api_base.or(llm.api_base);
        let llm_provider = llm.provider.unwrap_or(overrides.provider);
        // ... rest unchanged
    }
}
```

- [ ] **Step 4: 编译验证**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-cli 2>&1
```

---

### Task 5: LLM 缓存优化 — 返回 Arc<QueryPlan> 避免 plan 克隆

**Files:**
- Modify: `crates/vlorql/src/lib.rs`（query 方法中缓存的使用方式）

**分析:**
- `LlmResponseCache` 内部已存 `Arc<QueryPlan>` (L54)，但 `get()` 返回 `Option<Arc<QueryPlan>>`
- `query()` 中（lib.rs L239-240）：
  ```rust
  let cached_plan: Option<QueryPlan> =
      self.llm_cache.get(&cache_key).await.map(|a| (*a).clone());
  ```
  从 `Arc<QueryPlan>` 克隆整个 plan 再使用
- 又在 L246 `cached_plan.clone()` 克隆了整个 `Option<QueryPlan>`，里面又克隆了 plan
- L342 `self.llm_cache.insert(cache_key, Arc::new(plan.clone())).await;` 插入前又克隆了一次

**优化目标:** 返回 `Option<Arc<QueryPlan>>`，共享 Arc 而不是克隆数据。

- [ ] **Step 1: 修改 `query()` 缓存获取代码**

```rust
// 改为：
let cached_plan: Option<Arc<QueryPlan>> =
    self.llm_cache.get(&cache_key).await;
```

- [ ] **Step 2: 修改 cache hit 使用路径**

```rust
let plan: QueryPlan = if attempt == 0 {
    if let Some(ref cached) = cached_plan {
        (**cached).clone()  // 只需 clone 一次 Arc 的内容
    } else {
        // 从 LLM 获取...
        let result = client.generate_plan(...).await?;
        result
    }
} else {
    // retry...
};
```

> 注意：这里仍需要 clone 一次 Arc 的内容因为后续会修改/变异 plan（validate、compile）。Arc 共享只在只读场景最优。

- [ ] **Step 3: 修改 cache 插入**

```rust
// 插入 Arc<QueryPlan>，避免额外的 plan clone
self.llm_cache
    .insert(cache_key, Arc::new(plan.clone()))
    .await;
```

保持 `plan.clone()` 因为 `plan` 在 insert 后继续用于编译。这个 clone 是必需的。

- [ ] **Step 4: 编译验证**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql 2>&1
cargo test -p vlorql-core -- cache::llm_cache 2>&1 | tail -20
```

---

### Task 6: Normalize 管道重构 — 提取 optional_predicate / build_array_field helper

**Files:**
- Modify: `crates/vlorql-llm/src/parser_v2/builder/query_builder.rs`

**分析:**
- `build_plan_from_obj`（query_builder.rs L39-133）中有 5 个高度重复的模式：

**模式 A — optional_predicate:** (L57-61, L75-79)
```rust
obj.get("where")
    .and_then(|v| if v.is_null() { None } else { Some(v) })
    .map(|v| build_predicate(v).map_err(|e| e.at("where")))
    .transpose()?;
```

**模式 B — build_array_field:** (L63-73, L81-91, L96-105, L107-116)
```rust
obj.get("group_by")
    .and_then(|v| v.as_array())
    .map(|arr| {
        arr.iter()
            .enumerate()
            .map(|(i, v)| build_expression(v).map_err(|e| e.at(&format!("group_by[{i}]"))))
            .collect::<Result<Vec<_>, _>>()
    })
    .filter(|v| !v.as_ref().is_ok_and(|x| x.is_empty()))
    .transpose()?;
```

- [ ] **Step 1: 提取 `optional_predicate` helper**

在 `build_plan_from_obj` 之前（query_builder.rs 中）添加：

```rust
/// Extract an optional predicate field from a JSON object.
fn optional_predicate(
    obj: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<Predicate>, BuildError> {
    obj.get(field)
        .and_then(|v| if v.is_null() { None } else { Some(v) })
        .map(|v| build_predicate(v).map_err(|e| e.at(field)))
        .transpose()
}
```

- [ ] **Step 2: 提取 `build_array_field` helper**

```rust
/// Extract and build an array field from a JSON object, using the given
/// builder function.
fn build_array_field<T>(
    obj: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    builder: fn(&serde_json::Value, usize) -> Result<T, BuildError>,
) -> Result<Option<Vec<T>>, BuildError> {
    obj.get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| builder(v, i).map_err(|e| e.at(&format!("{field}[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .filter(|v: &Result<Vec<_>, _>| !v.as_ref().is_ok_and(|x| x.is_empty()))
        .transpose()
}
```

- [ ] **Step 3: 简化 `build_plan_from_obj`**

```rust
pub fn build_plan_from_obj(obj: &serde_json::Map<String, Value>) -> Result<QueryPlan, BuildError> {
    let select = {
        let arr = req_arr(
            obj.get("select")
                .ok_or_else(|| BuildError::new("select", "missing `select` field"))?,
            "select",
        )?;
        build_projections(arr)?
    };

    let from = build_from_clause(
        obj.get("from")
            .ok_or_else(|| BuildError::new("from", "missing `from` field"))?,
        "from",
    )?;

    let r#where = optional_predicate(obj, "where")?;
    let group_by = build_array_field(obj, "group_by", |v, _i| build_expression(v))?;
    let having = optional_predicate(obj, "having")?;
    let order_by = build_array_field(obj, "order_by", |v, _i| build_order_by_term(v))?;
    let joins = build_array_field(obj, "joins", |v, _i| build_join_clause(v))?;
    let ctes = build_array_field(obj, "ctes", |v, _i| build_cte(v))?;

    // limit, offset remain unchanged...

    Ok(QueryPlan { select, ... })
}
```

- [ ] **Step 4: 编译验证**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-llm 2>&1
cargo test -p vlorql-llm -- parser_v2 2>&1 | tail -20
```
