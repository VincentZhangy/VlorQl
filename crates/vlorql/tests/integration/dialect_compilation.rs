//! Multi-dialect compilation integration tests.
//!
//! These tests validate that a single [`ValidatedPlan`] compiles into
//! dialect-correct SQL on PostgreSQL, SQLite, and MySQL. They use
//! [`rstest`] to parameterize the suite across dialects so each test
//! case documents its expected behavior once and runs three times —
//! once per dialect.
//!
//! The tests rely on simple string-pattern checks rather than a full
//! SQL parser. The expected fragments are intentionally explicit so a
//! regression in any compiler surfaces immediately and is obvious from
//! the failing assertion.

use rstest::rstest;
use serde_json::json;
use std::sync::Arc;

use vlorql_core::compile::{
    CompiledQuery, MySQLCompiler, PostgresCompiler, SQLiteCompiler, SqlCompiler,
};
use vlorql_core::schema::{
    ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection, QueryPlan,
    SqlDialect,
};
use vlorql_core::validate::ValidatedPlan;

// ---------------------------------------------------------------------------
// rstest case constants
// ---------------------------------------------------------------------------

/// All three dialects, packaged as constants so each test can reference
/// the full set via `rstest`'s `#[case(...)]` attribute without
/// generating an extra fixture function (which `rstest` would treat as
/// a parameterless test target).
const DIALECT_CASES: &[DialectCase] = &[
    DialectCase {
        name: "postgres",
        sql_dialect: SqlDialect::Postgres,
        placeholder: "$1",
        quote: '"',
    },
    DialectCase {
        name: "sqlite",
        sql_dialect: SqlDialect::Sqlite,
        placeholder: "?",
        quote: '"',
    },
    DialectCase {
        name: "mysql",
        sql_dialect: SqlDialect::MySql,
        placeholder: "?",
        quote: '`',
    },
];

/// rstest case data describing a single dialect's expectations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DialectCase {
    name: &'static str,
    sql_dialect: SqlDialect,
    /// The expected parameter placeholder style (`$1`, `?`, …).
    placeholder: &'static str,
    /// The expected identifier quoting character (`"`, `` ` ``).
    quote: char,
}

// ---------------------------------------------------------------------------
// Plan builders
// ---------------------------------------------------------------------------

fn plan_with_where() -> QueryPlan {
    let mut plan = plan_with_limit_offset();
    plan.r#where = Some(Predicate::Comparison {
        left: Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
        },
        op: ComparisonOperator::Gt,
        right: Expression::Literal {
            value: json!(10),
            data_type: DataType::Int,
        },
    });
    plan
}

fn plan_with_limit_offset() -> QueryPlan {
    let mut plan = plan_with_two_columns();
    plan.limit = Some(25);
    plan.offset = Some(5);
    plan
}

fn plan_with_two_columns() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: Some("display_name".to_owned()),
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
            set_operation: None,    }
}

fn validate(plan: &QueryPlan) -> ValidatedPlan {
    ValidatedPlan(Arc::new(plan.clone()))
}

fn compile_with(case: DialectCase, plan: &ValidatedPlan) -> CompiledQuery {
    match case.sql_dialect {
        SqlDialect::Postgres => PostgresCompiler.compile(plan),
        SqlDialect::Sqlite => SQLiteCompiler.compile(plan),
        SqlDialect::MySql => MySQLCompiler.compile(plan),
    }
    .expect("validated plan should compile on the chosen dialect")
}

fn expected_identifier_quote(quote: char) -> String {
    format!("{quote}users{quote}.{quote}id{quote}")
}

// ---------------------------------------------------------------------------
// Parameterized tests
// ---------------------------------------------------------------------------

/// Every dialect must respect the `ValidatedPlan` and emit a `SELECT`
/// projection whose columns are wrapped in the dialect's preferred
/// quoting style.
#[rstest]
#[case::postgres(DIALECT_CASES[0])]
#[case::sqlite(DIALECT_CASES[1])]
#[case::mysql(DIALECT_CASES[2])]
fn select_uses_dialect_quoting(#[case] case: DialectCase) {
    let compiled = compile_with(case, &validate(&plan_with_two_columns()));
    let expected = expected_identifier_quote(case.quote);
    assert_eq!(compiled.dialect, case.sql_dialect);
    assert!(
        compiled.sql.contains(&expected),
        "expected `{expected}` in `{}` (dialect = {:?})",
        compiled.sql,
        case.sql_dialect
    );
}

/// PostgreSQL uses numbered placeholders (`$1`, `$2`); SQLite and MySQL
/// use positional `?`. The compilation must emit exactly the right
/// style for each dialect.
#[rstest]
#[case::postgres(DIALECT_CASES[0])]
#[case::sqlite(DIALECT_CASES[1])]
#[case::mysql(DIALECT_CASES[2])]
fn where_clause_uses_dialect_placeholder(#[case] case: DialectCase) {
    let compiled = compile_with(case, &validate(&plan_with_where()));
    assert!(
        compiled.sql.contains(case.placeholder),
        "expected placeholder `{}` in `{}` (dialect = {:?})",
        case.placeholder,
        compiled.sql,
        case.sql_dialect
    );
    assert_eq!(
        compiled.parameters.len(),
        3,
        "three parameters expected (where, limit, offset)"
    );
    assert_eq!(compiled.parameters[0].value, json!(10));
}

