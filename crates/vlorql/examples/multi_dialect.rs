//! Compile the same `QueryPlan` across every supported SQL dialect.
//!
//! Building three different `VlorQl` facades — one per dialect — and running
//! the same `QueryPlan` through each makes the dialect-specific differences
//! obvious:
//!
//!   * identifier quoting: `"users"` vs. `` `users` ``,
//!   * parameter placeholders: `$1`, `$2` (Postgres) vs. `?` (SQLite/MySQL),
//!   * limit/offset syntax: `LIMIT n OFFSET m` (Postgres/SQLite) vs.
//!     `LIMIT m, n` (MySQL).
//!
//! Run with:
//!   cargo run --example multi_dialect --quiet
//!
//! The example deliberately skips the LLM round-trip: we construct a plan
//! directly and use the facade's `validate_only` + `compile_only` helpers.

use std::error::Error;
use std::sync::Arc;

use serde_json::json;
use vlorql::{CompiledQuery, SchemaSnapshot, SqlDialect, VlorQl};
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection,
    QueryPlan, SchemaMetadata, TableSchema,
};

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
                    name: "name".to_owned(),
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

/// A canonical plan: select two columns, apply a `WHERE`, paginate with
/// `LIMIT 50 OFFSET 100`. We don't add features like CTEs or joins here so
/// the only differences between the dialects are quoting, placeholders, and
/// limit/offset syntax.
fn canonical_plan() -> QueryPlan {
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
        r#where: Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: Expression::Literal {
                value: json!(1000),
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

/// Builds a `VlorQl` facade for the requested dialect. The compiler defaults
/// match the dialect (`PostgresCompiler`, `SQLiteCompiler`, `MySQLCompiler`).
fn facade_for(dialect: SqlDialect) -> Result<VlorQl, Box<dyn Error>> {
    let dialect_name = match dialect {
        SqlDialect::Postgres => "postgres",
        SqlDialect::Sqlite => "sqlite",
        SqlDialect::MySql => "mysql",
    };
    Ok(VlorQl::builder()
        .with_schema(build_schema())
        .with_dialect_name(dialect_name)
        .with_policy(PolicyConfig::default())
        // No LLM client — `compile_only` doesn't need one.
        .build()?)
}

/// Runs the supplied plan through one facade and prints the produced SQL.
fn print_compiled(dialect: SqlDialect, plan: &QueryPlan) -> Result<CompiledQuery, Box<dyn Error>> {
    let vlorql = facade_for(dialect)?;
    let validated = vlorql.validate_only(plan)?;
    let compiled = vlorql.compile_only(&validated)?;
    println!("--- {dialect:?} ---");
    println!("sql:        {}", compiled.sql);
    println!("placeholders: {:?}", parameter_placeholders(&compiled));
    println!();
    Ok(compiled)
}

/// Renders each parameter's placeholder as it would appear in the SQL string.
/// Postgres numbers them `$1, $2, …`; SQLite/MySQL share a single `?` glyph.
fn parameter_placeholders(compiled: &CompiledQuery) -> Vec<String> {
    compiled
        .parameters
        .iter()
        .enumerate()
        .map(|(index, _parameter)| match compiled.dialect {
            SqlDialect::Postgres => format!("${}", index + 1),
            _ => "?".to_owned(),
        })
        .zip(compiled.parameters.iter().map(|p| p.value.clone()))
        .map(|(placeholder, value)| format!("{placeholder} = {value}"))
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let plan = canonical_plan();

    // Run the *same* `QueryPlan` through each compiler. Differences in
    // quoting, placeholder style, and limit/offset syntax appear here.
    for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::MySql] {
        print_compiled(dialect, &plan)?;
    }

    // As a sanity check, show that the compiled SQL has a different length
    // and shape per dialect. This is useful when you want to confirm in CI
    // that all compilers are wired up correctly.
    let compiled: Vec<(SqlDialect, String)> =
        [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::MySql]
            .into_iter()
            .map(|dialect| {
                let vlorql = facade_for(dialect).expect("facade");
                let validated = vlorql.validate_only(&plan).expect("validate");
                let compiled = vlorql.compile_only(&validated).expect("compile");
                (dialect, compiled.sql)
            })
            .collect();

    println!("--- summary ---");
    for (dialect, sql) in compiled {
        println!("{dialect:?}: {} chars", sql.len());
    }
    Ok(())
}
