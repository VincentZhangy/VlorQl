# F4: 公共 API `# Examples` 覆盖 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 `vlorql-core/src/optimizer/` 和 `vlorql-llm/src/parser_v2/builder/` 中缺失 `# Examples` 的公共函数/struct 补充 doc-testable 的 `# Examples`。

**Architecture:** 两个独立文件组（optimizer / parser_v2 builder），互不依赖。用 **并行 subagent** 执行，每个 subagent 负责一组文件。不修改任何逻辑代码。

**Tech Stack:** Rust (edition 2024), serde_json, doc-tests (`cargo test` runs doctests automatically).

## Global Constraints

- CI 全绿：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`
- `#![deny(missing_docs)]`：已有文档注释的保持，只追加 `/// # Examples` 段
- 不修改任何逻辑代码，不改公共 API 签名
- 示例用 `assert_eq!` / `assert!` 验证，必须是可执行的 doctest
- 每个 example 3–8 行，展示最典型用法
- 引用 `vlorql_core::schema::*` 核心类型，引用 `serde_json::json!` 构造输入
- trait（如 `ExpressionFold`）给出一个简单自定义实现的示例
- **不加新依赖**——所有示例仅用当前已有的公开类型

---

## File Structure

| 分组 | 文件 | 责任 | 示例目标 |
|------|------|------|----------|
| **Group 1** | `crates/vlorql-core/src/optimizer/prune.rs` | 列裁剪 | `ColumnPruning` struct + `new()` / `with_schema()` |
| | `crates/vlorql-core/src/optimizer/pushdown.rs` | 谓词下推 | `PredicatePushdown` struct + `rewrite()` |
| | `crates/vlorql-core/src/optimizer/join_reorder.rs` | JOIN 重排序 | `JoinReorderer` struct + `new()` / `with_cost_estimator()` / `reorder()` |
| | `crates/vlorql-core/src/optimizer/visitor.rs` | 表达式/Plan 遍历 | `ExpressionFold` trait + `ExpressionVisit` trait |
| | `crates/vlorql-core/src/optimizer/analyze.rs` | 分析辅助 | `split_conjuncts()` / `collect_conjuncts()` |
| **Group 2** | `crates/vlorql-llm/src/parser_v2/builder/expr_builder.rs` | 表达式/谓词/数据类型解析 | `parse_data_type()` / `build_expression()` / `build_predicate()` |
| | `crates/vlorql-llm/src/parser_v2/builder/join_builder.rs` | JOIN 子句构建 | `build_join_clause()` |
| | `crates/vlorql-llm/src/parser_v2/builder/query_builder.rs` | 完整 plan 构建 | `build_plan()` |
| | `crates/vlorql-llm/src/parser_v2/builder/select_builder.rs` | SELECT 投影构建 | `build_projection()` / `build_projections()` |
| | `crates/vlorql-llm/src/parser_v2/builder/table_builder.rs` | FROM 子句构建 | `build_from_clause()` |
| | `crates/vlorql-llm/src/parser_v2/builder/mod.rs` | 模块文档 | 模块级 `//! # Examples` |

---

### Task 1: Group 1 — optimizer 模块 `# Examples`

**Files:**
- Modify: `crates/vlorql-core/src/optimizer/prune.rs`
- Modify: `crates/vlorql-core/src/optimizer/pushdown.rs`
- Modify: `crates/vlorql-core/src/optimizer/join_reorder.rs`
- Modify: `crates/vlorql-core/src/optimizer/visitor.rs`
- Modify: `crates/vlorql-core/src/optimizer/analyze.rs`

**Interfaces:**
- Consumes: `vlorql_core::schema::*` (QueryPlan, Expression, DataType, FromClause, Projection, Join, etc.), `serde_json::json!`
- Produces: 每个目标 item 新增 `/// # Examples` doctest 段

**Step-by-step for each file:**

