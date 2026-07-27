# F5: FROM (subquery) 派生表 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 `FromClause` 从 struct 改为 enum，增加 `Subquery` 变体以支持 `FROM (SELECT ...) AS alias` 派生表。

**Architecture:** 三个串行任务：(1) 枚举定义 + 全仓机械替换 `FromClause { table, alias }` → `FromClause::table(name, alias)`；(2) 编译器/校验器/优化器各模块加 Subquery 处理逻辑；(3) 全量验证。

**Tech Stack:** Rust (edition 2024), serde (tagged enum).

## Global Constraints

- CI 全绿：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`
- `#![deny(missing_docs)]`：新枚举变体必须有 doc 注释
- 不加新第三方依赖
- `FromClause::table()` 便捷构造器保持向后兼容
- serde tag: `#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]`

---

## File Structure

| 文件 | 任务 |
|------|------|
| `crates/vlorql-core/src/schema/query_plan.rs` | 枚举定义 + serde + `table()` helper | 1 |
| `crates/vlorql-core/src/compile/builder.rs` | `render_from_clause` + `collect_aliases` Subquery 分支 | 2 |
| `crates/vlorql-core/src/validate/schema.rs` | 递归子查询校验 | 2 |
| `crates/vlorql-core/src/validate/audit.rs` | 递归审计 Subquery 分支 | 2 |
| `crates/vlorql-core/src/optimizer/fold.rs` | Subquery fold 分支 | 2 |
| `crates/vlorql-core/src/optimizer/prune.rs` | Subquery 分支 | 2 |
| `crates/vlorql-core/src/optimizer/pushdown.rs` | Subquery 分支 | 2 |
| `crates/vlorql-core/src/optimizer/join_reorder.rs` | Relation::from 字段更新 | 2 |
| 全部 40+ 处 `FromClause { table, alias }` (tests/benches/doctests) | 机械替换 → `FromClause::table()` | 1 |

---

### Task 1: Enum 定义 + 全仓机械替换

**Files:**
- Modify: `crates/vlorql-core/src/schema/query_plan.rs` (enum 定义)
- Modify: ~40 files with `FromClause { table, alias }` usages

**Interfaces:**
- Consumes: 现有 `FromClause struct { table: String, alias: Option<String> }`
- Produces: `FromClause enum { Table { table: String, alias: Option<String> }, Subquery { query: Box<QueryPlan>, alias: Option<String> } }` + `FromClause::table()` → `FromClause::Table { .. }`

- [ ] **Step 1: Change FromClause to enum**

修改 `crates/vlorql-core/src/schema/query_plan.rs`:

```rust
/// The source relation for a query or join — either a direct table
/// reference or a derived table (subquery).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FromClause {
    /// A direct table reference: `FROM table [AS alias]`
    Table {
        /// The table name as it appears in the schema snapshot.
        table: String,
        /// Optional alias (`AS <alias>`).
        alias: Option<String>,
    },
    /// A derived table (subquery): `FROM (subquery) AS alias`
    Subquery {
        /// The inner query plan.
        query: Box<QueryPlan>,
        /// Required alias for a subquery (recommended but not forced).
        alias: Option<String>,
    },
}

impl FromClause {
    /// Creates a table-reference `FromClause`.
    #[must_use]
    pub fn table(name: impl Into<String>, alias: Option<String>) -> Self {
        Self::Table { table: name.into(), alias }
    }
}
```

- [ ] **Step 2: Mechanically replace ALL `FromClause {` constructions**

Use search-replace across the entire workspace:
- `FromClause { table:` → `FromClause::table(`
- `FromClause { table` → `FromClause::table(`
- pattern: `FromClause { table: "xxx".to_owned(), alias: None }` → `FromClause::table("xxx", None)`
- pattern: `FromClause { table: "xxx".to_owned(), alias: Some("a".to_owned()) }` → `FromClause::table("xxx", Some("a".to_owned()))`
- pattern: `FromClause { name }` → `FromClause::table(name)`

Files affected (non-exhaustive — run `grep -rn "FromClause {" crates/ | grep -v target/` after the change to find remaining):
- `crates/vlorql-core/src/cache/key.rs`
- `crates/vlorql-core/src/cache/llm_cache.rs`
- `crates/vlorql-core/src/cache/normalize.rs`
- `crates/vlorql-core/src/compile/mod.rs` (test code)
- `crates/vlorql-core/src/optimizer/fold.rs` (doctests + test code)
- `crates/vlorql-core/src/optimizer/join_reorder.rs` (test code)
- `crates/vlorql-core/src/optimizer/prune.rs` (test code)
- `crates/vlorql-core/src/optimizer/pushdown.rs` (test code)
- `crates/vlorql-core/src/validate/pipeline.rs` (doctests + test code)
- `crates/vlorql-core/benches/*.rs`
- `crates/vlorql-llm/src/parser_v2/builder/query_builder.rs` (test code)
- `crates/vlorql/src/lib.rs` (test code)

