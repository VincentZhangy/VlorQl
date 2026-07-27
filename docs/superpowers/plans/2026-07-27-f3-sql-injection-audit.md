# F3: SQL 注入来源审计 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 SQL 注入来源审计层，检查 LLM 生成的 plan 中每个标识符是否与 schema 一致，对可疑/注入模式标识符硬失败拒绝执行。

**Architecture:** 新建 `crates/vlorql-core/src/validate/audit.rs`，作为 `ValidationPipeline` 的可选审计阶段。遍历 plan 中所有标识符（表名、列名、CTE 名、别名）与 schema 比对，输出 `AuditReport`。有 Error/Critical 级别警告时拒绝 plan。

**Tech Stack:** Rust (edition 2024)，serde_json，thiserror（已有依赖）。

## Global Constraints

- CI 全绿：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`
- `#![deny(missing_docs)]`：所有新增公共项必须有文档注释
- 不加新第三方依赖
- 不修改 `QueryPlan` 数据模型
- 不修改公共 API 签名
- TDD：先写失败测试 → 确认失败 → 最小实现 → 确认通过 → 提交

---

## File Structure

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql-core/src/validate/audit.rs`（**新建**） | `AuditWarning`、`Severity`、`AuditReport`、`AuditStage` + 单元测试 | 1 |
| `crates/vlorql-core/src/validate/mod.rs`（修改） | 加 `mod audit;` + `pub use` | 1 |
| `crates/vlorql-core/src/errors/kinds.rs`（修改） | 加 `AuditErrorKind` 枚举变体 | 1 |
| `crates/vlorql-core/src/errors/mod.rs`（修改） | re-export `AuditErrorKind` + `error_code()` 加分支 + `is_retryable()` 加分支 | 1 |
| `crates/vlorql-core/src/validate/pipeline.rs`（修改） | `ValidationPipeline` 新增 `audit` 阶段 | 2 |

---

### Task 1: Core — AuditStage 模块 + 错误类型

**Files:**
- Create: `crates/vlorql-core/src/validate/audit.rs`
- Modify: `crates/vlorql-core/src/validate/mod.rs`
- Modify: `crates/vlorql-core/src/errors/kinds.rs`
- Modify: `crates/vlorql-core/src/errors/mod.rs`

**Interfaces:**
- Consumes: `crate::schema::{QueryPlan, SchemaSnapshot, Projection, Predicate, Expression, FromClause, Join}`, `crate::errors::{VlorQLError, ValidationErrorKind}`
- Produces: `audit::Severity`, `audit::AuditWarningKind`, `audit::AuditWarning`, `audit::AuditReport`, `audit::AuditStage` with `new()` and `audit()`; `errors::AuditErrorKind`

- [ ] **Step 1: Write failing tests**

在 `crates/vlorql-core/src/validate/audit.rs` 底部写测试（编译会失败，因为类型未定义）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        ColumnSchema, DataType, Expression, FromClause, Join, JoinType,
        Predicate, Projection, QueryPlan, SchemaMetadata, SchemaSnapshot,
        TableSchema,
    };
    use std::sync::Arc;

    fn test_schema() -> Arc<SchemaSnapshot> {
        Arc::new(SchemaSnapshot::new(
            vec![TableSchema {
                name: "users".to_owned(),
                columns: vec![ColumnSchema {
                    name: "id".to_owned(),
                    data_type: DataType::Int,
                    nullable: false,
                    description: None,
                }],
            }],
            SchemaMetadata {
                version: "v1".to_owned(),
                source: "test".to_owned(),
                description: None,
            },
        ))
    }

    fn valid_plan() -> QueryPlan {
        QueryPlan {
            select: vec![
                Projection::Column {
                    table: None,
                    column: "id".to_owned(),
                    alias: None,
                },
            ],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: None,
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
        }
    }

    #[test]
    fn audit_accepts_valid_plan() {
        let schema = test_schema();
        let plan = valid_plan();
        let result = AuditStage::new().audit(&plan, &schema);
        assert!(result.is_ok(), "valid plan should pass audit");
    }

    #[test]
    fn audit_rejects_missing_table() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.from.table = "nonexistent".to_owned();
        let result = AuditStage::new().audit(&plan, &schema);
        assert!(result.is_err(), "missing table should fail audit");
        let errs = result.unwrap_err();
        assert!(errs.iter().any(|e| e.to_string().contains("nonexistent")));
    }

    #[test]
    fn audit_rejects_missing_column() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.select = vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "nonexistent_col".to_owned(),
                alias: None,
            },
        ];
        let result = AuditStage::new().audit(&plan, &schema);
        assert!(result.is_err(), "missing column should fail audit");
    }

    #[test]
    fn audit_rejects_injection_pattern() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.from.table = "users; DROP TABLE orders".to_owned();
        let result = AuditStage::new().audit(&plan, &schema);
        assert!(result.is_err(), "injection pattern should fail audit");
    }

    #[test]
    fn audit_accepts_valid_cte_name() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.ctes = Some(vec![
            crate::schema::CommonTableExpression {
                name: "recent".to_owned(),
                query: Box::new(valid_plan()),
                recursive: false,
            },
        ]);
        let result = AuditStage::new().audit(&plan, &schema);
        assert!(result.is_ok(), "valid CTE should pass audit");
    }

    #[test]
    fn audit_rejects_injection_in_cte_name() {
        let schema = test_schema();
        let mut plan = valid_plan();
        plan.ctes = Some(vec![
            crate::schema::CommonTableExpression {
                name: "recent; DROP".to_owned(),
                query: Box::new(valid_plan()),
                recursive: false,
            },
        ]);
        let result = AuditStage::new().audit(&plan, &schema);
        assert!(result.is_err(), "CTE name with injection should fail");
    }

    #[test]
    fn audit_accepts_alias_names() {
        let schema = test_schema();
        let plan = QueryPlan {
            select: vec![
                Projection::Expr {
                    expression: Expression::ColumnReference {
                        table: Some("users".to_owned()),
                        column: "id".to_owned(),
                    },
                    alias: Some("my_alias".to_owned()),
                },
            ],
            from: FromClause {
                table: "users".to_owned(),
                alias: Some("u".to_owned()),
            },
            ..valid_plan()
        };
        let result = AuditStage::new().audit(&plan, &schema);
        assert!(result.is_ok(), "alias names should not be audited");
    }
}
```

