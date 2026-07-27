# F4: 公共 API `# Examples` 覆盖 — Design Spec

> **Date:** 2026-07-27
> **Branch:** `feat/0.4.0`
> **Status:** Approved

## Goal

为 `vlorql-core/src/optimizer/` 和 `vlorql-llm/src/parser_v2/builder/` 中缺失 `# Examples` 的公共函数 / struct 补充 doc-testable 的 `# Examples`，确保 `cargo test --workspace` 中 doctest 全部通过。

**不涉及：** 逻辑变更、公共 API 签名变更、功能增删。

---

## Scope

### Group 1 — `vlorql-core/src/optimizer/`（5 个文件，~2800 行）

| File | Target |
|------|--------|
| `prune.rs` | `ColumnPruning` struct + `new()` / `with_schema()` |
| `pushdown.rs` | `PredicatePushdown` struct + `rewrite()` |
| `join_reorder.rs` | `JoinReorderer` struct + `new()` / `with_cost_estimator()` / `reorder()` |
| `visitor.rs` | `ExpressionFold` trait + `ExpressionVisit` trait (trivial impl example) |
| `analyze.rs` | `split_conjuncts()` / `collect_conjuncts()` (public helpers) |

### Group 2 — `vlorql-llm/src/parser_v2/builder/`（6 个文件，~1500 行）

| File | Target |
|------|--------|
| `expr_builder.rs` | `parse_data_type()` / `build_expression()` / `build_predicate()` |
| `join_builder.rs` | `build_join_clause()` |
| `query_builder.rs` | `build_plan()` |
| `select_builder.rs` | `build_projection()` / `build_projections()` |
| `table_builder.rs` | `build_from_clause()` |
| `mod.rs` | Module-level `//! # Examples` |

### Non-goals

- `vlorql` (facade) 与 `vlorql-cli` 不在本次范围
- 不为私有/受限可见性函数加示例
- 不改动现有 examples，只补缺

---

## Style Guidelines

- 每个 `# Examples` 使用 `assert_eq!` / `assert!` / `assert!(...)` 验证结果，确保可作为 doctest 执行。
- 引用 `vlorql_core::schema::*` 中的 `QueryPlan`、`Expression`、`DataType` 等核心类型；必要时引用 `serde_json::json!` 构造输入。
- 短小聚焦：每个 example 3–8 行，展示最典型用法。不泛化。
- 对 trait（如 `ExpressionFold`），给出一个简单自定义实现的示例。
- 遵循 `#![deny(missing_docs)]`：仅有 `///` + `# Examples` 已满足要求；不需要单独写 "Returns..." 长篇说明已有文档的函数。

---

## Execution Plan

Use **parallel subagents** (approach A):

1. **Subagent 1 (optimizer)** — `crates/vlorql-core/src/optimizer/` 下 5 个文件
2. **Subagent 2 (parser_v2 builder)** — `crates/vlorql-llm/src/parser_v2/builder/` 下 6 个文件

Each subagent reads the file, identifies `pub` items without `# Examples`, and adds concise `# Examples` sections.

### Verification

After both subagents complete:

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### Commit

```
git add crates/vlorql-core/src/optimizer/ crates/vlorql-llm/src/parser_v2/builder/
git commit -m "docs: 补齐 optimizer 与 parser_v2 builder 公共 API 的 # Examples (F4)"
```

---

## Risks & Mitigations

| Risk | Mitigation |
|------|-----------|
| Doctest 中引用了跨 crate 私有类型 | 只使用公共 API 和 `serde_json`；subagent 验证 `cargo test` |
| 示例过长导致文档臃肿 | 每个示例控制在 3–8 行 |
| 两个 subagent 修改同一文件 | 文件无重叠，各自独立模块 |
