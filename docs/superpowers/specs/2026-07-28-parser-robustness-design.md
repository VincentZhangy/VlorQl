# 解析器鲁棒性 Phase 3 Design Spec

> **Date:** 2026-07-28
> **Status:** Draft

## Goal

提升 VlorQl 对不同 LLM（特别是小模型）输出的容错能力，从不鲁棒的内容提取、非标准 predicate 类型处理、小模型特定 normalize pipeline、重试策略分级 4 个维度降低 LLM 输出的"首次通过率"门槛。

## Architecture

4 个子任务串行执行，覆盖 `vlorql-llm` 和 `vlorql` 两个 crate。按依赖关系排列：JSON 提取 → 谓词 normalize → 小模型管道 → 重试策略。

```
Task 1 (extract) → Task 2 (predicate) → Task 3 (small-model) → Task 4 (retry)
 recover/extract.rs   normalize/expr.rs   normalize/pipeline.rs   vlorql/src/lib.rs
```

## Tech Stack

Rust, `vlorql-llm` crate, `vlorql` crate, `serde_json`, `tracing`

## File Structure

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql-llm/src/parser_v2/recover/extract.rs` | JSON 提取的 fallback 策略 | 1 |
| `crates/vlorql-llm/src/parser_v2/normalize/expr.rs` | `normalize_predicate` 的 catch-all 分支 | 2 |
| `crates/vlorql-llm/src/parser_v2/normalize/pipeline.rs` | 小模型特定 normalize 扩展 | 3 |
| `crates/vlorql/src/lib.rs` | `format_retry_question_str` 分级参数 | 4 |

## Design

### Task 1: `extract_json_content` — 最长有效 JSON 匹配

**背景:** `extract_json_content` 有 4 步提取策略。第 4 步 `find_best_json_obj` 已能选择"最像 QueryPlan"的对象。但第 4b 步 fallback（L86）使用 `find_balanced_object`，只**取第一个**平衡括号对。

**修改方案:**

在 `extract.rs` 的 `extract_json_content` 函数中，替换第 4b 步 fallback（约 L85-94）：

```rust
// 4b. Fallback: scan all balanced `{...}` candidates and pick the
//     longest that parses as valid JSON, rather than taking the first.
if let Some(best) = find_longest_valid_json(trimmed) {
    return best;
}
```

新增 `find_longest_valid_json` 函数：

```rust
/// Scan `text` for all balanced `{...}` substrings, parse each with
/// `serde_json`, and return the longest that parses successfully.
/// Returns `None` when no substring is parseable.
fn find_longest_valid_json(text: &str) -> Option<&str> {
    let mut best: Option<&str> = None;
    let mut best_len = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            match find_balanced_object_end(text, i) {
                Some(end) => {
                    let candidate = &text[i..=end];
                    if candidate.len() > best_len
                        && serde_json::from_str::<serde_json::Value>(candidate).is_ok()
                    {
                        best = Some(candidate);
                        best_len = candidate.len();
                    }
                    i = end;
                }
                None => {}
            }
        }
        i += 1;
    }
    best
}
```

需要从 `bracket.rs` 导出 `find_balanced_object_end`（当前可能不公开），或重新实现括号匹配逻辑。

**测试:**
1. `longest_valid_json_when_multiple_objects` — 多个 JSON 对象时选最长的
2. `longest_valid_json_no_valid_objects` — 无有效 JSON 时返回 None
3. `longest_valid_json_prefers_valid_over_first` — 验证不是简单地取第一个

### Task 2: `normalize_predicate` — 非标准谓词类型处理

**背景:** `normalize_predicate`（expr.rs:360）的 match 覆盖了 9 种标准类型。当 LLM 输出非标准类型（如 `literal` 出现在 predicate 位置，或完全未知的字符串）时，match 会 panic（Rust 编译器不会在非穷尽 match 上编译）。

**当前防御:** `normalize_impl` 中（L674）的 `EXPR_TYPES` 列表将 `literal`/`column_ref`/`function_call`/`binary_op`/`star`/`subquery`/`case`/`window_function` 标记为"表达式类型"，这些类型**不会**进入 `normalize_predicate` 路径。

**但有以下风险场景未覆盖：**
- LLM 输出一个完全没有 `type` 字段但包含 `left`/`op` 的 predicate 对象时被正确识别
- 但如果有 `type: "some_unknown_value"` + `value` 字段，当前无 catch-all

**修改方案:**

在 `normalize_predicate` 的末尾，在所有已知分支后添加 catch-all：

```rust
// Unknown predicate type — log and leave as-is (may still parse
// if the builder handles it, or fail with a clear error).
_other => {
    tracing::debug!("normalize_predicate: unknown predicate type `{_other}`");
}
```

注意：修改前需确认当前 match 是否已穷尽。如果 Rust 编译器已经在穷尽性检查下编译通过（没有 `_` 或 `catch-all`），说明确实已覆盖所有已知变体。添加 catch-all 只是为了 future-proof。

### Task 3: 小模型特定 normalize pipeline

**背景:** `normalize_for_model`（pipeline.rs:97）根据模型指纹决定是否运行 `normalize_small_model`（pipeline.rs:118）。当前 `normalize_small_model` 只做 select projection type 注入。

**修改方案:**

扩展 `normalize_small_model` 函数：

```rust
fn normalize_small_model(raw: &mut serde_json::Value) -> bool {
    let mut changed = false;
    let obj = match raw.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    // 1. Ensure select items have type fields (existing).
    changed |= fix_select_types(obj);

    // 2. Fix `"from": "table_name"` (string) → `"from": {"table": "table_name"}`.
    if let Some(from) = obj.get("from") {
        if let Some(table_name) = from.as_str() {
            obj["from"] = serde_json::json!({"table": table_name});
            changed = true;
        }
    }

    // 3. Fix LIMIT/offset as string → number.
    for field in &["limit", "offset"] {
        if let Some(v) = obj.get(*field) {
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<u64>() {
                    obj[field.to_string()] = serde_json::json!(n);
                    changed = true;
                }
            }
        }
    }

    // 4. Fix WHERE with missing `type` discriminator on predicates.
    if let Some(where_val) = obj.get_mut("where") {
        if let Some(where_obj) = where_val.as_object() {
            if !where_obj.contains_key("type")
                && where_obj.contains_key("left")
                && where_obj.contains_key("op")
            {
                where_obj.insert("type".to_owned(), "comparison".into());
                changed = true;
            }
        }
    }

    changed
}
```

**模型列表更新:** 在 `normalize_for_model` 中扩展小模型检测模式字符串，覆盖更多已知小模型：

```rust
fn is_small_model(fp: &str) -> bool {
    let fp_lower = fp.to_lowercase();
    fp_lower.contains("llama-3.2")
        || fp_lower.contains("qwen2.5")
        || fp_lower.contains("phi-3")
        || fp_lower.contains("deepseek-coder")
        || fp_lower.contains("gemma-2")
        || fp_lower.contains("mistral-7b")
        || fp_lower.contains("tiny")
}
```

### Task 4: 重试分级 + 温度调整

**背景:** `format_retry_question`（lib.rs:1042）已分级（attempt 0 只含前 3 错误，attempt 2+ 含全部）。但 `format_retry_question_str`（L981）**没有** attempt 参数，API 错误的重试每次都包含全部错误信息。

**修改方案 1 — `format_retry_question_str` 添加 attempt 参数：**

```rust
fn format_retry_question_str(question: &str, error: &VlorQLError, attempt: usize) -> String {
    let feedback = error.to_string();
    // First attempt: just the error summary (first line).
    // Subsequent: include the full error details.
    let detail = if attempt == 0 {
        feedback.lines().next().unwrap_or(&feedback).to_owned()
    } else {
        feedback
    };
    format!(
        "The previous query plan had an error. Please fix it.\n\
         Original question: {question}\n\
         Error: {detail}\n\
         Please generate a corrected query plan."
    )
}
```

同时更新所有 `format_retry_question_str` 的调用点（L267, L287, L1133, L1154, L1176）和 `stream_retry` 函数中的调用，传入 `attempt` 参数。

**修改方案 2 — 温度调整验证：**

检查 `retry_temperature` 函数（L244）是否已使用。如果已实现，则不做修改。

**修改文件:** `crates/vlorql/src/lib.rs`

## Global Constraints

- 所有现有测试必须全量通过（`cargo test --workspace`）
- 不修改公共 API 签名
- 小模型 (3B 以下) 的首次通过率不低于 60%（回归验证）
- 最大重试次数不超过 3 次
- TDD：先写失败测试 → 运行确认失败 → 最小实现 → 运行确认通过 → 提交
