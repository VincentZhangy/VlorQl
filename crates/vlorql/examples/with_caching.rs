//! Demonstrates the full cache layer end-to-end.
//!
//! This example shows how to:
//!   1. Enable all three caches (schema, compile, prompt) on a `VlorQl`
//!      facade.
//!   2. Run the same query twice and observe the compile cache hit on
//!      the second call.
//!   3. Print cache statistics (size) after each query.
//!
//! Run it with:
//!   cargo run --example with_caching --quiet
//!
//! The example uses `MockLlmClient` so no real LLM call occurs.

use std::error::Error;
use std::sync::Arc;

use vlorql::VlorQl;
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    ColumnSchema, DataType, SchemaMetadata, SchemaSnapshot, SqlDialect, TableSchema,
};
use vlorql_llm::MockLlmClient;

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
        SchemaMetadata {
            version: Some("1.0".to_owned()),
            source: Some("example".to_owned()),
            generated_at: None,
        },
    ))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Build the facade with all three caches enabled.
    let vlorql = VlorQl::builder()
        .with_schema(build_schema())
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .with_llm_client(MockLlmClient::success(vlorql_core::schema::QueryPlan {
            select: vec![vlorql_core::schema::Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            }],
            from: vlorql_core::schema::FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: None,
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
        }))
        // Enable all three caches.
        .with_schema_cache(10, 3600)
        .with_compile_cache(1024, 60)
        .with_prompt_cache(50, 3600)
        .build()?;

    println!("=== First query (all caches cold) ===");
    let start = std::time::Instant::now();
    let result1 = vlorql.query("List all users").await?;
    let elapsed1 = start.elapsed();
    println!("sql: {}", result1.sql);
    println!("time: {elapsed1:?}");

    // Print cache sizes after first query.
    print_cache_stats(&vlorql);

    println!();
    println!("=== Second query (same question — compile cache should hit) ===");
    let start = std::time::Instant::now();
    let result2 = vlorql.query("List all users").await?;
    let elapsed2 = start.elapsed();
    println!("sql: {}", result2.sql);
    println!("time: {elapsed2:?}");

    print_cache_stats(&vlorql);

    // Compare times.
    println!();
    println!("--- Summary ---");
    println!("First query (cold):  {elapsed1:?}");
    println!("Second query (warm): {elapsed2:?}");
    if elapsed2 < elapsed1 {
        println!("✓ Cached query was faster (compile cache hit)");
    } else {
        println!("(cache may not have been exercised for this simple plan)");
    }

    println!();
    println!("dialect: {:?}", SqlDialect::Postgres);

    Ok(())
}

fn print_cache_stats(vlorql: &VlorQl) {
    println!("Cache sizes:");
    if let Some(cache) = vlorql.compile_cache() {
        println!("  CompileCache: {} entries", cache.size());
    }
    if let Some(cache) = vlorql.schema_cache() {
        println!("  SchemaCache:  {} entries", cache.size());
    }
    if let Some(cache) = vlorql.prompt_cache() {
        println!("  PromptCache:  {} entries", cache.size());
    }
}
