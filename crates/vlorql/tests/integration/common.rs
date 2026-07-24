//! Shared fixtures and helpers for the `cargo test --test integration`
//! suite.
//!
//! All four submodules (`end_to_end`, `dialect_compilation`,
//! `policy_enforcement`, `error_recovery`) consume the helpers from this
//! file so the schemas, query plans, and policies stay in sync.
//!
//! The helpers are deliberately written against the public
//! [`vlorql`], [`vlorql_core`], and [`vlorql_llm`] APIs only — they
//! must not reach into private state.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::stream::{self, Stream};
use serde_json::json;
use vlorql::VlorQl;
use vlorql_core::errors::{LlmErrorKind, VlorQLError};
use vlorql_core::policy::{PolicyConfig, RowFilter, TablePolicy};
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, Predicate, Projection,
    QueryPlan, SchemaMetadata, SchemaSnapshot, TableSchema,
};
use vlorql_llm::{LlmClient, LlmConfig, LlmProvider};

/// Returns a single column schema with the supplied name and data type.
pub fn column(name: &str, data_type: DataType) -> ColumnSchema {
    ColumnSchema {
        name: name.to_owned(),
        data_type,
        nullable: false,
        description: None,
        is_primary_key: name == "id",
        foreign_key: None,
    }
}

