# VlorQl User Guide

This guide walks through everything you need to integrate VlorQl
into a Rust application. It assumes you have a working Rust 1.85+
toolchain and a database that you want to expose through natural-language
queries.

> Looking for **operational** guidance instead? See
> [`deployment.md`](./deployment.md) for vLLM/Ollama setup and
> production tuning.

---

## 1. Quick start

Add the workspace crates to `Cargo.toml`:

```toml
[dependencies]
vlorql = { path = "crates/vlorql" }
vlorql-core = { path = "crates/vlorql-core" }
vlorql-llm = { path = "crates/vlorql-llm" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

### 1.1 Define a schema

A schema is a list of tables and their columns. Build it from your
introspection code or hard-code it for the example:

```rust
use std::sync::Arc;
use vlorql_core::schema::{
    ColumnSchema, DataType, SchemaMetadata, SchemaSnapshot, TableSchema,
};

let schema = Arc::new(SchemaSnapshot::new(
    vec![TableSchema {
        name: "users".to_owned(),
        columns: vec![
            ColumnSchema {
                name: "id".to_owned(),
                data_type: DataType::Int,
                nullable: false,
                description: Some("User identifier".to_owned()),
                is_primary_key: true,
                foreign_key: None,
            },
            ColumnSchema {
                name: "name".to_owned(),
                data_type: DataType::String,
                nullable: false,
                description: Some("Display name".to_owned()),
                is_primary_key: false,
                foreign_key: None,
            },
        ],
        description: Some("Application users".to_owned()),
        primary_key: Some(vec!["id".to_owned()]),
    }],
    SchemaMetadata::default(),
));
```

### 1.2 Build the facade

```rust
use vlorql::{LlmConfig, LlmProvider, VlorQl};
use vlorql_core::policy::PolicyConfig;
use vlorql_llm::create_llm_client;

let client = create_llm_client(LlmConfig {
    provider: LlmProvider::OpenAi,
    api_key: Some(std::env::var("OPENAI_API_KEY")?),
    model: "gpt-4o-mini".to_owned(),
    ..LlmConfig::default()
})?;

let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(PolicyConfig::default())
    .with_llm_client(client)
    .build()?;
```

### 1.3 Run your first question

```rust
let compiled = vlorql.query("Show the 10 most recent users").await?;
println!("SQL:     {}", compiled.sql);
println!("Params:  {:?}", compiled.parameters);
```

That's the entire pipeline. The LLM is asked to emit a `QueryPlan`
JSON object, the plan is validated against the schema, policy,
operand types, and dialect profile, and the validated plan is
compiled to parameterized SQL with `$1`, `?`, or `` ` ` `` placeholders.

---

## 2. Configuration

VlorQl is configured through three complementary structs.

### 2.1 `LlmConfig` (provider, model, retries)

`LlmConfig` is the single source of truth for talking to the LLM:

| Field             | Type        | Purpose                                                                 |
|-------------------|-------------|-------------------------------------------------------------------------|
| `provider`        | `LlmProvider` | `OpenAi` / `Anthropic` / `DeepSeek` / `Zhipu` / `Vllm` / `Ollama`  |
| `api_key`         | `Option<String>` | API key (also read from env vars; see below)                      |
| `api_base`        | `Option<String>` | Override the default endpoint                                       |
| `model`           | `String`    | Provider-specific model identifier                                      |
| `max_tokens`      | `u32`       | Maximum number of tokens the LLM may emit                              |
| `temperature`     | `f32`       | `0.0` for deterministic output                                        |
| `timeout_seconds` | `u64`       | Per-request HTTP timeout                                              |
| `max_retries`     | `u32`       | Retries on transient HTTP errors (5xx, 429, timeout)                  |
| `extra`           | `HashMap<String, Value>` | Provider-specific overrides (e.g. `"backend": "ollama"`) |

The factory [`vlorql_llm::create_llm_client`] inspects `api_key` first
and falls back to the documented environment variable:

| Provider   | Environment variable  |
|------------|-----------------------|
| Anthropic  | `ANTHROPIC_API_KEY`   |
| DeepSeek   | `DEEPSEEK_API_KEY`    |
| Zhipu      | `ZHIPU_API_KEY`       |
| OpenAI     | `OPENAI_API_KEY`      |
| vLLM       | _not required_        |
| Ollama     | _not required_        |

### 2.2 `DialectProfile` (SQL features)

`DialectProfile` describes the SQL features the validator should
allow and the compiler should emit. Build it from defaults with
[`DialectProfile::builder`]:

```rust
use vlorql_core::schema::{DialectProfile, IdentifierQuoting, JoinType, SqlDialect};

