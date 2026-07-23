//! SQL-injection defense tests.
//!
//! These tests are deliberately adversarial: each one constructs a
//! `QueryPlan` whose literal values contain payloads that would
//! compromise a SQL backend that interpolates user input into the
//! query string, then asserts that the VlorQl compiler:
//!
//! 1. Emits parameter placeholders (`$1`, `?`, …) instead of the
//!    literal value in the rendered SQL.
//! 2. Preserves the original value verbatim in the `parameters` list.
//! 3. Refuses identifiers that would inject SQL keywords or
//!    punctuation into the rendered query.

use std::sync::Arc;

use vlorql::{SchemaSnapshot, SqlDialect, VlorQl};
use vlorql_core::compile::{CompiledQuery, PostgresCompiler, SQLiteCompiler, SqlCompiler};
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection,
    QueryPlan, SchemaMetadata, SqlDialect as CoreSqlDialect, TableSchema,
};
use vlorql_core::validate::ValidatedPlan;

fn column(name: &str, data_type: DataType) -> ColumnSchema {
    ColumnSchema {
        name: name.to_owned(),
        data_type,
        nullable: false,
        description: None,
        is_primary_key: false,
        foreign_key: None,
    }
}

fn schema() -> Arc<SchemaSnapshot> {
    Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
            name: "users".to_owned(),
            columns: vec![
                column("id", DataType::Int),
                column("name", DataType::String),
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        }],
        SchemaMetadata::default(),
    ))
}

fn base_plan() -> QueryPlan {
    QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
            alias: None,
        }],
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
            set_operation: None,    }
}

fn plan_with_string_literal(value: &str) -> QueryPlan {
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
        },
        op: ComparisonOperator::Eq,
        right: Expression::Literal {
            value: serde_json::Value::String(value.to_owned()),
            data_type: DataType::String,
        },
    });
    plan
}

fn compile_postgres(plan: ValidatedPlan) -> CompiledQuery {
    PostgresCompiler
        .compile(&plan)
        .expect("postgres should compile literal as a parameter")
}

fn compile_sqlite(plan: ValidatedPlan) -> CompiledQuery {
    SQLiteCompiler
        .compile(&plan)
        .expect("sqlite should compile literal as a parameter")
}

#[test]
fn classic_or_injection_is_parameterized_not_interpolated() {
    let payload = "' OR '1'='1";
    let plan = plan_with_string_literal(payload);
    let validated = ValidatedPlan(Arc::new(plan));
    let pg = compile_postgres(validated.clone());
    let sq = compile_sqlite(validated);

    // The literal value must NEVER appear verbatim in the rendered SQL.
    assert!(
        !pg.sql.contains(payload),
        "Postgres SQL must not embed the payload: {}",
        pg.sql
    );
    assert!(
        !sq.sql.contains(payload),
        "SQLite SQL must not embed the payload: {}",
        sq.sql
    );

    // The compiler should emit placeholders.
    assert!(
        pg.sql.contains("$1"),
        "Postgres SQL must use $1: {}",
        pg.sql
    );
    assert!(sq.sql.contains('?'), "SQLite SQL must use ?: {}", sq.sql);

    // The original value must travel as a bind parameter.
    assert_eq!(pg.parameters.len(), 1);
    assert_eq!(
        pg.parameters[0].value,
        serde_json::Value::String(payload.to_owned())
    );
    assert_eq!(sq.parameters.len(), 1);
    assert_eq!(
        sq.parameters[0].value,
        serde_json::Value::String(payload.to_owned())
    );
}

#[test]
fn stacked_statement_injection_is_parameterized_not_interpolated() {
    let payload = "alice'; DROP TABLE users; --";
    let validated = ValidatedPlan(Arc::new(plan_with_string_literal(payload)));
    let compiled = compile_postgres(validated);
    assert!(
        !compiled.sql.contains("DROP TABLE"),
        "Postgres SQL must not embed DROP TABLE: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("--"),
        "Postgres SQL must not embed the SQL comment: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("$1"),
        "Postgres SQL must use $1: {}",
        compiled.sql
    );
    assert_eq!(compiled.parameters.len(), 1);
    assert_eq!(
        compiled.parameters[0].value,
        serde_json::Value::String(payload.to_owned())
    );
}

#[test]
fn union_select_injection_is_parameterized_not_interpolated() {
    let payload = "x' UNION SELECT password FROM users --";
    let validated = ValidatedPlan(Arc::new(plan_with_string_literal(payload)));
    let compiled = compile_sqlite(validated);
    assert!(
        !compiled.sql.contains("UNION"),
        "SQLite SQL must not embed UNION: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("password"),
        "SQLite SQL must not embed the column name: {}",
        compiled.sql
    );
    assert_eq!(
        compiled.parameters[0].value,
        serde_json::Value::String(payload.to_owned())
    );
}

#[test]
fn integer_overflow_or_tautology_is_parameterized_not_interpolated() {
    // Use a numeric payload; the value should be bound as a parameter and
    // never be substituted into the SQL.
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
        },
        op: ComparisonOperator::Eq,
        right: Expression::Literal {
            value: serde_json::json!(0_i64),
            data_type: DataType::Int,
        },
    });
    let validated = ValidatedPlan(Arc::new(plan));
    let compiled = compile_postgres(validated);
    assert!(compiled.sql.contains("$1"));
    assert!(
        !compiled.sql.contains("= 0 OR"),
        "literal 0 must not be expanded into a tautology: {}",
        compiled.sql
    );
    assert_eq!(compiled.parameters.len(), 1);
    assert_eq!(compiled.parameters[0].value, serde_json::json!(0_i64));
}

