# Dialect Adapter System — Design Doc

## Problem

VlorQl currently supports three hardcoded SQL dialects (Postgres, SQLite, MySQL) via a
`SqlDialect` enum with dedicated compiler structs. Adding a new dialect (MSSQL, Oracle,
BigQuery, Snowflake, etc.) requires modifying Rust source code. Users with small models
struggle with the complex `QueryPlan` JSON schema, and there is no mechanism for
per-deployment customization.

## Goals

1. Allow users to define custom SQL dialects via TOML/YAML config files
2. Provide a SQL rewrite engine for post-compilation transformations
3. Support "prompt skills" — injectable instructions that simplify the output schema for small models
4. Keep 100% backward compatibility with existing `SqlDialect::Postgres` / `Sqlite` / `MySql`
5. Update all documentation

## Non-Goals

- Online dialect editing / hot-reload
- GUI for dialect config
- Automatic dialect detection from connection strings

---

## Architecture Overview

```
User Query
    │
    ▼
┌──────────────┐     ┌──────────────────┐
│ PromptBuilder│────▶│      LLM         │
│  + Skills    │     │  (small / large) │
└──────────────┘     └────────┬─────────┘
                              │ QueryPlan JSON
                              ▼
┌───────────────────────────────────────┐
│        Validation Pipeline            │
│  (Schema → Policy → Operand → Dialect)│
└──────────────┬────────────────────────┘
               │ ValidatedPlan
               ▼
┌───────────────────────┐     ┌──────────────────┐
│   DialectRegistry     │────▶│  DialectConfig   │
│  (builtin + custom)   │     │  (quoting, ph,   │
│                       │     │   LIMIT syntax,  │
│                       │     │   feature flags) │
└──────────┬────────────┘     └──────────────────┘
           │
           ▼
┌───────────────────────┐
│    QueryBuilder       │
│  (dialect-aware SQL)  │
└──────────┬────────────┘
           │ CompiledQuery
           ▼
┌───────────────────────┐
│    RewriteEngine      │
│  (user-defined rules) │
└──────────┬────────────┘
           │ Final SQL
           ▼
      Database Driver
```

---

## Layer 1: Config-Driven Dialect Definitions

### Current State

```rust
// compile/registry.rs
pub fn get_compiler(dialect: SqlDialect) -> Box<dyn SqlCompiler> {
    match dialect {
        SqlDialect::Postgres => Box::new(PostgresCompiler),
        SqlDialect::Sqlite => Box::new(SQLiteCompiler),
        SqlDialect::MySql => Box::new(MySQLCompiler),
    }
}
```

### Target State

```rust
// New: compile/dialect_config.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialectConfig {
    pub name: String,
    pub identifier_quote: String,       // "double_quote" | "backtick" | "bracket" | "never"
    pub placeholder: String,            // "$index" | "?" | "@p{index}"
    pub limit_offset: String,           // "LIMIT {limit} OFFSET {offset}" | "LIMIT {offset}, {limit}" | "OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
    pub top_syntax: Option<String>,     // "SELECT TOP {limit}"
    pub supports_cte: bool,
    pub supports_window_functions: bool,
    pub supports_json_operations: bool,
    pub supports_offset: bool,
    pub supports_fetch: bool,
    pub allow_distinct: bool,
    pub allow_select_distinct: bool,
    pub max_joins: Option<usize>,
    pub allowed_join_types: Vec<String>,
    pub allowed_functions: Vec<String>,
    pub denied_functions: Vec<String>,
    pub max_group_by_columns: Option<usize>,
    pub type_mappings: HashMap<String, String>,   // "ilike" -> "LIKE"
    pub function_name_mappings: HashMap<String, String>, // "NOW" -> "GETDATE"
}
```

### DialectRegistry

```rust
#[derive(Debug, Default)]
pub struct DialectRegistry {
    builtin: HashMap<SqlDialect, Box<dyn SqlCompiler>>,
    custom: HashMap<String, Arc<DialectConfig>>,
}

impl DialectRegistry {
    pub fn register(name: &str, config: DialectConfig) -> Result<()>;
    pub fn get(name: &str) -> Result<Box<dyn SqlCompiler>>;
    pub fn load_from_file(path: &str) -> Result<DialectConfig>;
}
```

