//! Throughput benchmark: validate + compile under concurrent load.
//!
//! LLM calls are mocked out (they dominate wall-clock in production); we drive
//! only the validate → compile path that the LLM response would feed into.
//! Each "request" runs [`ValidationPipeline::validate`] followed by
//! [`QueryBuilder::build`] for the requested dialect.
//!
//! `criterion::Throughput::Elements(N)` makes `cargo bench` report
//! `elements/s`, which equals QPS for one element per request.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::future::join_all;
use serde_json::json;
use std::sync::Arc;
use vlorql_core::compile::QueryBuilder;
use vlorql_core::policy::{PolicyConfig, PolicyEngine};
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, DialectProfile, Expression, FromClause,
    IdentifierQuoting, JoinClause, JoinType, Predicate, Projection, QueryPlan, SchemaMetadata,
    SchemaSnapshot, SqlDialect, TableSchema,
};
use vlorql_core::validate::ValidationPipeline;

const CONCURRENCY: usize = 100;

fn column(name: &str, data_type: DataType) -> ColumnSchema {
    ColumnSchema {
        name: name.to_owned(),
        data_type,
        nullable: true,
        description: None,
        is_primary_key: false,
        foreign_key: None,
    }
}

/// Builds a small but realistic snapshot — 6 tables with primary keys and a
/// couple of foreign keys. Bigger than this just inflates build time without
/// changing the throughput numbers we care about.
fn build_snapshot() -> Arc<SchemaSnapshot> {
    let tables = vec![
        TableSchema {
            name: "users".to_owned(),
            columns: vec![
                column("id", DataType::Uuid),
                column("email", DataType::String),
                column("tenant_id", DataType::String),
                column("active", DataType::Boolean),
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        },
        TableSchema {
            name: "accounts".to_owned(),
            columns: vec![
                column("id", DataType::Uuid),
                column("owner_id", DataType::Uuid),
                column("plan", DataType::String),
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        },
        TableSchema {
            name: "orders".to_owned(),
            columns: vec![
                column("id", DataType::Uuid),
                column("customer_id", DataType::Uuid),
                column("status", DataType::String),
                column("total", DataType::Float),
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        },
        TableSchema {
            name: "payments".to_owned(),
            columns: vec![
                column("id", DataType::Uuid),
                column("order_id", DataType::Uuid),
                column("amount", DataType::Float),
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        },
        TableSchema {
            name: "subscriptions".to_owned(),
            columns: vec![
                column("id", DataType::Uuid),
                column("customer_id", DataType::Uuid),
                column("tier", DataType::String),
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        },
        TableSchema {
            name: "addresses".to_owned(),
            columns: vec![
                column("id", DataType::Uuid),
                column("customer_id", DataType::Uuid),
                column("city", DataType::String),
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        },
    ];

    Arc::new(SchemaSnapshot::new(tables, SchemaMetadata::default()))
}

/// Builds the plan each concurrent request will run through.
fn build_query_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("u".to_owned()),
                column: "email".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "COUNT".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("o".to_owned()),
                        column: "id".to_owned(),
                    }],
                    distinct: false,
                },
                alias: Some("order_count".to_owned()),
            },
        ],
        from: FromClause {
            table: "users".to_owned(),
            alias: Some("u".to_owned()),
        },
        r#where: Some(Predicate::And {
            left: Box::new(Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("u".to_owned()),
                    column: "active".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: json!(true),
                    data_type: DataType::Boolean,
                },
            }),
            right: Box::new(Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("u".to_owned()),
                    column: "tenant_id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: json!("tenant-1"),
                    data_type: DataType::String,
                },
            }),
        }),
        group_by: Some(vec![Expression::ColumnRef {
            table: Some("u".to_owned()),
            column: "email".to_owned(),
        }]),
        having: None,
        order_by: None,
        limit: Some(50),
        offset: None,
        joins: Some(vec![JoinClause {
            join_type: JoinType::Left,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: Some("o".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("u".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("o".to_owned()),
                    column: "customer_id".to_owned(),
                },
            },
        }]),
        ctes: None,
    }
}

#[derive(Clone)]
struct Harness {
    pipeline: ValidationPipeline,
    plan: Arc<QueryPlan>,
    dialect: SqlDialect,
}

impl Harness {
    fn new(dialect: SqlDialect) -> Self {
        let schema = build_snapshot();
        let profile = DialectProfile {
            dialect,
            ..DialectProfile::default()
        };
        let pipeline =
            ValidationPipeline::new(schema, profile, PolicyEngine::new(PolicyConfig::default()));
        Self {
            pipeline,
            plan: Arc::new(build_query_plan()),
            dialect,
        }
    }

    /// Runs validate + compile exactly once. Used inside each concurrent task.
    fn run_once(&self) -> (String, usize) {
        let validated = self
            .pipeline
            .validate(&self.plan)
            .expect("plan should validate");
        let (sql, params) =
            QueryBuilder::new(&validated, self.dialect, IdentifierQuoting::DoubleQuote)
                .build()
                .expect("plan should compile");
        (sql, params.len())
    }
}

/// Benchmarks `CONCURRENCY` concurrent validate+compile requests against a
/// shared `Harness` instance. Reports QPS via `Throughput::Elements`.
fn bench_concurrent_throughput(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    let mut group = c.benchmark_group("concurrent_throughput");
    group.throughput(Throughput::Elements(CONCURRENCY as u64));

    for (label, dialect) in [
        ("postgres", SqlDialect::Postgres),
        ("sqlite", SqlDialect::Sqlite),
    ] {
        let harness = Harness::new(dialect);
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &harness,
            |bencher, harness| {
                bencher.iter(|| {
                    let harness = harness.clone();
                    let results: Vec<(String, usize)> = runtime.block_on(async move {
                        let tasks: Vec<_> = (0..CONCURRENCY)
                            .map(|_| {
                                let harness = harness.clone();
                                tokio::spawn(async move { harness.run_once() })
                            })
                            .collect();
                        join_all(tasks)
                            .await
                            .into_iter()
                            .map(|join_result| join_result.expect("task should not panic"))
                            .collect()
                    });
                    criterion::black_box(results)
                })
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_concurrent_throughput);
criterion_main!(benches);