#[test]
fn unsafe_identifier_is_rejected_before_sql_is_built() {
    // An identifier that contains characters which cannot appear in an
    // unquoted SQL identifier must be rejected by the compiler. The
    // `QueryBuilder` is exercised directly so we can force
    // `IdentifierQuoting::Never` (unquoted) which exercises the
    // strict validator; with `DoubleQuote` quoting the double quote
    // would be safely escaped to `""`.
    use vlorql_core::compile::QueryBuilder;
    use vlorql_core::schema::IdentifierQuoting;

    let mut plan = base_plan();
    plan.from = FromClause {
        table: "users; DROP TABLE x; --".to_owned(),
        alias: None,
    };
    let validated = ValidatedPlan(Arc::new(plan));
    let error = QueryBuilder::new(
        &validated,
        vlorql_core::schema::SqlDialect::Postgres,
        IdentifierQuoting::Never,
    )
    .build()
    .expect_err("unsafe identifier must be rejected in unquoted mode");
    assert_eq!(error.error_code(), "C001");
    assert!(error.to_string().contains("invalid_unquoted_identifier"));
}

#[test]
fn double_quoted_identifier_escapes_dangerous_characters() {
    // When `IdentifierQuoting::DoubleQuote` is used, the compiler must
    // escape any embedded `"` characters instead of letting them break
    // out of the identifier. The compiled SQL is still safe even when
    // the table name contains characters that would otherwise be
    // syntactically significant.
    use vlorql_core::compile::QueryBuilder;
    use vlorql_core::schema::IdentifierQuoting;

    let mut plan = base_plan();
    plan.from = FromClause {
        table: "users\"; DROP TABLE x; --".to_owned(),
        alias: None,
    };
    let validated = ValidatedPlan(Arc::new(plan));
    let (sql, _params) = QueryBuilder::new(
        &validated,
        vlorql_core::schema::SqlDialect::Postgres,
        IdentifierQuoting::DoubleQuote,
    )
    .build()
    .expect("double-quote mode must safely escape the identifier");
    // The whole name, including the dangerous characters, must be
    // wrapped in double quotes with embedded quotes doubled.
    assert!(sql.contains("\"users\"\"; DROP TABLE x; --\""));
}

#[test]
fn facade_passes_literals_through_compile_only_without_interpolation() {
    // End-to-end: build a real facade, then push a literal-bearing plan
    // through `validate_only` and `compile_only`. The compiled SQL must
    // contain a placeholder, not the literal value.
    let facade = VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .build()
        .expect("facade should build");
    let payload = "alice' OR 1=1 --";
    let plan = plan_with_string_literal(payload);
    let validated = facade
        .validate_only(&plan)
        .expect("malicious literal should still validate when column is known");
    let compiled = facade
        .compile_only(&validated)
        .expect("facade should compile");
    assert_eq!(compiled.dialect, SqlDialect::Postgres);
    assert!(!compiled.sql.contains(payload), "{}", compiled.sql);
    assert!(compiled.sql.contains("$1"), "{}", compiled.sql);
    assert_eq!(
        compiled.parameters[0].value,
        serde_json::Value::String(payload.to_owned())
    );
}

#[test]
fn mysql_limit_offset_does_not_interpolate_literals() {
    use vlorql_core::compile::MySQLCompiler;
    let mut plan = base_plan();
    plan.r#where = Some(Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
        },
        op: ComparisonOperator::Eq,
        right: Expression::Literal {
            value: serde_json::Value::String("alice'; DROP TABLE x".to_owned()),
            data_type: DataType::String,
        },
    });
    let validated = ValidatedPlan(Arc::new(plan));
    let compiled = MySQLCompiler.compile(&validated).expect("mysql compiles");
    assert!(compiled.sql.contains('?'));
    assert!(
        !compiled.sql.contains("alice"),
        "MySQL SQL must not embed the literal: {}",
        compiled.sql
    );
    assert_eq!(
        compiled.parameters[0].value,
        serde_json::Value::String("alice'; DROP TABLE x".to_owned())
    );
}

#[test]
fn dialect_round_trip_preserves_payload_verbatim_in_parameters() {
    // The literal value must be preserved verbatim across all dialects
    // so the database driver can apply its own escaping if necessary.
    use vlorql_core::compile::MySQLCompiler;
    let payload = "𝓤𝓷𝓲𝓬𝓸𝓭𝓮'; /* ⌘ */ --";
    let plan = plan_with_string_literal(payload);
    for dialect in [
        CoreSqlDialect::Postgres,
        CoreSqlDialect::Sqlite,
        CoreSqlDialect::MySql,
    ] {
        let validated = ValidatedPlan(Arc::new(plan.clone()));
        let compiled = match dialect {
            CoreSqlDialect::Postgres => compile_postgres(validated),
            CoreSqlDialect::Sqlite => compile_sqlite(validated),
            CoreSqlDialect::MySql => MySQLCompiler.compile(&validated).expect("mysql compiles"),
        };
        assert!(
            !compiled.sql.contains(payload),
            "{dialect:?} leaked payload: {}",
            compiled.sql
        );
        assert_eq!(
            compiled.parameters[0].value,
            serde_json::Value::String(payload.to_owned())
        );
    }
}