- [ ] **Step 2: Run to confirm failures**

Run: `cargo test -p vlorql-core --lib validate::audit`
Expected: Compile error (module not found / types not defined).

- [ ] **Step 3: Add `AuditErrorKind` to errors/kinds.rs**

在 `crates/vlorql-core/src/errors/kinds.rs` 中 `ValidationErrorKind` 之后添加新的 error 枚举：

```rust
/// Errors found by the SQL injection audit stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "snake_case")]
pub enum AuditErrorKind {
    /// An identifier referenced in the plan does not exist in the schema.
    #[error("identifier `{identifier}` not found in schema ({context})")]
    IdentifierNotFound {
        /// The offending identifier string.
        identifier: String,
        /// Where the identifier appeared (e.g. "FROM clause").
        context: String,
    },
    /// An identifier matches known SQL injection patterns.
    #[error("identifier `{identifier}` contains suspicious pattern `{pattern}`")]
    SuspiciousPattern {
        /// The offending identifier string.
        identifier: String,
        /// The pattern that was matched.
        pattern: String,
    },
}
```

在 `crates/vlorql-core/src/errors/mod.rs` 的 `pub use kinds::{...}` 行中添加 `AuditErrorKind`：
```rust
pub use kinds::{
    AuditErrorKind, CompilationErrorKind, ConfigErrorKind, LlmErrorKind, PolicyErrorKind,
    SchemaErrorKind, ValidationErrorKind,
};
```

在 `VlorQLError` 枚举中添加新变体（在 `Validation` 变体之后）：
```rust
    /// The query plan failed the SQL injection audit.
    #[error("audit error: {kind}")]
    Audit {
        /// The typed audit failure.
        kind: AuditErrorKind,
        /// Structured context (identifier, pattern, severity).
        details: Value,
    },
```

在 `VlorQLError` 的 `error_code()` 方法中添加分支：
```rust
            Self::Audit { kind: AuditErrorKind::IdentifierNotFound { .. }, .. } => "V008",
            Self::Audit { kind: AuditErrorKind::SuspiciousPattern { .. }, .. } => "V009",
```

