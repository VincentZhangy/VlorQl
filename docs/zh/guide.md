# VlorQl 用户指南

本指南将引导您完成将 VlorQl 集成到 Rust 应用程序中的所有步骤。假设您已具备可用的 Rust 1.75+ 工具链以及一个希望通过自然语言查询暴露的数据库。

> 需要**运维**指导？请参阅 [`deployment.md`](./deployment.md) 了解 vLLM/Ollama 的部署和生成环境调优。

---

## 1. 快速开始

### 1.1 定义 Schema

Schema 是表及其列的列表。您可以通过内省代码构建它，或为了示例而硬编码：

```rust
use std::sync::Arc;
use vlorql_core::schema::{
    ColumnSchema, DataType, SchemaMetadata, SchemaSnapshot, TableSchema,
};

let schema = Arc::new(SchemaSnapshot::new(
    vec![TableSchema {
        name: "users".to_owned(),
        columns: vec![
            ColumnSchema {
                name: "id".to_owned(),
                data_type: DataType::Int,
                nullable: false,
                description: Some("用户标识符".to_owned()),
                is_primary_key: true,
                foreign_key: None,
            },
            ColumnSchema {
                name: "name".to_owned(),
                data_type: DataType::String,
                nullable: false,
                description: Some("显示名称".to_owned()),
                is_primary_key: false,
                foreign_key: None,
            },
        ],
        description: Some("应用用户".to_owned()),
        primary_key: Some(vec!["id".to_owned()]),
    }],
    SchemaMetadata::default(),
));
```

### 1.2 构建 Facade

```rust
use vlorql::{LlmConfig, LlmProvider, VlorQl};
use vlorql_core::policy::PolicyConfig;
use vlorql_llm::create_llm_client;

let client = create_llm_client(LlmConfig {
    provider: LlmProvider::OpenAi,
    api_key: Some(std::env::var("OPENAI_API_KEY")?),
    model: "gpt-4o-mini".to_owned(),
    ..LlmConfig::default()
})?;

let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(PolicyConfig::default())
    .with_llm_client(client)
    .build()?;
```

### 1.3 运行您的第一个问题

```rust
let compiled = vlorql.query("显示最近10个用户").await?;
println!("SQL:     {}", compiled.sql);
println!("参数:  {:?}", compiled.parameters);
```

这就是完整的流水线。LLM 被要求输出一个 `QueryPlan` JSON 对象，然后对该计划进行 schema、策略、操作数类型和方言配置的验证，最后将验证后的计划编译为带有 `$1`、`?` 或 `` ` `` 占位符的参数化 SQL。

---

## 2. 配置

VlorQl 通过三个互补的结构体进行配置。

### 2.1 `LlmConfig`（Provider、模型、重试）

`LlmConfig` 是与 LLM 通信的唯一配置来源：

| 字段 | 类型 | 用途 |
|------|------|------|
| `provider` | `LlmProvider` | `OpenAi` / `Anthropic` / `DeepSeek` / `Zhipu` / `Vllm` / `Ollama` |
| `api_key` | `Option<String>` | API 密钥（也可从环境变量读取，见下文） |
| `api_base` | `Option<String>` | 覆盖默认端点 |
| `model` | `String` | Provider 特定的模型标识符 |
| `max_tokens` | `u32` | LLM 可输出的最大 token 数 |
| `temperature` | `f32` | `0.0` 表示确定性输出 |
| `timeout_seconds` | `u64` | 每次请求的 HTTP 超时时间 |
| `max_retries` | `u32` | 临时 HTTP 错误（5xx、429、超时）的重试次数 |
| `extra` | `HashMap<String, Value>` | Provider 特定的覆盖参数（例如 `"backend": "ollama"`） |

工厂函数 [`vlorql_llm::create_llm_client`] 首先检查 `api_key`，然后回退到文档化的环境变量：

| Provider | 环境变量 |
|----------|----------|
| Anthropic | `ANTHROPIC_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Zhipu | `ZHIPU_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| vLLM | _不需要_ |
| Ollama | _不需要_ |

### 2.2 `DialectProfile`（SQL 特性）

`DialectProfile` 描述了验证器应允许且编译器应输出的 SQL 特性。通过 [`DialectProfile::builder`] 从默认值构建：

```rust
use vlorql_core::schema::{DialectProfile, IdentifierQuoting, JoinType, SqlDialect};