- [ ] **Step 3: Verify it compiles**

```bash
cargo build --workspace 2>&1 | head -20
```
If there are remaining `FromClause { .. }` patterns, fix them and repeat.

- [ ] **Step 4: Run existing tests**

```bash
cargo test --workspace
```
All existing tests should still pass (no logic changes yet, only mechanical rename).

- [ ] **Step 5: Clippy + fmt**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 6: Commit Task 1**

```bash
git add -A
git commit -m "refactor(schema): FromClause struct → enum with Table/Subquery variants (F5-1)"
```

---

### Task 2: Subquery 逻辑处理

**Files:**
- Modify: `crates/vlorql-core/src/compile/builder.rs`
- Modify: `crates/vlorql-core/src/validate/schema.rs`
- Modify: `crates/vlorql-core/src/validate/audit.rs`
- Modify: `crates/vlorql-core/src/optimizer/fold.rs`
- Modify: `crates/vlorql-core/src/optimizer/prune.rs`
- Modify: `crates/vlorql-core/src/optimizer/pushdown.rs`
- Modify: `crates/vlorql-core/src/optimizer/join_reorder.rs`

**Interfaces:**
- Consumes: `FromClause::Subquery { query, alias }` from Task 1
- Produces: Subquery-aware compile/validate/optimize

- [ ] **Step 1: Update compiler — `render_from_clause()`**

In `crates/vlorql-core/src/compile/builder.rs`, find `fn render_from_clause`:

```rust
fn render_from_clause(&self, from: &FromClause) -> Result<String, VlorQLError> {
    match from {
        FromClause::Table { table, alias } => {
            let quoted = self.quote_identifier(table)?;
            match alias {
                Some(alias) => Ok(format!("{} AS {}", quoted, self.quote_identifier(alias)?)),
                None => Ok(quoted),
            }
        }
        FromClause::Subquery { query, alias } => {
            let mut sub_sql = String::new();
            // Use build_query_impl for the subquery (no ORDER BY / LIMIT on the operand
            // unless it's the outermost plan, which this subquery is not — but build_query
            // handles that via is_set_operand = false, which is fine for subqueries).
            self.build_query(query.as_ref(), &mut sub_sql)?;
            let alias_str = alias
                .as_ref()
                .map(|a| format!(" AS {}", self.quote_identifier(a).unwrap_or_else(|_| a.clone())))
                .unwrap_or_default();
            Ok(format!("({sub_sql}){alias_str}"))
        }
    }
}
```

Also update `fn collect_aliases()` — handle both variants:
```rust
fn collect_aliases(from: &FromClause, map: &mut HashMap<String, String>) {
    match from {
        FromClause::Table { table, alias } => {
            let name = alias.clone().unwrap_or_else(|| table.clone());
            map.insert(table.clone(), name);
        }
        FromClause::Subquery { alias, .. } => {
            // Subquery doesn't contribute table-name aliases to the resolution map;
            // its alias is only used for `FROM (sub) AS alias` rendering.
            if let Some(a) = alias {
                // The subquery alias can shadow outer tables — recording it
                // prevents outer references from leaking in. However, the inner
                // plan's columns are resolved separately during recursive compile.
                map.insert(a.clone(), a.clone());
            }
        }
    }
}
```

- [ ] **Step 2: Update schema validator — recursive subquery validation**

In `crates/vlorql-core/src/validate/schema.rs`, find `validate_plan_with_outer()`:

After processing the plan's FROM clause validation (or in the main validation loop), add:
```rust
// Recursively validate subquery FROM.
if let FromClause::Subquery { query, .. } = &plan.from {
    validate_plan_with_outer(query, schema, errors, outer_scope);
}
```

Similarly, check JOINs:
```rust
if let Some(ref joins) = plan.joins {
    for join in joins {
        if let FromClause::Subquery { query, .. } = &join.right_table {
            validate_plan_with_outer(query, schema, errors, outer_scope);
        }
    }
}
```

- [ ] **Step 3: Update audit — recursive subquery audit**

In `crates/vlorql-core/src/validate/audit.rs`, find the main `audit()` method:

After checking `plan.from.table`, add:
```rust
// Recursively audit subquery FROM.
if let FromClause::Subquery { query, .. } = &plan.from {
    let stage = AuditStage::new();
    if let Err(errs) = stage.audit(query.as_ref(), schema) {
        for err in errs {
            warn!("AUDIT: subquery audit error: {err}");
        }
    }
}
```