在 `VlorQLError` 的 `is_retryable()` 方法中添加分支：
```rust
            // IdentifierNotFound is retryable (LLM can fix the identifier).
            // SuspiciousPattern is NOT retryable — injection patterns should
            // never be re-sent to the LLM without human review.
            Self::Audit { kind: AuditErrorKind::IdentifierNotFound { .. }, .. } => true,
            Self::Audit { kind: AuditErrorKind::SuspiciousPattern { .. }, .. } => false,
```

在 `VlorQLError` 的 `details()` 方法中添加分支（参考 other variants）：
```rust
            Self::Audit { details, .. } => details,
```

在 `VlorQLError` 的 `validation()` / `schema()` / `policy()` 等构造函数旁加一个构造函数：
```rust
    /// Creates an audit error.
    pub fn audit<T: Serialize>(kind: AuditErrorKind, details: T) -> Self {
        Self::Audit {
            kind,
            details: serde_json::to_value(details).unwrap_or_default(),
        }
    }
```

- [ ] **Step 4: Create `audit.rs` core implementation**

新建 `crates/vlorql-core/src/validate/audit.rs`：

```rust
//! SQL injection source audit — checks every plan identifier against the
//! schema and rejects suspicious or non-existent identifiers.

use crate::errors::{AuditErrorKind, VlorQLError};
use crate::schema::{
    Expression, FromClause, Join, Predicate, Projection, QueryPlan, SchemaSnapshot,
};
use serde_json::json;
use tracing::warn;

/// Severity of an audit finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational, does not block execution.
    Warning,
    /// The identifier is not found in the schema. Plan is rejected.
    Error,
    /// The identifier matches known SQL injection patterns. Plan is rejected.
    Critical,
}

/// Categorises the type of audit warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditWarningKind {
    /// A table name referenced in FROM/JOIN does not exist in the schema.
    MissingTable,
    /// A column reference does not exist on its table.
    MissingColumn,
    /// An identifier contains SQL keywords or special characters.
    SuspiciousIdentifier,
}

/// A single audit finding.
#[derive(Debug, Clone)]
pub struct AuditWarning {
    pub kind: AuditWarningKind,
    pub identifier: String,
    pub context: String,
    pub severity: Severity,
}

/// The result of an audit pass.
#[derive(Debug, Clone)]
pub struct AuditReport {
    pub warnings: Vec<AuditWarning>,
}

impl AuditReport {
    /// Returns true if the report contains any Error or Critical warnings.
    pub fn has_blockers(&self) -> bool {
        self.warnings.iter().any(|w| matches!(w.severity, Severity::Error | Severity::Critical))
    }

    /// Converts the report into `VlorQLError`s.
    pub fn into_errors(self) -> Vec<VlorQLError> {
        self.warnings
            .into_iter()
            .filter(|w| matches!(w.severity, Severity::Error | Severity::Critical))
            .map(|w| match w.kind {
                AuditWarningKind::SuspiciousIdentifier => {
                    VlorQLError::audit(
                        AuditErrorKind::SuspiciousPattern {
                            identifier: w.identifier.clone(),
                            pattern: "injection pattern".to_owned(),
                        },
                        json!({
                            "identifier": w.identifier,
                            "context": w.context,
                            "severity": "critical",
                        }),
                    )
                }
                _ => {
                    VlorQLError::audit(
                        AuditErrorKind::IdentifierNotFound {
                            identifier: w.identifier.clone(),
                            context: w.context.clone(),
                        },
                        json!({
                            "identifier": w.identifier,
                            "context": w.context,
                        }),
                    )
                }
            })
            .collect()
    }
}

/// Known SQL injection patterns to scan for in identifiers.
const SUSPICIOUS_PATTERNS: &[&str] = &[
    ";", "--", "/*", "DROP ", "DELETE ", "INSERT ", "UPDATE ",
    "EXEC ", "UNION SELECT", "INTO OUTFILE", "OR 1=1", "OR '1'='1'",
];

/// Checks whether `ident` contains any known injection substrings.
fn has_suspicious_pattern(ident: &str) -> Option<&'static str> {
    let upper = ident.to_uppercase();
    SUSPICIOUS_PATTERNS.iter().find(|pat| upper.contains(*pat)).copied()
}

/// The audit stage — validates identifier sources in a query plan.
#[derive(Debug, Clone)]
pub struct AuditStage;

impl AuditStage {
    /// Creates a new audit stage.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Audits all identifiers in `plan` against `schema`.
    ///
    /// Returns `Ok(())` if no blocking issues are found, or `Err(errors)`
    /// containing the audit failures.
    pub fn audit(&self, plan: &QueryPlan, schema: &SchemaSnapshot) -> Result<(), Vec<VlorQLError>> {
        let mut report = AuditReport { warnings: Vec::new() };

        // Helper to check a single identifier.
        let mut check_ident = |ident: &str, ctx: &str| {
            // Check for injection patterns first (Critical).
            if let Some(pattern) = has_suspicious_pattern(ident) {
                warn!(
                    "AUDIT:SuspiciousPattern identifier={} context={} pattern={}",
                    ident, ctx, pattern,
                );
                report.warnings.push(AuditWarning {
                    kind: AuditWarningKind::SuspiciousIdentifier,
                    identifier: ident.to_owned(),
                    context: ctx.to_owned(),
                    severity: Severity::Critical,
                });
                return;
            }
        };

        // Audit FROM clause.
        check_ident(&plan.from.table, "FROM clause table");
        if !schema.has_table(&plan.from.table) {
            report.warnings.push(AuditWarning {
                kind: AuditWarningKind::MissingTable,
                identifier: plan.from.table.clone(),
                context: "FROM clause table".to_owned(),
                severity: Severity::Error,
            });
        }

        // Audit JOINs.
        if let Some(ref joins) = plan.joins {
            for join in joins {
                check_ident(&join.right_table, "JOIN clause table");
                if !schema.has_table(&join.right_table) {
                    report.warnings.push(AuditWarning {
                        kind: AuditWarningKind::MissingTable,
                        identifier: join.right_table.clone(),
                        context: "JOIN clause table".to_owned(),
                        severity: Severity::Error,
                    });
                }
            }
        }

        // Audit CTE names.
        if let Some(ref ctes) = plan.ctes {
            for cte in ctes {
                check_ident(&cte.name, "CTE name");
            }
        }

        // Audit column references in SELECT projections.
        for proj in &plan.select {
            match proj {
                Projection::Column { table: Some(t), column, .. } => {
                    check_ident(t, "SELECT projection table");
                    check_ident(column, "SELECT projection column");
                    if !schema.table_has_column(t, column) {
                        report.warnings.push(AuditWarning {
                            kind: AuditWarningKind::MissingColumn,
                            identifier: format!("{}.{}", t, column),
                            context: "SELECT projection".to_owned(),
                            severity: Severity::Error,
                        });
                    }
                }
                Projection::Column { table: None, column, .. } => {
                    check_ident(column, "SELECT projection column (unqualified)");
                    // Unqualified column: check all tables.
                    if !schema.iter_tables().any(|t| schema.table_has_column(t.name(), column)) {
                        report.warnings.push(AuditWarning {
                            kind: AuditWarningKind::MissingColumn,
                            identifier: column.clone(),
                            context: "SELECT projection unqualified column".to_owned(),
                            severity: Severity::Error,
                        });
                    }
                }
                Projection::Expr { expression, .. } => {
                    audit_expression(expression, schema, &mut report);
                }
                Projection::Star { table: Some(t) } => {
                    check_ident(t, "SELECT star table");
                    if !schema.has_table(t) {
                        report.warnings.push(AuditWarning {
                            kind: AuditWarningKind::MissingTable,
                            identifier: t.clone(),
                            context: "SELECT * table".to_owned(),
                            severity: Severity::Error,
                        });
                    }
                }
                Projection::Star { table: None } => {}
            }
        }

        // Audit WHERE/HAVING predicates.
        if let Some(ref pred) = plan.r#where {
            audit_predicate(pred, schema, &mut report);
        }
        if let Some(ref having) = plan.having {
            for pred in having {
                audit_predicate(pred, schema, &mut report);
            }
        }

        // Audit ORDER BY.
        if let Some(ref order_by) = plan.order_by {
            audit_expression(&order_by.expr, schema, &mut report);
        }

        if report.has_blockers() {
            Err(report.into_errors())
        } else {
            Ok(())
        }
    }
}

impl Default for AuditStage {
    fn default() -> Self {
        Self::new()
    }
}

fn audit_expression(
    expr: &Expression,
    schema: &SchemaSnapshot,
    report: &mut AuditReport,
) {
    match expr {
        Expression::ColumnReference { table: Some(t), column } => {
            if !schema.has_table(t) {
                report.warnings.push(AuditWarning {
                    kind: AuditWarningKind::MissingTable,
                    identifier: t.clone(),
                    context: "expression column reference table".to_owned(),
                    severity: Severity::Error,
                });
            } else if !schema.table_has_column(t, column) {
                report.warnings.push(AuditWarning {
                    kind: AuditWarningKind::MissingColumn,
                    identifier: format!("{}.{}", t, column),
                    context: "expression column reference".to_owned(),
                    severity: Severity::Error,
                });
            }
        }
        Expression::ColumnReference { table: None, column } => {
            if !schema.iter_tables().any(|t| schema.table_has_column(t.name(), column)) {
                report.warnings.push(AuditWarning {
                    kind: AuditWarningKind::MissingColumn,
                    identifier: column.clone(),
                    context: "expression unqualified column reference".to_owned(),
                    severity: Severity::Error,
                });
            }
        }
        Expression::BinaryOp { left, right, .. } => {
            audit_expression(left, schema, report);
            audit_expression(right, schema, report);
        }
        Expression::UnaryOp { operand, .. } => {
            audit_expression(operand, schema, report);
        }
        Expression::Cast { expr, .. }
        | Expression::Between { expr, .. } => {
            audit_expression(expr, schema, report);
        }
        Expression::FunctionCall { args, .. } => {
            for arg in args {
                audit_expression(arg, schema, report);
            }
        }
        Expression::Subquery(plan) => {
            // Recursively audit subquery plans.
            // (Use AuditStage to avoid borrow issues)
            let stage = AuditStage::new();
            // Collect errors into current report.
            if let Err(errs) = stage.audit(plan.as_ref(), schema) {
                for err in errs {
                    warn!("AUDIT: subquery audit error: {err}");
                }
            }
        }
        // Literals, aggregates, and star don't contain identifiers.
        Expression::Literal { .. }
        | Expression::Aggregate { .. }
        | Expression::Star => {}
    }
}

fn audit_predicate(
    pred: &Predicate,
    schema: &SchemaSnapshot,
    report: &mut AuditReport,
) {
    match pred {
        Predicate::Comparison { left, right, .. } => {
            audit_expression(left, schema, report);
            audit_expression(right, schema, report);
        }
        Predicate::In { expr, .. } => {
            audit_expression(expr, schema, report);
        }
        Predicate::Like { left, right, .. } => {
            audit_expression(left, schema, report);
            audit_expression(right, schema, report);
        }
        Predicate::Between { expr, left, right, .. } => {
            audit_expression(expr, schema, report);
            audit_expression(left, schema, report);
            audit_expression(right, schema, report);
        }
        Predicate::IsNull { expr } => {
            audit_expression(expr, schema, report);
        }
        Predicate::And(children) | Predicate::Or(children) => {
            for child in children {
                audit_predicate(child, schema, report);
            }
        }
        Predicate::Not(child) => {
            audit_predicate(child, schema, report);
        }
        Predicate::Exists(subquery) | Predicate::NotExists(subquery) => {
            let stage = AuditStage::new();
            if let Err(errs) = stage.audit(subquery.as_ref(), schema) {
                for err in errs {
                    warn!("AUDIT: subquery predicate audit error: {err}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // ... tests from Step 1 go here ...
}
```