let profile = DialectProfile::builder()
    .dialect(SqlDialect::Postgres)
    .max_joins(3usize)
    .supports_cte(true)
    .allowed_join_types(vec![JoinType::Inner, JoinType::Left])
    .allowed_functions(vec!["count".to_owned(), "sum".to_owned()])
    .denied_functions(vec!["pg_sleep".to_owned()])
    .allow_distinct(true)
    .supports_offset(true)
    .build()?;
```

构建器将任何未设置的字段保留为 [`DialectProfile::default`] 值。方言感知的编译标志（如占位符风格和标识符引用）从 `dialect` 字段中选取。

**方言特定行为：**

| 方言 | 占位符 | 标识符引用 | 分页 | 备注 |
|------|--------|------------|------|------|
| Postgres | `$1`, `$2` | `"双引号"` | `LIMIT n OFFSET m` | 默认。支持 `ILIKE` 运算符。 |
| SQLite | `?` | `"双引号"` | `LIMIT -1 OFFSET m` | `OFFSET` 无 `LIMIT` 时使用 `LIMIT -1`。 |
| MySQL | `?` | `` `反引号` `` | `LIMIT m, n` / `LIMIT m, 18446744073709551615` | `FULL JOIN` 在编译时被拒绝。`OFFSET` 无 `LIMIT` 时使用 `BIGINT UNSIGNED` 最大值作为哨兵。 |

### 2.3 `PolicyConfig`（访问控制）

`PolicyConfig` 是一个自由格式的策略包。默认策略允许访问每个表和每个可见列。通过逐表的 [`TablePolicy`] 条目、`global_denied_columns` 列表以及强制性的行过滤器来收紧权限。

```rust
use vlorql_core::policy::{PolicyConfig, RowFilter, TablePolicy};
use vlorql_core::schema::{ComparisonOperator, DataType, Expression, Predicate};
use std::collections::HashMap;

let mut table_policies = HashMap::new();
table_policies.insert("users".to_owned(), TablePolicy {
    allowed: true,
    allowed_columns: Some(vec!["id".to_owned(), "email".to_owned()]),
    denied_columns: vec!["password_hash".to_owned()],
    row_filter: Some(RowFilter {
        condition: Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: Expression::Literal {
                value: serde_json::json!(0),
                data_type: DataType::Int,
            },
        },
        description: "租户隔离".to_owned(),
    }),
});
let policy = PolicyConfig {
    table_policies,
    global_denied_columns: vec!["password_hash".to_owned()],
    ..PolicyConfig::default()
};
```

参见 [§4](#4-策略配置) 了解完整示例。

---

## 3. 多 Provider 设置

VlorQl 为六个 Provider 提供了开箱即用的客户端。在 `LlmConfig::provider` 中修改一行即可切换。

### 3.1 托管 Provider（Anthropic, DeepSeek, OpenAI, Zhipu）

每个托管客户端共享相同的 JSON 契约，只有网络协议不同。以下示例假设相关环境变量已在环境中设置。

**Anthropic Claude：**

```rust
use vlorql::{LlmConfig, LlmProvider, VlorQl};
use vlorql_llm::create_llm_client;

let config = LlmConfig {
    provider: LlmProvider::Anthropic,
    api_key: Some(std::env::var("ANTHROPIC_API_KEY")?),
    model: "claude-sonnet-4-5".to_owned(),
    max_tokens: 4096,
    ..LlmConfig::default()
};
let client = create_llm_client(config)?;
```

**DeepSeek：**

```rust
let config = LlmConfig {
    provider: LlmProvider::DeepSeek,
    api_key: Some(std::env::var("DEEPSEEK_API_KEY")?),
    model: "deepseek-v4-pro".to_owned(),   // `deepseek-chat` / `deepseek-reasoner` 已弃用
    max_tokens: 4096,
    ..LlmConfig::default()
};
```

**Zhipu GLM：**

```rust
let config = LlmConfig {
    provider: LlmProvider::Zhipu,
    api_key: Some(std::env::var("ZHIPU_API_KEY")?),
    model: "glm-4.7".to_owned(),
    max_tokens: 4096,
    ..LlmConfig::default()
};
```

### 3.2 本地 Provider（vLLM, Ollama）

`vLLM` 和 `Ollama` 的选择方式相同，`api_key` 是可选的，`api_base` 默认为标准本地 URL（vLLM 为 `http://localhost:8000/v1`，Ollama 为 `http://localhost:11434`）。

**vLLM（兼容 OpenAI）：**

