# Dialect Adapter System Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow users to define custom SQL dialects via TOML/YAML config, add a SQL rewrite engine, and support prompt skills for small models.

**Architecture:** 3-layer system: (1) `DialectConfig`-driven compiler dispatch replacing hardcoded `SqlDialect` enum matching, (2) `RewriteEngine` for post-compilation SQL transformation, (3) `PromptSkill` for custom prompt injection. Backward compatible — existing `SqlDialect::Postgres/Sqlite/MySql` continue working.

**Tech Stack:** Rust, serde (Serialize/Deserialize), serde_yaml, toml, regex, existing schemars/derive_builder

## Global Constraints

- Keep 100% backward compatibility with existing `SqlDialect::Postgres | Sqlite | MySql`
- All new config types must implement `Serialize` + `Deserialize` + `JsonSchema`
- Existing tests must pass without modification
- No new dependencies on external crates beyond `toml`, `serde_yaml`, `regex` (already in workspace or trivial)
- Follow existing code style: no comments, `use` grouping, error types from `VlorQLError`

---

### Task 1: DialectConfig struct + serialization

**Files:**
- Create: `crates/vlorql-core/src/compile/dialect_config.rs`

**Interfaces:**
- Consumes: `SqlDialect` enum, `IdentifierQuoting` from `schema/types.rs`
- Produces: `DialectConfig` struct, `DialectConfig::from_toml(path)`, `DialectConfig::from_yaml(path)`, `DialectConfig::placeholder(&self, index) -> String`, `DialectConfig::quote_style(&self) -> IdentifierQuoting`, builders for builtin defaults

- [ ] **Step 1: Define DialectConfig**

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DialectConfig {
    pub name: String,
    pub identifier_quote: String,
    pub placeholder: String,
    pub limit_offset: String,
    pub top_syntax: Option<String>,
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
    pub type_mappings: HashMap<String, String>,
    pub function_name_mappings: HashMap<String, String>,
}

impl DialectConfig {
    pub fn placeholder_str(&self, index: usize) -> String {
        self.placeholder
            .replace("{index}", &index.to_string())
    }

