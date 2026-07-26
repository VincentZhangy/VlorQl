# 解析器 / 重试优化 Implementation Plan (O1–O3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development / superpowers:executing-plans。串行执行 O1 → O2 → O3。O1、O2 同改 `format_retry_question`（O2 依赖 O1），O3 独立但改动面最大。

**Goal:** 降低重试反馈对小模型的信息过载，并让重试逐步提高采样温度，提升不同 LLM（尤其 <3B 小模型）的最终通过率。

**Architecture:** 三处独立优化，均在 `vlorql`（facade）与 `vlorql-llm` 层，不改 QueryPlan 数据模型。O1/O2 改 `format_retry_question`（反馈生成）；O3 给 `LlmClient::generate_plan` 增加 `temperature: Option<f32>` 参数，重试循环按次数递增温度。

**Tech Stack:** Rust (edition 2024)、`#[async_trait]`、既有 6 个 LLM 提供商。分支 `feat/0.3.0`。

## Global Constraints

- **CI 全绿**：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、`cargo check -p vlorql --examples`、docs job（`RUSTDOCFLAGS=-D warnings cargo doc`）。
- `RUSTFLAGS: -D warnings`；`#![deny(missing_docs)]`（新增公共项必须有文档；改动公共方法签名时同步文档）。
- **必须兼容全部 6 个提供商**：Anthropic / DeepSeek / Zhipu / Ollama / vLLM / OpenAI。
- 最大重试次数 ≤ 3（沿用 `max_retries`）。
- **O3 是公共 API 变更**（`LlmClient::generate_plan` 增加 `temperature: Option<f32>` 参数）——已获用户批准；`None` 表示沿用 `config().temperature`（保持既有行为）。必须同步**全部** `LlmClient` 实现与调用点。
- TDD：先失败测试 → 确认失败 → 最小实现 → 确认通过 → 提交。
- 设计目标（非 CI 单测）：小模型首次通过率不低于 60%——通过分级反馈 + 温度调度改善，不作为自动化断言。

---

## File Structure

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql/src/lib.rs`（`format_retry_question` ~:877、重试循环 ~:302） | 反馈截断 + 分级 | O1、O2 |
| `crates/vlorql-llm/src/lib.rs`（trait :245、`Box<T>` :270、`OpenAIClient` :581、`MockLlmClient` :766） | generate_plan 加温度参数 | O3 |
| `crates/vlorql-llm/src/{anthropic,deepseek,zhipu,local}.rs`（各 `generate_plan` + 请求体 temperature 行） | 各 provider 支持温度覆盖 | O3 |
| `crates/vlorql/src/lib.rs`（重试循环 :220、测试 `SequenceClient` :1188） | 重试循环按 attempt 递增温度 + 更新测试 client | O3 |

---

## Task O1: 重试反馈截断（避免小模型信息过载）

**Files:**
- Modify: `crates/vlorql/src/lib.rs`（`format_retry_question` ~:877-918）
- Test: `crates/vlorql/src/lib.rs`（`#[cfg(test)] mod tests`）

**Interfaces:** 不改签名（内部截断）。新增模块级常量 `const MAX_RETRY_FEEDBACK_ERRORS: usize = 3;`。

**问题：** `format_retry_question`（:878-883）用 `; ` 拼接**全部** validation errors，并为每个 ColumnNotFound 追加 TIP（:884-909），无上限，对小模型信息过载。

- [ ] **Step 1: 写失败测试**

在 `crates/vlorql/src/lib.rs` 的 `mod tests` 内新增（`format_retry_question` 是模块私有，`use super::*;` 可调用；`ValidationErrors`/`VlorQLError` 构造参考同文件既有测试或 `vlorql_core::errors`）：

