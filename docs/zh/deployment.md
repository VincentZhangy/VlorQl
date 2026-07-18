# VlorQl 部署指南

本文档涵盖如何在真实环境中部署 VlorQl：运行本地模型服务器、调整生产环境规模以及针对您关心的工作负载调优 LLM/验证器参数。

有关语言级配置（LlmConfig、DialectProfile、PolicyConfig），请参阅 [`guide.md`](./guide.md)。

---

## 1. 运行本地 LLM

VlorQl 开箱即用支持两种本地推理引擎：vLLM（兼容 OpenAI 的 HTTP）和 Ollama（原生 `/api/chat`）。

### 1.1 vLLM

vLLM 是一个高吞吐量的兼容 OpenAI 的服务器。结构化输出使其成为一流的 VlorQl 后端——模型被约束为输出与 `QueryPlan` schema 匹配的 JSON。

**安装和启动（Linux，单 GPU）：**

```bash
pip install vllm

# 使用引导解码（guided decoding）提供模型（推荐后端：xgrammar）
vllm serve Qwen/Qwen2.5-7B-Instruct \
    --host 0.0.0.0 \
    --port 8000 \
    --gpu-memory-utilization 0.85 \
    --max-model-len 8192 \
    --guided-decoding-backend xgrammar
```

VlorQl 客户端默认启用 `response_format.json_schema`，如果引擎拒绝 schema（HTTP 4xx），则回退到 `response_format.json_object`。如需更严格的保证，请显式指定后端：

| 模型系列 | 推荐的 `--guided-decoding-backend` |
|----------|--------------------------------------|
| Qwen 2.5 / 3 / 3.5 | `xgrammar` |
| Llama 3 / 3.1 / 3.3 | `xgrammar` 或 `outlines` |
| Mistral 7B / 22B | `guidance` |
| Yi / DeepSeek / GLM | `xgrammar` |

**验证部署：**

```bash
curl -s http://localhost:8000/v1/models | jq .
```

您应该看到 `Qwen/Qwen2.5-7B-Instruct` 被列出。

**连接 VlorQl：**

```rust
use vlorql::{LlmConfig, LlmProvider, VlorQl};
use vlorql_llm::create_llm_client;

let config = LlmConfig {
    provider: LlmProvider::Vllm,
    api_key: None,
    api_base: Some("http://gpu-host.internal:8000/v1".to_owned()),
    model: "Qwen/Qwen2.5-7B-Instruct".to_owned(),
    max_tokens: 4096,
    ..LlmConfig::default()
};
let client = create_llm_client(config)?;
```

### 1.2 Ollama

Ollama 是一个单二进制模型服务器。其 `/api/chat` 端点在 `format` 参数中接受 JSON Schema 对象，这正是 VlorQl 用于约束输出的方式。

**安装和启动（macOS / Linux）：**

```bash
# 安装
curl -fsSL https://ollama.com/install.sh | sh

# 下载模型
ollama pull llama3.2

# 启动服务器（默认端口 11434）
ollama serve
```

**验证：**

```bash
curl -s http://localhost:11434/api/tags | jq .
```

**连接 VlorQl：**

```rust
use std::collections::HashMap;
use serde_json::json;
use vlorql::{LlmConfig, LlmProvider, VlorQl};
use vlorql_llm::create_llm_client;

let mut extra = HashMap::new();
extra.insert("backend".to_owned(), json!("ollama"));
extra.insert("strict_json_schema".to_owned(), json!(true));

let config = LlmConfig {
    provider: LlmProvider::Ollama,
    api_key: None,
    api_base: Some("http://localhost:11434".to_owned()),
    model: "llama3.2".to_owned(),
    max_tokens: 4096,
    extra,
    ..LlmConfig::default()
};
let client = create_llm_client(config)?;
```

> **关于兼容性的说明：** 某些 Ollama 构建版本（尤其是较旧的 Qwen 3.5/3.6）会忽略 `format` 中的 JSON Schema，只遵守较宽松的 `format: "json"` 模式。如果您看到不符合 schema 的计划，请在 `LlmConfig` 中设置 `extra["strict_json_schema"] = false` 以回退到 `format: "json"`。系统提示始终会内联 schema 作为文本回退，因此模型仍然能生成有效输出。

