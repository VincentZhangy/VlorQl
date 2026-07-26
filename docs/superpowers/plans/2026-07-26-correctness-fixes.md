# 正确性修复 Implementation Plan (C1–C4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复当前代码中 4 个已核实的正确性 bug，它们会导致非法 SQL 或解析失败。

**Architecture:** 四个独立子任务。任务 1、2 集中在 `compile/builder.rs` 的 `build_query`（任务 2 依赖任务 1 引入的 `build_query_impl`，须在其之后顺序执行）；任务 3 在 `parser_v2/recover`；任务 4 在 `parser_v2/normalize`。三、四与一/二完全独立（不同 crate/文件）。均不改动任何公共 API 签名。

**Tech Stack:** Rust (edition 2024)、serde_json、workspace 0.2.0。

**执行顺序（subagent-driven，串行）：** 任务 1 → 任务 2 → 任务 3 → 任务 4。任务 1 与任务 2 修改同一函数，必须串行；任务 3、4 与前两者无文件重叠。

## Global Constraints

以下为**全部三份计划共享**的项目级约束，每个任务的验收隐含包含本节。数值/命令逐字复制自 CI 与配置：

- **CI 必须全绿**（`.github/workflows/ci.yml`）：
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
  - `cargo check -p vlorql --examples`
  - docs job（`cargo doc`）
- **`RUSTFLAGS: -D warnings`** —— 任何 warning 即失败。
- **`#![deny(missing_docs)]`** 在 `vlorql-core` 与 `vlorql-llm` 均启用：所有新增**公共**项必须有文档注释；新增公共方法尽量带 `# Examples`（私有项不需要）。
- **不修改公共 API 签名**（本正确性计划严格遵守；私有 fn 签名可改）。
- **三方言语法各自正确**：PostgreSQL / MySQL / SQLite；参数占位符 `$N`（PG）/ `?`（MySQL、SQLite）与方言一致。
- **零第三方运行时依赖新增**（`deny.toml` 许可证白名单：MIT / Apache-2.0 / BSD-3-Clause / Unicode-3.0）。如需新依赖必须先过 `cargo deny check`。
- **TDD**：先写失败测试 → 运行确认失败 → 最小实现 → 运行确认通过 → 提交。DRY、YAGNI、频繁提交。
- 编译后 SQL 必须在目标数据库上语法合法（可直接执行）。

---

## File Structure

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql-core/src/compile/builder.rs` | 引入 `build_query_impl`，抑制 set-operation 操作数的 ORDER BY/LIMIT（任务1）；修复作用域 pop 时机（任务2） | 1、2 |
| `crates/vlorql-core/src/compile/mod.rs`（`#[cfg(test)]`） | 任务1/2 回归测试 | 1、2 |
| `crates/vlorql-llm/src/parser_v2/recover/bracket.rs` | 新增 `find_best_json_obj`（最优 JSON 候选匹配）+ 单测 | 3 |
| `crates/vlorql-llm/src/parser_v2/recover/extract.rs` | 接入 `find_best_json_obj` | 3 |
| `crates/vlorql-llm/tests/parser_v2/recover_test.rs` | 任务3 集成回归测试 | 3 |
| `crates/vlorql-llm/src/parser_v2/normalize/expr.rs` | 抽取统一的 `canonical_literal_type`，消除 int/float 不一致 | 4 |

**测试辅助（`compile/mod.rs` 内已存在，可直接用）：** `base_plan() -> QueryPlan`、`column_ref(table, column) -> Expression`、`literal(value, data_type) -> Expression`、`validated(plan) -> ValidatedPlan`。`OrderByTerm { expr: Expression, descending: bool }`（无 nulls/direction）。`CommonTableExpression { name, query, recursive }`。

---

## Task 1 (C1): 抑制 set-operation 操作数的 ORDER BY / LIMIT

