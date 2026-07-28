# 架构清理 Phase 1 Design Spec

> **Date:** 2026-07-28
> **Status:** Draft

## Goal

消除 `vlorql-llm` crate 中 normalize 管道的数据类型处理重复，修复 `data_type` 字段在 `normalize_impl` 的 `std::mem::take` + `normalize_predicate` 流程中可能丢失的问题。

## Architecture

两个紧密关联的子任务串行执行。

```text
Task 1: 统一数据类型映射入口 → Task 2: 修复 data_type 字段丢失
 (common.rs 合并)           (expr.rs normalize_impl)
```

## Tech Stack

Rust, `vlorql-llm` crate, `serde_json`

## File Structure

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql-llm/src/parser_v2/normalize/common.rs`（新建） | 统一数据类型映射入口 `canonical_data_type()` | 1 |
| `crates/vlorql-llm/src/parser_v2/normalize/expr.rs` | 使用统一入口，修复 data_type 丢失 | 1、2 |
| `crates/vlorql-llm/src/parser_v2/normalize/value.rs` | 移除 `resolve_data_type` 改用统一入口 | 1 |

## Design

### Task 1: 统一数据类型映射入口

**现状:** 两套独立的类型规范化逻辑：

1. `expr.rs` 的 `canonical_literal_type()` — 将 LLM 输出的 type 标签（`"string"`, `"integer"`, `"number"`）转为规范化的 `data_type` 字符串。含 `"number"` 歧义消解（整→int，浮→float）。

```rust
fn canonical_literal_type(type_val: &str, value: Option<&Value>) -> &'static str {
    match type_val {
        "string" => "string",
        "integer" => "int",
        "number" => match value.and_then(|v| v.as_f64()) {
            Some(f) if f.fract() == 0.0 && f.is_finite() => "int",
            Some(_) => "float",
            None => "int",
        },
        "float" | "double" | "real" => "float",
        "boolean" | "bool" => "boolean",
        "null" => "null",
        _ => type_val,
    }
}
```

2. `value.rs` 的 `resolve_data_type()` + `DATA_TYPE_ALIASES` — 将 SQL 类型别名转为规范化名称。

```rust
const DATA_TYPE_ALIASES: &[(&str, &str)] = &[
    ("int2", "int"), ("int4", "int"), ("int8", "int"),
    ("integer", "int"), ("smallint", "int"), ("bigint", "int"),
    ("tinyint", "int"), ("varchar", "string"), ("text", "string"),
    ("char", "string"), ("character", "string"),
    ("float4", "float"), ("float8", "float"),
    ("decimal", "decimal"), ("numeric", "decimal"),
    ("bool", "boolean"), ("timestampz", "timestamp"),
    ("jsonb", "json"),
];
```

**修改方案:**

新建 `crates/vlorql-llm/src/parser_v2/normalize/common.rs`，包含统一入口：

```rust
//! Common helper functions shared across normalize modules.
//!
//! Provides the single canonical entry point for data-type name
//! normalization, used by both `expr.rs` and `value.rs`.

use serde_json::Value;

/// Map of SQL type aliases to canonical `data_type` names.
/// Used by `canonical_data_type()` when no value context is available.
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
///
/// When the type is `"number"` and a `value` is provided, the type is
/// further disambiguated as `"int"` or `"float"` based on the value.
/// When the type is an SQL alias (e.g. `"varchar"`), the canonical
/// name (e.g. `"string"`) is returned.
///
/// Returns `None` when the type is already canonical.
#[must_use]
pub fn canonical_data_type(dt: &str, value: Option<&Value>) -> Option<&'static str> {
    // 1. Value-aware disambiguation (for "number" from LLM type tags).
    if dt == "number" {
        return Some(match value.and_then(|v| v.as_f64()) {
            Some(f) if f.fract() == 0.0 && f.is_finite() => "int",
            Some(_) => "float",
            None => "int",
        });
    }

    // 2. Known LLM type tag → canonical data_type.
    let llm_tag = match dt {
        "string" => "string",
        "integer" => "int",
        "float" | "double" | "real" => "float",
        "boolean" | "bool" => "boolean",
        "null" => "null",
        _ => return None, // Not an LLM tag — try SQL alias below.
    };
    Some(llm_tag)
}

