//! Policy enforcement end-to-end.
//!
//! The facade layers access control on top of schema + dialect validation via
//! `PolicyConfig`. This example builds a policy that lets users query
//! `users.id` and `users.name` only — and demonstrates two outcomes:
//!
//!   * An allowed query (`SELECT id, name FROM users ...`) compiles cleanly.
//!   * A query that touches the disallowed `email` column is rejected by
//!     `VlorQl::query` with a `ValidationErrors` containing a
//!     `ColumnDenied` policy violation.
//!
//! Run with:
//!   cargo run --example with_policy --quiet

use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;

use serde_json::json;
use vlorql::{SchemaSnapshot, VlorQl};
use vlorql_core::policy::{PolicyConfig, TablePolicy};
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection,
    QueryPlan, SchemaMetadata, TableSchema,
};
use vlorql_llm::{LlmClient, MockLlmClient};

/// Schema with three columns: `id`, `name`, `email`. We will allow only `id`
/// and `name`, leaving `email` denied.
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
                ColumnSchema {
                    name: "email".to_owned(),
                    data_type: DataType::String,
                    nullable: false,
                    description: Some("Personally identifiable information".to_owned()),
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

/// Builds the policy used in this example:
///
/// * `users.allowed = true` keeps the table accessible.
/// * `allowed_columns = Some(vec!["id", "name"])` whitelists two columns.
/// * `denied_columns = vec!["email"]` double-locks `email` even if the
///   allowlist were loosened.
fn build_policy() -> PolicyConfig {
    let mut table_policies = HashMap::new();
    table_policies.insert(
        "users".to_owned(),
        TablePolicy {
            allowed: true,
            allowed_columns: Some(vec!["id".to_owned(), "name".to_owned()]),
            denied_columns: vec!["email".to_owned()],
            row_filter: None,
        },
    );
    PolicyConfig {
        table_policies,
        global_denied_columns: Vec::new(),
        row_filters: Vec::new(),
    }
}

/// A `SELECT id, name FROM users WHERE id > 10` plan — should pass.
fn allowed_plan() -> QueryPlan {
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
                value: json!(10),
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

/// A `SELECT email FROM users` plan — should be denied.
fn denied_plan() -> QueryPlan {
    QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "email".to_owned(),
            alias: None,
        }],
        from: FromClause {
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
            set_operation: None,    }
}

/// Runs a single query scenario: builds a facade with the supplied mock
/// client, calls `query`, and prints whether it succeeded or what kind of
/// validation error was raised.
async fn run_scenario(label: &str, client: Box<dyn LlmClient>) -> Result<(), Box<dyn Error>> {
    let vlorql = VlorQl::builder()
        .with_schema(build_schema())
        .with_dialect_name("postgres")
        .with_policy(build_policy())
        .with_llm_client(client)
        // Disable retries so the *first* validation result is what we see.
        .with_max_retries(0)
        .build()?;

    println!("--- {label} ---");
    match vlorql.query("policy demo").await {
        Ok(compiled) => {
            println!("PASS  sql:     {}", compiled.sql);
            println!("      params:  {}", compiled.parameters.len());
        }
        Err(error) => {
            // Validation failures surface as a `ValidationErrors` aggregation.
            // Show the kind and code so the caller can branch on them.
            println!("DENY  code:    {}", error.error_code());
            println!("      error:   {error}");
            if matches!(error, vlorql_core::errors::VlorQLError::Validation { .. }) {
                println!("      kind:    Validation (policy/schema/dialect)");
            }
        }
    }
    println!();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 1. Allowed scenario: only id+name selected, policy is satisfied.
    run_scenario(
        "allowed: SELECT id, name FROM users WHERE id > 10",
        Box::new(MockLlmClient::success(allowed_plan())),
    )
    .await?;

    // 2. Denied scenario: LLM returns a plan touching the locked `email`
    //    column. The policy engine must reject it before compilation.
    run_scenario(
        "denied: SELECT email FROM users",
        Box::new(MockLlmClient::success(denied_plan())),
    )
    .await?;

    // 3. Inspection helper: call `validate_only` directly so you can see the
    //    structured `ValidationErrors` without going through the LLM loop.
    let vlorql = VlorQl::builder()
        .with_schema(build_schema())
        .with_dialect_name("postgres")
        .with_policy(build_policy())
        // No LLM client — we only use `validate_only` here.
        .build()?;
    let errors = vlorql
        .validate_only(&denied_plan())
        .expect_err("policy should deny email");
    println!("--- validate_only summary for denied plan ---");
    println!("error_count: {}", errors.len());
    for error in errors.as_slice() {
        // `VlorQLError` exposes an enum tag (Policy / Schema / Validation / …)
        // and a stable error code (e.g. "P002" for `ColumnDenied`) that you
        // can branch on from production code.
        let variant = match error {
            vlorql_core::errors::VlorQLError::Policy { .. } => "Policy",
            vlorql_core::errors::VlorQLError::Schema { .. } => "Schema",
            vlorql_core::errors::VlorQLError::Validation { .. } => "Validation",
            _ => "Other",
        };
        println!("- variant: {variant}, code: {}", error.error_code());
    }
    Ok(())
}