let profile = DialectProfile::builder()
    .dialect(SqlDialect::Postgres)
    .max_joins(3usize)
    .supports_cte(true)
    .allowed_join_types(vec![JoinType::Inner, JoinType::Left])
    .allowed_functions(vec!["count".to_owned(), "sum".to_owned()])
    .denied_functions(vec!["pg_sleep".to_owned()])
    .allow_distinct(true)
    .supports_offset(true)
    .build()?;
```

The builder leaves any unset field at the [`DialectProfile::default`]
value. Dialect-aware compile flags like the placeholder style and
identifier quoting are picked from the `dialect` field.

**Dialect-specific behaviour:**

| Dialect   | Placeholder | Identifier quoting | Pagination                    | Notes                                                                 |
|-----------|-------------|--------------------|-------------------------------|-----------------------------------------------------------------------|
| Postgres  | `$1`, `$2`  | `"double quotes"`  | `LIMIT n OFFSET m`            | Default. Supports `ILIKE` operator.                                   |
| SQLite    | `?`         | `"double quotes"`  | `LIMIT -1 OFFSET m`           | `OFFSET` without `LIMIT` uses `LIMIT -1`.                             |
| MySQL     | `?`         | `` `backticks` ``  | `LIMIT m, n` / `LIMIT m, 18446744073709551615` | `FULL JOIN` is rejected at compile time. `OFFSET` without `LIMIT` uses the maximum `BIGINT UNSIGNED` sentinel. |

### 2.3 `PolicyConfig` (access control)

`PolicyConfig` is a free-form bag of rules. The default policy allows
access to every table and every visible column. Tighten it with
per-table [`TablePolicy`] entries, a list of `global_denied_columns`,
and a list of mandatory row filters.

```rust
use vlorql_core::policy::{PolicyConfig, RowFilter, TablePolicy};
use vlorql_core::schema::{ComparisonOperator, DataType, Expression, Predicate};
use std::collections::HashMap;

let mut table_policies = HashMap::new();
table_policies.insert("users".to_owned(), TablePolicy {
    allowed: true,
    allowed_columns: Some(vec!["id".to_owned(), "email".to_owned()]),
    denied_columns: vec!["password_hash".to_owned()],
    row_filter: Some(RowFilter {
        condition: Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: Expression::Literal {
                value: serde_json::json!(0),
                data_type: DataType::Int,
            },
        },
        description: "tenant isolation".to_owned(),
    }),
});
let policy = PolicyConfig {
    table_policies,
    global_denied_columns: vec!["password_hash".to_owned()],
    ..PolicyConfig::default()
};
```

See [§4](#4-policy-configuration) for a worked example.

---

## 3. Multi-provider setup

VlorQl ships first-class clients for six providers. Switching between
them is a one-line change in `LlmConfig::provider`.

### 3.1 Hosted providers (Anthropic, DeepSeek, OpenAI, Zhipu)

Each hosted client shares the same JSON contract; only the wire
protocol differs. The examples below assume the relevant env var is
set in the environment.

**Anthropic Claude:**

```rust
use vlorql::{LlmConfig, LlmProvider, VlorQl};
use vlorql_llm::create_llm_client;

let config = LlmConfig {
    provider: LlmProvider::Anthropic,
    api_key: Some(std::env::var("ANTHROPIC_API_KEY")?),
    model: "claude-sonnet-4-5".to_owned(),
    max_tokens: 4096,
    ..LlmConfig::default()
};
let client = create_llm_client(config)?;
```

**DeepSeek:**

```rust
let config = LlmConfig {
    provider: LlmProvider::DeepSeek,
    api_key: Some(std::env::var("DEEPSEEK_API_KEY")?),
    model: "deepseek-v4-pro".to_owned(),   // `deepseek-chat` / `deepseek-reasoner` are deprecated
    max_tokens: 4096,
    ..LlmConfig::default()
};
```

**Zhipu GLM:**

```rust
let config = LlmConfig {
    provider: LlmProvider::Zhipu,
    api_key: Some(std::env::var("ZHIPU_API_KEY")?),
    model: "glm-4.7".to_owned(),
    max_tokens: 4096,
    ..LlmConfig::default()
};
```

### 3.2 Local providers (vLLM, Ollama)

`vLLM` and `Ollama` are selected the same way; the `api_key` is
optional, and the `api_base` defaults to the standard local URL
(`http://localhost:8000/v1` for vLLM, `http://localhost:11434` for
Ollama).

