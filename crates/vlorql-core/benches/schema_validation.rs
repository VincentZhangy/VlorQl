//! Benchmarks for `ValidationPipeline::validate` against large schemas.
//!
//! Scenario: a 1000-table × 50-column snapshot where the validator must look up
//! 5 tables and 10 columns on the hot path. The target p100 wall-clock is well
//! under 10 ms — large enough to make `cargo bench` regressions easy to spot
//! but tight enough to catch accidental O(n²) traversals.

use criterion::{Criterion, criterion_group, criterion_main};
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
                right_table: FromClause {
                    table: right_table,
                    alias: Some(right_alias),
                },
                on,
            }
        })
        .collect();

    QueryPlan {
        select,
        from: FromClause {
            table: from_table,
            alias: Some("t0".to_owned()),
        },
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
            set_operation: None,    }
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

criterion_group!(benches, bench_validate_large_schema);
criterion_main!(benches);
