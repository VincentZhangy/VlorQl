//! Custom `SqlCompiler` implementation (DuckDB-style).
//!
//! `vlorql_core::compile::SqlCompiler` is the trait every dialect-specific
//! compiler implements. The default implementations (`PostgresCompiler`,
//! `SQLiteCompiler`, `MySQLCompiler`) all delegate to `QueryBuilder`; writing
//! your own follows the same pattern.
//!
//! In this example we build a DuckDB-flavoured compiler that:
//!
//!   * reuses `QueryBuilder` so we don't duplicate any rendering logic,
//!   * uses positional `?` placeholders and unquoted identifiers
//!     (DuckDB accepts both, and many shops prefer the leaner look),
//!   * rewrites `LIMIT n OFFSET m` to `OFFSET m LIMIT n` (DuckDB's idiomatic
//!     ordering — both forms parse, but the leading-`OFFSET` form is what the
//!     DuckDB docs reach for first).
//!
//! Registering the custom compiler is a one-liner on the facade builder:
//! `VlorQlBuilder::with_compiler(compiler)`. The dialect you pass via
//! `with_dialect_name` is what the `CompiledQuery::dialect` field reports, so
//! downstream consumers can still branch on it.
//!
//! Run with:
//!   cargo run --example custom_compiler --quiet

use std::error::Error;
use std::sync::Arc;

use serde_json::json;
use vlorql::{SchemaSnapshot, SqlDialect, VlorQl};
use vlorql_core::compile::{CompiledQuery, DialectConfig, QueryBuilder, SqlCompiler};
use vlorql_core::errors::VlorQLError;
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, Expression, FromClause,
    Predicate, Projection, QueryPlan, SchemaMetadata, TableSchema,
};
use vlorql_core::validate::ValidatedPlan;

/// A DuckDB-style SQL compiler.
///
/// Note we report `SqlDialect::Sqlite` for `CompiledQuery::dialect` because
/// `SqlDialect` doesn't have a `DuckDb` variant yet. Adding one is a small
/// follow-up; the trait doesn't care which variant we report — it just
/// threads through `CompiledQuery`.
#[derive(Debug, Clone, Copy, Default)]
struct DuckDbCompiler;

impl SqlCompiler for DuckDbCompiler {
    fn compile(&self, plan: &ValidatedPlan) -> Result<CompiledQuery, VlorQLError> {
        // 1. Defer the heavy lifting to QueryBuilder. We create a
        //    SQLite-style DialectConfig with `never` quoting (no quotes)
        //    so identifiers stay bare; DuckDB parses both quoted and
        //    unquoted identifiers, so this is safe.
        let mut config = DialectConfig::default_sqlite();
        config.identifier_quote = "never".to_owned();
        let (sql, parameters) = QueryBuilder::new(plan, &config).build()?;

        // 2. Apply DuckDB-specific post-processing. Here we just flip
        //    `LIMIT n OFFSET m` to `OFFSET m LIMIT n`. A real implementation
        //    might also rewrite `ILIKE` to DuckDB's case-insensitive
        //    matching helpers or expand `gen_random_uuid()` to a custom macro.
        let sql = rewrite_limit_offset(&sql);

        Ok(CompiledQuery {
            sql,
            parameters,
            dialect: self.dialect(),
        })
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }
}

