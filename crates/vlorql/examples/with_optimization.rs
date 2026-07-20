//! Demonstrates the [`QueryOptimizer`] end-to-end.
//!
//! This example shows how to:
//!   1. Load a statistics file and configure a `VlorQl` facade with the
//!      optimizer enabled.
//!   2. Validate and compile a plan both **without** and **with**
//!      optimisation, then print the SQL side-by-side.
//!   3. Observe the estimated cost improvement from constant folding,
//!      predicate pushdown, column pruning, and join reordering.
//!
//! Run it with:
//!   cargo run --example with_optimization --quiet
//!
//! Set `STATS_FILE` to a JSON or YAML statistics file, or leave it
//! unset to use the built-in `DummyStatisticsProvider` (no effect on
//! join reordering).

use std::error::Error;
use std::sync::Arc;

use vlorql::VlorQl;
use vlorql_core::optimizer::QueryOptimizer;
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    BinaryOperator, ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, JoinClause,
    JoinType, Predicate, Projection, QueryPlan, SchemaMetadata, SchemaSnapshot, SqlDialect,
    TableSchema,
};
use vlorql_core::statistics::{
    ConfigFileStatisticsProvider, DummyStatisticsProvider, StatisticsProvider,
};
use vlorql_llm::MockLlmClient;

/// Builds a `users` + `orders` + `products` schema.
fn build_schema() -> Arc<SchemaSnapshot> {
    Arc::new(SchemaSnapshot::new(
        vec![
            TableSchema {
                name: "users".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("User identifier".to_owned()),
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "name".to_owned(),
                        data_type: DataType::String,
                        nullable: false,
                        description: Some("User display name".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: Some("Application users".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
            TableSchema {
                name: "orders".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("Order identifier".to_owned()),
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "user_id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("FK to users.id".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "total".to_owned(),
                        data_type: DataType::Float,
                        nullable: false,
                        description: Some("Order total amount".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: Some("Customer orders".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
            TableSchema {
                name: "products".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("Product identifier".to_owned()),
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "price".to_owned(),
                        data_type: DataType::Float,
                        nullable: false,
                        description: Some("Product unit price".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: Some("Product catalog".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
        ],
        SchemaMetadata::default(),
    ))
}

/// A query plan that exercises constant folding, joins, and a filter.
/// `SELECT users.name, orders.total + 0.0 AS total
///  FROM users
///  JOIN orders ON users.id = orders.user_id
///  WHERE orders.total > 100 + 50`
fn build_test_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::BinaryOp {
                    left: Box::new(Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "total".to_owned(),
                    }),
                    op: BinaryOperator::Add,
                    right: Box::new(Expression::Literal {
                        value: serde_json::json!(0.0),
                        data_type: DataType::Float,
                    }),
                },
                alias: Some("total".to_owned()),
            },
        ],
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: Expression::BinaryOp {
                left: Box::new(Expression::Literal {
                    value: serde_json::json!(100),
                    data_type: DataType::Int,
                }),
                op: BinaryOperator::Add,
                right: Box::new(Expression::Literal {
                    value: serde_json::json!(50),
                    data_type: DataType::Int,
                }),
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
                table: "orders".to_owned(),
                alias: None,
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "user_id".to_owned(),
                },
            },
        }]),
        ctes: None,
    }
}

/// Selects a statistics provider:
///   * If `STATS_FILE` environment variable points to a JSON/YAML file,
///     load it with `ConfigFileStatisticsProvider`.
///   * Otherwise fall back to `DummyStatisticsProvider` (no join reordering).
fn select_stats_provider() -> Arc<dyn StatisticsProvider> {
    if let Ok(path) = std::env::var("STATS_FILE") {
        match ConfigFileStatisticsProvider::load(&path) {
            Ok(provider) => {
                eprintln!("[with_optimization] loaded stats from {path}");
                return Arc::new(provider);
            }
            Err(e) => {
                eprintln!("[with_optimization] warning: failed to load {path}: {e}");
            }
        }
    }
    eprintln!("[with_optimization] using DummyStatisticsProvider (no join reordering)");
    Arc::new(DummyStatisticsProvider::default())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let schema = build_schema();
    let plan = build_test_plan();
    let stats_provider = select_stats_provider();

    // 1. Build a facade **without** the optimizer.
    let vlorql_no_opt = VlorQl::builder()
        .with_schema(Arc::clone(&schema))
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .with_llm_client(MockLlmClient::success(plan.clone()))
        .with_max_retries(0)
        .build()?;

    // 2. Build a facade **with** the optimizer.
    let vlorql_opt = VlorQl::builder()
        .with_schema(schema)
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .with_llm_client(MockLlmClient::success(plan.clone()))
        .with_statistics_provider(stats_provider)
        .with_max_retries(0)
        .build()?;

    // 3. Compile without optimisation.
    let validated = vlorql_no_opt.validate_only(&plan)?;
    let compiled_no_opt = vlorql_no_opt.compile_only(&validated)?;

    // 4. Compile with optimisation.
    let optimized = vlorql_opt.validate_and_optimize(&plan).await?;
    let compiled_opt = vlorql_opt.compile_only(optimized.as_validated())?;

    // 5. Print side-by-side.
    println!("=== SQL without optimisation ===");
    println!("{}", compiled_no_opt.sql);
    println!();
    println!("=== SQL with optimisation ===");
    println!("{}", compiled_opt.sql);
    println!();

    // 6. Show the estimated cost if a real stats provider is available.
    //    We can reconstruct the optimizer from the VlorQl facade by
    //    building it directly.
    if std::env::var("STATS_FILE").is_ok() {
        let opt = QueryOptimizer::new(select_stats_provider());
        if let Some(before) = opt.estimated_cost(&plan).await {
            // Re-optimize to get the optimised plan cost.
            let optimized_plan = opt.optimize_async(&plan).await?;
            if let Some(after) = opt.estimated_cost(&optimized_plan).await {
                let improvement = (1.0 - after.total() / before.total()) * 100.0;
                println!(
                    "Cost: before={:.1}, after={:.1}, improvement={:.0}%",
                    before.total(),
                    after.total(),
                    improvement,
                );
            }
        }
    } else {
        println!("(no STATS_FILE set — cost comparison skipped)");
    }

    println!();
    println!("dialect: {:?}", SqlDialect::Postgres);

    Ok(())
}