**vLLM (OpenAI-compatible):**

```rust
let config = LlmConfig {
    provider: LlmProvider::Vllm,
    api_key: Some("not-required".to_owned()),   // or None
    api_base: Some("http://gpu-host.internal:8000/v1".to_owned()),
    model: "Qwen/Qwen2.5-7B-Instruct".to_owned(),
    max_tokens: 4096,
    ..LlmConfig::default()
};
```

**Ollama:**

```rust
use serde_json::json;
use std::collections::HashMap;

let mut extra = HashMap::new();
extra.insert("backend".to_owned(), json!("ollama"));

let config = LlmConfig {
    provider: LlmProvider::Ollama,
    api_key: None,
    api_base: Some("http://localhost:11434".to_owned()),
    model: "llama3.2".to_owned(),
    max_tokens: 4096,
    extra,
    ..LlmConfig::default()
};
```

See the runnable examples under `crates/vlorql-llm/examples/`:

```bash
# Switch provider via the LLM_PROVIDER env var
export LLM_PROVIDER=deepseek
export DEEPSEEK_API_KEY=sk-...
cargo run -p vlorql-llm --example multi_provider -- "List user ids"

# vLLM
vllm serve Qwen/Qwen2.5-7B-Instruct --port 8000 --guided-decoding-backend xgrammar
cargo run -p vlorql-llm --example local_vllm -- "List user ids"

# Ollama
ollama serve
ollama pull llama3.2
cargo run -p vlorql-llm --example local_ollama -- "List user ids"
```

---

## 4. Policy configuration

Policies are evaluated by [`PolicyEngine`] after schema validation
and before operand/dialect checks. They layer in three independent
checks that all run on the same plan:

| Level          | Where it lives                       | What it blocks                                     |
|----------------|--------------------------------------|---------------------------------------------------|
| Table          | [`TablePolicy::allowed`]             | A query that references a denied table             |
| Column         | `TablePolicy::allowed_columns` / `denied_columns` / `global_denied_columns` | Reads of disallowed columns                |
| Row            | `TablePolicy::row_filter` / `row_filters` | Queries that would bypass a mandatory condition |

### 4.1 Allow access (default)

```rust
let policy = PolicyConfig::default();
let engine = PolicyEngine::new(policy);
// Any plan that passes the schema check is allowed.
```

### 4.2 Deny a table

```rust
let mut table_policies = HashMap::new();
table_policies.insert("secrets".to_owned(), TablePolicy {
    allowed: false,
    ..TablePolicy::default()
});
let policy = PolicyConfig {
    table_policies,
    ..PolicyConfig::default()
};
```

A plan that selects from `secrets` is rejected with
`PolicyErrorKind::TableDenied`.

### 4.3 Column allowlist / denylist

```rust
table_policies.insert("users".to_owned(), TablePolicy {
    allowed: true,
    allowed_columns: Some(vec!["id".to_owned(), "email".to_owned()]),
    denied_columns: vec!["password_hash".to_owned()],
    ..TablePolicy::default()
});
```

`allowed_columns` is a positive allowlist (the only columns the LLM
may reference). `denied_columns` is a black list (a stronger form of
`allowed_columns: None` + removal of specific sensitive columns).

### 4.4 Globally denied columns

```rust
let policy = PolicyConfig {
    global_denied_columns: vec!["password_hash".to_owned()],
    ..PolicyConfig::default()
};
```

A `global_denied_columns` entry applies to every table; matching is
case-sensitive and accepts both bare column names (`password_hash`)
and `table.column` qualified names.

### 4.5 Mandatory row filters

```rust
table_policies.insert("users".to_owned(), TablePolicy {
    row_filter: Some(RowFilter {
        condition: Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "tenant_id".to_owned(),
            },
            op: ComparisonOperator::Eq,
            right: Expression::Literal {
                value: serde_json::json!("current-tenant"),
                data_type: DataType::String,
            },
        },
        description: "tenant isolation".to_owned(),
    }),
    ..TablePolicy::default()
});
```