### 1.3 托管 Provider

托管 API 只需要环境变量中的 API 密钥：

| Provider | 环境变量 | 备注 |
|----------|----------|------|
| Anthropic | `ANTHROPIC_API_KEY` | 使用 `claude-sonnet-4-5` 以获得最佳性价比 |
| DeepSeek | `DEEPSEEK_API_KEY` | 使用 `deepseek-v4-pro`（`deepseek-chat` 模型将于 2026-07-24 弃用） |
| Zhipu | `ZHIPU_API_KEY` | 使用 `glm-4.7` 或更新版本；较旧模型只接受 `json_object` 模式 |
| OpenAI | `OPENAI_API_KEY` | 默认模型 `gpt-4o-mini` |

在您的部署系统（Kubernetes Secret、systemd `EnvironmentFile=` 等）中设置环境变量，`create_llm_client` 工厂会自动读取。

---

## 2. 生产部署

### 2.1 使用 `Arc` 共享状态

VlorQl 提供的每个组件都可以廉价克隆并在线程间共享：

* [`SchemaSnapshot`] 内部是引用计数的；克隆只是一个原子递增。
* [`DialectProfile`] 是 `Clone + Send + Sync`。
* [`PolicyConfig`] 是 `Clone + Send + Sync`。
* [`VlorQl`] 是 `Send + Sync`；其拥有的 `Arc<dyn LlmClient>` 和 `ArcSchemaSnapshot` 可以廉价共享。

典型的设置是在启动时构造**一个** `VlorQl` 值，并在请求处理程序之间共享：

```rust
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    vlorql: Arc<VlorQl>,
}
```

### 2.2 面向用户的端点使用流式 API

对于任何面向用户的流程，`VlorQl::query_stream` 比 `VlorQl::query` 更优越。它会在 LLM 仍在生成时就开始输出文本块（几百毫秒内），这能显著改善感知延迟。

对于批处理作业，建议使用 `VlorQl::query`，因为它跳过流式通道，在单次调用中返回最终计划。

### 2.3 限制重试预算

`max_retries` 是每个 facade 的设置（默认 `2`）。LLM 客户端也会通过 `LlmConfig::max_retries`（默认 `3`）在临时 HTTP 错误上重试。在估算请求预算时，请将两者相乘：`LlmConfig::max_retries = 3` 加上 `with_max_retries(2)` 的 VlorQl 实例允许每个用户请求最多 12 次 LLM 调用。

```rust
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(PolicyConfig::default())
    .with_llm_config(LlmConfig {
        max_retries: 2,
        ..LlmConfig::default()
    })
    .with_max_retries(1)
    .build()?;
```

### 2.4 限制 LLM token 预算

`LlmConfig::max_tokens` 是模型允许输出的上限。大多数 `QueryPlan` JSON 即使在宽 schema 下也能轻松容纳在 2,000 token 内。除非有特殊原因需要更多，否则设置 `max_tokens: 2048`。

### 2.5 配置请求超时

`LlmConfig::timeout_seconds` 控制 `reqwest` 中每次 HTTP 请求的超时时间。默认值为 60 秒。对于同一主机上的本地服务器，可将其降至 15 秒；对于有冷启动延迟的云 Provider，可将其提高到 90 秒。LLM 客户端会在 `Timeout` 错误上重试，因此过低的数值只会浪费重试预算。

### 2.6 日志

VlorQl 使用 `tracing` 记录所有内容。连接一个 subscriber 以获得验证重试、回退行为和 SSE 流消费的可见性：

```rust
use tracing_subscriber::{fmt, EnvFilter};

tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,vlorql=debug")))
    .init();
```

有用的事件：

* `vlorql_client::local` — 当 vLLM 结构化输出请求被拒绝，客户端回退到 `response_format: json_object` 时发出。
* `vlorql_client::deepseek` / `zhipu` / `anthropic` — 每次重试时发出，包含尝试次数、配置的预算和退避延迟。
* `vlorql_client::local` — 当 SSE 流在产生任何内容之前结束时发出（消费者是"尽力而为"的，会在临时错误上重新连接）。

### 2.7 暴露错误响应

