# VlorQl 架构问题与不完善之处

## 一、严重问题 🔴

### P1. 两套解析流水线并存 —— `parse` 与 `parser_v2` 重复

- **位置**: `crates/vlorql-llm/src/parse/` vs `crates/vlorql-llm/src/parser_v2/`
- **风险**: `canonicalize.rs` (V1) 与 normalize 流水线 (V2) 功能重叠。`canonicalize.rs` 中的 `normalize_data_types` 与 `parser_v2/normalize/value.rs` 的 `normalize_data_types_inner` 逻辑重复，导致同一份修复需要同步到两个文件
- **建议**: 彻底移除 `parse/` 模块，统一使用 `parser_v2`
- **相关文件**:
  - `crates/vlorql-llm/src/parse/canonicalize.rs`
  - `crates/vlorql-llm/src/parser_v2/normalize/value.rs`
  - `crates/vlorql-llm/src/parse/build.rs`
  - `crates/vlorql-llm/src/parser_v2/builder/`

### P2. SQL 编译器中 `ORDER BY` 在 `UNION ALL` 前非法

- **位置**: `compile/builder.rs` 中 `build_query` 方法
- **风险**: 当 `QueryPlan` 同时包含 `set_operation` 和 `order_by` 时，编译器生成 `SELECT ... ORDER BY ... UNION ALL ...`，PostgreSQL 语法错误
- **根因**: 编译器按固定顺序渲染 `set_operation` 和 `order_by`，没有检测 `set_operation` 存在时应推迟 ORDER BY
- **状态**: 已在演示层面绕过（预设计划中移除 order_by），但编译器层未修复

### P3. 参数去重问题 —— 同一字面量不同占位符

- **位置**: `compile/builder.rs` 的 `add_parameter`
- **根因**: 相同值+类型的字面量在 SELECT/GROUP BY/ORDER BY 中出现时生成不同占位符 (`$1`, `$2`, `$3`)，PostgreSQL GROUP BY 要求表达式精确匹配
- **状态**: 已修复

### P4. 递归 CTE 类型推断问题

- **位置**: `compile/builder.rs` 的 `render_expression_to`
- **根因**: 参数化整数字面量 (`$1 AS level`) 在递归 CTE 中被 PostgreSQL 推断为 text 类型，递归部分的加法 `level + 1` 报错
- **状态**: 已修复（内联非字符串字面量）

---

## 二、中等问题 🟡

### P5. `QueryPlan` 的 `distinct` 字段未被验证器充分检查

- **位置**: `validate/schema.rs`
- **问题**: `distinct: true` + `group_by` 同时存在时语义不明确（先 DISTINCT 后 GROUP BY 或相反？），但验证器没有对此发出警告
- **影响文件**: `crates/vlorql-core/src/validate/schema.rs`

### P6. 错误重试策略过于简单

- **位置**: `vlorql/src/lib.rs` 的 `query` 方法
- **问题**: 重试时直接拼接所有验证错误信息到用户问题后，对于小模型 (llama3.2 等 3B 以下) 产生信息过载，模型反而输出更差的计划
- **影响文件**: `crates/vlorql/src/lib.rs` (`format_retry_question`, `format_retry_question_str`)

### P7. 别名解析器只支持单层

- **位置**: `compile/builder.rs` 的 `resolve_alias`
- **问题**: CTE 查询内外层作用域通过栈管理，但子查询中同表不同别名的场景可能解析错误
- **影响文件**: `crates/vlorql-core/src/compile/builder.rs`

---

## 三、轻度问题 🟢

### P8. 缺少多数据库连接执行层

- 当前只提供 SQL 编译，执行层完全由用户自己实现
- 建议提供统一的 `DatabaseExecutor` trait + PostgreSQL/MySQL/SQLite 实现
- **参考**: `crates/vlorql/examples/end_to_end_pg.rs` 中 200+ 行的执行样板代码

### P9. `DataType` 类型集过小

```rust
pub enum DataType {
    Int, Float, String, Boolean, Date, Timestamp, Json, Null, Uuid,
}
```

- 缺少 `Decimal/Numeric`、`Array`、`JSONB`、`Blob` 等常见类型
- 编译器遇到这些类型时需要 fallback 处理
- **影响文件**: `crates/vlorql-core/src/schema/types.rs`

### P10. 缺少 SQL 注入审计

- 参数化查询由编译器保证，但列名、表名的引用来自 QueryPlan。如果 PromptBuilder 在构建提示词时泄露了 SQL 片段，或 LLM 生成的 plan 中含有恶意列名，仍可能注入

### P11. 文档覆盖率不足

- `missing_docs` lint 启用，但很多公共方法缺少 `# Examples`
- 特别是 optimizer 模块和 parser_v2 模块

---

## 四、解析器问题

### N1. `normalize` 中 `data_type` 字段丢失问题

- **现象**: `parser_v2/normalize/value.rs` 的 `normalize_impl` 有逻辑去推断 `"data_type":"null"` 为实际类型，但 `expr::normalize_impl` 中 `std::mem::take(map)` 后 `normalize_predicate` 处理时字段被提前移除，导致 `build_literal_from_obj` 收到的 `data_type` 为 `None`
- **根因**: `expr::normalize_impl` 对谓词对象使用 `std::mem::take(map)`，然后递归调用 `normalize_predicate`。内部的 `repair_expression_value` 在遇到已有 `type` 字段的字面量时返回 `false`（不修改），但 `normalize_predicate` 的后续步骤可能重建对象导致字段丢失
- **影响**: llama3.2 的 `"data_type":"null"` 问题需要靠 validator 层兜底
- **相关代码**:
  - `crates/vlorql-llm/src/parser_v2/normalize/expr.rs` L379: `let mut tmp = Value::Object(std::mem::take(map));`
  - `crates/vlorql-llm/src/parser_v2/normalize/value.rs` L80-98: `data_type: "null"` 推断逻辑

### N2. 非 `predicate_type` 的字段未经验证的删除

- **位置**: `parser_v2/normalize/expr.rs` 的 `normalize_predicate`
- **问题**: 函数只检查已知的谓词类型 (`comparison`, `and`, `or`, `not`, `between`, `in`, `like`, `is_null`, `exists`)。对于字面量类型 (`literal`)，不符合任何条件但被 `std::mem::take` 处理，存在意外删除字段的风险

### N3. `extract_json_content` 不够鲁棒

- **位置**: `parser_v2/recover/mod.rs`
- **问题**: 只处理最外层的 `{...}` 对象。如果 LLM 输出包含多个 JSON 对象（例如先输出分析文字再输出 JSON），只会提取第一个，可能取到不完整的内容
- **建议**: 添加"最长有效 JSON 匹配"策略

### N4. 缺乏 JSON Schema 驱动的严格校验

- 当前 canonicalization 是硬编码的规则集；没有利用 Prompt 中的完整 JSON Schema 约束。这导致某些模型产生的边缘情况无法被 canonicalization 覆盖
