# 解析器鲁棒性 Phase 3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 从 JSON 提取、非标准谓词类型、小模型 normalize 管道、重试分级 4 个维度提升 LLM 输出的容错能力。

**Architecture:** 4 个子任务串行执行，互不依赖，按文件区域排列。

**Tech Stack:** Rust, `vlorql-llm` crate, `vlorql` crate, `serde_json`, `tracing`

## Global Constraints

- 所有现有测试必须全量通过（`cargo test --workspace`）
- 不修改公共 API 签名
- 小模型 (3B 以下) 的首次通过率不低于 60%（不退化）
- 最大重试次数不超过 3 次
- TDD：先写失败测试 → 运行确认失败 → 最小实现 → 运行确认通过 → 提交

---

### Task 1: `extract_json_content` — 最长有效 JSON 匹配

**Files:**
- Modify: `crates/vlorql-llm/src/parser_v2/recover/extract.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/recover/bracket.rs`（若需导出辅助函数）

**现状:** fallback 路径只取第一个平衡 `{...}`，不尝试其他候选。

- [ ] **Step 1: 导出 `find_balanced_object_end`**

在 `bracket.rs` 中，若 `find_balanced_object_end` 尚未是 `pub(crate)` 或 `pub`，改为 `pub(crate)` 以便 `extract.rs` 中使用。

- [ ] **Step 2: 添加 `find_longest_valid_json` 函数**

在 `extract.rs` 中添加：

```rust
/// Scan `text` for all balanced `{...}` substrings, parse each with
/// `serde_json`, and return the longest that parses successfully.
fn find_longest_valid_json(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut best: Option<&str> = None;
    let mut best_len = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = bracket::find_balanced_object_end(text, i) {
                let candidate = &text[i..=end];
                if candidate.len() > best_len
                    && serde_json::from_str::<serde_json::Value>(candidate).is_ok()
                {
                    best = Some(candidate);
                    best_len = candidate.len();
                }
                i = end;
            }
        }
        i += 1;
    }
    best
}
```

- [ ] **Step 3: 替换 fallback 路径**

在 `extract_json_content` 中，将当前 fallback（4b 步骤，约 L85-94）替换为：

```rust
// 4b. Fallback: scan all balanced `{...}` candidates and pick the
//     longest that parses as valid JSON.
if let Some(best) = find_longest_valid_json(trimmed) {
    return best;
}
```

- [ ] **Step 4: 添加测试**

在 `extract.rs` 的 `#[cfg(test)]` 中添加：

```rust
#[test]
fn longest_valid_json_when_multiple_objects() {
    let input = r#"{"a":1} followed by {"select":[{"type":"star"}],"from":{"table":"users"}}"#;
    let result = extract_json_content(input);
    let parsed: serde_json::Value = serde_json::from_str(result).unwrap();
    assert!(parsed.get("select").is_some(), "should extract the plan, not the first object");
}

#[test]
fn longest_valid_json_no_valid_objects() {
    let input = "just text {broken: json}";
    let result = extract_json_content(input);
    assert_eq!(result, input, "should return original when no valid JSON");
}
```

- [ ] **Step 5: 验证**

```bash
cargo check -p vlorql-llm
cargo test -p vlorql-llm -- parser_v2::recover
```

- [ ] **Step 6: 提交**

```bash
git add crates/vlorql-llm/src/parser_v2/recover/extract.rs crates/vlorql-llm/src/parser_v2/recover/bracket.rs
git commit -m "feat(recover): add find_longest_valid_json fallback for extract_json_content"
```

---

### Task 2: `normalize_predicate` — catch-all 分支

**Files:**
- Modify: `crates/vlorql-llm/src/parser_v2/normalize/expr.rs`

**现状:** `normalize_predicate` 的 match 已覆盖所有已知类型，但无 catch-all。

- [ ] **Step 1: 添加 catch-all 分支**

在 `normalize_predicate` 函数（expr.rs:360）的 match 末尾，在 `"exists"` 分支之后添加：