**Files:**
- Modify: `crates/vlorql-core/src/compile/builder.rs:347-369`（`build_query`）、`:764`（`render_set_operation`）
- Test: `crates/vlorql-core/src/compile/mod.rs`（`#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: 现有私有方法 `push_alias_scope`、`build_with/select/from/where/group_by/having/order_by/limit_offset`、`render_set_operation`、`alias_stack`。
- Produces: 新私有方法 `build_query_impl(&mut self, plan: &QueryPlan, sql: &mut String, is_set_operand: bool) -> Result<(), VlorQLError>`。`build_query` 保持原签名（其余 5 处调用者不受影响）。**任务 2 会继续修改此函数。**

**问题：** `render_set_operation`（:764）对右操作数调用 `build_query`，渲染其自身 `order_by`/`limit`/`offset`，生成非法的 `... UNION ALL SELECT ... ORDER BY[右] ... ORDER BY[主]`。标准 SQL 中 set-operation 操作数不得单独携带这些子句。

- [ ] **Step 1: 写失败测试（操作数 ORDER BY 被抑制）**

在 `crates/vlorql-core/src/compile/mod.rs` 的 `mod tests` 内新增：

```rust
#[test]
fn union_operand_order_by_is_suppressed() {
    let mut right = base_plan();
    right.order_by = Some(vec![OrderByTerm {
        expr: column_ref("users", "id"),
        descending: false,
    }]);
    right.limit = Some(5);

    let mut plan = base_plan();
    plan.set_operation = Some(SetOperationClause {
        operation: SetOperation::UnionAll,
        right: Box::new(right),
    });

    let compiled = PostgresCompiler
        .compile(&validated(plan))
        .expect("UNION ALL should compile");

    assert!(!compiled.sql.contains("ORDER BY"),
        "set-operation operand must not carry ORDER BY, got: {}", compiled.sql);
    assert!(!compiled.sql.contains("LIMIT"),
        "set-operation operand must not carry LIMIT, got: {}", compiled.sql);
}

#[test]
fn union_keeps_outer_order_by_after_set_op() {
    let right = base_plan();
    let mut plan = base_plan();
    plan.set_operation = Some(SetOperationClause {
        operation: SetOperation::UnionAll,
        right: Box::new(right),
    });
    plan.order_by = Some(vec![OrderByTerm {
        expr: column_ref("users", "id"),
        descending: false,
    }]);

    let compiled = PostgresCompiler.compile(&validated(plan)).expect("compile");
    let order_pos = compiled.sql.find("ORDER BY").expect("outer ORDER BY present");
    let union_pos = compiled.sql.find("UNION ALL").expect("UNION ALL present");
    assert!(order_pos > union_pos, "ORDER BY must come after UNION ALL: {}", compiled.sql);
}
```

> `OrderByTerm`/`SetOperationClause`/`SetOperation` 若未在测试模块 `use` 中，补进顶部 `use crate::schema::{...}`。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vlorql-core --lib compile::tests::union_operand_order_by_is_suppressed`
Expected: FAIL —— SQL 含操作数的 `ORDER BY` / `LIMIT`。

- [ ] **Step 3: 实现 —— 拆出 `build_query_impl`（本任务先保持 pop 原位置）**

将 `crates/vlorql-core/src/compile/builder.rs:347-369` 的 `build_query` 替换为：

```rust
fn build_query(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
    self.build_query_impl(plan, sql, false)
}

/// Renders `plan` into `sql`. When `is_set_operand` is true the plan is
/// the right-hand side of a set operation (UNION/INTERSECT/EXCEPT); in
/// that position standard SQL forbids ORDER BY / LIMIT / OFFSET on the
/// operand itself (they bind to the whole set operation), so they are
/// skipped.
fn build_query_impl(
    &mut self,
    plan: &QueryPlan,
    sql: &mut String,
    is_set_operand: bool,
) -> Result<(), VlorQLError> {
    self.push_alias_scope(plan);
    self.build_with(plan, sql)?;
    self.build_select(plan, sql)?;
    self.build_from(plan, sql)?;
    self.build_where(plan, sql)?;
    self.build_group_by(plan, sql)?;
    self.build_having(plan, sql)?;
    self.alias_stack.pop();

    if let Some(set_op) = &plan.set_operation {
        self.render_set_operation(set_op, sql)?;
    }

    // Operands of a set operation must not carry their own trailing clauses.
    if !is_set_operand {
        self.build_order_by(plan, sql)?;
        self.build_limit_offset(plan, sql)?;
    }
    Ok(())
}
```