```rust
let config = LlmConfig {
    provider: LlmProvider::Vllm,
    api_key: Some("not-required".to_owned()),   // 或 None
    api_base: Some("http://gpu-host.internal:8000/v1".to_owned()),
    model: "Qwen/Qwen2.5-7B-Instruct".to_owned(),
    max_tokens: 4096,
    ..LlmConfig::default()
};
```

**Ollama：**

```rust
use serde_json::json;
use std::collections::HashMap;

let mut extra = HashMap::new();
extra.insert("backend".to_owned(), json!("ollama"));

let config = LlmConfig {
    provider: LlmProvider::Ollama,
    api_key: None,
    api_base: Some("http://localhost:11434".to_owned()),
    model: "llama3.2".to_owned(),
    max_tokens: 4096,
    extra,
    ..LlmConfig::default()
};
```

请参阅 `crates/vlorql-llm/examples/` 下的可运行示例：

```bash
# 通过 LLM_PROVIDER 环境变量切换 Provider
export LLM_PROVIDER=deepseek
export DEEPSEEK_API_KEY=sk-...
cargo run -p vlorql-llm --example multi_provider -- "列出用户 ID"

# vLLM
vllm serve Qwen/Qwen2.5-7B-Instruct --port 8000 --guided-decoding-backend xgrammar
cargo run -p vlorql-llm --example local_vllm -- "列出用户 ID"

# Ollama
ollama serve
ollama pull llama3.2
cargo run -p vlorql-llm --example local_ollama -- "列出用户 ID"
```

---

## 4. 策略配置

策略由 [`PolicyEngine`] 在 schema 验证之后、操作数和方言检查之前进行评估。它们以三个独立的检查层作用于同一个计划：

| 层级 | 位置 | 阻止的内容 |
|------|------|-----------|
| 表 | [`TablePolicy::allowed`] | 引用被拒绝表的查询 |
| 列 | `TablePolicy::allowed_columns` / `denied_columns` / `global_denied_columns` | 读取不允许的列 |
| 行 | `TablePolicy::row_filter` / `row_filters` | 绕过强制条件的查询 |

### 4.1 允许访问（默认）

```rust
let policy = PolicyConfig::default();
let engine = PolicyEngine::new(policy);
// 任何通过 schema 检查的计划都被允许。
```

### 4.2 拒绝表

```rust
let mut table_policies = HashMap::new();
table_policies.insert("secrets".to_owned(), TablePolicy {
    allowed: false,
    ..TablePolicy::default()
});
let policy = PolicyConfig {
    table_policies,
    ..PolicyConfig::default()
};
```

从 `secrets` 表中选择的计划将被拒绝，返回 `PolicyErrorKind::TableDenied`。

### 4.3 列允许列表 / 拒绝列表

```rust
table_policies.insert("users".to_owned(), TablePolicy {
    allowed: true,
    allowed_columns: Some(vec!["id".to_owned(), "email".to_owned()]),
    denied_columns: vec!["password_hash".to_owned()],
    ..TablePolicy::default()
});
```

`allowed_columns` 是一个正向允许列表（LLM 可以引用的唯一列）。`denied_columns` 是一个黑名单（比 `allowed_columns: None` + 移除特定敏感列更强）。

### 4.4 全局拒绝列

```rust
let policy = PolicyConfig {
    global_denied_columns: vec!["password_hash".to_owned()],
    ..PolicyConfig::default()
};
```

`global_denied_columns` 条目应用于每个表；匹配区分大小写，接受纯列名（`password_hash`）和 `table.column` 限定名。

### 4.5 强制行过滤器

```rust
table_policies.insert("users".to_owned(), TablePolicy {
    row_filter: Some(RowFilter {
        condition: Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "tenant_id".to_owned(),
            },
            op: ComparisonOperator::Eq,
            right: Expression::Literal {
                value: serde_json::json!("current-tenant"),
                data_type: DataType::String,
            },
        },
        description: "租户隔离".to_owned(),
    }),
    ..TablePolicy::default()
});
```

`PolicyEngine::apply_row_filters(plan)` 返回一个组合谓词，调用者可以将其拼接到计划的 `WHERE` 子句中。验证流水线**不会**自动执行此操作——操作者通常在 `validate_only` 返回 `ValidatedPlan` 后将谓词附加到计划中。

### 4.6 组合多个违规

策略引擎永远不会快速失败。一个同时违反表和列策略的计划会收集所有错误：

