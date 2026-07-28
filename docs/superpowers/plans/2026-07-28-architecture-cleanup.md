# 架构清理 Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除 normalize 管道中数据类型处理的重复（`canonical_literal_type` vs `resolve_data_type`），修复 `normalize_impl` 中 `std::mem::take` + `normalize_predicate` 流程的字段丢失风险。

**Architecture:** 2 个子任务串行。Task 1 新建 `common.rs` 统一入口；Task 2 修复 `normalize_impl` 的字段保护。

**Tech Stack:** Rust, `vlorql-llm` crate, `serde_json`

## Global Constraints

- 所有现有测试必须全量通过（`cargo test --workspace`）
- 不修改公共 API 签名
- 数据类型映射的行为不变（只合并代码，不改语义）
- 不新增第三方依赖

---

### Task 1: 统一数据类型映射入口

**Files:**
- Create: `crates/vlorql-llm/src/parser_v2/normalize/common.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/normalize/mod.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/normalize/expr.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/normalize/value.rs`

**Interfaces:**
- Produces: `pub fn canonical_data_type(dt: &str, value: Option<&Value>) -> Option<&'static str>`
- Produces: `pub fn resolve_sql_type_alias(dt: &str) -> Option<&'static str>`
- Consumes from expr.rs: `canonical_literal_type` → replaced by `canonical_data_type`
- Consumes from value.rs: `resolve_data_type` + `DATA_TYPE_ALIASES` → replaced by `resolve_sql_type_alias`

- [ ] **Step 1: 创建 `common.rs`**

```rust
//! Common helper functions shared across normalize modules.

use serde_json::Value;

const DATA_TYPE_ALIASES: &[(&str, &str)] = &[
    ("int2", "int"), ("int4", "int"), ("int8", "int"),
    ("integer", "int"), ("smallint", "int"), ("bigint", "int"),
    ("tinyint", "int"),
    ("varchar", "string"), ("text", "string"),
    ("char", "string"), ("character", "string"),
    ("float4", "float"), ("float8", "float"),
    ("decimal", "decimal"), ("numeric", "decimal"),
    ("bool", "boolean"),
    ("timestampz", "timestamp"),
    ("jsonb", "json"),
];

/// Resolve a data-type name to its canonical form.
/// When the type is "number" and a value is provided, disambiguates
/// as "int" or "float". Returns None when already canonical.
#[must_use]
pub fn canonical_data_type(dt: &str, value: Option<&Value>) -> Option<&'static str> {
    if dt == "number" {
        return Some(match value.and_then(|v| v.as_f64()) {
            Some(f) if f.fract() == 0.0 && f.is_finite() => "int",
            Some(_) => "float",
            None => "int",
        });
    }
    match dt {
        "string" => Some("string"),
        "integer" => Some("int"),
        "float" | "double" | "real" => Some("float"),
        "boolean" | "bool" => Some("boolean"),
        "null" => Some("null"),
        _ => None,
    }
}

/// Resolve an SQL data-type alias to its canonical name.
#[must_use]
pub fn resolve_sql_type_alias(dt: &str) -> Option<&'static str> {
    DATA_TYPE_ALIASES.iter().find(|(from, _)| *from == dt).map(|(_, to)| *to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_data_type_string() {
        assert_eq!(canonical_data_type("string", None), Some("string"));
    }

    #[test]
    fn canonical_data_type_integer() {
        assert_eq!(canonical_data_type("integer", None), Some("int"));
    }

    #[test]
    fn canonical_data_type_number_int_value() {
        assert_eq!(canonical_data_type("number", Some(&json!(42))), Some("int"));
    }

    #[test]
    fn canonical_data_type_number_float_value() {
        assert_eq!(canonical_data_type("number", Some(&json!(3.14))), Some("float"));
    }

    #[test]
    fn canonical_data_type_already_canonical() {
        assert_eq!(canonical_data_type("int", None), None);
        assert_eq!(canonical_data_type("float", None), None);
    }

    #[test]
    fn resolve_sql_type_alias_works() {
        assert_eq!(resolve_sql_type_alias("integer"), Some("int"));
        assert_eq!(resolve_sql_type_alias("varchar"), Some("string"));
        assert_eq!(resolve_sql_type_alias("decimal"), Some("decimal"));
        assert_eq!(resolve_sql_type_alias("numeric"), Some("decimal"));
        assert_eq!(resolve_sql_type_alias("int"), None); // already canonical
    }
}
```

- [ ] **Step 2: 更新 `mod.rs` 添加 `mod common;`**

在 `mod.rs` 中，在现有模块声明附近添加 `mod common;`。

- [ ] **Step 3: 更新 `expr.rs`**

替换 `canonical_literal_type` 函数体（或整个替换为 `canonical_data_type` 调用）：

```rust
// 在文件顶部添加 use
use super::common::canonical_data_type;

// 替换 canonical_literal_type 函数体为对 canonical_data_type 的委托
fn canonical_literal_type(type_val: &str, value: Option<&Value>) -> &'static str {
    canonical_data_type(type_val, value).unwrap_or(type_val)
}
```

- [ ] **Step 4: 更新 `value.rs`**

替换 `resolve_data_type` 和 `DATA_TYPE_ALIASES`：

```rust
// 在文件顶部添加 use
use super::common::resolve_sql_type_alias;

// 替换 resolve_data_type 函数体
pub fn resolve_data_type(dt: &str) -> Option<&'static str> {
    resolve_sql_type_alias(dt)
}
```

并移除 `DATA_TYPE_ALIASES` 常量定义。

- [ ] **Step 5: 验证**

```bash
cargo check -p vlorql-llm
cargo test -p vlorql-llm -- parser_v2::normalize
```

- [ ] **Step 6: 提交**

```bash
git add crates/vlorql-llm/src/parser_v2/normalize/common.rs crates/vlorql-llm/src/parser_v2/normalize/mod.rs crates/vlorql-llm/src/parser_v2/normalize/expr.rs crates/vlorql-llm/src/parser_v2/normalize/value.rs
git commit -m "refactor(normalize): unify data type mapping into common.rs"
```

---

### Task 2: 修复 data_type 字段丢失

**Files:**
- Modify: `crates/vlorql-llm/src/parser_v2/normalize/expr.rs`

- [ ] **Step 1: 在 `normalize_impl` 的 predicate 路径添加字段保护**

找到 `normalize_impl` 中的 predicate-like 处理（约 L689-695）：

```rust
if is_predicate_like {
    // Preserve non-predicate fields that may be dropped by
    // normalize_predicate's clear+rebuild operations.
    let preserved: Vec<(String, Value)> = map
        .iter()
        .filter(|(k, _)| {
            !matches!(k.as_str(),
                "type" | "left" | "right" | "op" | "child"
                | "expr" | "low" | "high" | "target"
                | "pattern" | "query")
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let mut tmp = Value::Object(std::mem::take(map));
    changed |= normalize_predicate(&mut tmp);
    if let Value::Object(m) = tmp {
        for (k, v) in preserved {
            m.entry(k).or_insert(v);
        }
        *map = m;
    }
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
git commit -m "fix(normalize): preserve non-predicate fields through normalize_predicate take/rebuild"
```
