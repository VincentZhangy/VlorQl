//! Basic `VlorQl` end-to-end query.
//!
//! This example shows how to:
//!   1. Build a `SchemaSnapshot` describing the database the LLM is allowed
//!      to query.
//!   2. Construct a `VlorQl` facade via `VlorQl::builder()`, choosing either
//!      a deterministic `MockLlmClient` (the default — safe for CI / docs)
//!      or a real OpenAI-compatible client when the `OPENAI_API_KEY`
//!      environment variable is set.
//!   3. Call `vlorql.query("...").await`, which:
//!        * builds the system prompt from the schema/policy/dialect,
//!        * invokes the LLM to obtain a structured `QueryPlan`,
//!        * runs the validator + retry loop,
//!        * compiles the validated plan to parameterized SQL,
//!          and returns a `CompiledQuery`.
//!
//! Run it with:
//!   cargo run --example basic_query --quiet
//!
//! Switch on a live LLM call by exporting `OPENAI_API_KEY` (and optionally
//! `OPENAI_MODEL` / `OPENAI_API_BASE`) before running.

use std::error::Error;
use std::sync::Arc;

use vlorql::{SchemaSnapshot, SqlDialect, VlorQl};
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection,
    QueryPlan, SchemaMetadata, TableSchema,
};
use vlorql_llm::{LlmClient, LlmConfig, LlmProvider, MockLlmClient, OpenAIClient};

/// Builds a minimal `users` schema. Two columns (`id`, `email`) and a primary
/// key are enough for the demo.
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

/// Returns the canned `QueryPlan` the `MockLlmClient` will return.
/// `SELECT id, email FROM users WHERE id > $1`.
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
            distinct: false,
            distinct_on: None,
            set_operation: None,    }
}

/// Picks an LLM client based on the runtime environment.
///
/// * If `OPENAI_API_KEY` is set, build a real `OpenAIClient` against the
///   OpenAI public endpoint (or `OPENAI_API_BASE` if provided, which is how
///   you point this at DeepSeek, Zhipu, vLLM, etc.).
/// * Otherwise fall back to a deterministic `MockLlmClient` so the example
///   always produces output — useful for `cargo test` and CI.
fn select_llm_client() -> Box<dyn LlmClient> {
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY")
        && !api_key.trim().is_empty()
    {
        // Optional overrides — both fall back to OpenAI defaults.
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());
        let api_base = std::env::var("OPENAI_API_BASE").ok();

        let config = LlmConfig {
            provider: LlmProvider::OpenAi,
            api_key: Some(api_key),
            api_base,
            model,
            ..LlmConfig::default()
        };
        println!("[basic_query] using OpenAI-compatible client: {config:?}");
        return Box::new(OpenAIClient::from_config(config));
    }

    println!("[basic_query] OPENAI_API_KEY not set; using MockLlmClient");
    Box::new(MockLlmClient::success(sample_plan()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 1. Build the schema, then the facade. `with_dialect_name("postgres")`
    //    picks the default `PostgresCompiler`; swap the name to "sqlite" or
    //    "mysql" to use a different built-in compiler.
    let schema = build_schema();
    let vlorql = VlorQl::builder()
        .with_schema(schema)
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .with_llm_client(select_llm_client())
        .with_max_retries(2)
        .build()?;

    // 2. Send a natural-language question. Inside, `VlorQl::query` will:
    //      a. Build the system prompt from schema + dialect + policy.
    //      b. Call the LLM (mock or real).
    //      c. Validate the returned plan (schema + policy + operand + dialect).
    //      d. Retry up to `with_max_retries` times on validation errors that
    //         the LLM can plausibly correct.
    //      e. Compile the validated plan with the configured compiler.
    let compiled = vlorql.query("List users with id greater than 10").await?;

    // 3. Print the result.
    println!("dialect: {:?}", SqlDialect::Postgres);
    println!("sql:     {}", compiled.sql);
    if compiled.parameters.is_empty() {
        println!("parameters: []");
    } else {
        println!("parameters:");
        for (index, parameter) in compiled.parameters.iter().enumerate() {
            println!(
                "  ${}: value={}, type={:?}",
                index + 1,
                parameter.value,
                parameter.data_type
            );
        }
    }

    Ok(())
}