    pub fn quote_identifier(&self, ident: &str) -> String {
        match self.identifier_quote.as_str() {
            "double_quote" => format!("\"{}\"", ident.replace('"', "\"\"")),
            "backtick" => format!("`{}`", ident.replace('`', "``")),
            "bracket" => format!("[{}]", ident),
            "never" => ident.to_string(),
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    pub fn render_limit_offset(&self, limit: Option<u64>, offset: Option<u64>) -> Option<String> {
        // Parse the template and fill in {limit} and {offset}
        // If top_syntax is set and limit is present, use it for SELECT prefix
        todo!("implement limit/offset rendering based on template")
    }

    pub fn default_postgres() -> Self { ... }
    pub fn default_sqlite() -> Self { ... }
    pub fn default_mysql() -> Self { ... }
}
```

- [ ] **Step 2: Implement `fn placeholder_str`**
  Replace `{index}` with the 1-based index. E.g.:
  - `"$index"` → `"$1"`
  - `"?"` → `"?"` (no index needed)
  - `"@p{index}"` → `"@p1"`

- [ ] **Step 3: Implement `fn quote_identifier`**
  Support: `"double_quote"` → `"foo"`, `"backtick"` → `` `foo` ``, `"bracket"` → `[foo]`, `"never"` → `foo`

- [ ] **Step 4: Implement `fn render_limit_offset`**
  Parse `self.limit_offset` template replacing `{limit}` and `{offset}`. Handle cases where only one is present.

- [ ] **Step 5: Implement builtin defaults**
  `default_postgres()`, `default_sqlite()`, `default_mysql()` matching current behavior.

- [ ] **Step 6: Implement `from_toml(path)` and `from_yaml(path)`**

```rust
pub fn from_toml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> {
    let content = std::fs::read_to_string(path)?;
    toml::from_str(&content).map_err(|e| VlorQLError::config(...))
}
```

- [ ] **Step 7: Tests**

```rust
#[test]
fn placeholder_postgres_style() {
    let cfg = DialectConfig::default_postgres();
    assert_eq!(cfg.placeholder_str(1), "$1");
    assert_eq!(cfg.placeholder_str(3), "$3");
}

#[test]
fn placeholder_mysql_style() {
    let cfg = DialectConfig::default_mysql();
    assert_eq!(cfg.placeholder_str(1), "?");
}

#[test]
fn quote_identifier_double_quote() {
    let cfg = DialectConfig::default_postgres();
    assert_eq!(cfg.quote_identifier("users"), r#""users""#);
}

#[test]
fn quote_identifier_backtick() {
    let cfg = DialectConfig::default_mysql();
    assert_eq!(cfg.quote_identifier("users"), "`users`");
}
```

- [ ] **Step 8: Run tests** `cargo test -p vlorql-core -- dialect_config` — PASS

- [ ] **Step 9: Commit** `git add crates/vlorql-core/src/compile/dialect_config.rs && git commit -m "feat(core): add DialectConfig struct with serialization"`

---

### Task 2: DialectRegistry — replace CompilerRegistry

**Files:**
- Modify: `crates/vlorql-core/src/compile/registry.rs` (full rewrite)
- Modify: `crates/vlorql-core/src/compile/mod.rs` (add exports)
- Test: same file inline

**Interfaces:**
- Consumes: `SqlDialect`, `DialectConfig` (Task 1)
- Produces: `DialectRegistry` struct, `get_compiler(name: &str)`, `register_dialect(name, config)`, `CompilerError::DialectNotFound`

- [ ] **Step 1: Rewrite `CompilerRegistry` as `DialectRegistry`**

```rust
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct DialectRegistry {
    builtin: HashMap<&'static str, Box<dyn SqlCompiler>>,
    custom: HashMap<String, Arc<DialectConfig>>,
}

impl DialectRegistry {
    pub fn new() -> Self {
        let mut builtin: HashMap<&'static str, Box<dyn SqlCompiler>> = HashMap::new();
        builtin.insert("postgres", Box::new(PostgresCompiler));
        builtin.insert("sqlite", Box::new(SQLiteCompiler));
        builtin.insert("mysql", Box::new(MySQLCompiler));
        Self { builtin, custom: HashMap::new() }
    }

    pub fn register(&mut self, name: &str, config: DialectConfig) -> Result<(), VlorQLError> {
        let name_lower = name.to_lowercase();
        if self.builtin.contains_key(name_lower.as_str()) {
            return Err(VlorQLError::config(format!("Dialect '{name}' is a builtin and cannot be overridden")));
        }
        self.custom.insert(name_lower, Arc::new(config));
        Ok(())
    }

    pub fn get_compiler(&self, name: &str) -> Result<CompilerRef, VlorQLError> {
        let name = name.to_lowercase();
        if let Some(compiler) = self.builtin.get(name.as_str()) {
            return Ok(CompilerRef::Builtin(compiler.as_ref()));
        }
        if let Some(config) = self.custom.get(&name) {
            return Ok(CompilerRef::Config(config.clone()));
        }
        Err(VlorQLError::config(format!("Unknown dialect '{name}'")))
    }
}

pub enum CompilerRef<'a> {
    Builtin(&'a dyn SqlCompiler),
    Config(Arc<DialectConfig>),
}

impl SqlCompiler for CompilerRef<'_> { ... }
impl SqlCompiler for Arc<DialectConfig> { ... }
```

- [ ] **Step 2: Implement `SqlCompiler for Arc<DialectConfig>`**
  This is the key bridge: create a `QueryBuilder` from `DialectConfig` instead of `(SqlDialect, IdentifierQuoting)`.

- [ ] **Step 3: Keep backward compat `get_compiler(dialect: SqlDialect)`**
  Keep the old function signature that maps enum → builtin name → compiler.

- [ ] **Step 4: Update `compile/mod.rs` exports**
  Add `DialectRegistry`, `dialect_config`, `CompilerRef`.

- [ ] **Step 5: Tests**
  - Test `DialectRegistry::get_compiler("postgres")` returns builtin
  - Test `DialectRegistry::register("mssql", cfg)` then `get_compiler("mssql")` returns config-based
  - Test `get_compiler("unknown")` returns error
  - Test backward compat `get_compiler(SqlDialect::Postgres)` still works

- [ ] **Step 6: Run tests** — PASS

- [ ] **Step 7: Commit** `git commit -m "feat(core): DialectRegistry with builtin + custom dialect support"`

---

### Task 3: Refactor QueryBuilder to use DialectConfig

**Files:**
- Modify: `crates/vlorql-core/src/compile/builder.rs` (constructor signature, placeholder generation, quoting, limit/offset, ILIKE handling)
- Test: `crates/vlorql-core/src/compile/mod.rs` (existing tests must pass)

**Key changes:**
- `QueryBuilder::new()` takes `&DialectConfig` instead of `(SqlDialect, IdentifierQuoting)`
- `add_parameter` uses `config.placeholder_str(idx)` instead of matching on `SqlDialect`
- `quote_identifier` uses `config.quote_identifier(ident)` instead of matching on `IdentifierQuoting`
- `build_limit_offset` uses `config.render_limit_offset(limit, offset)`
- `render_comparison_operator` / ILIKE fallback checks `config.type_mappings.get("ilike")`
- `render_join_type` checks `config.type_mappings`
- Keep `SqlDialect` field for backward compat but compute it from config

- [ ] **Step 1: Change QueryBuilder constructor and internal state**

```rust
pub struct QueryBuilder<'a> {
    plan: &'a ValidatedPlan,
    config: &'a DialectConfig,
    dialect: SqlDialect,  // kept for backward compat, derived from config.name
    parameters: Vec<Parameter>,
    // ... rest unchanged
}

impl<'a> QueryBuilder<'a> {
    pub fn new(plan: &'a ValidatedPlan, config: &'a DialectConfig) -> Self {
        let dialect = match config.name.to_lowercase().as_str() {
            "postgres" => SqlDialect::Postgres,
            "sqlite" => SqlDialect::Sqlite,
            "mysql" => SqlDialect::MySql,
            _ => SqlDialect::Postgres, // fallback
        };
        Self { plan, config, dialect, parameters: Vec::new(), ... }
    }
}
```

- [ ] **Step 2: Refactor `add_parameter`**
  Replace `match self.dialect { SqlDialect::Postgres => format!("${}", idx+1), _ => "?".to_owned() }` with `self.config.placeholder_str(idx + 1)`.

- [ ] **Step 3: Refactor `quote_identifier`**
  Call `self.config.quote_identifier(ident)` instead of matching on `IdentifierQuoting`.

- [ ] **Step 4: Refactor `build_limit_offset`**
  Use `self.config.render_limit_offset(limit, offset)` if available; fall back to existing per-dialect logic for builtins.

- [ ] **Step 5: Refactor ILIKE handling**
  Check `self.config.type_mappings.get("ilike")` for the operator string.

- [ ] **Step 6: Update PostgresCompiler, SQLiteCompiler, MySQLCompiler**
  Each creates a `DialectConfig::default_*()` and passes to `QueryBuilder::new(plan, &config)`.

- [ ] **Step 7: Run existing tests** `cargo test -p vlorql-core` — all PASS

- [ ] **Step 8: Commit** `git commit -m "refactor(core): QueryBuilder uses DialectConfig internally"`

---

### Task 4: SQL Rewrite Engine

**Files:**
- Create: `crates/vlorql-core/src/compile/rewrite.rs`
- Modify: `crates/vlorql-core/src/compile/mod.rs` (add module)

**Interfaces:**
- Produces: `RewriteRule`, `RewriteEngine`, `RewriteEngine::apply(sql, dialect) -> Result<String>`

- [ ] **Step 1: Define RewriteRule and RewriteEngine**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RewriteRule {
    pub name: String,
    pub description: Option<String>,
    pub match_pattern: String,
    pub replace_template: String,
    pub dialect_filter: Option<Vec<String>>,
}

#[derive(Debug, Default)]
pub struct RewriteEngine {
    rules: Vec<RewriteRule>,
}

impl RewriteEngine {
    pub fn new(rules: Vec<RewriteRule>) -> Self { Self { rules } }

    pub fn apply(&self, sql: &str, dialect: &str) -> Result<String, VlorQLError> {
        let mut result = sql.to_string();
        for rule in &self.rules {
            if let Some(filter) = &rule.dialect_filter {
                if !filter.iter().any(|d| d.eq_ignore_ascii_case(dialect)) {
                    continue;
                }
            }
            let re = regex::Regex::new(&rule.match_pattern)
                .map_err(|e| VlorQLError::config(format!("Invalid regex in rule '{}': {}", rule.name, e)))?;
            result = re.replace_all(&result, rule.replace_template.as_str()).to_string();
        }
        Ok(result)
    }

    pub fn load_toml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> { ... }
    pub fn load_yaml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> { ... }
}
```

- [ ] **Step 2: Implement `load_toml` and `load_yaml`**

- [ ] **Step 3: Tests**

```rust
#[test]
fn rewrite_ilike_to_lower() {
    let engine = RewriteEngine::new(vec![RewriteRule {
        name: "ilike_to_lower".into(),
        description: None,
        match_pattern: r"(?P<left>\S+)\s+ILIKE\s+(?P<right>\S+)".into(),
        replace_template: "LOWER(${left}) LIKE LOWER(${right})".into(),
        dialect_filter: Some(vec!["mysql".into()]),
    }]);
    let sql = engine.apply("WHERE name ILIKE '%foo%'", "mysql").unwrap();
    assert_eq!(sql, "WHERE LOWER(name) LIKE LOWER('%foo%')");
}

#[test]
fn rewrite_skipped_for_wrong_dialect() {
    let engine = RewriteEngine::new(vec![RewriteRule {
        name: "ilike_to_lower".into(),
        description: None,
        match_pattern: "ILIKE".into(),
        replace_template: "LIKE".into(),
        dialect_filter: Some(vec!["mysql".into()]),
    }]);
    let sql = engine.apply("WHERE name ILIKE '%foo%'", "postgres").unwrap();
    assert_eq!(sql, "WHERE name ILIKE '%foo%'"); // unchanged
}
```

- [ ] **Step 4: Export from `compile/mod.rs`**

- [ ] **Step 5: Run tests** — PASS

- [ ] **Step 6: Commit** `git commit -m "feat(core): add RewriteEngine for SQL post-processing"`

---

### Task 5: PromptSkill system

**Files:**
- Create: `crates/vlorql-core/src/prompt/skill.rs`
- Modify: `crates/vlorql-core/src/prompt/builder.rs` (add `with_skill` methods)
- Modify: `crates/vlorql-core/src/prompt/mod.rs` (add module)

**Interfaces:**
- Produces: `PromptSkill`, `PromptSkill::load_toml(path) -> Result`, `PromptBuilder::with_skill(skill)`, `PromptBuilder::with_skills(skills)`

- [ ] **Step 1: Define PromptSkill**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PromptSkill {
    pub name: String,
    pub description: Option<String>,
    pub instructions: Vec<String>,
    pub simplify_schema: bool,
    pub forbid_features: Vec<String>,
    pub disable_output_fields: Vec<String>,
    pub examples: Vec<ExamplePair>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExamplePair {
    pub question: String,
    pub plan: serde_json::Value,
}

impl PromptSkill {
    pub fn load_toml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> { ... }
    pub fn load_yaml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> { ... }
    pub fn builtin_small_model() -> Self { ... }
}
```

- [ ] **Step 2: Add `with_skill` / `with_skills` to PromptBuilder**

```rust
impl PromptBuilder {
    pub fn with_skill(mut self, skill: PromptSkill) -> Self {
        self.skills.push(skill);
        self
    }
    pub fn with_skills(mut self, skills: Vec<PromptSkill>) -> Self {
        self.skills.extend(skills);
        self
    }
}
```

- [ ] **Step 3: Modify `build_system_prompt_for_tables`**
  - After `push_planning_rules`, iterate skills and append instructions
  - If any skill has `simplify_schema = true`, call a simplified schema variant
  - If any skill has `forbid_features`, add a `## Constraints` section
  - Append skill examples

- [ ] **Step 4: Add `compact_output_schema` simplified variant**
  When `simplify_schema = true`, remove `set_operation`, `window_functions`, `ctes`, `distinct_on` from the JSON Schema.

- [ ] **Step 5: Tests**

```rust
#[test]
fn skill_injects_instructions() {
    let skill = PromptSkill {
        name: "test".into(),
        instructions: vec!["Keep it simple".into()],
        ..Default::default()
    };
    let builder = PromptBuilder::new(schema, dialect, policy)
        .with_skill(skill);
    let prompt = builder.build_system_prompt("test");
    assert!(prompt.contains("Keep it simple"));
}
```

- [ ] **Step 6: Run tests** — PASS

- [ ] **Step 7: Commit** `git commit -m "feat(core): add PromptSkill system for custom prompt injection"`

---

### Task 6: Facade integration (VlorQlBuilder)

**Files:**
- Read: `crates/vlorql/src/lib.rs`
- Modify: `crates/vlorql/src/lib.rs`

- [ ] **Step 1: Add dialect_config and skills fields to VlorQlBuilder**

```rust
pub struct VlorQlBuilder {
    dialect_config: Option<DialectConfig>,
    rewrite_rules: Vec<RewriteRule>,
    skills: Vec<PromptSkill>,
    // ... existing fields
}
```

- [ ] **Step 2: Add builder methods**

```rust
pub fn with_dialect_config(mut self, config: DialectConfig) -> Self { ... }
pub fn with_dialect_config_file(mut self, path: &str) -> Result<Self> { ... }
pub fn with_rewrite_rules(mut self, rules: Vec<RewriteRule>) -> Self { ... }
pub fn with_rewrite_rules_file(mut self, path: &str) -> Result<Self> { ... }
pub fn with_skill(mut self, skill: PromptSkill) -> Self { ... }
pub fn with_skill_file(mut self, path: &str) -> Result<Self> { ... }
```

- [ ] **Step 3: Modify `build()` to use DialectConfig path**
  If `dialect_config` is set, use it instead of the old `dialect_name` → `SqlDialect` path.

- [ ] **Step 4: Integrate RewriteEngine**
  After `compile()` runs, pass through `RewriteEngine::apply()`.

- [ ] **Step 5: Integrate PromptSkill**
  Pass skills to `PromptBuilder`.

- [ ] **Step 6: Run tests** — PASS

- [ ] **Step 7: Commit** `git commit -m "feat: expose dialect config, rewrite rules, and skills via VlorQlBuilder"`

---

### Task 7: Example config files

**Files:**
- Create: `examples/configs/dialects/mssql.toml`
- Create: `examples/configs/dialects/bigquery.toml`
- Create: `examples/configs/skills/small-model.toml`
- Create: `examples/configs/rules/bigquery.toml`

- [ ] **Step 1: Create `examples/configs/dialects/mssql.toml`**

```toml
name = "mssql"
identifier_quote = "double_quote"
placeholder = "@p{index}"
limit_offset = "OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
supports_cte = true
supports_window_functions = true
supports_offset = false
supports_fetch = true
allow_distinct = true
allow_select_distinct = true
allowed_join_types = ["inner", "left", "right", "full", "cross"]

[type_mappings]
ilike = "LIKE"

[function_name_mappings]
"now" = "GETDATE"
```

- [ ] **Step 2-4: Create remaining example files**

- [ ] **Step 5: Commit** `git commit -m "docs: add example dialect, skill, and rewrite rule configs"`

---

### Task 8: Update documentation

**Files:**
- Modify: `docs/en/guide.md`
- Modify: `docs/en/deployment.md`

- [ ] **Step 1: Read current guide and deployment docs**

- [ ] **Step 2: Add custom dialects section to guide.md**
  - How to create a dialect TOML file
  - How to load via CLI (`--dialect-config`)
  - How to load via `VlorQlBuilder::with_dialect_config_file()`

- [ ] **Step 3: Add prompt skills section**
  - How to create a skill file
  - How small-model skill simplifies output
  - How to load via `--skill`

- [ ] **Step 4: Add rewrite rules section**
  - How to create transformation rules
  - Common use cases (ILIKE → LOWER, EXTRACT → DATEPART)

- [ ] **Step 5: Update deployment.md**
  - Best practices for dialect config in production
  - Skill selection for different model sizes

- [ ] **Step 6: Commit** `git commit -m "docs: add dialect config, rewrite rules, and prompt skills documentation"`

---

### Task 9: Final verification

- [ ] **Step 1: Full test suite**

```bash
cargo test --workspace
```

- [ ] **Step 2: Fix any regressions**

- [ ] **Step 3: Build check**

```bash
cargo build --workspace
```