```text
[
  Policy { kind: TableDenied { table: "users" }, ... },
  Policy { kind: ColumnDenied { table: "accounts", column: "owner_id" }, ... },
]
```

`VlorQl::query` 会用 LLM 重试一次（如果错误是 `is_retryable`），然后将最终的错误列表作为 [`ValidationErrors`] 返回。

---

## 5. 流式查询

对于交互式 UI，`VlorQl::query_stream` 返回一个 `Stream<Item = Result<StreamEvent, VlorQLError>>`，它在 LLM 输出时产生原始文本增量，随后是最终的 `PlanComplete`（或 `Error`）事件。

```rust
use futures::StreamExt;
use vlorql::StreamEvent;

let mut stream = vlorql.query_stream("显示用户 ID").await?;
let mut combined = String::new();
while let Some(item) = stream.next().await {
    match item? {
        StreamEvent::TextChunk(chunk) => {
            combined.push_str(&chunk);
            print!("[chunk] {chunk}");
        }
        StreamEvent::PlanComplete(plan) => {
            println!("\nplan = {}", serde_json::to_string_pretty(&plan)?);
        }
        StreamEvent::Error(error) => return Err(error),
    }
}
```

该流由一个 Tokio 任务支持，该任务在 LLM 关闭连接后执行验证和编译，因此 API 使用成本很低。

---

## 6. 验证流水线

四个验证阶段始终按此顺序运行：

1. **Schema** — 每个基表和列引用必须存在。
2. **策略** — §4 中的表/列/行规则必须满足。
3. **操作数** — 表达式类型必须兼容（`5 = '5'` 失败，`5 + 'five'` 失败，在数值列上使用 `LIKE` 失败等）。
4. **方言** — JOIN、CTE、OFFSET、函数必须被配置的 profile 允许。

`VlorQl::validate_only` 运行所有四个阶段，返回验证后的计划，或者返回一个聚合了 LLM 可以在一次重试中修复的所有问题的 `ValidationErrors`。

```rust
use vlorql_core::schema::QueryPlan;

let plan: QueryPlan = serde_json::from_str(&assistant_text)?;
let validated = vlorql.validate_only(&plan);
match validated {
    Ok(plan) => { /* 安全地编译 */ }
    Err(errors) => {
        for error in errors.as_slice() {
            println!("{}: {}", error.error_code(), error);
        }
    }
}
```

`VlorQl::compile_only` 然后将验证后的计划转换为参数化 SQL：

```rust
let compiled = vlorql.compile_only(&validated?)?;
println!("{}", compiled.sql);
for parameter in &compiled.parameters {
    println!("  {} = {:?}", parameter.data_type, parameter.value);
}
```

### 6.1 查询计划优化

VlorQl 包含一个可选的**逻辑查询优化器**，它在验证和编译之间运行。它应用三个同步重写规则和一个异步连接重排序器：

| 规则 | 效果 |
|------|------|
| 常量折叠 | 计算常量子表达式（`100 + 50` → `150`）。 |
| 谓词下推 | 将 `WHERE` 合取项移动到 CTE 体内部，以便尽早过滤。 |
| 列剪枝 | 移除 CTE 输出中未被引用的列。 |
| 连接重排序 | 重新排序 `INNER JOIN` 链以最小化总成本（需要统计信息）。 |

通过 `VlorQlBuilder::with_statistics_provider` 启用：

```rust
use std::sync::Arc;
use vlorql_core::statistics::DummyStatisticsProvider;

let stats = Arc::new(DummyStatisticsProvider::default());
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(PolicyConfig::default())
    .with_statistics_provider(stats)
    .build()?;
```

当统计信息可用时，`VlorQl::validate_and_optimize` 返回一个 `OptimizedPlan`，可以传递给 `compile_only`：

```rust
let optimized = vlorql.validate_and_optimize(&plan).await?;
let compiled = vlorql.compile_only(optimized.as_validated())?;
```

详见 [`optimization.md`](./optimization.md)。

---

## 7. 错误处理

每个 VlorQl 错误都是一个 [`VlorQLError`] 值，具有稳定的 `error_code()` 和机器可读的 `details` 负载。使用 `to_error_response` 转换为 API 友好的响应：

```rust
let response = error.to_error_response();
println!("code:      {}", response.code);
println!("message:   {}", response.message);
println!("details:   {}", response.details);
println!("suggestion: {:?}", response.suggestion);
```