`PolicyEngine::apply_row_filters(plan)` returns a combined predicate
the caller can splice into the plan's `WHERE` clause. The validation
pipeline does **not** do this automatically — operators typically
append the predicate to the plan after `validate_only` returns the
`ValidatedPlan`.

### 4.6 Combine multiple violations

The policy engine never fails fast. A single plan that violates both
a table and a column policy collects both errors:

```text
[
  Policy { kind: TableDenied { table: "users" }, ... },
  Policy { kind: ColumnDenied { table: "accounts", column: "owner_id" }, ... },
]
```

`VlorQl::query` then retries once with the LLM (if the error
is `is_retryable`) and surfaces the final list as a
[`ValidationErrors`].

---

## 5. Streaming queries

For interactive UIs, `VlorQl::query_stream` returns a
`Stream<Item = Result<StreamEvent, VlorQLError>>` that yields the
raw text deltas as the LLM emits them, followed by a final
`PlanComplete` (or `Error`) event.

```rust
use futures::StreamExt;
use vlorql::StreamEvent;

let mut stream = vlorql.query_stream("Show user ids").await?;
let mut combined = String::new();
while let Some(item) = stream.next().await {
    match item? {
        StreamEvent::TextChunk(chunk) => {
            combined.push_str(&chunk);
            print!("[chunk] {chunk}");
        }
        StreamEvent::PlanComplete(plan) => {
            println!("\nplan = {}", serde_json::to_string_pretty(&plan)?);
        }
        StreamEvent::Error(error) => return Err(error),
    }
}
```

The stream is backed by a Tokio task that performs validation and
compilation once the LLM closes the connection, so the API stays
cheap to consume.

---

## 6. Validation pipeline

The four validation stages always run in this order:

1. **Schema** — every base table and column reference must exist.
2. **Policy** — the table/column/row rules from §4 must be satisfied.
3. **Operand** — expression types must be compatible (`5 = '5'`
   fails, `5 + 'five'` fails, `LIKE` on a numeric column fails, …).
4. **Dialect** — joins, CTEs, OFFSET, functions must be allowed by
   the configured profile.

`VlorQl::validate_only` runs all four stages and returns the
validated plan, or a `ValidationErrors` aggregating every problem
the LLM can fix in one retry.

```rust
use vlorql_core::schema::QueryPlan;

let plan: QueryPlan = serde_json::from_str(&assistant_text)?;
let validated = vlorql.validate_only(&plan);
match validated {
    Ok(plan) => { /* safe to compile */ }
    Err(errors) => {
        for error in errors.as_slice() {
            println!("{}: {}", error.error_code(), error);
        }
    }
}
```

`VlorQl::compile_only` then turns a validated plan into
parameterized SQL:

```rust
let compiled = vlorql.compile_only(&validated?)?;
println!("{}", compiled.sql);
for parameter in &compiled.parameters {
    println!("  {} = {:?}", parameter.data_type, parameter.value);
}
```

### 6.1 Query plan optimisation

VlorQl includes an optional **logical query optimizer** that runs
between validation and compilation. It applies three synchronous
rewrite rules and one asynchronous join reorderer:

| Rule                | Effect                                                              |
|---------------------|---------------------------------------------------------------------|
| Constant folding    | Evaluates constant sub-expressions (`100 + 50` → `150`) and simplifies algebraic identities (`x + 0` → `x`, `true AND x` → `x`). |
| Predicate pushdown  | Moves `WHERE` conjuncts into CTE bodies for earlier filtering. Supports **multi-layer cascade** through nested CTEs. |
| Column pruning      | Removes unreferenced columns from CTE outputs. Aggregate arguments are only preserved when the aggregation is referenced. |
| Join reordering     | Reorders `INNER JOIN` chains to minimise total cost (requires statistics). Uses bitmask-accelerated DP search. |

The pipeline can optionally be run in **fixed-point iteration** mode
(up to 3 rounds) to capture cascading effects — constant folding may
expose new pushdown opportunities, and pushdown may enable more
column pruning. Use `QueryOptimizer::optimize_repeat()` or
`RewriterPipeline::repeat_until_stable()` directly.

Enable it with `VlorQlBuilder::with_statistics_provider`:

```rust
use std::sync::Arc;
use vlorql_core::statistics::DummyStatisticsProvider;

let stats = Arc::new(DummyStatisticsProvider::default());
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(PolicyConfig::default())
    .with_statistics_provider(stats)
    .build()?;
```

