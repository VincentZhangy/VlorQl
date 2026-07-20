//! End-to-end VlorQl query with OpenTelemetry observability.
//!
//! This example shows how to:
//!   1. Initialise OTLP telemetry (traces + metrics) with
//!      [`init_telemetry`](vlorql_core::observability::init_telemetry).
//!   2. Build a `VlorQl` facade with a `VlorqMetrics` handle.
//!   3. Run a query so that spans and metrics are exported via OTLP.
//!   4. Gracefully shut down the telemetry exporters.
//!
//! Prerequisites:
//!   docker compose -f docker-compose.observability.yml up -d
//!
//! Then run:
//!   cargo run --example with_observability --quiet
//!
//! Open Jaeger UI at http://localhost:16686, select service
//! "vlorql-example", and click "Find Traces" to see the span tree.
//! Open Prometheus at http://localhost:9090 to query metrics.

use std::error::Error;
use std::sync::Arc;

use vlorql::VlorQl;
use vlorql_core::observability::{VlorqMetrics, init_telemetry, shutdown_telemetry};
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{ColumnSchema, DataType, SchemaMetadata, SchemaSnapshot, TableSchema};
use vlorql_llm::{LlmConfig, LlmProvider, MockLlmClient};

/// Builds a minimal `users` schema.
fn build_schema() -> Arc<SchemaSnapshot> {
    Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
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
                    name: "email".to_owned(),
                    data_type: DataType::String,
                    nullable: false,
                    description: Some("Primary email address".to_owned()),
                    is_primary_key: false,
                    foreign_key: None,
                },
            ],
            description: Some("Application users".to_owned()),
            primary_key: Some(vec!["id".to_owned()]),
        }],
        SchemaMetadata::default(),
    ))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 1. Initialise OTLP telemetry.  By default the exporter connects
    //    to http://localhost:4317 (the Jaeger endpoint from the Docker
    //    Compose file).  Override with the OTEL_EXPORTER_OTLP_ENDPOINT
    //    environment variable.
    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_owned());
    let _guard = init_telemetry("vlorql-example", &otlp_endpoint)?;

    // 2. Create a metrics handle that uses the global meter.
    let metrics = Arc::new(VlorqMetrics::new());

    // 3. Build a VlorQl facade with a mock LLM client (no real API key needed).
    let vlorql = VlorQl::builder()
        .with_schema(build_schema())
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .with_llm_config(LlmConfig {
            provider: LlmProvider::Ollama,
            model: "mock-model".to_owned(),
            ..LlmConfig::default()
        })
        .with_llm_client(MockLlmClient::success({
            // A simple SELECT id, email FROM users WHERE id > 10
            use vlorql_core::schema::{
                ComparisonOperator, Expression, FromClause, Predicate, Projection, QueryPlan,
            };
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
                        value: serde_json::json!(10),
                        data_type: DataType::Int,
                    },
                }),
                group_by: None,
                having: None,
                order_by: None,
                limit: None,
                offset: None,
                joins: None,
                ctes: None,
            }
        }))
        .with_metrics(metrics)
        .with_max_retries(0)
        .build()?;

    // 4. Run a query.  The spans and metrics are exported via OTLP.
    println!("Sending query…");
    let compiled = vlorql.query("Show me active users with id > 10").await?;
    println!("SQL:     {}", compiled.sql);
    if !compiled.parameters.is_empty() {
        println!("Params:");
        for (i, param) in compiled.parameters.iter().enumerate() {
            println!("  ${}: {} ({:?})", i + 1, param.value, param.data_type);
        }
    }

    // 5. Shut down telemetry.  This flushes any remaining spans and
    //    metrics before the process exits.
    println!("Shutting down telemetry…");
    shutdown_telemetry(_guard);
    println!("Done.  Open http://localhost:16686 to view the trace.");

    Ok(())
}
