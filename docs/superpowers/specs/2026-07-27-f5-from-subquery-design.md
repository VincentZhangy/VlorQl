# F5: FROM (subquery) 派生表 — Design Spec

> **Date:** 2026-07-27
> **Branch:** `feat/0.4.0`
> **Status:** Approved

## Goal

将 `FromClause` 从 struct 改为 enum，增加 `Subquery` 变体使其支持 `FROM (SELECT ...) AS alias` 派生表语法，并同步更新编译器、校验器、优化器等全部消费方。

## Architecture

**核心变更：** `schema/query_plan.rs` 中 `FromClause` 从 `struct { table, alias }` 改为 `enum { Table, Subquery }`。序列化使用 `serde(tag = "type")`。新增 `FromClause::table()` 便捷构造器以简化已有调用点的迁移。

---

## Design

### Data Model

```rust
/// The source relation for a query or join.
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
        /// Required alias for a subquery.
        alias: Option<String>,
    },
}

impl FromClause {
    /// Creates a table-reference `FromClause` — convenience constructor
    /// that preserves backward compatibility for existing call sites.
    pub fn table(name: impl Into<String>, alias: Option<String>) -> Self {
        Self::Table { table: name.into(), alias }
    }
}
```

### Serialization

```json
// 表引用（向后兼容）
{"type": "table", "table": "users", "alias": "u"}
// 派生表
{"type": "subquery", "query": {"select": [...], "from": {...}}, "alias": "recent"}
```

### Changes by Module

| Module | Change |
|--------|--------|
| `schema/query_plan.rs` | `FromClause` struct → enum + `table()` helper |
| `compile/builder.rs` | `render_from_clause()`: add `Subquery` branch → recursive compile + `(...)` wrap; `collect_aliases()`: add `Subquery` branch |
| `validate/schema.rs` | `validate_plan()`: recursively validate subquery's plan against the same schema |
| `validate/audit.rs` | `audit()`: recursively audit subquery's plan |
| `optimizer/fold.rs` | `fold_plan()` / `default_fold_plan`: add `Subquery` match arm |
| `optimizer/prune.rs` | `prune_plan()` / `used_columns()`: handle `Subquery` |
| `optimizer/pushdown.rs` | `push_plan()`: handle `Subquery` |
| `optimizer/join_reorder.rs` | `Relation::from` field type update; `build()` handle `Subquery` |
| All `compile/mod.rs` tests | `FromClause { table, alias }` → `FromClause::table(name, alias)` |
| All `cache/**` tests | Same migration pattern |
| All `benches/` | Same migration pattern |
| All other test/doctest code | Same migration pattern |

### Recursive Validation

Schema validation must recurse into subquery plans:

```rust
// validate/schema.rs
fn validate_plan_with_outer(plan, schema, errors, outer_scope) {
    // ... existing checks ...
    
    // Recurse into FROM subquery
    if let FromClause::Subquery { query, .. } = &plan.from {
        validate_plan_with_outer(query, schema, errors, None);
    }
}
```

### Compiler

```rust
// compile/builder.rs
fn render_from_clause(&self, from: &FromClause) -> Result<String, VlorQLError> {
    match from {
        FromClause::Table { table, alias } => {
            // existing logic
        }
        FromClause::Subquery { query, alias } => {
            let mut sub_sql = String::new();
            // Push a new alias scope, build the inner query
            self.push_alias_scope(query);
            self.build_query(query, &mut sub_sql)?;
            self.alias_stack.pop();
            let alias_str = alias.as_deref()
                .map(|a| format!(" AS {}", self.quote_identifier(a)))
                .unwrap_or_default();
            Ok(format!("({sub_sql}){alias_str}"))
        }
    }
}
```

### Non-goals

- CTE 已在 `QueryPlan.ctes` 中支持，保持不动
- `Expression::Subquery` 已存在，保持不变
- 不改 `VlorQLError` 类型
- 不改 `Cache` trait 定义
- 不改公共 API 签名（仅在 `FromClause` 内部调整）

---

## Testing

| Test | Description |
|------|-------------|
| `compiles_subquery_in_from` | Plan with `FROM (SELECT ...) AS t` compiles correctly |
| `subquery_in_join` | `JOIN (SELECT ...) AS t ON ...` compiles correctly |
| `validates_subquery_schema` | Schema validation recurses into subquery |
| `audit_recurses_into_subquery` | Audit stage checks subquery identifiers |
| `fold_visits_subquery` | ExpressionFold visits subquery plan |
| `from_clause_table_helper` | `FromClause::table()` creates correct variant |

---

## Migration Strategy

All existing call sites using struct literal syntax `FromClause { table, alias }` must be updated to `FromClause::table(table, alias)`. This is mechanical and can be done via search-and-replace.