/// Resolve an SQL data-type alias to its canonical name.
///
/// Returns `None` when the type is already canonical or unknown.
#[must_use]
pub fn resolve_sql_type_alias(dt: &str) -> Option<&'static str> {
    DATA_TYPE_ALIASES
        .iter()
        .find(|(from, _)| *from == dt)
        .map(|(_, to)| *to)
}
```

然后在 `expr.rs` 中：
- 移除 `canonical_literal_type` 函数，使用 `canonical_data_type(type_val, value)` 替代
- 添加 `use super::common::{canonical_data_type};`

在 `value.rs` 中：
- 移除 `resolve_data_type` 函数和 `DATA_TYPE_ALIASES` 常量
- 使用 `common::resolve_sql_type_alias(dt)` 替代
- 添加 `use super::common::resolve_sql_type_alias;`

在 `mod.rs` 中添加：
```rust
mod common;
```

### Task 2: 修复 data_type 字段丢失

**根因:** `normalize_impl`（expr.rs:691）中：
```rust
let mut tmp = Value::Object(std::mem::take(map));
changed |= normalize_predicate(&mut tmp);
if let Value::Object(m) = tmp {
    *map = m;
}
```

`normalize_predicate` 在对 `"op": "is_null"` 或 `"op": "is not null"` 等场景（L527、L541）时执行 `obj.clear()` + 重建对象，这会**丢失**原始 `data_type` 字段。

但注意：`normalize_predicate` 中的 `clear()+rebuild` 场景是针对 `comparison` 类型的 predicate 对象，这些对象本就不该有 `data_type` 字段（`data_type` 是 expression 的属性）。所以实际上**风险不是 data_type 丢失，而是其他非 predicate 字段在 take→clear→rebuild 过程中丢失**。

**当前已有防御:** `normalize_impl`（L674）中的 `EXPR_TYPES` 列表已经将 `literal`, `column_ref`, `function_call`, `binary_op`, `star`, `subquery`, `case`, `window_function` 排除在 predicate 路径之外。所以这些 expression 类型的对象不会进入 `normalize_predicate` 的 take→clear→rebuild 路径。

**修改方案:**

更安全的做法：在 `normalize_impl` 的 `std::mem::take(map)` 前，保存 map 中的所有非 predicate 字段，处理后合并回去：

```rust
if is_predicate_like {
    // Before taking the map, extract fields that are not part of the
    // predicate itself but belong to the parent (e.g., `data_type` on
    // a literal that happens to look predicate-like due to other keys).
    // After normalize_predicate completes, merge these fields back.
    let preserved: Vec<(String, Value)> = map
        .iter()
        .filter(|(k, _)| {
            !matches!(k.as_str(),
                "type" | "left" | "right" | "op" | "child"
                | "expr" | "low" | "high" | "target"
                | "pattern" | "query" | "not")
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let mut tmp = Value::Object(std::mem::take(map));
    changed |= normalize_predicate(&mut tmp);
    if let Value::Object(m) = tmp {
        // Restore preserved fields that the predicate normalization
        // may have dropped.
        for (k, v) in preserved {
            m.entry(k).or_insert(v);
        }
        *map = m;
    }
}
```

## Testing

- 现有测试继续全部通过（`cargo test -p vlorql-llm`）
- `resolve_data_type` 测试迁移到 `common.rs` 的 `resolve_sql_type_alias` 测试
- `canonical_literal_type` 测试迁移到 `common.rs` 的 `canonical_data_type` 测试

## Global Constraints

- 所有现有测试必须全量通过（`cargo test --workspace`）
- 不修改公共 API 签名
- 数据类型映射的行为不变（只合并代码，不改语义）
- 不新增第三方依赖