| 代码 | 含义 | 可重试？ |
|------|------|----------|
| `V001`–`V009` | 验证错误 | 是 |
| `P001`–`P003` | 策略错误 | 否 |
| `C001`–`C005` | 编译错误 | 否 |
| `S001`–`S002` | Schema 错误 | 否 |
| `L001`–`L003` | LLM 错误 | 是 |
| `G001`–`G003` | 配置错误 | 否 |

编译错误包括：

| 错误码 | 特性名称 | 含义 |
|--------|----------|------|
| `C001` | `unsupported_full_join` | 目标方言不支持 `FULL JOIN`（MySQL）。 |
| `C002` | `reserved_keyword_unquoted` | 未加引号的标识符是 SQL 保留关键字。 |
| `C003` | `empty_in_list` | `IN` 谓词的值列表为空。 |
| `C004` | `empty_select_list` | `SELECT` 列表为空。 |
| `C005` | `sql_formatting` | 内部格式化错误。 |

`ValidationErrorKind::InvalidJson` (`V001`) 和 `is_retryable()` 标志是 `VlorQl::query` 重试循环用来决定是否重新提示 LLM 的依据。策略、schema 和配置错误会立即返回，因为重新提示 LLM 无济于事。

---

## 8. QueryPlan AST 参考

`QueryPlan` JSON 对象是 LLM 和 VlorQl 之间的契约。下面的变体展示了所有支持的表达式和谓词。

### 8.1 `Expression` 变体

```rust
pub enum Expression {
    /// 字面值。
    Literal { value: serde_json::Value, data_type: DataType },
    /// 列引用，可选表限定。
    ColumnRef { table: Option<String>, column: String },
    /// 函数调用（标量或聚合），可选 DISTINCT。
    FunctionCall { name: String, args: Vec<Expression>, distinct: bool },
    /// 二元运算符应用。
    BinaryOp { left: Box<Expression>, op: BinaryOperator, right: Box<Expression> },
    /// 用于 COUNT(*) 等聚合函数中的字面 *。
    Star,
    /// 标量子查询：`(SELECT ...)`。
    SubQuery { query: Box<QueryPlan> },
}
```

**`Star`** 在 `FunctionCall` 参数中用于表示 `COUNT(*)`：

```json
{
  "type": "function_call",
  "name": "COUNT",
  "args": [{ "type": "star" }],
  "distinct": false
}
```

**`SubQuery`** 表示标量子查询表达式：

```json
{
  "type": "sub_query",
  "query": { /* 嵌套的 QueryPlan */ }
}
```

### 8.2 `Predicate` 变体

```rust
pub enum Predicate {
    Comparison { left: Expression, op: ComparisonOperator, right: Expression },
    And { left: Box<Predicate>, right: Box<Predicate> },
    Or { left: Box<Predicate>, right: Box<Predicate> },
    Not { child: Box<Predicate> },
    Between { expr: Expression, low: Expression, high: Expression },
    In { expr: Expression, target: InTarget },
    Like { expr: Expression, pattern: String },
    IsNull { expr: Expression },
    Exists { query: Box<QueryPlan> },
}
```

**`In`** 可以针对值列表或子查询，通过 `InTarget` 枚举：

```rust
pub enum InTarget {
    Values(Vec<Expression>),          // WHERE id IN (1, 2, 3)
    SubQuery(Box<QueryPlan>),         // WHERE id IN (SELECT user_id FROM ...)
}
```

**`Exists`** 检查子查询是否返回任何行：

```json
{
  "type": "exists",
  "query": { /* 嵌套的 QueryPlan */ }
}
```

### 8.3 编译后的 SQL 输出

| 表达式 | 编译后的 SQL |
|--------|-------------|
| `FunctionCall { name: "COUNT", args: [Star], distinct: false }` | `COUNT(*)` |
| `FunctionCall { name: "COUNT", args: [Star], distinct: true }` | `COUNT(DISTINCT *)` |
| `SubQuery { query: ... }` | `(SELECT ...)` |
| `In { expr, target: SubQuery(query) }` | `expr IN (SELECT ...)` |
| `Exists { query: ... }` | `EXISTS (SELECT ...)` |

---

## 9. 常见问题

### LLM 一直输出无法通过验证的计划。怎么办？

查看响应中的 `error_code()` 和 `suggestion` 字段。LLM 在每次重试时都会收到一个 `ValidationErrors` 数据块，建议的设计就是为了可以被提示使用（"将列 `users.emali` 替换为 `users.email`"）。如果在 `max_retries` 之后验证仍然失败，facade 会返回原始错误。

