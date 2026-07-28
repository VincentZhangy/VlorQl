# 编译器正确性 Phase 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 完善 VlorQl 编译器管道的 3 项正确性工作：CTE 类型断言的方言扩展、DISTINCT+GROUP BY 警告化、SELECT * + GROUP BY 语义检测。

**Architecture:** 3 个子任务串行执行。Task 1 修改 `compile/builder.rs`（添加 `render_cte_cast` 方法）；Task 2 修改 `validate/schema.rs`（DISTINCT 警告 + SELECT * 检测）；Task 3 补测试。

**Tech Stack:** Rust (edition 2024), `vlorql-core` crate

## Global Constraints

- 所有现有测试必须全量通过（`cargo test --workspace`）
- 不修改公共 API 签名
- PostgreSQL / MySQL / SQLite 三方言语法各自正确
- 不允许新增第三方运行时依赖
- TDD：先写失败测试 → 运行确认失败 → 最小实现 → 运行确认通过 → 提交

---

### Task 1: CTE 类型断言 — MySQL/SQLite + 更多数据类型

**Files:**
- Modify: `crates/vlorql-core/src/compile/builder.rs:117-141`

**Interfaces:**
- Consumes: `SqlDialect::Postgres/MySql/Sqlite`, `DataType::Int/Float/Boolean/Decimal/Date/Timestamp/Json/String/Uuid/Null` (已有)
- Produces: `fn render_cte_cast(&mut self, placeholder: &str, data_type: DataType, buf: &mut String) -> Result<(), VlorQLError>` — 新私有方法

**现状:** `render_expression_to` 中 Literal 分支（L124-141）只在 `self.dialect == SqlDialect::Postgres` 时添加 CAST，MySQL 和 SQLite 的 CTE 字面量裸露。

- [ ] **Step 1: 提取 `render_cte_cast` 方法**

在 `render_expression_to` 的 Literal 分支附近，添加新方法：

```rust
/// Render a CTE type cast for the given placeholder and data type.
///
/// PostgreSQL / MySQL / SQLite each have different CAST syntax and
/// target type names. This method abstracts the dialect differences.
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
                DataType::Boolean => "UNSIGNED",
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
                DataType::Boolean => "INTEGER",
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

- [ ] **Step 2: 修改 Literal 分支调用新方法**

将 L129-137 替换为：

```rust
if self.in_cte {
    self.render_cte_cast(&placeholder, *data_type, buf)?;
} else {
    buf.push_str(&placeholder);
}
```

- [ ] **Step 3: 运行检查确认编译通过**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-core
```

预期：编译成功，无告警。

- [ ] **Step 4: 运行现有测试确认无回归**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo test -p vlorql-core -- compile 2>&1 | tail -15
```

预期：所有 compile 测试通过。

- [ ] **Step 5: 提交**

```bash
git add crates/vlorql-core/src/compile/builder.rs
git commit -m "feat(compile): extend CTE type CAST to MySQL/SQLite + more data types
```

---

### Task 2: DISTINCT+GROUP BY 警告化 + SELECT * + GROUP BY 检测

**Files:**
- Modify: `crates/vlorql-core/src/validate/schema.rs:33-46`

**现状:** `validate_plan_with_outer` 中 `distinct + group_by` 同时存在时返回硬错误（`AggregationMismatch`）。无 `SELECT * + GROUP BY` 检测。

- [ ] **Step 1: DISTINCT + GROUP BY 改为 `tracing::warn!`**

当前代码（L33-46）：
```rust
if plan.distinct
    && plan.group_by.is_some()
    && plan.group_by.as_ref().is_some_and(|g| !g.is_empty())
{
    errors.push(VlorQLError::validation(
        crate::errors::ValidationErrorKind::AggregationMismatch {
            message: "DISTINCT and GROUP BY cannot be used together: the combination is semantically ambiguous".to_owned(),
        },
        serde_json::json!({"distinct": true, "group_by": plan.group_by}),
    ));
}
```

替换为：
```rust
if plan.distinct
    && plan.group_by.is_some()
    && plan.group_by.as_ref().is_some_and(|g| !g.is_empty())
{
    tracing::warn!(
        "DISTINCT and GROUP BY used together: DISTINCT is applied after aggregation. \
         This is valid in MySQL but may be semantically ambiguous in other dialects. \
         distinct=true group_by={:?}",
        plan.group_by,
    );
}
```

- [ ] **Step 2: SELECT * + GROUP BY 检测**

在 DISTINCT+GROUP BY 警告代码之后（同一 `validate_plan_with_outer` 函数），添加新的检查：

```rust
// SELECT * combined with GROUP BY: star-expanded columns may not all
// be in the GROUP BY clause, producing invalid SQL in most dialects.
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