将 `VlorQl::query` 包装在您自己的错误类型中，并将 `VlorQLError` 序列化为稳定的线格式。错误辅助函数设计为可供 LLM 重试循环提示使用，并在日志中可读：

```rust
let response = error.to_error_response();
serde_json::to_value(response)?;  // 可通过 serde 往返
```

`ValidationErrorKind::InvalidJson` (`V001`) 是最常见的可重试错误；其余的 `V*` 代码也是可重试的。`P*` / `S*` / `C*` / `G*` 代码不可重试——将它们以操作员原始的 `details` 负载返回给调用者。

---

## 3. 性能调优

### 3.1 验证器参数

| 参数 | 位置 | 效果 |
|------|------|------|
| `max_joins` | `DialectProfile` | 拒绝超过 JOIN 预算的计划。`None` = 无限制。 |
| `max_group_by_columns` | `DialectProfile` | 拒绝分组列过多的计划。 |
| `allowed_join_types` | `DialectProfile` | 限制 LLM 可输出的 JOIN 类型集合。 |
| `allowed_functions` / `denied_functions` | `DialectProfile` | 约束 LLM 可调用的 SQL 函数。空的允许列表意味着"所有非拒绝的"。 |
| `allow_distinct` | `DialectProfile` | 切换函数调用中的 `DISTINCT`。 |
| `supports_offset` / `supports_fetch` | `DialectProfile` | 切换分页子句。 |
| `supports_cte` | `DialectProfile` | 切换 `WITH` 子句。 |
| `max_tokens` | `LlmConfig` | 限制 LLM 的输出长度。 |
| `max_retries` | `LlmConfig` + `VlorQl` builder | 限制重试预算。 |

### 3.2 提示词大小

[`PromptBuilder`] 将 schema、策略、方言、JSON Schema 和示例部分渲染到每个系统提示中。默认情况下提示词远小于 10 KB。要进一步缩小：

* **禁用示例**（当部署经过充分测试时）：`PromptBuilder::with_examples(false)`（CLI 在未来的版本中通过 `VlorQl::builder().with_examples_disabled()` 风格的配置暴露此功能）。
* **隐藏全局拒绝的列**（默认已经这样做）。
* **收紧 `max_description_chars`** 通过编辑 schema——长文本描述在 JSON Schema 本身之后是最大的贡献者。

### 3.3 SQL 编译

[`QueryBuilder`] 是轻量级分配的：

* 标识符通过 `std::fmt::Write` 推入，而不是构建中间 `String`。
* 参数通过 `add_parameter` 按文本顺序追加到 `Vec<Parameter>`。PostgreSQL 路径跟踪一个计数器；SQLite/MySQL 为每个槽输出 `"?"`。
* 标识符验证在编译时拒绝空或非 ASCII 名称（当 `quote_style = Never` 时），因此攻击者控制的 LLM 无法通过标识符注入 SQL 片段。

如果您每秒编译数千个计划，请对 [`crate::compile::QueryBuilder::build`] 进行性能分析，并考虑在计划的 JSON 哈希上缓存编译后的 SQL。

### 3.4 缓存

对于给定的 `(schema, policy, plan, dialect)` 元组，流水线是纯的。常见模式：

| 缓存键 | 缓存值 |
|--------|--------|
| `serde_json::to_string(plan)` + `dialect` | `CompiledQuery` |
| `(user_question, schema_hash, policy_hash)` | `QueryPlan` |
| `schema_hash` | `SchemaSnapshot` (Arc) |

`CompiledQuery` 缓存是安全的，因为参数列表已经按文本顺序排列，SQL 字符串是纯文本。当 schema、策略或方言版本发生变化时，缓存会自动失效。

### 3.5 LLM 端调优

* **尽可能使用严格的 `response_format`**。DeepSeek 客户端默认启用；OpenAI 客户端为支持严格 JSON Schema 的模型（GPT-4o、o1/o3/o4、GPT-4.1 等）自动启用。
* **设置 `temperature: 0.0`**（`LlmConfig::default()`）以获得可重复的计划输出。只有在您有意探索计划变体时，非零温度才有用。
* **在本地模型上使用引导解码（guided decoding）**——vLLM 中的 xgrammar、Ollama 中的内置 JSON 模式。VlorQl 提示始终包含一个 JSON Schema 部分，因此引导解码可以无缝集成。