### 我可以在没有 LLM 的情况下运行验证器和编译器吗？

可以。`VlorQl::validate_only` 和 `VlorQl::compile_only` 都是 `pub` 的，可以在预构建的 `QueryPlan` 值上工作，这对于测试、服务端渲染存储的计划以及构建时验证 fixture 非常有用。

### 如何在编译前优化计划？

调用 `VlorQl::validate_and_optimize` 来运行查询优化器（常量折叠、谓词下推、列剪枝和可选的连接重排序）。结果是一个 `OptimizedPlan`，它解引用为 `ValidatedPlan`，可以传递给 `compile_only`。详见 [`optimization.md`](./optimization.md)。

### 如何缓存编译后的 SQL？

对于给定的 `(plan, dialect)` 对，编译输出是确定性的。将 [`QueryBuilder`]（或 `SqlCompiler` trait）包装在您自己的缓存中，以计划的 JSON 为键。编译后的 `Vec<Parameter>` 已经按任意驱动程序的正确顺序排列。

VlorQl 还提供了内置缓存——详见 [`caching.md`](./caching.md)。

### LLM 输出了一个 `or` / `OR` 谓词，但方言不支持 `OR`。VlorQl 如何处理？

VlorQl 的角色是将结构化计划转换为 SQL。方言配置（以及底层的 `DialectValidator`）决定是否允许 `OR`；如果您在 `allowed_functions` 中禁用布尔组合符或限制方言，LLM 将被强制重新表述。请谨慎配置 `DialectProfile`，提示词会回显每个允许和禁止的特性。

### 我可以添加新的 SQL 方言吗？

可以。为新的结构体实现 [`SqlCompiler`] trait，在 [`CompilerRegistry::get`] 中注册（目前是手动匹配），然后使用 `VlorQl::with_compiler` 注入。新的编译器只需要知道占位符语法、标识符引用和分页子句；其他所有内容都与 [`QueryBuilder`] 共享。

### MySQL 如何处理没有 `LIMIT` 的 `OFFSET`？

MySQL 不支持没有 limit 参数的 `LIMIT <offset>`。VlorQl 输出 `LIMIT ?, 18446744073709551615`，其中第二个值是 MySQL 的最大 `BIGINT UNSIGNED` 值，实际上意味着"无上限"。这是一个众所周知的 MySQL 惯用法。

### 我可以在 MySQL 上使用 `FULL JOIN` 吗？

不可以。MySQL 不支持 `FULL OUTER JOIN`。VlorQl 在编译时拒绝 `JoinType::Full`，返回 `compilation_error("unsupported_full_join")` 错误。

### VlorQl 会验证标识符是否与 SQL 保留关键字冲突吗？

会，当使用 `IdentifierQuoting::Never`（或回退到它）时。与 SQL 保留关键字（例如 `select`、`table`、`from`）匹配的未加引号的标识符在编译时会被拒绝，并返回 `compilation_error("reserved_keyword_unquoted")` 错误。当标识符用双引号或反引号引用时，关键字检查会被跳过，因为引用会转义关键字。

### VlorQl 会流式传输 JSON token 吗？

不会。VlorQl 返回 LLM 的完整文本增量。如果 LLM 输出部分 JSON，解析器会等待完整响应后再调用 `serde_json::from_str`。`MockLlmClient` 是了解该契约的有用参考。

### 我在哪里可以获取计划的 JSON Schema？

`schemars::schema_for!(QueryPlan)` 已经在提示词构建器内部使用。从 Rust 中获取：

```rust
let schema = schemars::schema_for!(vlorql_core::schema::QueryPlan);
let json = serde_json::to_string_pretty(&schema)?;
```

提示词构建器在每个系统提示的 **## Required JSON Output** 部分下渲染了该 schema 的简化版本。

---

## 10. 参见

* [`deployment.md`](./deployment.md) — 本地 vLLM/Ollama 设置、生产部署和性能调优。
* [`optimization.md`](./optimization.md) — 查询优化器文档（常量折叠、谓词下推、列剪枝、连接重排序）。
* [`caching.md`](./caching.md) — 内置缓存系统（SchemaCache、CompileCache、PromptCache）。
* [`README.md`](../README.md) — 高层项目介绍。
* [API 参考](https://docs.rs/vlorql) — 从源码通过 `cargo doc --workspace --no-deps` 生成。