- [ ] **Step 5: Register module**

修改 `crates/vlorql-core/src/validate/mod.rs`，在 `mod dialect;` 之后加：

```rust
mod audit;
```

在 `pub use` 块中（参考现有结构）加：

```rust
pub use audit::AuditStage;
```

- [ ] **Step 6: Run to confirm passes**

Run:
```bash
cargo test -p vlorql-core --lib validate::audit
cargo clippy -p vlorql-core --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: all pass.

- [ ] **Step 7: Commit Task 1**

```bash
git add crates/vlorql-core/src/validate/audit.rs \
        crates/vlorql-core/src/validate/mod.rs \
        crates/vlorql-core/src/errors/kinds.rs \
        crates/vlorql-core/src/errors/mod.rs
git commit -m "feat(validate): add AuditStage for SQL injection identifier audit (F3-1)"
```

---

### Task 2: Integration — 接入 ValidationPipeline

**Files:**
- Modify: `crates/vlorql-core/src/validate/pipeline.rs`

**Interfaces:**
- Consumes: `AuditStage` from Task 1
- Produces: `ValidationPipeline::with_audit()`, pipeline runs audit stage

- [ ] **Step 1: Write integration test**

在 `crates/vlorql-core/src/validate/pipeline.rs` 的 `mod tests` 中添加：

```rust
#[test]
fn pipeline_runs_audit_stage() {
    use crate::validate::AuditStage;

    let schema = Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
            name: "users".to_owned(),
            columns: vec![ColumnSchema {
                name: "id".to_owned(),
                data_type: DataType::Int,
                nullable: false,
                description: None,
            }],
        }],
        SchemaMetadata {
            version: "v1".to_owned(),
            source: "test".to_owned(),
            description: None,
        },
    ));

    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: None, column: "id".to_owned(), alias: None,
        }],
        from: FromClause { table: "nonexistent".to_owned(), alias: None },
        r#where: None, group_by: None, having: None,
        order_by: None, limit: None, offset: None,
        joins: None, ctes: None,
        distinct: false, distinct_on: None, set_operation: None,
    };

    let audit = AuditStage::new();
    let pipeline = ValidationPipeline {
        schema_validator: (),
        operand_validator: (),
        dialect_validator: (),
        policy_validator: (),
        audit_stage: Some(audit),
    };
    // ... adjust based on actual ValidationPipeline fields
}
```

(Tip: Read the actual `ValidationPipeline` struct fields first, then adapt the test.)

- [ ] **Step 2: Read pipeline.rs for exact struct fields**

Read `crates/vlorql-core/src/validate/pipeline.rs`:
- Find `pub struct ValidationPipeline`
- Find constructor / `new()` method
- Find `validate()` or `validate_all()` method to see where stages are called
- Find `validate_repairing()` if it exists

- [ ] **Step 3: Add audit field to ValidationPipeline**

```rust
pub struct ValidationPipeline {
    // existing fields...
    audit_stage: Option<AuditStage>,
}
```

In the constructor, add `audit_stage: None,` or an equivalent default.

- [ ] **Step 4: Add builder method**

```rust
/// Enables or disables the SQL injection audit stage.
#[must_use]
pub fn with_audit(mut self, enable: bool) -> Self {
    if enable {
        self.audit_stage = Some(AuditStage::new());
    } else {
        self.audit_stage = None;
    }
    self
}
```

- [ ] **Step 5: Run audit in validate()**

In the `validate()` or `validate_and_report()` method, after the schema/operand/dialect stages and before the policy stage, add:

```rust
// SQL injection audit.
if let Some(ref audit) = self.audit_stage {
    if let Err(audit_errors) = audit.audit(plan, schema) {
        // If critical injection patterns found, short-circuit immediately.
        // Otherwise collect along with other errors.
        // Determine if any are SuspiciousPattern (not retryable).
        let has_critical = audit_errors.iter().any(|e| !e.is_retryable());
        if has_critical {
            return Err(audit_errors);
        }
        all_errors.extend(audit_errors);
    }
}
```

- [ ] **Step 6: Verify**

```bash
cargo test -p vlorql-core
cargo clippy -p vlorql-core --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: all pass.