---

## 4. 可观测性

该 crate 为每个 HTTP 请求和每次重试发出结构化的 `tracing` 事件。要查看它们，请安装一个启用了 `vlorql` 目标的 subscriber：

```bash
RUST_LOG=info,vlorql=debug cargo run …
```

相关事件：

* `vlorql_client::local` — `"structured-output request rejected; retrying with json_object mode"` 当 vLLM 对 schema 请求返回 HTTP 4xx 时。
* `vlorql_client::deepseek` / `zhipu` / `anthropic` — 每次重试时发出，包含尝试次数、配置的最大尝试次数和退避延迟。
* `vlorql::query` — 当 LLM 输出的计划违反了一个或多个策略或 schema 规则，触发验证重试时发出。

在生产环境中，将这些叠加到您现有的指标流水线上。一个简单的基于 `tracing` 的 OpenTelemetry 导出器就足够了；不需要自定义插桩。

---

## 5. 安全检查清单

在推广部署之前，请检查以下列表：

* [ ] `LlmConfig::api_key` 从密钥存储中读取（环境变量、Kubernetes Secret、Vault 等），并且永远不会被记录。
* [ ] 当服务器未认证时，vLLM/Ollama 的 `LlmConfig::api_key` 为 `None`。设置任意密钥不会破坏请求，但您不应在公共环境中配置一个。
* [ ] 所有 LLM 输出都被视为不受信任的输入。`validate_only` 流水线是唯一接触 `QueryPlan` 的地方；LLM 永远没有通往数据库驱动程序的路径。
* [ ] 每个查询都通过带有生产环境 `PolicyConfig` 的 `PolicyEngine`。`crates/vlorql/tests/security/` 中的集成测试涵盖了常见的策略绕过尝试。
* [ ] 对您工作负载中的安全关键查询进行编译后的 SQL 审查。`QueryBuilder` 总是为字面值输出占位符，但您仍应审查 schema、策略以及 LLM 选择的 SQL。
* [ ] 部署运行安全测试套件（`cargo test -p vlorql --test security_*`）作为 CI 的一部分，以便 SQL 注入和策略绕过回归测试能响亮地失败。

---

## 6. 故障排除

### "vLLM returned 400: structured output backend unavailable"

引擎拒绝了严格的 schema。客户端使用 `response_format: json_object` 重试一次。如果该重试也失败，`LlmError` 会在 `details.body` 中携带完整的 HTTP 响应体。使用结构化解码后端重新启动 vLLM：

```bash
vllm serve ... --guided-decoding-backend xgrammar
```

### "Anthropic returned 401"

`ANTHROPIC_API_KEY` 环境变量缺失或无效。`create_llm_client` 工厂在第一次请求时惰性读取它；请仔细检查部署清单。`LlmErrorKind::ApiError` 状态 401 也会返回建议 `"Check the LLM provider credentials and permissions."`，该建议会被 facade 记录。

### "Validation error: dialect feature `cte` is disabled"

`DialectProfile` 说 `supports_cte = false`，但 LLM 输出了一个 CTE。错误码是 `V007`。两种修复方式：

* 启用 CTE（`DialectProfile::builder().supports_cte(true)`）。
* 收紧提示或系统消息，引导 LLM 避免使用 CTE。（默认提示已经在 **## Dialect Constraints** 下列出了禁用的特性。）

### "QueryPlan round-trip fails: unknown field `extra`"

`QueryPlan` schema 使用 `#[serde(deny_unknown_fields)]` 拒绝额外的键。最常见的原因是 LLM 幻觉出了一个键。错误码是 `V001`（如果解析失败更早，则为 `L003`）。错误消息会指出违规字段，LLM 重试提示会将其回显给模型。

### 测试在本地通过但在 CI 中失败

最可能的原因：`DEEPSEEK_API_KEY` / `ZHIPU_API_KEY` / `ANTHROPIC_API_KEY` 环境变量从开发者 shell 泄漏到测试进程中。`crates/vlorql-llm/tests/integration/multi_provider.rs` 中的集成测试现在使用 Drop 风格的守卫来清理环境变量修改。如果您编写自己的环境变量修改测试，请遵循相同的模式。