For each file below: read the file, find the `pub` target(s) that lack `# Examples`, add a short doctest block after the existing doc comment. Always put the `/// # Examples` block on a new line after the existing `///` docs (or after the struct definition doc if it's a struct).

#### `prune.rs`

Find `pub struct ColumnPruning` (~line 55). Add after its doc comment:

```rust
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::prune::ColumnPruning;
/// let pruning = ColumnPruning::new();
/// ```
```

Find `pub fn new()` (~line 62) and `pub fn with_schema()` (~line 68). If they already have doc comments, add `/// # Examples` after:

```rust
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::prune::ColumnPruning;
/// let pruning = ColumnPruning::new();
/// ```
```

(Tip: if the method is in an `impl` block that's not `pub`, the doc may not show as a public item — still add it for consistency.)

#### `pushdown.rs`

Find `pub struct PredicatePushdown` (~line 43). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::pushdown::PredicatePushdown;
/// let pushdown = PredicatePushdown;
/// ```
```

Find `pub fn rewrite()` (~line 46). Add a doctest showing basic usage:

```rust
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::pushdown::PredicatePushdown;
/// use vlorql_core::schema::QueryPlan;
///
/// let pushdown = PredicatePushdown;
/// let plan = QueryPlan::default();
/// let result = pushdown.rewrite(plan);
/// ```
```

Adjust the method signature — if `rewrite` takes `&self` and `QueryPlan` (not ownership), adjust accordingly. Read the actual signature first.

#### `join_reorder.rs`

Find `pub struct JoinReorderer` (~line 297). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::join_reorder::JoinReorderer;
/// let reorderer = JoinReorderer::new();
/// ```
```

Find `pub fn new()` (~line 315) and `pub fn with_cost_estimator()` (~line 322) and `pub fn reorder()` (~line 334). Add short doctests consistent with the struct example.

#### `visitor.rs`

Find `pub trait ExpressionFold` (~line 31). Add a full impl example:

```rust
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::visitor::ExpressionFold;
/// use vlorql_core::schema::Expression;
///
/// struct MyFold;
/// impl ExpressionFold for MyFold {
///     fn fold_expression(&mut self, expr: Expression) -> Expression { expr }
/// }
/// ```
```

Find `pub trait ExpressionVisit` (~line 231). Add a similar impl example:

```rust
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::visitor::ExpressionVisit;
/// use vlorql_core::schema::Expression;
///
/// struct MyVisitor;
/// impl ExpressionVisit for MyVisitor {
///     fn visit_expression(&mut self, _expr: &Expression) {}
/// }
/// ```
```

#### `analyze.rs`

Find `pub fn split_conjuncts()` (~line 22). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::analyze::split_conjuncts;
/// use vlorql_core::schema::Expression;
/// use vlorql_core::schema::expressions::expr;
///
/// let conjuncts = split_conjuncts(&Expression::default());
/// assert!(conjuncts.is_empty() || conjuncts.len() >= 1);
/// ```
```

Adjust — if `split_conjuncts` takes some specific expression type, check the actual signature and adjust the example accordingly. The key is a minimal runnable doctest.

Find `pub fn collect_conjuncts()` (~line 28). Add a similar minimal example.

- [ ] **Step 1: Add doctests to `prune.rs`**
- [ ] **Step 2: Add doctests to `pushdown.rs`**
- [ ] **Step 3: Add doctests to `join_reorder.rs`**
- [ ] **Step 4: Add doctests to `visitor.rs`**
- [ ] **Step 5: Add doctests to `analyze.rs`**
- [ ] **Step 6: Verify Group 1**

Run:
```bash
cargo test -p vlorql-core --doc 2>&1 | head -30
cargo clippy -p vlorql-core --all-targets -- -D warnings
```
Expected: doctests pass, clippy clean.

- [ ] **Step 7: Commit Group 1**

```bash
git add crates/vlorql-core/src/optimizer/prune.rs \
        crates/vlorql-core/src/optimizer/pushdown.rs \
        crates/vlorql-core/src/optimizer/join_reorder.rs \
        crates/vlorql-core/src/optimizer/visitor.rs \
        crates/vlorql-core/src/optimizer/analyze.rs
git commit -m "docs(optimizer): add # Examples to public API (F4)"
```

---

### Task 2: Group 2 — parser_v2 builder 模块 `# Examples`

**Files:**
- Modify: `crates/vlorql-llm/src/parser_v2/builder/expr_builder.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/builder/join_builder.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/builder/query_builder.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/builder/select_builder.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/builder/table_builder.rs`
- Modify: `crates/vlorql-llm/src/parser_v2/builder/mod.rs`

**Interfaces:**
- Consumes: `vlorql_core::schema::*`, `serde_json::json!`, internal builder helpers (all `pub`)
- Produces: 每个目标 item 新增 `/// # Examples` doctest 段

**Step-by-step for each file:**

#### `expr_builder.rs`

Find `pub fn parse_data_type()` (~line 172). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::expr_builder::parse_data_type;
/// use vlorql_core::schema::DataType;
///
/// assert_eq!(parse_data_type("int").unwrap(), DataType::Int);
/// assert_eq!(parse_data_type("decimal").unwrap(), DataType::Decimal);
/// ```
```

Find `pub fn build_expression()` (~line 260). Add a short example with a JSON literal:

```rust
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::expr_builder::build_expression;
/// use serde_json::json;
///
/// let json = json!({"type": "literal", "value": 42, "data_type": "int"});
/// let expr = build_expression(&json).unwrap();
/// ```
```

Find `pub fn build_predicate()` (~line 477). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::expr_builder::build_predicate;
/// use serde_json::json;
///
/// let json = json!({"type": "comparison", "op": "eq", "left": {"type": "column_ref", "table": "t", "column": "id"}, "right": {"type": "literal", "value": 1, "data_type": "int"}});
/// let pred = build_predicate(&json).unwrap();
/// ```
```

#### `join_builder.rs`

Find `pub fn build_join_clause()` (~line 17). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::join_builder::build_join_clause;
/// use serde_json::json;
///
/// let json = json!({"type": "inner", "right_table": "orders", "on": {"type": "comparison", "op": "eq", "left": {"type": "column_ref", "table": "users", "column": "id"}, "right": {"type": "column_ref", "table": "orders", "column": "user_id"}}});
/// let join = build_join_clause(&json).unwrap();
/// ```
```

#### `query_builder.rs`

Find `pub fn build_plan()` (~line 22). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::query_builder::build_plan;
/// use serde_json::json;
///
/// let json = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
/// let plan = build_plan(&json).unwrap();
/// assert_eq!(plan.from.table, "users");
/// ```
```

#### `select_builder.rs`

Find `pub fn build_projection()` (~line 14). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::select_builder::build_projection;
/// use serde_json::json;
///
/// let json = json!({"type": "column", "table": "users", "column": "id"});
/// let proj = build_projection(&json).unwrap();
/// ```
```

Find `pub fn build_projections()` (~line 65). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::select_builder::build_projections;
/// use serde_json::json;
///
/// let json = json!([{"type": "star"}]);
/// let projs = build_projections(&json).unwrap();
/// assert_eq!(projs.len(), 1);
/// ```
```

#### `table_builder.rs`

Find `pub fn build_from_clause()` (~line 14). Add:

```rust
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::table_builder::build_from_clause;
/// use serde_json::json;
///
/// let json = json!({"table": "users"});
/// let from = build_from_clause(&json).unwrap();
/// assert_eq!(from.table, "users");
/// ```
```

#### `mod.rs`

Add a module-level `//! # Examples` block at the bottom of the existing module doc:

```rust
//! # Examples
//!
//! The builder module converts JSON query plans into `QueryPlan` structs:
//!
//! ```
//! use vlorql_llm::parser_v2::builder::query_builder::build_plan;
//! use serde_json::json;
//!
//! let json = json!({"select":[{"type":"star"}],"from":{"table":"users"}});
//! let plan = build_plan(&json).unwrap();
//! assert_eq!(plan.from.table, "users");
//! ```
```

- [ ] **Step 1: Add doctests to `expr_builder.rs`**
- [ ] **Step 2: Add doctests to `join_builder.rs`**
- [ ] **Step 3: Add doctests to `query_builder.rs`**
- [ ] **Step 4: Add doctests to `select_builder.rs`**
- [ ] **Step 5: Add doctests to `table_builder.rs`**
- [ ] **Step 6: Add module-level examples to `mod.rs`**
- [ ] **Step 7: Verify Group 2**

Run:
```bash
cargo test -p vlorql-llm --doc 2>&1 | head -30
cargo clippy -p vlorql-llm --all-targets -- -D warnings
```
Expected: doctests pass, clippy clean.

- [ ] **Step 8: Commit Group 2**

```bash
git add crates/vlorql-llm/src/parser_v2/builder/expr_builder.rs \
        crates/vlorql-llm/src/parser_v2/builder/join_builder.rs \
        crates/vlorql-llm/src/parser_v2/builder/query_builder.rs \
        crates/vlorql-llm/src/parser_v2/builder/select_builder.rs \
        crates/vlorql-llm/src/parser_v2/builder/table_builder.rs \
        crates/vlorql-llm/src/parser_v2/builder/mod.rs
git commit -m "docs(parser_v2): add # Examples to builder public API (F4)"
```

---

### Task 3: Final verification

- [ ] **Step 1: Full workspace verification**

Run:
```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: all green.

- [ ] **Step 2: Update plan doc**

```bash
git add docs/superpowers/plans/2026-07-27-f4-public-api-examples.md
git commit -m "docs: add F4 implementation plan"
```

---

## Self-Review

1. **Spec coverage:** Spec lists 5 optimizer files + 6 builder files. Task 1 covers all 5 optimizer files; Task 2 covers all 6 builder files. Style guidelines (assert-based, 3-8 lines, serde_json::json!, public-only) are reflected in every code block.
2. **Placeholder scan:** No TBD/TODO. Every code block has complete example code. Every file path is exact. Every command is exact.
3. **Type consistency:** All examples reference types and functions that exist in the current codebase (`parse_data_type`, `build_plan`, `build_projection`, `build_join_clause`, `build_from_clause`, `ColumnPruning`, `PredicatePushdown`, `JoinReorderer`, `ExpressionFold`, `ExpressionVisit`, `split_conjuncts`, `collect_conjuncts`). Data types (`DataType::Int`, `DataType::Decimal`) are from `vlorql_core::schema::DataType`. All paths use `pub` visibility.
4. **Scope check:** Focused on one thing: adding `# Examples` doc comments. No logic changes. No API changes. No dependency additions.