- [ ] **Step 4: 实现 —— `render_set_operation` 传 `true`**

将 `builder.rs:764` 的 `self.build_query(&set_op.right, sql)` 改为 `self.build_query_impl(&set_op.right, sql, true)`。

- [ ] **Step 5: 运行确认通过**

Run: `cargo test -p vlorql-core --lib compile`
Expected: PASS（含既有 `postgres_compiles_union_all`）。

- [ ] **Step 6: lint + Commit**

Run: `cargo clippy -p vlorql-core --all-targets -- -D warnings && cargo fmt --all -- --check`
```bash
git add crates/vlorql-core/src/compile/builder.rs crates/vlorql-core/src/compile/mod.rs
git commit -m "fix(compile): 抑制 set-operation 操作数的 ORDER BY/LIMIT (C1)"
```

---

## Task 2 (C2): 修复子查询 ORDER BY 的别名作用域时机

**依赖：任务 1（需要 `build_query_impl` 已存在）。**

**Files:**
- Modify: `crates/vlorql-core/src/compile/builder.rs`（`build_query_impl` 的 `alias_stack.pop()` 位置）
- Test: `crates/vlorql-core/src/compile/mod.rs`

**Interfaces:** 无签名变化，仅移动 `alias_stack.pop()`。

**问题：** `build_query_impl` 在 `alias_stack.pop()` 之后才 `build_order_by`，导致子查询/CTE 自身的 ORDER BY 在其别名作用域已弹出的情况下解析别名，回退到外层作用域或原样输出，产生错误的表限定符。

- [ ] **Step 1: 写失败测试（CTE 内 ORDER BY 应解析表别名）**

```rust
#[test]
fn cte_order_by_resolves_table_alias() {
    // CTE: SELECT o.total FROM orders AS o ORDER BY orders.total
    // orders 被别名为 o；ORDER BY 引用表名 orders，必须解析为别名 o。
    let cte_query = QueryPlan {
        select: vec![Projection::Column {
            table: Some("o".to_owned()),
            column: "total".to_owned(),
            alias: None,
        }],
        from: FromClause { table: "orders".to_owned(), alias: Some("o".to_owned()) },
        r#where: None,
        group_by: None,
        having: None,
        order_by: Some(vec![OrderByTerm {
            expr: column_ref("orders", "total"),
            descending: false,
        }]),
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
        distinct: false,
        distinct_on: None,
        set_operation: None,
    };

    let mut plan = base_plan();
    plan.ctes = Some(vec![CommonTableExpression {
        name: "recent".to_owned(),
        query: Box::new(cte_query),
        recursive: false,
    }]);

    let compiled = PostgresCompiler.compile(&validated(plan)).expect("compile");
    assert!(compiled.sql.contains(r#"ORDER BY "o"."total""#),
        "CTE ORDER BY must resolve the table alias to \"o\"; got: {}", compiled.sql);
}
```

