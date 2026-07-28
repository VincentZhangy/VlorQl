//! Benchmarks for `ValidationPipeline::validate` against large schemas.
//!
//! Scenario: a 1000-table × 50-column snapshot where the validator must look up
//! 5 tables and 10 columns on the hot path. The target p100 wall-clock is well
//! under 10 ms — large enough to make `cargo bench` regressions easy to spot
//! but tight enough to catch accidental O(n²) traversals.
//!
//! A second benchmark exercises the same pipeline with audit-stage (SQL-injection
//! detection) enabled, which adds per-identifier checks against known suspicious
//! patterns.

use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::json;
use std::sync::Arc;
use vlorql_core::policy::{PolicyConfig, PolicyEngine};
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, DialectProfile, Expression, FromClause, JoinClause,
    JoinType, Predicate, Projection, QueryPlan, SchemaMetadata, SchemaSnapshot, SqlDialect,
    TableSchema,
};
use vlorql_core::validate::ValidationPipeline;

const TABLE_COUNT: usize = 1000;
const COLUMNS_PER_TABLE: usize = 50;
const REFERENCED_TABLES: usize = 5;
const REFERENCED_COLUMNS: usize = 10;

/// Builds a snapshot with `TABLE_COUNT` tables of `COLUMNS_PER_TABLE` columns each.
fn build_large_snapshot() -> Arc<SchemaSnapshot> {
    let mut tables = Vec::with_capacity(TABLE_COUNT);
    for table_index in 0..TABLE_COUNT {
        let mut columns = Vec::with_capacity(COLUMNS_PER_TABLE);
        for column_index in 0..COLUMNS_PER_TABLE {
            columns.push(ColumnSchema {
                name: format!("col_{column_index}"),
                data_type: DataType::String,
                nullable: true,
                description: None,
                is_primary_key: column_index == 0,
                foreign_key: None,
            });
        }
        tables.push(TableSchema {
            name: format!("table_{table_index:04}"),
            columns,
            description: Some(format!("Synthetic table #{table_index}")),
            primary_key: Some(vec!["col_0".to_owned()]),
        });
    }

    Arc::new(SchemaSnapshot::new(tables, SchemaMetadata::default()))
}

/// Builds a plan that exercises `REFERENCED_TABLES` and `REFERENCED_COLUMNS`.
fn build_query_plan() -> QueryPlan {
    let mut select = Vec::with_capacity(REFERENCED_COLUMNS);
    for table_index in 0..REFERENCED_TABLES {
        for column_index in 0..(REFERENCED_COLUMNS / REFERENCED_TABLES) {
            select.push(Projection::Column {
                table: Some(format!("table_{table_index:04}")),
                column: format!("col_{column_index}"),
                alias: None,
            });
        }
    }

    let from_table = "table_0000".to_owned();
    let joins: Vec<JoinClause> = (1..REFERENCED_TABLES)
        .map(|i| {
            let right_table = format!("table_{i:04}");
            let right_alias = format!("t{i}");
            let on = Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("table_0000".to_owned()),
                    column: "col_0".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some(right_alias.clone()),
                    column: "col_0".to_owned(),
                },
            };
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause::table(right_table, Some(right_alias)),
                on,
            }
        })
        .collect();

    QueryPlan {
        select,
        from: FromClause::table(from_table, Some("t0".to_owned())),
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: Some(100),
        offset: None,
        joins: Some(joins),
        ctes: None,
        distinct: false,
        distinct_on: None,
        set_operation: None,
    }
}

fn bench_validate_large_schema(c: &mut Criterion) {
    let schema = build_large_snapshot();
    let dialect = DialectProfile {
        dialect: SqlDialect::Postgres,
        ..DialectProfile::default()
    };
    let pipeline =
        ValidationPipeline::new(schema, dialect, PolicyEngine::new(PolicyConfig::default()));
    let plan = build_query_plan();

    c.bench_function("validate/1000_tables_50_cols", |bencher| {
        bencher.iter(|| {
            let result = pipeline.validate(&plan);
            criterion::black_box(result.expect("plan should validate"))
        })
    });
}

