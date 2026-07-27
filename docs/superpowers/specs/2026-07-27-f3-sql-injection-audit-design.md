# F3: SQL 注入来源审计 — Design Spec

> **Date:** 2026-07-27
> **Branch:** `feat/0.4.0`
> **Status:** Approved

## Goal

为 VlorQl 新增 SQL 注入来源审计层，检查 LLM 生成的 plan 中每个标识符（表名、列名、CTE 名、别名）是否与 schema 一致，对可疑/注入模式标识符**硬失败拒绝执行**。

## Architecture

新建 `crates/vlorql-core/src/validate/audit.rs`，作为 `ValidationPipeline` 的可选阶段。遍历 plan 中的所有标识符，与 schema 比对，输出 `AuditReport`。有 `Error`/`Critical` 级别警告时拒绝 plan。

**不涉及：** 修改 `QueryPlan` 数据模型、修改公共 API 签名、新增第三方依赖。

---

## Design

### Core Data Types

```rust
/// Severity of an audit finding.
pub enum Severity {
    /// Informational, does not block execution.
    Warning,
    /// The identifier is not found in the schema. Plan is rejected.
    Error,
    /// The identifier matches known SQL injection patterns. Plan is rejected.
    Critical,
}

pub enum AuditWarningKind {
    /// A table name referenced in FROM/JOIN does not exist in the schema.
    MissingTable,
    /// A column reference does not exist on its table.
    MissingColumn,
    /// An identifier contains SQL keywords or special characters.
    SuspiciousIdentifier,
}

pub struct AuditWarning {
    pub kind: AuditWarningKind,
    pub identifier: String,
    pub context: String,       // e.g. "FROM clause", "CTE name", "JOIN alias"
    pub severity: Severity,
}

pub struct AuditReport {
    pub warnings: Vec<AuditWarning>,
}
```

### AuditStage

```rust
pub struct AuditStage;

impl AuditStage {
    /// Creates a new audit stage.
    pub fn new() -> Self;

    /// Audits all identifiers in the plan against the schema.
    pub fn audit(plan: &QueryPlan, schema: &SchemaSnapshot) -> Result<(), Vec<VlorQLError>>;
}
```

### Identifier Coverage

| Identifier | Source | Check | Fail Severity |
|------------|--------|-------|---------------|
| `FROM.table` | `FromClause` | `schema.has_table(name)` | Error |
| `SELECT col` (unqualified) | `Projection::Column.column` | Check all tables in scope have this column | Error |
| `SELECT table.col` | `Projection::Column.table + column` | `schema.table_has_column(table, col)` | Error |
| `WHERE table.col` | Predicate column refs | `schema.table_has_column(table, col)` | Error |
| `JOIN table` | `Join.right_table` | `schema.has_table(name)` | Error |
| `WITH name` | CTE name | Must appear in `plan.ctes[i].name` | Error |
| `AS alias` | All alias fields | Skip (alias is always user-defined) | — |
| Any identifier | All string fields | Check against SQL injection patterns (`;`, `--`, `DROP`, `UNION`, etc.) | Critical |

### Injection Pattern Detection

A simple set of known-bad substrings / regex patterns:

```rust
const SUSPICIOUS_PATTERNS: &[&str] = &[
    ";", "--", "/*", "DROP ", "DELETE ", "INSERT ", "UPDATE ",
    "EXEC ", "UNION SELECT", "INTO OUTFILE", "OR 1=1", "OR '1'='1'",
];
```

### Integration: ValidationPipeline

In `crates/vlorql-core/src/validate/pipeline.rs`, add an optional `audit` stage:

```rust
pub struct ValidationPipeline {
    schema: SchemaValidator,
    operand: OperandValidator,
    dialect: DialectValidator,
    policy: PolicyValidator,
    audit: Option<AuditStage>,   // new
}
```

The audit runs **after schema validation, before policy validation**:

```
plan → schema validate → operand validate → dialect validate → audit → policy validate
```

If `audit` returns errors, the pipeline short-circuits and returns the audit errors.

### Error Type

New error kind in `VlorQLError`:

```rust
pub enum AuditErrorKind {
    IdentifierNotFound { identifier: String, context: String },
    SuspiciousPattern { identifier: String, pattern: String },
}
```

Audit errors are **not retryable** for `Critical` severity (injection patterns), but **are retryable** for `Error` severity (the LLM can fix the identifier on retry).

### Default

`AuditStage` is **enabled by default** (no configuration needed). Users who want to opt out can configure `with_audit(false)` on the builder.

---

## Testing

| Test | Description |
|------|-------------|
| `audit_accepts_valid_plan` | Plan with all identifiers existing in schema passes |
| `audit_rejects_missing_table` | FROM table not in schema → Error |
| `audit_rejects_missing_column` | Column not on the referenced table → Error |
| `audit_rejects_injection_pattern` | Identifier with `;` or `DROP` → Critical |
| `audit_accepts_valid_cte_name` | CTE name matches its definition |
| `audit_rejects_undefined_cte` | CTE name referenced but not defined → Error |
| `audit_accepts_alias_names` | Aliases are skipped, no false positive |
| `audit_report_has_severity` | Verify severity levels are correctly assigned |
| `validation_pipeline_runs_audit_stage` | Integration test: pipeline runs audit step |

---

## Non-goals

- 不在本 F3 范围内：自动修复标识符（仅检测拒绝）
- 不修改 `QueryPlan` 数据模型
- 不加新第三方依赖