> 若 `PostgresCompiler` 对 CTE 里的整数/列渲染带 CAST 影响断言子串，仅断言 `ORDER BY "o"."total"` 子串即可，不受其它子句影响。若断言意外未命中，先 `println!("{}", compiled.sql)` 观察实际输出再调整断言字面量（大小写/引号）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vlorql-core --lib compile::tests::cte_order_by_resolves_table_alias`
Expected: FAIL —— 实际输出 `ORDER BY "orders"."total"`（作用域已 pop，未解析别名）。

- [ ] **Step 3: 实现 —— 将 `alias_stack.pop()` 移到 ORDER BY 之后**

在 `build_query_impl` 中，删除 `self.build_having(plan, sql)?;` 之后那一行 `self.alias_stack.pop();`，改为在函数末尾（`build_limit_offset` 之后、`Ok(())` 之前）弹出。改写后主体为：

```rust
    self.build_having(plan, sql)?;

    if let Some(set_op) = &plan.set_operation {
        self.render_set_operation(set_op, sql)?;
    }

    if !is_set_operand {
        self.build_order_by(plan, sql)?;
        self.build_limit_offset(plan, sql)?;
    }

    // Pop AFTER ORDER BY so it resolves against this plan's own scope.
    self.alias_stack.pop();
    Ok(())
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p vlorql-core --lib compile`
Expected: PASS —— 新测试通过，且既有 compile 测试（含 union、CTE、子查询）全部保持通过。

- [ ] **Step 5: lint + Commit**

Run: `cargo clippy -p vlorql-core --all-targets -- -D warnings && cargo fmt --all -- --check`
```bash
git add crates/vlorql-core/src/compile/builder.rs crates/vlorql-core/src/compile/mod.rs
git commit -m "fix(compile): 子查询/CTE 的 ORDER BY 在作用域 pop 之前渲染 (C2)"
```

---

## Task 3 (C3): `extract_json_content` 最优 JSON 候选匹配

**Files:**
- Modify: `crates/vlorql-llm/src/parser_v2/recover/bracket.rs`（新增 `find_best_json_obj` + 单测）
- Modify: `crates/vlorql-llm/src/parser_v2/recover/extract.rs:79-82`
- Test: `crates/vlorql-llm/tests/parser_v2/recover_test.rs`

**Interfaces:**
- Consumes: 现有私有 `find_matching_close(text, open, close) -> Option<usize>`（bracket.rs:36）。
- Produces: `pub fn find_best_json_obj(text: &str) -> Option<&str>`（bracket.rs）。`extract_json_content` 签名不变。

**问题：** `find_outermost_json_obj`（bracket.rs:12-16）只取文本中**第一个** `{` 及其配对 `}`。当模型先输出含 `{` 的分析文字、或输出多个 JSON 对象时，会取到错误/不完整对象，且无回退。

- [ ] **Step 1: 写失败单测（bracket.rs 内 `mod tests`）**

```rust
#[test]
fn find_best_json_obj_skips_leading_prose_braces() {
    let input = r#"Here is my reasoning {note: skip me} and the plan:
        {"select":[{"type":"star"}],"from":{"table":"users"}}"#;
    let found = find_best_json_obj(input).expect("should find the plan object");
    let v: serde_json::Value = serde_json::from_str(found).unwrap();
    assert!(v.get("select").is_some(), "should pick the object with select/from, got: {found}");
}

#[test]
fn find_best_json_obj_prefers_plan_over_first() {
    let input = r#"{"error":"none"} {"select":[{"type":"star"}],"from":{"table":"t"}}"#;
    let found = find_best_json_obj(input).unwrap();
    let v: serde_json::Value = serde_json::from_str(found).unwrap();
    assert!(v.get("from").is_some());
}

#[test]
fn find_best_json_obj_none_when_no_valid_json() {
    assert_eq!(find_best_json_obj("no json { unbalanced"), None);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vlorql-llm --lib recover::bracket::tests::find_best_json_obj`
Expected: FAIL —— `find_best_json_obj` 未定义（编译错误）。

- [ ] **Step 3: 实现 `find_best_json_obj`（加在 `find_outermost_json_obj` 之后）**

```rust
/// Finds the "best" balanced JSON object in `text`.
///
/// Unlike [`find_outermost_json_obj`], which returns the first balanced
/// `{…}`, this scans **every** `{` start position, keeps only candidates
/// that parse as JSON, and returns the best one: objects that look like a
/// query plan (contain a `select` or `from` key) win over those that
/// don't, and among equals the longest wins. This tolerates models that
/// emit reasoning prose (possibly containing braces) before the plan, or
/// multiple JSON objects.
///
/// Returns `None` if no substring parses as a JSON object.
#[must_use]
pub fn find_best_json_obj(text: &str) -> Option<&str> {
    let mut best: Option<&str> = None;
    let mut best_score = (false, 0usize); // (looks_like_plan, byte_len)
    let mut idx = 0;
    while let Some(rel) = text[idx..].find('{') {
        let start = idx + rel;
        if let Some(end) = find_matching_close(&text[start..], '{', '}') {
            let candidate = &text[start..=start + end];
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
                let looks_like_plan = v.get("select").is_some() || v.get("from").is_some();
                let score = (looks_like_plan, candidate.len());
                if best.is_none() || score > best_score {
                    best = Some(candidate);
                    best_score = score;
                }
            }
        }
        idx = start + 1; // '{' is ASCII → safe byte boundary
    }
    best
}
```

> 不加 `# Examples` doctest（避免跨 crate 路径可达性问题；missing_docs 对函数不强制 examples，已有普通文档即满足）。`find_best_json_obj` 可见性与 `find_outermost_json_obj` 一致（`pub`）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p vlorql-llm --lib recover::bracket`
Expected: PASS。

- [ ] **Step 5: 在 `extract_json_content` 接入**

将 `crates/vlorql-llm/src/parser_v2/recover/extract.rs:79-82` 的：

```rust
    // 4. Find first JSON object anywhere in the text.
    if let Some(obj) = bracket::find_outermost_json_obj(trimmed) {
        return obj;
    }
```

替换为：

```rust
    // 4. Find the best JSON object anywhere in the text (prefers a plan-
    //    shaped object; tolerates leading reasoning prose / multiple objects).
    if let Some(obj) = bracket::find_best_json_obj(trimmed) {
        return obj;
    }
    // 4b. Fallback: first balanced object (may be recoverable by later repair).
    if let Some(obj) = bracket::find_outermost_json_obj(trimmed) {
        return obj;
    }
```

- [ ] **Step 6: 写集成回归测试**

在 `crates/vlorql-llm/tests/parser_v2/recover_test.rs` 追加（先确认该文件顶部已 `use` `extract_json_content`，样式参考文件现有测试）：

```rust
#[test]
fn extract_skips_reasoning_prose_before_plan() {
    let raw = "Let me think about {this} first.\n\
        {\"select\":[{\"type\":\"star\"}],\"from\":{\"table\":\"users\"}}";
    let extracted = extract_json_content(raw);
    let v: serde_json::Value = serde_json::from_str(extracted).unwrap();
    assert!(v.get("select").is_some(), "extracted: {extracted}");
}
```

- [ ] **Step 7: 运行 + lint**

Run: `cargo test -p vlorql-llm && cargo clippy -p vlorql-llm --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全绿。

- [ ] **Step 8: Commit**

```bash
git add crates/vlorql-llm/src/parser_v2/recover/bracket.rs \
        crates/vlorql-llm/src/parser_v2/recover/extract.rs \
        crates/vlorql-llm/tests/parser_v2/recover_test.rs
git commit -m "fix(parser_v2): 增加最优 JSON 候选匹配，容忍分析文字/多对象 (C3)"
```

---

## Task 4 (C4): 统一字面量数值类型规范化

**Files:**
- Modify: `crates/vlorql-llm/src/parser_v2/normalize/expr.rs`（`fix_literal_type_aliases` :35-42、`repair_expression_value` :84-90，新增私有 `canonical_literal_type`）
- Test: `crates/vlorql-llm/src/parser_v2/normalize/expr.rs` 内 `#[cfg(test)] mod tests`

**Interfaces:** Produces 私有 `fn canonical_literal_type(type_val: &str, value: Option<&serde_json::Value>) -> &'static str`。无公共 API 变化。

**问题：** 两条路径对同一 `type:"integer"` 字面量映射不一致 —— `fix_literal_type_aliases`（:37）→ `"int"`，`repair_expression_value`（:86）把 `integer|number|float` **全部** → `"float"`。

- [ ] **Step 1: 写失败单测**

```rust
#[test]
fn integer_literal_normalizes_to_int_consistently() {
    let mut v = serde_json::json!({"type": "integer", "value": 5});
    assert!(repair_expression_value(&mut v));
    assert_eq!(v.get("data_type").and_then(|d| d.as_str()), Some("int"),
        "integer 字面量应规范化为 int，而非 float");
    assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("literal"));
}

#[test]
fn number_literal_disambiguates_by_value() {
    let mut i = serde_json::json!({"type": "number", "value": 3});
    assert!(repair_expression_value(&mut i));
    assert_eq!(i.get("data_type").and_then(|d| d.as_str()), Some("int"));

    let mut f = serde_json::json!({"type": "number", "value": 3.5});
    assert!(repair_expression_value(&mut f));
    assert_eq!(f.get("data_type").and_then(|d| d.as_str()), Some("float"));
}
```

> 测试所在 `mod tests` 用 `use super::*;` 即可调用同模块私有 `repair_expression_value`。若 expr.rs 尚无 `mod tests`，新建一个。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p vlorql-llm --lib normalize::expr`
Expected: FAIL —— `integer_literal_normalizes_to_int_consistently` 得到 `Some("float")`。

- [ ] **Step 3: 新增共享 `canonical_literal_type`（加在 expr.rs 这两个函数附近）**

```rust
/// Maps a raw literal type tag plus its JSON value to the canonical
/// `data_type` string. The ambiguous `"number"` tag is disambiguated by
/// inspecting whether the value is integral, so both normalization paths
/// agree on `int` vs `float`.
fn canonical_literal_type(type_val: &str, value: Option<&Value>) -> &'static str {
    match type_val {
        "string" => "string",
        "integer" => "int",
        "float" => "float",
        "number" => match value {
            Some(Value::Number(n)) if n.as_i64().is_some() || n.as_u64().is_some() => "int",
            Some(Value::Number(_)) => "float",
            _ => "int",
        },
        "boolean" => "boolean",
        "null" => "null",
        _ => "null",
    }
}
```

- [ ] **Step 4: 让 `repair_expression_value` 使用它**

将 `expr.rs:84-90` 的 `let canonical_dt = match type_val.as_str() { ... };` 替换为：

```rust
                let value = obj.get("value");
                let canonical_dt = canonical_literal_type(type_val.as_str(), value);
