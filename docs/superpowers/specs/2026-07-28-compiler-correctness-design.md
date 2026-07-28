# 编译器正确性 Phase 2 Implementation Design

> **Date:** 2026-07-28
> **Status:** Draft

## Goal

修复并完善 VlorQl 编译器管道的 4 个正确性子项：SET OPERATION ORDER BY 测试覆盖、CTE 类型断言方言扩展、DISTINCT+GROUP BY 警告化、SELECT * + GROUP BY 语义检测。所有修复均在 `vlorql-core` crate 内，不改动公共 API。

## Architecture

4 个子任务串行执行，分为 3 个执行步骤（Task 3+4 合并）。它们共享同一组测试设施（`compile/mod.rs` 的 `#[cfg(test)]` helper 函数），修改范围集中在 `compile/builder.rs` 和 `validate/schema.rs` 两个文件。

```
Task 1 (test only) → Task 2 (builder.rs) → Task 3+4 (schema.rs)
     compile/mod.rs       compile/builder.rs      validate/schema.rs
```

## Tech Stack

Rust (edition 2024), `vlorql-core` crate, `serde_json`, `tracing`

## File Structure

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql-core/src/compile/mod.rs`（`#[cfg(test)]`） | SET OPERATION + CTE 回归测试 | 1、2 |
| `crates/vlorql-core/src/compile/builder.rs` | CTE CAST 的方言扩展 + 更多类型 | 2 |
| `crates/vlorql-core/src/validate/schema.rs` | DISTINCT+GROUP BY 警告化 + SELECT * + GROUP BY 检测 | 3 |

## Design

### Task 1: SET OPERATION ORDER BY — 补回归测试

**背景:** `build_query_impl` 已通过 `is_set_operand` 参数抑制了 set-operation 操作数的 ORDER BY / LIMIT / OFFSET。`build_query` 方法渲染顺序为：SELECT → FROM → WHERE → GROUP BY → HAVING → SET OPERATION → ORDER BY → LIMIT。但这种行为没有任何测试覆盖。

**所需测试:**

1. `set_operation_with_order_by` — 创建一个包含 `set_operation` + `order_by` 的 `QueryPlan`，验证编译后的 SQL 中 ORDER BY 出现在 UNION ALL 之后而非之前。

2. `set_operation_operand_suppresses_trailing` — 创建一个包含两个 operand 的 UNION ALL，右 operand 自身有 order_by/limit/offset，验证它们在 operand 级别被抑制，不会出现在右 operand 的 SQL 中。

**测试方法:** 使用 `compile/mod.rs` 已有的测试 helper：
- `base_plan()` → `QueryPlan`
- `column_ref(table, column)` → `Expression`
- `literal(value, data_type)` → `Expression`
- `validated(plan)` → `ValidatedPlan`
- `"postgres"` / `"sqlite"` dialect name

**不需修改生产代码。**

### Task 2: CTE 类型断言 — MySQL/SQLite + 更多类型

**背景:** `render_expression_to` 中已有 `in_cte` 标志：当 `in_cte=true` 且 `dialect==Postgres` 时，字面量被包裹在 `CAST($N AS ...)` 中。但 MySQL 和 SQLite 的 CTE 同样需要类型断言。

**设计变更:**

在 `render_expression_to` 的 Literal 分支（约 L126-136），将当前的：

```rust
if self.in_cte && self.dialect == SqlDialect::Postgres {
    match data_type {
        DataType::Int => write!(buf, "CAST({placeholder} AS INTEGER)"),
        DataType::Float => write!(buf, "CAST({placeholder} AS DOUBLE PRECISION)"),
        DataType::Boolean => write!(buf, "CAST({placeholder} AS BOOLEAN)"),
        DataType::Decimal => write!(buf, "CAST({placeholder} AS NUMERIC)"),
        _ => write!(buf, "{placeholder}"),
    }
}
```

改为按方言分派：

```rust
if self.in_cte {
    self.render_cte_cast(placeholder, data_type, buf)?;
} else {
    write!(buf, "{placeholder}").map_err(formatting_error)?;
}
```

新增 `render_cte_cast` 方法：