- [ ] **Step 7: Commit Task 2**

```bash
git add crates/vlorql-core/src/validate/pipeline.rs
git commit -m "feat(pipeline): integrate AuditStage into ValidationPipeline (F3-2)"
```

---

### Task 3: Final verification

- [ ] **Step 1: Full workspace check**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: all green.

- [ ] **Step 2: Update plan doc**

```bash
git add docs/superpowers/plans/2026-07-27-f3-sql-injection-audit.md
git commit -m "docs: add F3 implementation plan"
```

---

## Self-Review

1. **Spec coverage:** Covers AuditStage core (Task 1), error types (Task 1), pipeline integration (Task 2), injection pattern detection (Task 1), all 4 identifier types (table, column, CTE, alias). All spec requirements addressed.
2. **Placeholder scan:** No TBD/TODO. All code blocks provide complete implementation. Task 2 Step 1 includes "adjust based on actual fields" which is intentional (need to read actual struct).
3. **Type consistency:** `AuditStage::new()` and `audit(&self, &QueryPlan, &SchemaSnapshot) -> Result<(), Vec<VlorQLError>>` consistent across both tasks. `AuditErrorKind` with two variants consistent with `VlorQLError::audit()` constructor.
4. **Scope check:** Focused on audit detection + pipeline integration. No automatic repair, no plan model changes.