When statistics are available, `VlorQl::validate_and_optimize`
returns an `OptimizedPlan` that can be passed to `compile_only`:

```rust
let optimized = vlorql.validate_and_optimize(&plan).await?;
let compiled = vlorql.compile_only(optimized.as_validated())?;
```

See [`optimization.md`](./optimization.md) for detailed documentation.

---

## 7. Error handling

Every VlorQl error is a [`VlorQLError`] value with a stable
`error_code()` and a machine-readable `details` payload. Convert
to an API-friendly response with `to_error_response`:

```rust
let response = error.to_error_response();
println!("code:      {}", response.code);
println!("message:   {}", response.message);
println!("details:   {}", response.details);
println!("suggestion: {:?}", response.suggestion);
```

| Code  | Meaning                          | Retryable? |
|-------|----------------------------------|------------|
| `V001`–`V009` | Validation errors        | yes        |
| `P001`–`P003` | Policy errors            | no         |
| `C001`–`C005` | Compilation errors       | no         |
| `S001`–`S002` | Schema errors            | no         |
| `L001`–`L003` | LLM errors                | yes        |
| `G001`–`G003` | Configuration errors     | no         |

Compilation errors include:

| Error code | Feature name                        | Meaning                                                       |
|------------|--------------------------------------|---------------------------------------------------------------|
| `C001`     | `unsupported_full_join`              | `FULL JOIN` is not supported by the target dialect (MySQL).   |
| `C002`     | `reserved_keyword_unquoted`          | An unquoted identifier is a SQL reserved keyword.             |
| `C003`     | `empty_in_list`                      | `IN` predicate has an empty value list.                       |
| `C004`     | `empty_select_list`                  | `SELECT` list is empty.                                       |
| `C005`     | `sql_formatting`                     | Internal formatting error.                                    |

`ValidationErrorKind::InvalidJson` (`V001`) and the
`is_retryable()` flag are what the `VlorQl::query` retry loop uses
to decide whether to re-prompt the LLM. Policy, schema, and
configuration errors are surfaced immediately because re-prompting
the LLM will not help.

---

## 8. QueryPlan AST reference

The `QueryPlan` JSON object is the contract between the LLM and
VlorQl. The variants below show the full set of supported
expressions and predicates.

### 8.1 `Expression` variants

```rust
pub enum Expression {
    /// A literal value.
    Literal { value: serde_json::Value, data_type: DataType },
    /// A column reference, optionally table-qualified.
    ColumnRef { table: Option<String>, column: String },
    /// A function call (scalar or aggregate), with optional DISTINCT.
    FunctionCall { name: String, args: Vec<Expression>, distinct: bool },
    /// A binary operator application.
    BinaryOp { left: Box<Expression>, op: BinaryOperator, right: Box<Expression> },
    /// A literal `*` used in aggregate functions such as COUNT(*).
    Star,
    /// A scalar subquery: `(SELECT ...)`.
    SubQuery { query: Box<QueryPlan> },
    /// A CASE WHEN ... THEN ... ELSE ... END expression.
    Case {
        operand: Option<Box<Expression>>,
        when_thens: Vec<WhenThen>,
        else_result: Option<Box<Expression>>,
    },
    /// A window function call with an OVER clause.
    WindowFunction {
        name: String,
        args: Vec<Expression>,
        distinct: bool,
        over: WindowSpec,
    },
}
```

**`Star`** is used inside `FunctionCall` arguments to represent `COUNT(*)`:

```json
{
  "type": "function_call",
  "name": "COUNT",
  "args": [{ "type": "star" }],
  "distinct": false
}
```

**`SubQuery`** represents a scalar subquery expression:

```json
{
  "type": "sub_query",
  "query": { /* nested QueryPlan */ }
}
```

### 8.2 `Predicate` variants

```rust
pub enum Predicate {
    Comparison { left: Expression, op: ComparisonOperator, right: Expression },
    And { left: Box<Predicate>, right: Box<Predicate> },
    Or { left: Box<Predicate>, right: Box<Predicate> },
    Not { child: Box<Predicate> },
    Between { expr: Expression, low: Expression, high: Expression },
    In { expr: Expression, target: InTarget },
    Like { expr: Expression, pattern: String },
    IsNull { expr: Expression },
    Exists { query: Box<QueryPlan> },
}
```

**`In`** can target either a list of values or a subquery, via the `InTarget` enum:

