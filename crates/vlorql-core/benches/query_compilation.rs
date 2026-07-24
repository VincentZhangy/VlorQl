//! Benchmarks for `QueryBuilder::build` on intentionally large plans.
//!
//! The plan exercises every clause VlorQl currently compiles:
//!   * three nested CTEs, two of which chain into one another,
//!   * four joins mixing INNER / LEFT / RIGHT / FULL,
//!   * a `WHERE` predicate that combines `AND`, `OR`, `IN`, `BETWEEN`, and `LIKE`,
//!   * a multi-column `GROUP BY`,
//!   * a `HAVING` clause with a function call,
//!   * `ORDER BY` with both directions,
//!   * `LIMIT` and `OFFSET`.
//!
//! Each iteration is a fresh `QueryBuilder` so the measured time covers string
//! allocation, placeholder numbering, and identifier quoting.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use serde_json::json;
use std::sync::Arc;
use vlorql_core::compile::{DialectConfig, QueryBuilder};
use vlorql_core::schema::{
    BinaryOperator, CommonTableExpression, ComparisonOperator, DataType, Expression, FromClause,
    InTarget, JoinClause, JoinType, OrderByTerm, Predicate, Projection,
    QueryPlan, SqlDialect,
};
use vlorql_core::validate::ValidatedPlan;

fn column_ref(table: &str, column: &str) -> Expression {
    Expression::ColumnRef {
        table: Some(table.to_owned()),
        column: column.to_owned(),
    }
}

fn literal_int(value: i64) -> Expression {
    Expression::Literal {
        value: json!(value),
        data_type: DataType::Int,
    }
}

fn literal_str(value: &str) -> Expression {
    Expression::Literal {
        value: json!(value),
        data_type: DataType::String,
    }
}

fn count_of(table: &str, column: &str) -> Expression {
    Expression::FunctionCall {
        name: "COUNT".to_owned(),
        args: vec![column_ref(table, column)],
        distinct: false,
    }
}

/// Builds the inner-most CTE: a tiny two-column projection with a predicate.
fn leaf_cte() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: Some("order_id".to_owned()),
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "customer_id".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Comparison {
            left: column_ref("orders", "status"),
            op: ComparisonOperator::Eq,
            right: literal_str("paid"),
        }),
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

/// Builds a CTE that joins `orders` and `customers`.
fn orders_with_customers_cte() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("o".to_owned()),
                column: "order_id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("c".to_owned()),
                column: "name".to_owned(),
                alias: Some("customer_name".to_owned()),
            },
            Projection::Column {
                table: Some("c".to_owned()),
                column: "region".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "paid_orders".to_owned(),
            alias: Some("o".to_owned()),
        },
        r#where: Some(Predicate::Comparison {
            left: column_ref("c", "active"),
            op: ComparisonOperator::Eq,
            right: Expression::Literal {
                value: json!(true),
                data_type: DataType::Boolean,
            },
        }),
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "customers".to_owned(),
                alias: Some("c".to_owned()),
            },
            on: Predicate::Comparison {
                left: column_ref("o", "customer_id"),
                op: ComparisonOperator::Eq,
                right: column_ref("c", "id"),
            },
        }]),
        ctes: Some(vec![CommonTableExpression {
            name: "paid_orders".to_owned(),
            recursive: false,
            query: Box::new(leaf_cte()),
        }]),
        distinct: false,
        distinct_on: None,
        set_operation: None,
    }
}

