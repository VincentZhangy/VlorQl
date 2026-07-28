# DataType::Decimal 端到端落地 Design Spec

> **Date:** 2026-07-28
> **Status:** Draft

## Goal

完成 `DataType::Decimal` 在 normalize 管道、builder 解析和 LLM JSON Schema 中的端到端支持，使 `decimal` 类型的字面量能够被 LLM 生成、解析、规范化并正确编译为参数化 SQL。

## Architecture

单一子任务，串行修改 4 个文件。全部在 `vlorql-llm` 和 `vlorql-core` crate 内。

## File Structure

| 文件 | 修改内容 |
|------|---------|
| `crates/vlorql-llm/src/parser_v2/normalize/common.rs` | `canonical_data_type()` 添加 "decimal" 映射 |
| `crates/vlorql-llm/src/parser_v2/builder/expr_builder.rs` | 确认/补充 `parse_data_type("decimal")` 支 |
| `crates/vlorql-llm/src/lib.rs` | `simplified_query_plan_schema()` 的 data_type enum 添加 "decimal" |
| `crates/vlorql-core/src/compile/mod.rs` | 添加 Decimal 字面量编译的回归测试 |

## Design

### 1. `canonical_data_type` 添加 "decimal" 映射

当前 `canonical_data_type` 在 match block 中处理 LLM type 标签。添加 "decimal" 标签：

```rust
"decimal" => Some("decimal"),
```

注意：此映射仅在 `dt != "number"` 时进入 match block。`decimal` 本身不需要值消歧。

### 2. `parse_data_type` 确认

检查 `expr_builder.rs` 的 `parse_data_type` 函数中 `"decimal"` 是否已在 match 中。如果不在，添加。

### 3. LLM JSON Schema 添加 "decimal"

在 `simplified_query_plan_schema()` 或 `compact_query_plan_schema()` 中生成的 `data_type` JSON Schema enum 中添加 `"decimal"`。

### 4. 编译测试

在 `compile/mod.rs` 测试区添加 Decimal 字面量编译测试：

```rust
#[test]
fn postgres_compiles_decimal_literal() {
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::Comparison {
        left: column_ref("users", "price"),
        op: ComparisonOperator::Gt,
        right: literal(json!(99.99), DataType::Decimal),
    });
    let compiled = PostgresCompiler
        .compile(&validated(plan))
        .expect("Decimal literal should compile");
    assert_eq!(compiled.parameters[0].value, json!(99.99));
    assert_eq!(compiled.parameters[0].data_type, DataType::Decimal);
}
```

## Global Constraints

- 所有现有测试必须全量通过
- 不修改公共 API 签名
- 不新增第三方依赖