### Compiler Dispatch

When a dialect string is provided, registry checks:
1. Builtin hardcoded compilers first (Postgres, SQLite, MySQL)
2. Custom config-based dialects second
3. Error if neither matches

### TOML Config Example

```toml
# dialects/mssql.toml
name = "mssql"
identifier_quote = "double_quote"
placeholder = "@p{index}"
limit_offset = "OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"

supports_cte = true
supports_window_functions = true
supports_offset = false  # uses FETCH instead
supports_fetch = true
allow_distinct = true
allow_select_distinct = true

[type_mappings]
ilike = "LIKE"

[function_name_mappings]
"now" = "GETDATE"
"string_agg" = "STRING_AGG"
```

### QueryBuilder Changes

`QueryBuilder` is refactored from enum-based dispatch to config-based:

```rust
pub fn new(
    plan: &ValidatedPlan,
    config: &DialectConfig,   // was SqlDialect + IdentifierQuoting
) -> Self;
```

`render_expression`, `render_predicate`, `build_limit_offset` all read from `config` instead of matching on `SqlDialect` variants.

---

## Layer 2: SQL Rewrite Engine

### RewriteRule

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewriteRule {
    pub name: String,
    pub description: Option<String>,
    /// Regex pattern with named captures, or SQL pattern with {placeholders}
    pub match_pattern: String,
    /// Replacement template referencing captures/placeholders
    pub replace_template: String,
    /// Optional: only apply for specific dialect(s)
    pub dialect_filter: Option<Vec<String>>,
}

#[derive(Debug, Default)]
pub struct RewriteEngine {
    rules: Vec<RewriteRule>,
}

impl RewriteEngine {
    pub fn apply(&self, sql: &str, dialect: &str) -> Result<String>;
    pub fn load_rules(path: &str) -> Result<Vec<RewriteRule>>;
}
```

### Execution Flow

After `QueryBuilder::build()`, the `CompiledQuery.sql` passes through `RewriteEngine::apply()`.

### TOML Rules Example

```toml
# rules/bigquery.toml
[[rules]]
name = "ilike_to_lower"
description = "BigQuery has no ILIKE; use LOWER() on both sides"
match_pattern = "(?P<left>.*?)\s+ILIKE\s+(?P<right>.*?)(?P<rest>\s|$)"
replace_template = "LOWER(${left}) LIKE LOWER(${right})${rest}"
dialect_filter = ["bigquery"]

[[rules]]
name = "extract_to_date_trunc"
match_pattern = "EXTRACT\\((?P<part>\\w+)\\s+FROM\\s+(?P<expr>[^)]+)\\)"
replace_template = "DATE_TRUNC('${part}', ${expr})"
```

---

## Layer 3: Prompt Skills System

### PromptSkill

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSkill {
    pub name: String,
    pub description: Option<String>,
    /// Instructions injected into the system prompt
    pub instructions: Vec<String>,
    /// Optional: simplify JSON output schema for small models
    pub simplify_schema: bool,
    /// Features to explicitly forbid (removes from schema)
    pub forbid_features: Vec<String>,  // "set_operation", "window_functions", "ctes"
    /// Optional: force disable certain QueryPlan fields
    pub disable_output_fields: Vec<String>,
    /// Optional: add extra examples
    pub examples: Vec<ExamplePair>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExamplePair {
    pub question: String,
    pub plan: serde_json::Value,
}
```

### Integration with PromptBuilder

```rust
impl PromptBuilder {
    pub fn with_skill(mut self, skill: &PromptSkill) -> Self;
    pub fn with_skills(mut self, skills: &[PromptSkill]) -> Self;
}
```