```rust
#[test]
fn retry_feedback_is_truncated_when_many_errors() {
    // 构造 5 个 schema 校验错误。
    let errs: Vec<VlorQLError> = (0..5)
        .map(|i| {
            VlorQLError::schema(
                SchemaErrorKind::ColumnNotFound {
                    table: "t".to_owned(),
                    column: format!("c{i}"),
                },
                serde_json::json!({"table": "t", "column": format!("c{i}")}),
            )
        })
        .collect();
    let errors = ValidationErrors(errs);
    let q = format_retry_question("q", &errors);
    // 只应展示前 MAX_RETRY_FEEDBACK_ERRORS 个，并提示其余被省略。
    assert!(q.contains("c0") && q.contains("c1") && q.contains("c2"),
        "should include first 3 errors: {q}");
    assert!(!q.contains("c4"), "should not include the 5th error: {q}");
    assert!(q.contains("2 more"), "should note omitted count: {q}");
}
```

> `VlorQLError::schema(kind, details)` 与 `SchemaErrorKind::ColumnNotFound` 的确切构造方式以 `crates/vlorql-core/src/errors.rs` 为准；先 `grep -n "pub fn schema\|enum SchemaErrorKind" crates/vlorql-core/src/errors.rs` 核对，必要时按既有测试构造错误的方式调整。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vlorql --lib retry_feedback_is_truncated_when_many_errors`
Expected: FAIL（当前展示全部 5 个、无 "2 more"）。

- [ ] **Step 3: 实现截断**

在 `crates/vlorql/src/lib.rs` 顶部（其它常量附近）新增：

```rust
/// Maximum number of validation errors surfaced in a single retry
/// feedback message. Smaller models degrade when flooded with every
/// error at once, so the feedback is capped and the remainder summarised.
const MAX_RETRY_FEEDBACK_ERRORS: usize = 3;
```

将 `format_retry_question`（:877-918）改为：只取前 `MAX_RETRY_FEEDBACK_ERRORS` 个 error 拼接 feedback 与 TIP，超出部分追加 `\n(… and {N} more errors omitted)`：

```rust
fn format_retry_question(original_question: &str, errors: &ValidationErrors) -> String {
    let all = errors.as_slice();
    let shown = all.len().min(MAX_RETRY_FEEDBACK_ERRORS);
    let feedback = all[..shown]
        .iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let hints: Vec<String> = all[..shown]
        .iter()
        .filter_map(|error| match error {
            VlorQLError::Schema {
                kind: SchemaErrorKind::ColumnNotFound { table, column },
                ..
            } => {
                let available = error
                    .details()
                    .get("available_columns")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                if available.is_empty() {
                    Some(format!("TIP: Column `{column}` does not exist on table `{table}`. Use exact column names from the Schema."))
                } else {
                    Some(format!("TIP: Column `{column}` does not exist on table `{table}`. Available: `{available}`."))
                }
            }
            _ => None,
        })
        .collect();
    let hints_str = if hints.is_empty() {
        String::new()
    } else {
        format!("\n{}", hints.join("\n"))
    };
    let omitted = all.len().saturating_sub(shown);
    let omitted_str = if omitted > 0 {
        format!("\n(… and {omitted} more errors omitted)")
    } else {
        String::new()
    };
    format!(
        "{original_question}\n\nThe previous QueryPlan failed validation. Correct it and return only a new JSON QueryPlan. Feedback:\n{feedback}{hints_str}{omitted_str}"
    )
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p vlorql --lib format_retry` — 新测试通过，既有重试相关测试保持通过。

- [ ] **Step 5: 校验 + 提交**

```
cargo test -p vlorql
cargo clippy -p vlorql --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/vlorql/src/lib.rs
git commit -m "feat(retry): 截断重试反馈错误数，避免小模型信息过载 (O1)"
```

---

## Task O2: 分级重试（首次摘要 → 后续增加细节）

**依赖 O1。**

**Files:**
- Modify: `crates/vlorql/src/lib.rs`（`format_retry_question` 增加 `attempt` 参数；调用点 ~:302）
- Test: `crates/vlorql/src/lib.rs` `mod tests`

**Interfaces:** `format_retry_question(original_question: &str, errors: &ValidationErrors, attempt: usize) -> String`（模块私有，改签名不影响公共 API）。

**问题：** 重试循环（:302）每次失败都用同样详略调用 `format_retry_question`，不利用 `attempt` 分级。首次失败应给精简摘要，后续逐步增加细节。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn retry_feedback_is_tiered_by_attempt() {
    let errs: Vec<VlorQLError> = (0..5)
        .map(|i| VlorQLError::schema(
            SchemaErrorKind::ColumnNotFound { table: "t".to_owned(), column: format!("c{i}") },
            serde_json::json!({"table": "t", "column": format!("c{i}")}),
        ))
        .collect();
    let errors = ValidationErrors(errs);
    let first = format_retry_question("q", &errors, 0);
    let later = format_retry_question("q", &errors, 2);
    // 首次（attempt 0）比后续更精简：展示的错误数更少。
    assert!(first.matches("does not exist").count() < later.matches("does not exist").count(),
        "attempt 0 should be terser than attempt 2:\nfirst={first}\nlater={later}");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vlorql --lib retry_feedback_is_tiered_by_attempt`
Expected: FAIL（编译错误：`format_retry_question` 尚无 `attempt` 参数）。

- [ ] **Step 3: 实现分级**

给 `format_retry_question` 增加 `attempt: usize` 参数；用它计算本次展示的错误上限（首次更少），再取 `min(MAX_RETRY_FEEDBACK_ERRORS)`：

```rust
fn format_retry_question(original_question: &str, errors: &ValidationErrors, attempt: usize) -> String {
    // Tiered detail: the first retry gets a terse summary (1 error);
    // later retries progressively surface more, capped by O1's constant.
    let tier_cap = (1 + attempt).min(MAX_RETRY_FEEDBACK_ERRORS);
    let all = errors.as_slice();
    let shown = all.len().min(tier_cap);
    // …（其余同 O1，把 MAX_RETRY_FEEDBACK_ERRORS 处的 shown 计算替换为上面的 tier_cap 版本）
```

即把 O1 里 `let shown = all.len().min(MAX_RETRY_FEEDBACK_ERRORS);` 改为基于 `tier_cap` 的两行；`omitted` 计算不变（`all.len() - shown`）。

- [ ] **Step 4: 更新调用点**

`crates/vlorql/src/lib.rs:302`：`llm_question = format_retry_question(question, &errors);` 改为 `llm_question = format_retry_question(question, &errors, attempt);`（`attempt` 来自外层 `for attempt in 0..=self.max_retries`，在作用域内）。

- [ ] **Step 5: 运行确认通过**

Run: `cargo test -p vlorql --lib` — 新测试 + O1 测试 + 既有测试全部通过（注意 O1 的测试若调用了旧签名需同步加 `attempt` 实参，如 `format_retry_question("q", &errors, 2)`）。

- [ ] **Step 6: 校验 + 提交**

```
cargo test -p vlorql
cargo clippy -p vlorql --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/vlorql/src/lib.rs
git commit -m "feat(retry): 重试反馈按 attempt 分级，首次摘要后续增细节 (O2)"
```

---

## Task O3: 重试温度动态调整

**Files（改动面最大，逐一同步）:**
- `crates/vlorql-llm/src/lib.rs`：trait `generate_plan`（:245）、`Box<T>` 转发（:270）、`OpenAIClient`（:581）、`MockLlmClient`（:766）
- `crates/vlorql-llm/src/anthropic.rs`（generate_plan :147，请求体 temperature :100）
- `crates/vlorql-llm/src/deepseek.rs`（:188，:146）
- `crates/vlorql-llm/src/zhipu.rs`（:230，:184）
- `crates/vlorql-llm/src/local.rs`（:332，:199/224/254/271 —— 多处含流式变体）
- `crates/vlorql/src/lib.rs`：重试循环调用（:220）、测试 `SequenceClient`（:1188）

**Interfaces（公共 API 变更，已批准）:**
```rust
async fn generate_plan(
    &self,
    question: &str,
    system_prompt: &str,
    temperature: Option<f32>,   // 新增；None = 用 config().temperature
) -> Result<QueryPlan, VlorQLError>;
```
各 provider 构造请求体时把 `self.config.temperature` 改为 `temperature.unwrap_or(self.config.temperature)`。

**问题：** 温度为静态 config，重试不变化。目标：首次低温（确定性），重试逐步升温以跳出错误模式。

- [ ] **Step 1: 写失败测试（provider 层温度覆盖）**

选一个易于断言请求体的 provider（如 deepseek，已有 `body["temperature"]` 断言，见 deepseek.rs:454）。新增测试：以 `Some(0.7)` 调用 `generate_plan`，断言请求体 `temperature == 0.7`；以 `None` 调用，断言等于 config 默认（0.0）。参考该文件既有 mockito 测试样式。

```rust
// 伪代码，按 deepseek.rs 既有测试样式落地：
// let client = DeepSeekClient::new(config_with_temp_0);
// client.generate_plan("q", "sys", Some(0.7)).await ...
// assert_eq!(captured_body["temperature"], 0.7);
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vlorql-llm --lib deepseek`（编译错误：generate_plan 尚无第 4 参数）。

- [ ] **Step 3: 改 trait + Box 转发 + 全部 provider**

1. trait（lib.rs:245）与 `Box<T>` 转发（:270）签名加 `temperature: Option<f32>`；Box 转发体把 `temperature` 透传：`(**self).generate_plan(question, system_prompt, temperature).await`（按既有转发写法）。
2. 每个 provider（anthropic/deepseek/zhipu/local/OpenAI）的 `generate_plan` 签名加参数；请求体里 `self.config.temperature` → `temperature.unwrap_or(self.config.temperature)`。`local.rs` 有 4 处 temperature（含流式），本任务只改 `generate_plan` 走的那条非流式路径对应的请求体；流式 `stream_plan` 不在本任务范围（保持读 config）。
3. `MockLlmClient`（:766）签名加参数（忽略即可，返回既定 plan）。
4. 文档：trait 方法 doc 补一句 `temperature` 语义（`None` = 用 config 默认）。

- [ ] **Step 4: 改重试循环 + 温度调度**

在 `crates/vlorql/src/lib.rs` 新增私有辅助：

```rust
/// Sampling temperature for retry `attempt` (0 = first call). The first
/// call keeps the configured default (deterministic); each retry nudges
/// the temperature up so the model can escape a repeated bad output.
fn retry_temperature(base: f32, attempt: usize) -> Option<f32> {
    if attempt == 0 {
        None
    } else {
        Some((base + 0.2 * attempt as f32).min(1.0))
    }
}
```

重试循环调用点（:220）：
```rust
let temperature = retry_temperature(client.config().temperature, attempt);
let plan = match client.generate_plan(&llm_question, &system_prompt, temperature).await {
```

- [ ] **Step 5: 更新测试 client `SequenceClient`（:1188）+ 其余调用点**

`SequenceClient::generate_plan`（vlorql/lib.rs:1188）签名加 `temperature: Option<f32>`（忽略或记录均可）。全仓 `grep -rn "\.generate_plan(" crates/` 找出所有调用点（含测试、examples）逐一补第 4 实参（多数传 `None`）。`cargo check -p vlorql --examples` 必须通过。

- [ ] **Step 6: 运行确认通过**

Run: `cargo test -p vlorql-llm && cargo test -p vlorql` — provider 温度测试通过，既有测试保持通过。

- [ ] **Step 7: 全量校验 + 提交**

```
cargo test --workspace
cargo check -p vlorql --examples
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git add -A
git commit -m "feat(retry): generate_plan 支持 per-call 温度，重试逐步升温 (O3)"
```

---

## Self-Review

- **Spec coverage**：O1（反馈截断）、O2（按 attempt 分级）、O3（per-call 温度 + 重试升温）三项均有独立任务。
- **依赖**：O2 依赖 O1（同改 `format_retry_question`）；执行顺序 O1→O2→O3，串行，绝不并行派实现 subagent。
- **Type consistency**：`format_retry_question(&str, &ValidationErrors, usize)` 在 O2 定稿并在调用点同步；`generate_plan(..., Option<f32>)` 在 O3 于 trait/Box/5 provider/Mock/SequenceClient 全部同步；`retry_temperature(f32, usize) -> Option<f32>` 在 O3 定义并在重试循环使用。
- **Placeholder**：O3 Step 1/3 的 provider 请求体细节以各文件既有 temperature 行为准（已给行号），实现者按现有请求体构造方式落地。