/// Benchmarks validation with the SQL-injection audit stage enabled.
/// The audit stage inspects every identifier for suspicious patterns (; -- /* etc.)
/// on top of the regular schema validation.
fn bench_validate_with_audit(c: &mut Criterion) {
    let schema = build_large_snapshot();
    let dialect = DialectProfile {
        dialect: SqlDialect::Postgres,
        ..DialectProfile::default()
    };
    let pipeline =
        ValidationPipeline::new(schema, dialect, PolicyEngine::new(PolicyConfig::default()))
            .with_audit(true);
    let plan = build_query_plan();

    c.bench_function("validate/1000_tables_50_cols_with_audit", |bencher| {
        bencher.iter(|| {
            let result = pipeline.validate(&plan);
            criterion::black_box(result.expect("plan should validate with audit"))
        })
    });
}

/// Builds a snapshot with a dedicated table containing Decimal columns.
fn build_decimal_schema_snapshot() -> Arc<SchemaSnapshot> {
    let mut tables: Vec<TableSchema> = (0..10)
        .map(|i| {
            let columns = vec![
                ColumnSchema {
                    name: "id".to_owned(),
                    data_type: DataType::Uuid,
                    nullable: false,
                    description: None,
                    is_primary_key: true,
                    foreign_key: None,
                },
                ColumnSchema {
                    name: "amount".to_owned(),
                    data_type: DataType::Decimal,
                    nullable: false,
                    description: Some("Monetary amount".to_owned()),
                    is_primary_key: false,
                    foreign_key: None,
                },
                ColumnSchema {
                    name: "rate".to_owned(),
                    data_type: DataType::Decimal,
                    nullable: true,
                    description: Some("Exchange rate".to_owned()),
                    is_primary_key: false,
                    foreign_key: None,
                },
                ColumnSchema {
                    name: "description".to_owned(),
                    data_type: DataType::String,
                    nullable: true,
                    description: None,
                    is_primary_key: false,
                    foreign_key: None,
                },
            ];
            TableSchema {
                name: format!("financials_{i:02}"),
                columns,
                description: Some(format!("Financial table #{i}")),
                primary_key: Some(vec!["id".to_owned()]),
            }
        })
        .collect();

    // Add one large table to keep the snapshot realistic in size.
    let mut large_cols = Vec::with_capacity(COLUMNS_PER_TABLE);
    for j in 0..COLUMNS_PER_TABLE {
        large_cols.push(ColumnSchema {
            name: format!("col_{j}"),
            data_type: if j % 3 == 0 {
                DataType::Decimal
            } else {
                DataType::String
            },
            nullable: true,
            description: None,
            is_primary_key: false,
            foreign_key: None,
        });
    }
    tables.push(TableSchema {
        name: "large_financials".to_owned(),
        columns: large_cols,
        description: Some("Large financial table with mixed Decimal/String columns".to_owned()),
        primary_key: None,
    });

    Arc::new(SchemaSnapshot::new(tables, SchemaMetadata::default()))
}

/// Builds a plan that references Decimal columns along with regular ones.
fn build_decimal_query_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("f00".to_owned()),
                column: "amount".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("f00".to_owned()),
                column: "rate".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("f01".to_owned()),
                column: "amount".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::Literal {
                    value: json!(99.99),
                    data_type: DataType::Decimal,
                },
                alias: Some("computed".to_owned()),
            },
        ],
        from: FromClause::table("financials_00".to_owned(), Some("f00".to_owned())),
        r#where: Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("f00".to_owned()),
                column: "amount".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: Expression::Literal {
                value: json!(0),
                data_type: DataType::Decimal,
            },
        }),
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause::table("financials_01".to_owned(), Some("f01".to_owned())),
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("f00".to_owned()),
                    column: "amount".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("f01".to_owned()),
                    column: "amount".to_owned(),
                },
            },
        }]),
        ctes: None,
        distinct: false,
        distinct_on: None,
        set_operation: None,
    }
}

fn bench_validate_decimal_schema(c: &mut Criterion) {
    let schema = build_decimal_schema_snapshot();
    let dialect = DialectProfile {
        dialect: SqlDialect::Postgres,
        ..DialectProfile::default()
    };
    let pipeline =
        ValidationPipeline::new(schema, dialect, PolicyEngine::new(PolicyConfig::default()));
    let plan = build_decimal_query_plan();

    c.bench_function("validate/decimal_columns", |bencher| {
        bencher.iter(|| {
            let result = pipeline.validate(&plan);
            criterion::black_box(result.expect("plan with Decimal columns should validate"))
        })
    });
}

criterion_group!(
    benches,
    bench_validate_large_schema,
    bench_validate_with_audit,
    bench_validate_decimal_schema,
);
criterion_main!(benches);