/// Builds the canonical two-table schema used by every integration test.
///
/// The schema intentionally exposes enough surface area to exercise
/// `SELECT`, `WHERE`, `JOIN`, `LIMIT`/`OFFSET`, row filters, and policy
/// filters without ever needing to mutate it from the tests themselves.
pub fn snapshot() -> std::sync::Arc<SchemaSnapshot> {
    std::sync::Arc::new(SchemaSnapshot::new(
        vec![
            TableSchema {
                name: "users".to_owned(),
                columns: vec![
                    column("id", DataType::Int),
                    column("name", DataType::String),
                    column("email", DataType::String),
                    column("tenant_id", DataType::Int),
                    column("active", DataType::Boolean),
                ],
                description: Some("Application users".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
            TableSchema {
                name: "orders".to_owned(),
                columns: vec![
                    column("id", DataType::Int),
                    column("owner_id", DataType::Int),
                    column("total", DataType::Float),
                ],
                description: Some("Customer orders".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
        ],
        SchemaMetadata {
            version: Some("1.0".to_owned()),
            source: Some("integration-tests".to_owned()),
            generated_at: Some("2026-01-01T00:00:00Z".to_owned()),
        },
    ))
}

/// A bare-bones `SELECT id FROM users` plan used as a starting point for
/// every test in this suite.
pub fn base_plan() -> QueryPlan {
    QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "users".to_owned(),
            alias: Some("t1".to_owned()),
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

/// A permissive policy: every table is reachable, every column is
/// readable, and there are no mandatory row filters. Used by tests that
/// care only about plan shape, not about access control.
pub fn open_policy() -> PolicyConfig {
    PolicyConfig::default()
}

/// The strict policy referenced by [`crate::policy_enforcement`]:
///
/// * The `orders` table is denied entirely.
/// * The `users` table only exposes the `id` and `name` columns.
/// * `email` is globally denied across every table.
pub fn strict_policy() -> PolicyConfig {
    PolicyConfig {
        table_policies: HashMap::from([
            (
                "users".to_owned(),
                TablePolicy {
                    allowed: true,
                    allowed_columns: Some(vec!["id".to_owned(), "name".to_owned()]),
                    denied_columns: Vec::new(),
                    row_filter: None,
                },
            ),
            (
                "orders".to_owned(),
                TablePolicy {
                    allowed: false,
                    allowed_columns: None,
                    denied_columns: Vec::new(),
                    row_filter: None,
                },
            ),
        ]),
        global_denied_columns: vec!["email".to_owned()],
        row_filters: Vec::new(),
    }
}

/// A policy that allows `users` but mandates a `users.id > 10` row
/// filter on every query that touches the table.
pub fn row_filter_policy() -> PolicyConfig {
    PolicyConfig {
        table_policies: HashMap::from([(
            "users".to_owned(),
            TablePolicy {
                allowed: true,
                allowed_columns: None,
                denied_columns: Vec::new(),
                row_filter: Some(RowFilter {
                    condition: Predicate::Comparison {
                        left: Expression::ColumnRef {
                            table: Some("users".to_owned()),
                            column: "id".to_owned(),
                        },
                        op: ComparisonOperator::Gt,
                        right: Expression::Literal {
                            value: json!(10),
                            data_type: DataType::Int,
                        },
                    },
                    description: "Only expose users with id greater than 10".to_owned(),
                }),
            },
        )]),
        global_denied_columns: Vec::new(),
        row_filters: Vec::new(),
    }
}

/// Builds a [`VlorQl`] facade wired up with the supplied mock plan and
/// policy. The dialect is named via `dialect` so the facade picks the
/// matching compiler automatically.
pub fn facade_with(plan: QueryPlan, policy: PolicyConfig, dialect: &str) -> VlorQl {
    VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name(dialect)
        .with_policy(policy)
        .with_llm_client(sequence_client(vec![plan]))
        .build()
        .expect("facade should build")
}

/// Builds a [`VlorQl`] facade that returns the supplied plans in order.
/// The first plan is returned on the first call, the second on the
/// next, and so on.
///
/// `#[allow(dead_code)]` because the helper is part of the public
/// fixture API even when no test in the suite consumes it.
#[allow(dead_code)]
pub fn facade_with_sequence(plans: Vec<QueryPlan>, policy: PolicyConfig, dialect: &str) -> VlorQl {
    VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name(dialect)
        .with_policy(policy)
        .with_llm_client(sequence_client(plans))
        .build()
        .expect("facade should build")
}

/// Builds a [`VlorQl`] facade whose LLM client emits a sequence of
/// pre-baked text chunks. The facade concatenates the chunks and tries
/// to parse the result as a [`QueryPlan`] when the stream closes.
pub fn facade_with_streaming_chunks(
    chunks: Vec<String>,
    policy: PolicyConfig,
    dialect: &str,
) -> VlorQl {
    VlorQl::builder()
        .with_schema(snapshot())
        .with_dialect_name(dialect)
        .with_policy(policy)
        .with_llm_client(streaming_client(chunks))
        .build()
        .expect("facade should build")
}

/// A deterministic LLM client that pops plans from a shared queue.
/// Each call to [`LlmClient::generate_plan`] returns the next plan in
/// the queue, returning an [`LlmErrorKind::ParseError`] if the queue
/// is exhausted.
#[derive(Debug)]
pub struct SequenceMockClient {
    plans: Mutex<Vec<QueryPlan>>,
    config: LlmConfig,
}

impl SequenceMockClient {
    /// Creates a new client that will yield the supplied plans in order.
    pub fn new(plans: Vec<QueryPlan>) -> Self {
        let config = LlmConfig {
            provider: LlmProvider::OpenAi,
            model: "mock-sequence".to_owned(),
            ..LlmConfig::default()
        };
        Self {
            plans: Mutex::new(plans),
            config,
        }
    }
}

#[async_trait]
impl LlmClient for SequenceMockClient {
    async fn generate_plan(
        &self,
        _question: &str,
        _system_prompt: &str,
    ) -> Result<QueryPlan, VlorQLError> {
        self.plans
            .lock()
            .expect("sequence lock should not be poisoned")
            .pop()
            .ok_or_else(|| {
                VlorQLError::llm(
                    LlmErrorKind::ParseError {
                        details: "sequence exhausted".to_owned(),
                    },
                    json!({"source": "sequence_mock"}),
                )
            })
    }

    async fn stream_plan(
        &self,
        _question: String,
        _system_prompt: String,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        let plan = self
            .generate_plan("", "")
            .await
            .expect("sequence client should yield a plan");
        let serialized = serde_json::to_string(&plan).unwrap_or_default();
        Ok(Box::new(stream::iter(vec![Ok(serialized)])))
    }

    fn provider(&self) -> LlmProvider {
        self.config.provider
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

/// Helper that turns a vector of plans into a [`SequenceMockClient`].
pub fn sequence_client(plans: Vec<QueryPlan>) -> SequenceMockClient {
    SequenceMockClient::new(plans)
}

/// A streaming LLM client that emits a fixed list of text chunks
/// followed by the end-of-stream sentinel.
#[derive(Debug)]
pub struct StreamingMockClient {
    chunks: Vec<String>,
    config: LlmConfig,
}

impl StreamingMockClient {
    /// Creates a new streaming client that emits the supplied chunks in
    /// order, then closes the stream.
    pub fn new(chunks: Vec<String>) -> Self {
        let config = LlmConfig {
            provider: LlmProvider::OpenAi,
            model: "mock-streaming".to_owned(),
            ..LlmConfig::default()
        };
        Self { chunks, config }
    }
}

#[async_trait]
impl LlmClient for StreamingMockClient {
    async fn generate_plan(
        &self,
        _question: &str,
        _system_prompt: &str,
    ) -> Result<QueryPlan, VlorQLError> {
        // Concatenate the chunks and try to deserialize them. This makes
        // the streaming client also usable from the non-streaming code
        // paths so the same fixture can power both kinds of test.
        let combined = self.chunks.concat();
        serde_json::from_str(&combined).map_err(|error| {
            VlorQLError::llm(
                LlmErrorKind::ParseError {
                    details: format!("streaming client cannot decode its own chunks: {error}"),
                },
                json!({"chunks": self.chunks.len()}),
            )
        })
    }

    async fn stream_plan(
        &self,
        _question: String,
        _system_prompt: String,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        let chunks = self.chunks.clone();
        let stream = stream::iter(chunks.into_iter().map(Ok::<String, VlorQLError>));
        Ok(Box::new(Box::pin(stream)))
    }

    fn provider(&self) -> LlmProvider {
        self.config.provider
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

/// Helper that turns a vector of text chunks into a
/// [`StreamingMockClient`].
pub fn streaming_client(chunks: Vec<String>) -> StreamingMockClient {
    StreamingMockClient::new(chunks)
}

/// Splits a serialized [`QueryPlan`] into three text chunks whose
/// concatenation round-trips to the original JSON.
///
/// The chunks themselves are not valid JSON on their own — that is
/// intentional: each one is the payload of a separate
/// [`StreamEvent::TextChunk`], and only after all three have been
/// concatenated does the streaming facade attempt to parse the result.
pub fn chunked_plan_payload(plan: &QueryPlan) -> Vec<String> {
    let serialized = serde_json::to_string(plan).expect("plan should serialize");
    let total = serialized.len();
    let first_break = total / 3;
    let second_break = total * 2 / 3;
    vec![
        serialized[..first_break].to_owned(),
        serialized[first_break..second_break].to_owned(),
        serialized[second_break..].to_owned(),
    ]
}