/// Converts `LIMIT n OFFSET m` (the form `QueryBuilder` emits for SQLite) into
/// `OFFSET m LIMIT n` (the form DuckDB users tend to prefer). Only touches the
/// trailing `LIMIT/OFFSET` pair so it leaves any `LIMIT` inside CTEs or
/// subqueries alone — they would need their own pass to be fully rewritten.
fn rewrite_limit_offset(sql: &str) -> String {
    let lower = sql.to_ascii_lowercase();
    let limit_pos = lower.rfind(" limit ");
    let offset_pos = lower.rfind(" offset ");
    let (Some(limit_pos), Some(offset_pos)) = (limit_pos, offset_pos) else {
        return sql.to_owned();
    };

    // Only swap if `LIMIT` currently appears before `OFFSET`; if the SQL is
    // already in the form we want, leave it alone.
    if limit_pos >= offset_pos {
        return sql.to_owned();
    }

    // Slice into four parts while preserving the original casing:
    //   prefix     | " LIMIT " | limit_value | middle | " OFFSET " | offset_value | suffix
    // We emit: prefix + " OFFSET " + offset_value + " LIMIT " + limit_value + suffix
    // so the numeric values stay attached to their original clauses.
    let limit_value_start = limit_pos + " limit ".len();
    let limit_value_end = end_of_token(&sql[limit_value_start..]);
    let offset_value_start = offset_pos + " offset ".len();
    let offset_value_end = end_of_token(&sql[offset_value_start..]);

    let prefix = &sql[..limit_pos];
    let limit_value = &sql[limit_value_start..limit_value_start + limit_value_end];
    let middle = &sql[limit_value_start + limit_value_end..offset_pos];
    let offset_value = &sql[offset_value_start..offset_value_start + offset_value_end];
    let suffix = &sql[offset_value_start + offset_value_end..];

    format!("{prefix} OFFSET {offset_value} LIMIT {limit_value}{middle}{suffix}")
}

/// Returns the byte length of the next whitespace-separated token starting at
/// the beginning of `tail`. A trailing `;` is also treated as a terminator.
fn end_of_token(tail: &str) -> usize {
    tail.char_indices()
        .find(|(_, character)| character.is_whitespace() || *character == ';')
        .map(|(index, _)| index)
        .unwrap_or(tail.len())
}

fn build_schema() -> Arc<SchemaSnapshot> {
    Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
            name: "users".to_owned(),
            columns: vec![
                ColumnSchema {
                    name: "id".to_owned(),
                    data_type: DataType::Int,
                    nullable: false,
                    description: None,
                    is_primary_key: true,
                    foreign_key: None,
                },
                ColumnSchema {
                    name: "email".to_owned(),
                    data_type: DataType::String,
                    nullable: false,
                    description: None,
                    is_primary_key: false,
                    foreign_key: None,
                },
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        }],
        SchemaMetadata::default(),
    ))
}

fn sample_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "email".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: Expression::Literal {
                value: json!(10),
                data_type: DataType::Int,
            },
        }),
        group_by: None,
        having: None,
        order_by: None,
        limit: Some(50),
        offset: Some(100),
        joins: None,
        ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let vlorql = VlorQl::builder()
        .with_schema(build_schema())
        // Use SQLite as the dialect profile so the *validator* accepts the
        // plan (e.g. SQL functions allowed). The actual SQL rendering is
        // overridden by `with_compiler`.
        .with_dialect_name("sqlite")
        .with_policy(PolicyConfig::default())
        // Hand the facade our DuckDB compiler. After this call, any
        // `VlorQl::compile_only` or `VlorQl::query` will go through
        // `DuckDbCompiler::compile` rather than the default
        // `SQLiteCompiler`.
        .with_compiler(DuckDbCompiler)
        .build()?;

    let validated = vlorql.validate_only(&sample_plan())?;
    let compiled = vlorql.compile_only(&validated)?;

    println!("--- DuckDB-flavoured compilation ---");
    println!("dialect (from CompiledQuery): {:?}", compiled.dialect);
    println!("sql:\n  {}", compiled.sql);
    println!(
        "parameters: {} (rendered as ? placeholders)",
        compiled.parameters.len()
    );

    // Show the same plan rendered by the built-in SQLite compiler so the
    // difference is obvious: the custom compiler produced bare identifiers
    // and `OFFSET m LIMIT n`, while SQLite produces `"users"."id"` and
    // `LIMIT n OFFSET m`.
    let builtin = VlorQl::builder()
        .with_schema(build_schema())
        .with_dialect_name("sqlite")
        .with_policy(PolicyConfig::default())
        .build()?;
    let builtin_sql = builtin.compile_only(&validated)?.sql;
    println!("\n--- Built-in SQLite for comparison ---");
    println!("sql:\n  {}", builtin_sql);
    Ok(())
}
