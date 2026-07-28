# Schema 驱动的校验层 Design Spec

> **Date:** 2026-07-28
> **Status:** Draft

## Goal

在 normalize 管道之后、builder 之前新增一个 JSON Schema 校验层，利用 `schemars` 自动生成的 Schema 对 LLM 输出做结构校验，替代当前纯硬编码的规范化规则，在结构不匹配时给出精确的错误定位。

## Architecture

在 `parser_v2` 中新建 `schema_check` 模块，集成到 `pipeline.rs` 的 normalize 和 builder 之间。

```
pipeline 新流程:
  recover → normalize → **schema_check** → builder → fix → validate → optimize
                             │
                             ▼
                     精确的结构错误
                     (用于重试提示)
```

## Tech Stack

`schemars` (已存在依赖), `serde_json`, `jsonschema` (新依赖, Apache-2.0)

## File Structure

| 文件 | 责任 |
|------|------|
| `crates/vlorql-llm/src/parser_v2/schema_check/mod.rs`（新建） | Schema 校验层主模块 |
| `crates/vlorql-llm/src/parser_v2/pipeline.rs`（修改） | 集成 schema_check 到主 pipeline |
| `crates/vlorql-llm/Cargo.toml`（修改） | 添加 jsonschema 依赖 |

## Design

### 模块设计

```rust
//! JSON Schema validation layer for the V2 parse pipeline.
//!
//! Uses `schemars::schema_for` to generate the canonical QueryPlan
//! schema and validates normalized JSON against it, producing
//! precise structural error messages.

use schemars::schema_for;
use serde_json::Value;

/// Validate a normalized JSON value against the QueryPlan JSON Schema.
///
/// Returns `Ok(())` when the value conforms to the schema, or `Err`
/// with a list of human-readable structural errors.
pub fn validate_against_schema(val: &Value) -> Result<(), Vec<String>> {
    let schema = schema_for!(vlorql_core::schema::QueryPlan);
    let schema_value = serde_json::to_value(&schema).unwrap();
    
    // TODO: Use jsonschema crate to compile and validate
    let compiled = jsonschema::JSONSchema::compile(&schema_value)
        .map_err(|e| vec![format!("Failed to compile schema: {e}")])?;
    
    let mut errors = Vec::new();
    if let Err(validation_errors) = compiled.validate(val) {
        for error in validation_errors {
            errors.push(format!(
                "Schema validation error at {}: {} (expected {}, got {})",
                error.instance_path,
                error.kind,
                // Additional context from schema
            ));
        }
    }
    
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
```

### Pipeline 集成

在 `pipeline.rs` 的 `parse_query_plan` 函数中，在 normalize 之后、builder 之前添加：

```rust
// Stage 2b: Schema validation
if let Err(schema_errors) = schema_check::validate_against_schema(&value) {
    // Schema validation failed — log detailed errors
    for err in &schema_errors {
        tracing::debug!("Schema check: {err}");
    }
    // Don't block — let the builder try and produce its own errors,
    // but include schema errors in the debug output for retry prompts.
}
```

## Why jsonschema

`schemars` 只生成 Schema，不提供校验功能。`jsonschema` crate 是 Apache-2.0 license（与项目现有许可证兼容），支持 JSON Schema 2020-12，能对任意 `serde_json::Value` 做结构校验。

## Global Constraints

- 所有现有测试必须全量通过
- `cargo deny check` 必须通过（jsonschema 许可证为 Apache-2.0）
- 不修改公共 API 签名