/// The literal value must never be interpolated; it must appear in the
/// `parameters` vector with its declared `DataType` so a downstream
/// driver can bind it safely.
#[rstest]
#[case::postgres(DIALECT_CASES[0])]
#[case::sqlite(DIALECT_CASES[1])]
#[case::mysql(DIALECT_CASES[2])]
fn literal_value_is_parameterized_never_inlined(#[case] case: DialectCase) {
    let compiled = compile_with(case, &validate(&plan_with_where()));
    assert!(
        !compiled.sql.contains("10"),
        "the literal `10` must not appear inline in `{}`",
        compiled.sql
    );
    assert_eq!(compiled.parameters.len(), 3);
    assert_eq!(compiled.parameters[0].value, json!(10));
    assert_eq!(compiled.parameters[0].data_type, DataType::Int);
    let (limit_val, offset_val): (serde_json::Value, serde_json::Value) = match case.sql_dialect {
        // MySQL adds offset first, then limit
        SqlDialect::MySql => (json!(5), json!(25)),
        _ => (json!(25), json!(5)),
    };
    assert_eq!(compiled.parameters[1].value, limit_val);
    assert_eq!(compiled.parameters[1].data_type, DataType::Int);
    assert_eq!(compiled.parameters[2].value, offset_val);
    assert_eq!(compiled.parameters[2].data_type, DataType::Int);
}

/// `LIMIT`/`OFFSET` semantics differ between dialects:
///
/// * PostgreSQL emits `LIMIT <n> OFFSET <m>`.
/// * SQLite emits `LIMIT <n> OFFSET <m>`.
/// * MySQL collapses `LIMIT n OFFSET m` into `LIMIT m, n`.
#[rstest]
#[case::postgres(DIALECT_CASES[0])]
#[case::sqlite(DIALECT_CASES[1])]
#[case::mysql(DIALECT_CASES[2])]
fn limit_offset_uses_dialect_syntax(#[case] case: DialectCase) {
    let compiled = compile_with(case, &validate(&plan_with_limit_offset()));
    let sql = &compiled.sql;
    match case.sql_dialect {
        SqlDialect::Postgres => {
            assert!(
                sql.contains("LIMIT $1"),
                "expected `LIMIT $1` in `{sql}` (dialect = {:?})",
                case.sql_dialect
            );
            assert!(
                sql.contains("OFFSET $2"),
                "expected `OFFSET $2` in `{sql}` (dialect = {:?})",
                case.sql_dialect
            );
        }
        SqlDialect::Sqlite => {
            assert!(
                sql.contains("LIMIT ?"),
                "expected `LIMIT ?` in `{sql}` (dialect = {:?})",
                case.sql_dialect
            );
            assert!(
                sql.contains("OFFSET ?"),
                "expected `OFFSET ?` in `{sql}` (dialect = {:?})",
                case.sql_dialect
            );
        }
        SqlDialect::MySql => {
            // MySQL uses the `LIMIT <offset>, <limit>` form.
            assert!(
                sql.contains("LIMIT ?, ?"),
                "expected `LIMIT ?, ?` in `{sql}` (dialect = {:?})",
                case.sql_dialect
            );
        }
    }
    assert_eq!(compiled.parameters.len(), 2);
    if case.sql_dialect == SqlDialect::MySql {
        // MySQL adds offset first, then limit
        assert_eq!(compiled.parameters[0].value, json!(5));
        assert_eq!(compiled.parameters[1].value, json!(25));
    } else {
        assert_eq!(compiled.parameters[0].value, json!(25));
        assert_eq!(compiled.parameters[1].value, json!(5));
    }
}

/// When `LIMIT` is set without `OFFSET`, every dialect should still
/// render `LIMIT <n>` consistently.
#[rstest]
#[case::postgres(DIALECT_CASES[0])]
#[case::sqlite(DIALECT_CASES[1])]
#[case::mysql(DIALECT_CASES[2])]
fn limit_only_uses_dialect_syntax(#[case] case: DialectCase) {
    let mut plan = plan_with_two_columns();
    plan.limit = Some(7);
    let compiled = compile_with(case, &validate(&plan));
    assert!(
        compiled.sql.contains("LIMIT ?") || compiled.sql.contains("LIMIT $1"),
        "expected `LIMIT ?` or `LIMIT $1` in `{}` (dialect = {:?})",
        compiled.sql,
        case.sql_dialect
    );
    assert!(
        !compiled.sql.contains("OFFSET"),
        "no OFFSET clause should be emitted when unset (got `{}`)",
        compiled.sql
    );
    assert_eq!(compiled.parameters.len(), 1);
    assert_eq!(compiled.parameters[0].value, json!(7));
}