```rust
// Unknown predicate type — log and leave as-is.
other => {
    tracing::debug!("normalize_predicate: unknown predicate type `{other}`");
    false
}
```

- [ ] **Step 2: 验证**

```bash
cargo check -p vlorql-llm
cargo test -p vlorql-llm -- parser_v2::normalize
```

- [ ] **Step 3: 提交**

```bash
git add crates/vlorql-llm/src/parser_v2/normalize/expr.rs
git commit -m "feat(normalize): add catch-all branch to normalize_predicate"
```

---

### Task 3: 小模型特定 normalize pipeline

**Files:**
- Modify: `crates/vlorql-llm/src/parser_v2/normalize/pipeline.rs`

**现状:** `normalize_small_model` 只修复 select projection type。

- [ ] **Step 1: 展开 `normalize_small_model`**

将当前函数替换为：

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
        if from.is_str() {
            let table_name = from.as_str().unwrap().to_owned();
            obj["from"] = serde_json::json!({"table": table_name});
            changed = true;
        }
    }

    // 3. Fix LIMIT/offset as string → number.
    for &field in &["limit", "offset"] {
        if let Some(v) = obj.get(field) {
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

- [ ] **Step 2: 提取 `fix_select_types` 辅助函数**

将现有的 select type 修复逻辑提取为 `fix_select_types(obj: &mut Map<String, Value>) -> bool`。

- [ ] **Step 3: 扩展 `is_small_model`**

确保模型检测代码覆盖更多小模型（`llama-3.2`, `qwen2.5`, `phi-3`, `deepseek-coder`, `gemma-2`, `mistral-7b`）。

- [ ] **Step 4: 添加测试**

在 `pipeline.rs` 的 `#[cfg(test)]` 中添加：

```rust
#[test]
fn small_model_fixes_from_string() {
    let mut val = serde_json::json!({"select": [{"column": "id"}], "from": "users"});
    assert!(normalize_small_model(&mut val));
    assert_eq!(val["from"]["table"], "users");
}

#[test]
fn small_model_fixes_limit_string() {
    let mut val = serde_json::json!({"select": [{"column": "id"}], "from": {"table": "users"}, "limit": "10"});
    assert!(normalize_small_model(&mut val));
    assert_eq!(val["limit"], 10);
}

#[test]
fn small_model_fixes_where_missing_type() {
    let mut val = serde_json::json!({"select": [{"column": "id"}], "from": {"table": "users"}, "where": {"left": {"column": "age"}, "op": "gt", "right": {"literal": 18}}});
    assert!(normalize_small_model(&mut val));
    assert_eq!(val["where"]["type"], "comparison");
}
```

- [ ] **Step 5: 验证**

```bash
cargo check -p vlorql-llm
cargo test -p vlorql-llm -- parser_v2::normalize::pipeline
```

- [ ] **Step 6: 提交**

```bash
git add crates/vlorql-llm/src/parser_v2/normalize/pipeline.rs
git commit -m "feat(normalize): expand small model pipeline with from/limit/where fixes"
```

---

### Task 4: 重试分级 — `format_retry_question_str` 添加 attempt 参数

**Files:**
- Modify: `crates/vlorql/src/lib.rs`

**现状:** `format_retry_question_str` 没有 attempt 参数，每次重试都包含相同级别的错误详情。

- [ ] **Step 1: 修改 `format_retry_question_str` 签名和实现

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

- [ ] **Step 2: 更新所有调用点**

在 `lib.rs` 中搜索所有 `format_retry_question_str(` 调用，在每个后面添加 `attempt` 参数：
- L267: `format_retry_question_str(&llm_question, &e)` → 需要 `attempt`（此调用在循环中，`attempt` 是外层变量）
- L287: 同上
- L1133, L1154, L1176: 流重试中的调用

每个调用点需要注入当前 attempt 值。

- [ ] **Step 3: 验证**

```bash
cargo check -p vlorql
cargo test -p vlorql
```

- [ ] **Step 4: 提交**

```bash
git add crates/vlorql/src/lib.rs
git commit -m "feat(retry): add attempt-graded error details to format_retry_question_str"
```