注意：确保 `Projection` 已在 `use crate::schema::...` 导入中（如果当前导入在 `validate/schema.rs` 中不是通配符的话）。

- [ ] **Step 3: 运行检查确认编译通过**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-core
```

- [ ] **Step 4: 提交**

```bash
git add crates/vlorql-core/src/validate/schema.rs
git commit -m "fix(validate): demote DISTINCT+GROUP BY to warning; add SELECT * + GROUP BY check"
```

---

### Task 3: 补充测试覆盖

**Files:**
- Modify: `crates/vlorql-core/src/compile/mod.rs` — 添加 CTE CAST 测试
- Modify: `crates/vlorql-core/src/validate/schema.rs` — 已有测试在 `#[cfg(test)]` 区（确认是否存在）

- [ ] **Step 1: 在 compile/mod.rs 中添加 CTE CAST 测试**

在已有 `cte_parameters_share_one_postgres_placeholder_sequence` 测试之后添加：

```rust
#[test]
fn postgres_cte_literals_get_cast() {
    let cte_query = QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
            alias: Some("level".to_owned()),
        }],
        from: FromClause::table("users".to_owned(), None),
        r#where: Some(Predicate::Comparison {
            left: column_ref("users", "level"),
            op: ComparisonOperator::Lte,
            right: literal(json!(10), DataType::Int),
        }),
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
        distinct: false,
        distinct_on: None,
        set_operation: None,
    };
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("cte".to_owned()),
            column: "level".to_owned(),
            alias: None,
        }],
        from: FromClause::table("cte".to_owned(), None),
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: Some(vec![CommonTableExpression {
            name: "cte".to_owned(),
            query: Box::new(cte_query),
            recursive: false,
        }]),
        distinct: false,
        distinct_on: None,
        set_operation: None,
    };
    let compiled = PostgresCompiler
        .compile(&validated(plan))
        .expect("CTE should compile");
    assert!(
        compiled.sql.contains("CAST("),
        "CTE literal should be CAST-wrapped in PostgreSQL, got: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("AS INTEGER"),
        "Int literal should CAST to INTEGER, got: {}",
        compiled.sql
    );
}
```

```rust
#[test]
fn non_cte_literals_are_not_cast() {
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::Comparison {
        left: column_ref("users", "level"),
        op: ComparisonOperator::Lte,
        right: literal(json!(10), DataType::Int),
    });
    let compiled = PostgresCompiler
        .compile(&validated(plan))
        .expect("non-CTE should compile");
    assert!(
        !compiled.sql.contains("CAST("),
        "non-CTE literals should not be CAST-wrapped, got: {}",
        compiled.sql
    );
}
```

- [ ] **Step 2: 运行测试验证通过**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo test -p vlorql-core -- compile::tests::cte 2>&1 | tail -10
cargo test -p vlorql-core -- compile::tests::non_cte 2>&1 | tail -10
```

- [ ] **Step 3: 提交**

```bash
git add crates/vlorql-core/src/compile/mod.rs
git commit -m "test(compile): add CTE CAST and non-CTE regression tests"
```