/// Builds the full plan: 3 CTEs, 4 joins, complex WHERE, GROUP BY, HAVING, ORDER BY.
fn build_complex_plan() -> ValidatedPlan {
    let plan = QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("r".to_owned()),
                column: "region".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::BinaryOp {
                    left: Box::new(count_of("r", "order_id")),
                    op: BinaryOperator::Add,
                    right: Box::new(literal_int(0)),
                },
                alias: Some("orders_count".to_owned()),
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: vec![column_ref("p", "amount")],
                    distinct: true,
                },
                alias: Some("revenue".to_owned()),
            },
        ],
        from: FromClause {
            table: "regional_orders".to_owned(),
            alias: Some("r".to_owned()),
        },
        r#where: Some(Predicate::And {
            left: Box::new(Predicate::Or {
                left: Box::new(Predicate::In {
                    expr: column_ref("r", "region"),
                    target: InTarget::Values(vec![
                        literal_str("us"),
                        literal_str("eu"),
                        literal_str("ap"),
                    ]),
                }),
                right: Box::new(Predicate::Between {
                    expr: column_ref("r", "order_id"),
                    low: literal_int(1000),
                    high: literal_int(999_999),
                }),
            }),
            right: Box::new(Predicate::And {
                left: Box::new(Predicate::Like {
                    expr: column_ref("r", "customer_name"),
                    pattern: "A%".to_owned(),
                }),
                right: Box::new(Predicate::IsNull {
                    expr: column_ref("s", "refunded_at"),
                }),
            }),
        }),
        group_by: Some(vec![
            column_ref("r", "region"),
            column_ref("r", "customer_name"),
            column_ref("s", "tier"),
        ]),
        having: Some(Predicate::Comparison {
            left: count_of("r", "order_id"),
            op: ComparisonOperator::Gt,
            right: literal_int(5),
        }),
        order_by: Some(vec![
            OrderByTerm {
                expr: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: vec![column_ref("p", "amount")],
                    distinct: true,
                },
                descending: true,
            },
            OrderByTerm {
                expr: column_ref("r", "region"),
                descending: false,
            },
        ]),
        limit: Some(50),
        offset: Some(100),
        joins: Some(vec![
            JoinClause {
                join_type: JoinType::Left,
                right_table: FromClause {
                    table: "payments".to_owned(),
                    alias: Some("p".to_owned()),
                },
                on: Predicate::Comparison {
                    left: column_ref("r", "order_id"),
                    op: ComparisonOperator::Eq,
                    right: column_ref("p", "order_id"),
                },
            },
            JoinClause {
                join_type: JoinType::Right,
                right_table: FromClause {
                    table: "subscriptions".to_owned(),
                    alias: Some("s".to_owned()),
                },
                on: Predicate::Comparison {
                    left: column_ref("r", "customer_name"),
                    op: ComparisonOperator::Eq,
                    right: column_ref("s", "customer_name"),
                },
            },
            JoinClause {
                join_type: JoinType::Full,
                right_table: FromClause {
                    table: "tiers".to_owned(),
                    alias: Some("t".to_owned()),
                },
                on: Predicate::Comparison {
                    left: column_ref("s", "tier"),
                    op: ComparisonOperator::Eq,
                    right: column_ref("t", "name"),
                },
            },
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "addresses".to_owned(),
                    alias: Some("a".to_owned()),
                },
                on: Predicate::Comparison {
                    left: column_ref("r", "customer_name"),
                    op: ComparisonOperator::Eq,
                    right: column_ref("a", "customer_name"),
                },
            },
        ]),
        ctes: Some(vec![
            CommonTableExpression {
                name: "regional_orders".to_owned(),
                recursive: false,
                query: Box::new(orders_with_customers_cte()),
            },
            CommonTableExpression {
                name: "high_value".to_owned(),
                recursive: false,
                query: Box::new(QueryPlan {
                    select: vec![Projection::Column {
                        table: Some("orders".to_owned()),
                        column: "id".to_owned(),
                        alias: Some("order_id".to_owned()),
                    }],
                    from: FromClause {
                        table: "orders".to_owned(),
                        alias: None,
                    },
                    r#where: Some(Predicate::Comparison {
                        left: column_ref("orders", "total"),
                        op: ComparisonOperator::Gt,
                        right: literal_int(10_000),
                    }),
                    group_by: None,
                    having: None,
                    order_by: None,
                    limit: None,
                    offset: None,
                    joins: None,
                    ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,                }),
            },
            CommonTableExpression {
                name: "active_customers".to_owned(),
                recursive: false,
                query: Box::new(QueryPlan {
                    select: vec![Projection::Column {
                        table: Some("customers".to_owned()),
                        column: "id".to_owned(),
                        alias: Some("customer_id".to_owned()),
                    }],
                    from: FromClause {
                        table: "customers".to_owned(),
                        alias: None,
                    },
                    r#where: Some(Predicate::IsNull {
                        expr: column_ref("customers", "deleted_at"),
                    }),
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
                }),
            },
        ]),
        distinct: false,
        distinct_on: None,
        set_operation: None,
    };

    ValidatedPlan(Arc::new(plan))
}

fn bench_query_build(c: &mut Criterion) {
    let plan = build_complex_plan();
    let mut group = c.benchmark_group("query_build");

    for name in ["postgres", "sqlite"] {
        let config = match name {
            "postgres" => DialectConfig::default_postgres(),
            _ => DialectConfig::default_sqlite(),
        };
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &config,
            |bencher, config| {
                bencher.iter(|| {
                    let result = QueryBuilder::new(
                        criterion::black_box(&plan),
                        config,
                    )
                    .build();
                    criterion::black_box(result.expect("complex plan should compile"))
                })
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_query_build);
criterion_main!(benches);
