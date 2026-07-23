//! Streaming `VlorQl::query_stream` end-to-end.
//!
//! This example prints each text delta the LLM emits in real time, then takes
//! the assembled `QueryPlan`, validates it, and renders the final SQL. We
//! use a small in-memory `ChunkyMock` so the example runs offline without any
//! API key, while still exercising the streaming code path.
//!
//! Run with:
//!   cargo run --example stream_query --quiet

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use futures::stream::{Stream, StreamExt};
use vlorql::{CompiledQuery, SchemaSnapshot, SqlDialect, StreamEvent, VlorQl};
use vlorql_core::errors::VlorQLError;
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection,
    QueryPlan, SchemaMetadata, TableSchema,
};
use vlorql_llm::{LlmClient, LlmConfig, LlmProvider};

/// A minimal LLM client that emits a precomputed plan as a series of small
/// fragments. It implements the full `LlmClient` trait so we can exercise
/// `query_stream` without a real provider.
struct ChunkyMock {
    plan: QueryPlan,
    chunks: Vec<String>,
    counter: Arc<AtomicUsize>,
    config: LlmConfig,
}

impl ChunkyMock {
    fn new(plan: QueryPlan, chunks: Vec<String>) -> Self {
        let config = LlmConfig {
            provider: LlmProvider::OpenAi,
            model: "mock".to_owned(),
            ..LlmConfig::default()
        };
        Self {
            plan,
            chunks,
            counter: Arc::new(AtomicUsize::new(0)),
            config,
        }
    }
}

#[async_trait::async_trait]
impl LlmClient for ChunkyMock {
    async fn generate_plan(
        &self,
        _question: &str,
        _system_prompt: &str,
    ) -> Result<QueryPlan, VlorQLError> {
        Ok(self.plan.clone())
    }

    async fn stream_plan(
        &self,
        _question: String,
        _system_prompt: String,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        let chunks = self.chunks.clone();
        let counter = Arc::clone(&self.counter);
        let stream = futures::stream::unfold(
            (chunks.into_iter(), counter),
            |(mut iter, counter)| async move {
                if let Some(chunk) = iter.next() {
                    counter.fetch_add(1, Ordering::SeqCst);
                    // Tiny delay so the streaming nature is visible on stdout.
                    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                    Some((Ok::<String, VlorQLError>(chunk), (iter, counter)))
                } else {
                    None
                }
            },
        );
        Ok(Box::new(Box::pin(stream)))
    }

    fn provider(&self) -> LlmProvider {
        self.config.provider
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
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
                    description: Some("User identifier".to_owned()),
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
            description: Some("Application users".to_owned()),
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
                value: serde_json::json!(100),
                data_type: DataType::Int,
            },
        }),
        group_by: None,
        having: None,
        order_by: None,
        limit: Some(50),
        offset: None,
        joins: None,
        ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Build the facade. Note we use a non-default LLM client — the chunky
    // mock — so the streaming pipeline is fully exercised offline.
    let schema = build_schema();
    let plan = sample_plan();
    let streamed_json = serde_json::to_string(&plan)?;

    // Split the JSON plan into three intentionally awkward chunks so the
    // consumer has to reassemble partial JSON across boundaries.
    let (a, rest) = streamed_json.split_at(40);
    let (b, c) = rest.split_at(40);

    let vlorql = VlorQl::builder()
        .with_schema(schema)
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .with_llm_client(ChunkyMock::new(
            plan.clone(),
            vec![a.to_owned(), b.to_owned(), c.to_owned()],
        ))
        .build()?;

    // Drive `query_stream` and print each delta as it arrives. The facade
    // emits `StreamEvent::TextChunk` for raw deltas and a single
    // `StreamEvent::PlanComplete` once the assembled JSON parses cleanly.
    println!("--- streaming plan ---");
    let mut stream = vlorql.query_stream("List users with id > 100").await?;
    let mut chunk_count = 0usize;
    let mut final_plan: Option<QueryPlan> = None;

    while let Some(item) = stream.next().await {
        match item? {
            StreamEvent::TextChunk(chunk) => {
                chunk_count += 1;
                print!("[chunk {chunk_count:02}] {chunk}");
            }
            StreamEvent::PlanComplete(plan) => {
                println!();
                final_plan = Some(*plan);
            }
            StreamEvent::Error(error) => return Err(error.into()),
        }
    }

    println!("\n--- assembled plan ---");
    let plan = final_plan.ok_or_else(|| anyhow!("stream ended without a plan"))?;
    println!("{plan:#?}");

    // Once the plan is fully assembled we still need to validate + compile it
    // explicitly. `query_stream` only emits the validated plan; if you want
    // the SQL, ask the facade.
    let validated = vlorql.validate_only(&plan)?;
    let compiled: CompiledQuery = vlorql.compile_only(&validated)?;
    println!("--- compiled SQL ---");
    println!("dialect: {:?}", SqlDialect::Postgres);
    println!("sql:     {}", compiled.sql);
    println!("parameters: {}", compiled.parameters.len());
    Ok(())
}