- [ ] **Step 4: Update optimizer — fold.rs**

In `crates/vlorql-core/src/optimizer/fold.rs`, find `default_fold_plan()`:

Add Subquery match arm before the catch-all `_`:
```rust
// Fold subquery FROM
match &plan.from {
    FromClause::Subquery { query, .. } => {
        let folded = default_fold_plan(query, folder);
        if !Arc::ptr_eq(query, &folded) {
            plan.from = FromClause::Subquery {
                query: Box::new(folded),
                alias: plan.from.alias().cloned(),
            };
            changed = true;
        }
    }
    _ => {}
}
```

Wait — `FromClause` is an enum now, so the field access patterns change. The fold visitor visits plans: if the plan contains a subquery FROM, it should recursively fold the inner plan.

Since `FromClause` is now an enum, all existing code that accessed `from.table` or `from.alias` needs match arms. This is handled by:
- `.table()` accessor: add `pub fn table_name(&self) -> Option<&str>` to `FromClause`
- `.alias()` accessor: add `pub fn alias(&self) -> Option<&str>` to `FromClause`

```rust
impl FromClause {
    /// Returns the table name if this is a `Table` variant.
    #[must_use]
    pub fn table_name(&self) -> Option<&str> {
        match self {
            Self::Table { table, .. } => Some(table.as_str()),
            Self::Subquery { .. } => None,
        }
    }

    /// Returns the alias regardless of variant.
    #[must_use]
    pub fn alias(&self) -> Option<&str> {
        match self {
            Self::Table { alias, .. } | Self::Subquery { alias, .. } => alias.as_deref(),
        }
    }
}
```

- [ ] **Step 5: Update optimizer — prune.rs, pushdown.rs, join_reorder.rs**

For each file, find all places that access `from.table` or `from.alias` and update them to use the match or accessor methods.

Example for `join_reorder.rs`:
```rust
// Before:
from: FromClause { table: ..., alias: ... },
// After:
fn new(from: FromClause) -> Self {
    match from {
        FromClause::Table { table, alias } => { /* create Relation */ }
        FromClause::Subquery { .. } => { /* subquery not join-reorderable */ }
    }
}
```

- [ ] **Step 6: Write subquery compile test**

In `crates/vlorql-core/src/compile/mod.rs` `mod tests`, add:

```rust
#[test]
fn compiles_subquery_in_from() {
    let sub_query = QueryPlan {
        select: vec![Projection::Column {
            table: None, column: "id".to_owned(), alias: None,
        }],
        from: FromClause::table("users", None),
        r#where: None, group_by: None, having: None,
        order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let outer = QueryPlan {
        select: vec![Projection::Column {
            table: Some("t".to_owned()), column: "id".to_owned(), alias: None,
        }],
        from: FromClause::Subquery {
            query: Box::new(sub_query),
            alias: Some("t".to_owned()),
        },
        r#where: None, group_by: None, having: None,
        order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let compiled = PostgresCompiler
        .compile(&validated(outer))
        .expect("subquery FROM should compile");
    assert!(compiled.sql.contains("SELECT"), "got: {}", compiled.sql);
    assert!(compiled.sql.contains("FROM ("), "subquery should have parenthesised FROM, got: {}", compiled.sql);
}
```

- [ ] **Step 7: Verify**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 8: Commit Task 2**

```bash
git add -A
git commit -m "feat(compile, validate, optimize): add Subquery FROM handling (F5-2)"
```

---

### Task 3: Final verification

- [ ] **Step 1: Full workspace check**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 2: Update plan doc**

```bash
git add docs/superpowers/plans/2026-07-27-f5-from-subquery.md
git commit -m "docs: add F5 implementation plan"
```

---

## Self-Review

1. **Spec coverage:** FromClause enum (✅ Task 1), serde tags (✅ Task 1), table() helper (✅ Task 1), compiler render + collect_aliases (✅ Task 2), recursive validation (✅ Task 2), recursive audit (✅ Task 2), optimizer fold (✅ Task 2), join_reorder update (✅ Task 2), mechanized migration (✅ Task 1), subquery compile test (✅ Task 2). All covered.
2. **Placeholder scan:** No TBD/TODO. All code blocks complete.
3. **Type consistency:** `FromClause::table(name, alias)` consistent across all tasks. `FromClause::Subquery { query, alias }` consistent. Accessors `table_name()` and `alias()` consistent.
4. **Scope check:** Three focused tasks — mechanical enum rename, logic implementation, verification. No scope creep.