```rust
fn render_cte_cast(
    &mut self,
    placeholder: &str,
    data_type: DataType,
    buf: &mut String,
) -> Result<(), VlorQLError> {
    match self.dialect {
        SqlDialect::Postgres => {
            let cast_type = match data_type {
                DataType::Int => "INTEGER",
                DataType::Float => "DOUBLE PRECISION",
                DataType::Boolean => "BOOLEAN",
                DataType::Decimal => "NUMERIC",
                DataType::Date => "DATE",
                DataType::Timestamp => "TIMESTAMP",
                DataType::Json => "JSON",
                _ => return write!(buf, "{placeholder}").map_err(formatting_error),
            };
            write!(buf, "CAST({placeholder} AS {cast_type})").map_err(formatting_error)
        }
        SqlDialect::MySql => {
            let cast_type = match data_type {
                DataType::Int => "SIGNED",
                DataType::Float => "DECIMAL(65, 30)",
                DataType::Boolean => "UNSIGNED",  // MySQL has no BOOLEAN cast
                DataType::Decimal => "DECIMAL(65, 10)",
                DataType::Date => "DATE",
                DataType::Timestamp => "DATETIME",
                DataType::Json => "JSON",
                DataType::String => "CHAR",
                _ => return write!(buf, "{placeholder}").map_err(formatting_error),
            };
            write!(buf, "CAST({placeholder} AS {cast_type})").map_err(formatting_error)
        }
        SqlDialect::Sqlite => {
            let cast_type = match data_type {
                DataType::Int => "INTEGER",
                DataType::Float => "REAL",
                DataType::Boolean => "INTEGER",   // SQLite uses 0/1 for bool
                DataType::Decimal => "REAL",
                DataType::Date => "TEXT",
                DataType::Timestamp => "TEXT",
                DataType::String => "TEXT",
                DataType::Json => "TEXT",
                DataType::Uuid => "TEXT",
                DataType::Null => "INTEGER",
                _ => return write!(buf, "{placeholder}").map_err(formatting_error),
            };
            write!(buf, "CAST({placeholder} AS {cast_type})").map_err(formatting_error)
        }
    }
}
```

**测试:** 在 `compile/mod.rs` 测试区添加：
1. `cte_cast_postgres` — CTE 中 Int/Float/Boolean 字面量被 CAST
2. `cte_cast_mysql` — MySQL CTE 中的 CAST 语法
3. `cte_cast_sqlite` — SQLite CTE 中的 CAST 语法
4. `non_cte_no_cast` — 非 CTE 上下文不产生 CAST

### Task 3+4: DISTINCT+GROUP BY 警告化 + SELECT * + GROUP BY 检测

**背景:** 当前 `validate_plan_with_outer` 在 `distinct + group_by` 同时存在时返回硬错误（`AggregationMismatch`）。但 MySQL 允许 `SELECT DISTINCT ... GROUP BY`，含义是"分组后去重"。所以硬错误过于严格，应改为警告。同时，`SELECT * ... GROUP BY col` 在语义上可疑（`*` 展开后可能包含不在 GROUP BY 中的列）。

**设计变更 1 — DISTINCT + GROUP BY 改为非阻断性警告：**

当前代码（schema.rs:36-46）：
```rust
if plan.distinct
    && plan.group_by.is_some()
    && plan.group_by.as_ref().is_some_and(|g| !g.is_empty())
{
    errors.push(VlorQLError::validation(
        crate::errors::ValidationErrorKind::AggregationMismatch {
            message: "DISTINCT and GROUP BY cannot be used together: ...".to_owned(),
        },
        serde_json::json!({"distinct": true, "group_by": plan.group_by}),
    ));
}
```

改为使用 `tracing::warn!` 记录警告且不阻断：

```rust
if plan.distinct
    && plan.group_by.is_some()
    && plan.group_by.as_ref().is_some_and(|g| !g.is_empty())
{
    tracing::warn!(
        "DISTINCT and GROUP BY used together: DISTINCT is applied after aggregation. \
         This is valid in MySQL but may be ambiguous. \
         distinct=true group_by={:?}",
        plan.group_by,
    );
}
```

**设计变更 2 — SELECT * + GROUP BY 检测：**

在 `validate_plan_with_outer` 的 DISTINCT+GROUP BY 检查后添加：

```rust
// Detect SELECT * combined with GROUP BY: star-expanded columns may
// not all be in the GROUP BY clause, causing invalid SQL.
if plan.group_by.is_some()
    && plan.group_by.as_ref().is_some_and(|g| !g.is_empty())
    && plan.select.iter().any(|p| matches!(p, Projection::Star))
{
    errors.push(VlorQLError::validation(
        crate::errors::ValidationErrorKind::AggregationMismatch {
            message: "SELECT * with GROUP BY may include columns that are not \
                      in the GROUP BY clause; consider listing columns explicitly"
                .to_owned(),
        },
        serde_json::json!({"select": plan.select, "group_by": plan.group_by}),
    ));
}
```

此处的错误保留为硬错误，因为 `SELECT * + GROUP BY` 几乎总是产生非法 SQL（除非表只有一列）。

**测试:**
1. `distinct_and_group_by_emits_warning` — 验证 distinct + group_by 不阻断，发出 `warn!`
2. `star_select_with_group_by_emits_error` — 验证 SELECT * + GROUP BY 产生错误
3. `star_select_without_group_by_no_error` — 验证仅 SELECT * 不报错

## Global Constraints

- 所有现有测试必须全量通过（`cargo test --workspace`）
- 不修改公共 API 签名
- PostgreSQL / MySQL / SQLite 三方言语法各自正确
- 不允许新增第三方运行时依赖
- TDD：先写失败测试 → 运行确认失败 → 最小实现 → 运行确认通过 → 提交