/// Column aliases must round-trip through the dialect compiler. The
/// alias `display_name` should appear wrapped in the dialect's quote
/// style.
#[rstest]
#[case::postgres(DIALECT_CASES[0])]
#[case::sqlite(DIALECT_CASES[1])]
#[case::mysql(DIALECT_CASES[2])]
fn column_alias_uses_dialect_quoting(#[case] case: DialectCase) {
    let compiled = compile_with(case, &validate(&plan_with_two_columns()));
    let expected = format!("AS {}display_name{}", case.quote, case.quote);
    assert!(
        compiled.sql.contains(&expected),
        "expected `{expected}` in `{}` (dialect = {:?})",
        compiled.sql,
        case.sql_dialect
    );
}

/// A consolidated fixture-style test that documents the full set of
/// dialect differences in one place. Helpful when debugging a single
/// failure across all three compilers.
#[rstest]
#[case::postgres(DIALECT_CASES[0])]
#[case::sqlite(DIALECT_CASES[1])]
#[case::mysql(DIALECT_CASES[2])]
fn full_query_matches_dialect_expectations(#[case] case: DialectCase) {
    let compiled = compile_with(case, &validate(&plan_with_where()));
    let q = case.quote;
    let expected = match case.sql_dialect {
        SqlDialect::Postgres => format!(
            "SELECT {q}users{q}.{q}id{q}, {q}users{q}.{q}name{q} AS {q}display_name{q} \
             FROM {q}users{q} WHERE {q}users{q}.{q}id{q} > $1 \
             LIMIT $2 OFFSET $3"
        ),
        SqlDialect::Sqlite => format!(
            "SELECT {q}users{q}.{q}id{q}, {q}users{q}.{q}name{q} AS {q}display_name{q} \
             FROM {q}users{q} WHERE {q}users{q}.{q}id{q} > ? \
             LIMIT ? OFFSET ?"
        ),
        SqlDialect::MySql => format!(
            "SELECT {q}users{q}.{q}id{q}, {q}users{q}.{q}name{q} AS {q}display_name{q} \
             FROM {q}users{q} WHERE {q}users{q}.{q}id{q} > ? \
             LIMIT ?, ?"
        ),
    };
    assert_eq!(compiled.sql, expected);
    assert_eq!(compiled.parameters.len(), 3);
    assert_eq!(compiled.parameters[0].value, json!(10));
    if case.sql_dialect == SqlDialect::MySql {
        // MySQL adds offset first, then limit
        assert_eq!(compiled.parameters[1].value, json!(5));
        assert_eq!(compiled.parameters[2].value, json!(25));
    } else {
        assert_eq!(compiled.parameters[1].value, json!(25));
        assert_eq!(compiled.parameters[2].value, json!(5));
    }
}

// ---------------------------------------------------------------------------
// Cross-dialect sanity checks (not parameterized, for clarity)
// ---------------------------------------------------------------------------

/// The three dialects produce different concrete SQL strings for the
/// same plan. This single test snapshots the exact output for each
/// compiler so that any accidental cross-pollination between compilers
/// is caught by a single glance at the test report.
#[test]
fn snapshots_for_all_three_dialects() {
    let plan = plan_with_where();

    let postgres = PostgresCompiler
        .compile(&validate(&plan))
        .expect("postgres should compile");
    let sqlite = SQLiteCompiler
        .compile(&validate(&plan))
        .expect("sqlite should compile");
    let mysql = MySQLCompiler
        .compile(&validate(&plan))
        .expect("mysql should compile");

    assert_eq!(
        postgres.sql,
        "SELECT \"users\".\"id\", \"users\".\"name\" AS \"display_name\" \
         FROM \"users\" WHERE \"users\".\"id\" > $1 \
         LIMIT $2 OFFSET $3"
    );
    assert_eq!(
        sqlite.sql,
        "SELECT \"users\".\"id\", \"users\".\"name\" AS \"display_name\" \
         FROM \"users\" WHERE \"users\".\"id\" > ? \
         LIMIT ? OFFSET ?"
    );
    assert_eq!(
        mysql.sql,
        "SELECT `users`.`id`, `users`.`name` AS `display_name` \
         FROM `users` WHERE `users`.`id` > ? \
         LIMIT ?, ?"
    );

    // PostgreSQL and SQLite share the same parameter order (where, limit, offset).
    // MySQL stores offset before limit because of the `LIMIT offset, limit` syntax.
    assert_eq!(postgres.parameters, sqlite.parameters);
    assert_ne!(sqlite.parameters, mysql.parameters);
    assert_eq!(postgres.parameters.len(), 3);
    assert_eq!(postgres.parameters[0].value, json!(10));
    assert_eq!(postgres.parameters[1].value, json!(25));
    assert_eq!(postgres.parameters[2].value, json!(5));

    // MySQL stores offset at index 1, limit at index 2
    assert_eq!(mysql.parameters[1].value, json!(5));
    assert_eq!(mysql.parameters[2].value, json!(25));
}