```rust
pub enum InTarget {
    Values(Vec<Expression>),          // WHERE id IN (1, 2, 3)
    SubQuery(Box<QueryPlan>),         // WHERE id IN (SELECT user_id FROM ...)
}
```

**`Exists`** checks whether a subquery returns any rows:

```json
{
  "type": "exists",
  "query": { /* nested QueryPlan */ }
}
```

### 8.3 Compiled SQL output

| Expression                                 | Compiled SQL                          |
|--------------------------------------------|---------------------------------------|
| `FunctionCall { name: "COUNT", args: [Star], distinct: false }` | `COUNT(*)`            |
| `FunctionCall { name: "COUNT", args: [Star], distinct: true }`  | `COUNT(DISTINCT *)`   |
| `SubQuery { query: ... }`                 | `(SELECT ...)`                        |
| `In { expr, target: SubQuery(query) }`    | `expr IN (SELECT ...)`                |
| `Exists { query: ... }`                   | `EXISTS (SELECT ...)`                 |

---

## 9. FAQ

### The LLM keeps emitting a plan that fails validation. What now?

Look at the `error_code()` and `suggestion` fields in the response.
The LLM gets a `ValidationErrors` blob on every retry, and the
suggestions are designed to be promptable ("Replace column
`users.emali` with `users.email`"). If validation still fails
after `max_retries`, the facade surfaces the original error.

### Can I run the validator and compiler without the LLM?

Yes. `VlorQl::validate_only` and `VlorQl::compile_only` are both
`pub` and work on a pre-built `QueryPlan` value, which is useful
for tests, server-side rendering of stored plans, and
build-time validation of fixtures.

### How do I optimise a plan before compilation?

Call `VlorQl::validate_and_optimize` to run the query optimizer
(constant folding, predicate pushdown, column pruning, and optional
join reordering). The result is an `OptimizedPlan` that derefs to
`ValidatedPlan` and can be passed to `compile_only`. See
[`optimization.md`](./optimization.md) for details.

### How do I cache the compiled SQL?

Compile output is deterministic for a given `(plan, dialect)`
pair. Wrap the [`QueryBuilder`] (or the `SqlCompiler` trait) in
your own cache, keyed by the JSON of the plan. The compiled
`Vec<Parameter>` is already in the right order for any driver.

VlorQl also provides built-in caches — see [`caching.md`](./caching.md).

### The LLM emits an `or` / `OR` predicate, but the dialect doesn't
support `OR`. How does VlorQl handle it?

VlorQl's role is to translate a structured plan into SQL. The
dialect profile (and the underlying `DialectValidator`) decides
whether `OR` is allowed; if you disable boolean combinators in
`allowed_functions` or restrict the dialect, the LLM is forced to
rephrase. Configure the `DialectProfile` deliberately; the prompt
echoes back every allowed and denied feature.

### Can I add a new SQL dialect?

Yes. Implement [`SqlCompiler`] for a new struct, register it in
[`CompilerRegistry::get`] (currently matched by hand), and use
`VlorQl::with_compiler` to inject it. The new compiler only needs
to know the placeholder syntax, the identifier quoting, and the
pagination clause; everything else is shared with
[`QueryBuilder`].

### How does MySQL handle `OFFSET` without `LIMIT`?

MySQL does not support `LIMIT <offset>` without a limit argument.
VlorQl emits `LIMIT ?, 18446744073709551615` where the second
value is MySQL's maximum `BIGINT UNSIGNED` value, effectively
meaning "no upper bound". This is a well-known MySQL idiom.

### Can I use `FULL JOIN` with MySQL?

No. MySQL does not support `FULL OUTER JOIN`. VlorQl rejects
`JoinType::Full` at compile time with a
`compilation_error("unsupported_full_join")` error.

### Does VlorQl validate identifiers against SQL reserved keywords?

Yes, when `IdentifierQuoting::Never` is used (or falls back to it).
An unquoted identifier that matches a SQL reserved keyword
(e.g., `select`, `table`, `from`) is rejected at compile time
with a `compilation_error("reserved_keyword_unquoted")` error.
When identifiers are quoted with double quotes or backticks, the
keyword check is skipped because quoting escapes the keyword.

### Does VlorQl stream JSON tokens?

No. VlorQl returns complete text deltas from the LLM. If the LLM
emits partial JSON, the parser waits for the full response before
calling `serde_json::from_str`. The `MockLlmClient` is a useful
reference for the contract.

### Where do I get a JSON Schema for the plan?

`schemars::schema_for!(QueryPlan)` is already used internally by
the prompt builder. From Rust:

```rust
let schema = schemars::schema_for!(vlorql_core::schema::QueryPlan);
let json = serde_json::to_string_pretty(&schema)?;
```

The prompt builder renders a stripped version of this schema in
every system prompt under the **## Required JSON Output** section.

---

## 10. See also

* [`deployment.md`](./deployment.md) — local vLLM/Ollama setup,
  production deployment, and performance tuning.
* [`optimization.md`](./optimization.md) — query optimizer
  documentation (constant folding, predicate pushdown, column
  pruning, join reordering).
* [`caching.md`](./caching.md) — built-in caching system
  (SchemaCache, CompileCache, PromptCache).
* [`README.md`](../README.md) — the high-level elevator pitch.
* [API reference](https://docs.rs/vlorql) — generated from the
  source with `cargo doc --workspace --no-deps`.

---

## 11. Dialect adapter system

VlorQl ships with a three-layer dialect adapter system for
configuring SQL dialect features, rewriting compiled SQL, and
injecting prompt-level instructions.

### 11.1 `DialectConfig` (Layer 1)

A `DialectConfig` value defines every tunable SQL behaviour —
identifier quoting, placeholders, pagination syntax, feature flags,
type mappings, and function name mappings. It replaces the older
`DialectProfile` + `SqlDialect` + `IdentifierQuoting` tuple in the
compiler layer.

**Built-in defaults:**

```rust
use vlorql_core::compile::DialectConfig;

let pg = DialectConfig::default_postgres();
assert_eq!(pg.name, "postgres");
assert_eq!(pg.identifier_quote, "double_quote");

let sqlite = DialectConfig::default_sqlite();
assert_eq!(sqlite.placeholder, "?");
```

**Load from TOML or YAML:**

```rust
let config = DialectConfig::from_toml("examples/custom-dialect.toml")?;
let config = DialectConfig::from_yaml("path/to/dialect.yaml")?;
```

The TOML file at `examples/custom-dialect.toml` shows the full
set of configurable fields.

**Using a config directly with the compiler:**

```rust
use vlorql_core::compile::{ConfigCompiler, DialectConfig};

let config = DialectConfig::default_mysql();
let compiler = ConfigCompiler(Arc::new(config));
let compiled = compiler.compile(&validated)?;
```

### 11.2 `DialectRegistry` (Layer 1)

```rust
use vlorql_core::compile::{DialectRegistry, DialectConfig};
use std::sync::Arc;

let mut registry = DialectRegistry::new();
registry.register("mysql_legacy", Arc::new(DialectConfig::default_mysql()));
let compiler = registry.get_compiler("mysql_legacy")?;
let config = registry.get_config("mysql_legacy")?;
```

### 11.3 `RewriteEngine` (Layer 2)

Applies regex-based rewrite rules to compiled SQL before it is
returned to the caller.

```rust
use vlorql_core::compile::{RewriteEngine, RewriteRule};

let rules = vec![RewriteRule {
    name: "strip-semicolons".to_owned(),
    match_pattern: r";\s*$".to_owned(),
    replace_template: "".to_owned(),
    description: Some("Remove trailing semicolons".to_owned()),
    dialect_filter: None,
}];
let engine = RewriteEngine::new(rules);
let clean = engine.apply("SELECT * FROM users;", "postgres")?;
assert_eq!(clean, "SELECT * FROM users");
```

Load from TOML:

```rust
let engine = RewriteEngine::load_toml("examples/rewrite-rules.toml")?;
```

Attach to the facade builder:

```rust
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_rewrite_engine(engine)
    .build()?;
```

### 11.4 `PromptSkill` (Layer 3)

A `PromptSkill` injects custom instructions, schema simplification
flags, and few-shot examples into the system prompt generated by
`PromptBuilder`.

```rust
use vlorql_core::prompt::PromptSkill;

let skill = PromptSkill::builtin_small_model();
let builder = PromptBuilder::new(schema, dialect, policy)
    .with_skill(skill);
let prompt = builder.build_system_prompt("Show users");
```

Load from TOML:

```rust
let skill = PromptSkill::load_toml("examples/small-model-skill.toml")?;
```

The skill's `forbid_features` overrides dialect feature flags in
the prompt (e.g. disabling CTEs or window functions), and its
`instructions` are injected as additional guidance to the LLM.