```

保留其后 `obj.insert("type", Value::String("literal".to_owned()))` 与 `obj.insert("data_type", Value::String(canonical_dt.to_owned()))`。注意借用顺序：先取 `value` / 算 `canonical_dt`，再对 `obj` 可变 `insert`（`value` 是不可变借用，需在可变 insert 前结束其使用；`canonical_dt` 为 `&'static str` 不借 `obj`，安全）。

- [ ] **Step 5: 让 `fix_literal_type_aliases` 使用它（消除 :35-42 重复表）**

将 `expr.rs:35-42` 的 `let canonical_dt = match type_val { ... _ => return false };` 替换为：

```rust
    let canonical_dt = match type_val {
        "string" | "integer" | "number" | "float" | "boolean" | "null" => {
            canonical_literal_type(type_val, obj.get("value"))
        }
        _ => return false,
    };
```

保留原有 `obj.contains_key("value")` 早退与其后 `insert` 逻辑不变。

- [ ] **Step 6: 运行确认通过**

Run: `cargo test -p vlorql-llm --lib normalize`
Expected: PASS，且既有 expr/value 测试（value.rs:245-293 等）保持通过。

- [ ] **Step 7: 全量测试 + lint**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全绿。

- [ ] **Step 8: Commit**

```bash
git add crates/vlorql-llm/src/parser_v2/normalize/expr.rs
git commit -m "fix(parser_v2): 统一字面量数值类型规范化，integer→int 一致 (C4)"
```

---

## Self-Review

- **Spec coverage**：C1（set-op 操作数 ORDER BY 泄漏）→任务1；C2（子查询 ORDER BY 作用域时机）→任务2；C3（最优有效 JSON 匹配）→任务3；C4（integer 规范化不一致）→任务4。四项现状核实的正确性 bug 均有独立任务。
- **Placeholder scan**：无 TBD/TODO；每个代码步骤给出完整代码。字段名已按当前源码核对。
- **Type consistency**：`build_query_impl(&mut self, &QueryPlan, &mut String, bool)` 在任务1定义、任务2沿用、`render_set_operation` 调用；`find_best_json_obj(&str)->Option<&str>` 任务3定义并在 `extract.rs` 使用；`canonical_literal_type(&str, Option<&Value>)->&'static str` 任务4定义并被两个调用点使用。
- **依赖**：任务2依赖任务1（同函数）。执行顺序 1→2→3→4，串行，绝不并行派实现 subagent。