When skills are provided:
1. `instructions` are appended to the Planning Rules section
2. `simplify_schema` = true → `compact_output_schema()` emits a reduced schema (remove `set_operation`, `window_functions`, `ctes`, etc.)
3. `forbid_features` → extra `## Constraints` section telling the model not to use those features
4. `examples` are appended to the Examples section

### TOML Skill Example

```toml
# skills/small-model.toml
name = "small-model-skill"
description = "Simplifies output schema for small / local LLMs"
simplify_schema = true
forbid_features = ["set_operation", "window_functions", "ctes", "distinct_on"]
disable_output_fields = ["set_operation", "ctes", "distinct_on"]

instructions = [
    "Keep queries simple: max 2 joins, no subqueries in WHERE.",
    "Always use table aliases (first letter of table name).",
    "Prefer LEFT JOIN over NOT IN / NOT EXISTS.",
    "GROUP BY must include every non-aggregated column in SELECT.",
]

[[examples]]
question = "Show user names"
plan = { select = [{ type = "column_ref", table = "users", column = "name", alias = null }], from = { table = "users", alias = null } }
```

---

## File Changes Summary

### New Files

| File | Purpose |
|------|---------|
| `crates/vlorql-core/src/compile/dialect_config.rs` | `DialectConfig` struct + serialization |
| `crates/vlorql-core/src/compile/registry.rs` | Refactored `DialectRegistry` (replaces current `CompilerRegistry`) |
| `crates/vlorql-core/src/compile/rewrite.rs` | `RewriteEngine`, `RewriteRule` |
| `crates/vlorql-core/src/prompt/skill.rs` | `PromptSkill` struct, loading from file |
| `docs/en/dialect-config.md` | User-facing docs for custom dialects |
| `docs/en/prompt-skills.md` | User-facing docs for prompt skills |
| `examples/configs/dialects/mssql.toml` | Example MSSQL dialect config |
| `examples/configs/dialects/bigquery.toml` | Example BigQuery dialect config |
| `examples/configs/skills/small-model.toml` | Example small-model skill |

### Modified Files

| File | Change |
|------|--------|
| `crates/vlorql-core/src/compile/mod.rs` | Export new modules |
| `crates/vlorql-core/src/compile/builder.rs` | `QueryBuilder` uses `DialectConfig` instead of `(SqlDialect, IdentifierQuoting)` |
| `crates/vlorql-core/src/compile/postgres.rs` | Keep as builtin, adapt to new trait if needed |
| `crates/vlorql-core/src/compile/sqlite.rs` | Same |
| `crates/vlorql-core/src/compile/mysql.rs` | Same |
| `crates/vlorql-core/src/prompt/mod.rs` | Export `skill` module |
| `crates/vlorql-core/src/prompt/builder.rs` | Add `with_skill`, `with_skills`, `simplify_schema` support |
| `crates/vlorql-core/src/schema/dialect.rs` | `DialectProfile` gains `to_dialect_config()` conversion |
| `crates/vlorql-core/src/schema/types.rs` | `SqlDialect` gets `to_config()` or similar bridge method |
| `crates/vlorql/src/lib.rs` | Expose dialect/skill loading on `VlorQlBuilder` |
| `docs/en/guide.md` | New sections for custom dialects and skills |
| `docs/en/deployment.md` | Notes on dialect config deployment |

---

## Backward Compatibility

- `SqlDialect` enum is preserved with its 3 variants
- `DialectProfile` is preserved and gains a `to_dialect_config()` method
- `get_compiler(SqlDialect::Postgres)` still works and returns `PostgresCompiler`
- New API: `VlorQlBuilder::with_dialect_config(path)` / `VlorQlBuilder::with_skill(path)`
- Default behavior unchanged when no custom config or skill is provided

---

## Testing Strategy

1. **Unit tests** for `DialectConfig` parsing, `RewriteEngine` pattern matching
2. **Integration tests** for compiling with custom dialect config → correct SQL
3. **Integration tests** for rewrite rules → transformed SQL
4. **Integration tests** for prompt skills → system prompt contains injected instructions
5. **Backward compatibility tests** — existing tests pass unchanged
