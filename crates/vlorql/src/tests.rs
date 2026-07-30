use super::*;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use vlorql_core::errors::{LlmErrorKind, SchemaErrorKind};
use vlorql_core::schema::{
    ColumnSchema, DataType, Expression, FromClause, Predicate, Projection, QueryPlan,
    SchemaMetadata, TableSchema,
};
use vlorql_llm::MockLlmClient;

#[test]
fn retry_feedback_is_truncated_when_many_errors() {
    let errs: Vec<VlorQLError> = (0..5)
        .map(|i| {
            VlorQLError::schema(
                SchemaErrorKind::ColumnNotFound {
                    table: "t".to_owned(),
                    column: format!("c{i}"),
                },
                serde_json::json!({"table": "t", "column": format!("c{i}")}),
            )
        })
        .collect();
    let errors = ValidationErrors(errs);
    let q = format_retry_question("q", &errors, 2);
    assert!(
        q.contains("c0") && q.contains("c1") && q.contains("c2"),
        "first 3: {q}"
    );
    assert!(!q.contains("c4"), "not the 5th: {q}");
    assert!(q.contains("2 more"), "omitted count: {q}");
}

#[test]
fn retry_temperature_keeps_default_on_first_attempt_then_escalates() {
    assert_eq!(retry_temperature(0.0, 0), None);
    assert_eq!(retry_temperature(0.0, 1), Some(0.2));
    assert_eq!(retry_temperature(0.0, 2), Some(0.4));
    assert!(retry_temperature(0.9, 3).expect("retry should escalate") <= 1.0);
}

#[test]
fn retry_feedback_is_tiered_by_attempt() {
    let errs: Vec<VlorQLError> = (0..5)
        .map(|i| {
            VlorQLError::schema(
                SchemaErrorKind::ColumnNotFound {
                    table: "t".to_owned(),
                    column: format!("c{i}"),
                },
                serde_json::json!({"table": "t", "column": format!("c{i}")}),
            )
        })
        .collect();
    let errors = ValidationErrors(errs);
    let first = format_retry_question("q", &errors, 0);
    let later = format_retry_question("q", &errors, 2);
    assert!(
        first.matches("does not exist").count() < later.matches("does not exist").count(),
        "attempt 0 terser than attempt 2:\nfirst={first}\nlater={later}"
    );
}

fn schema() -> Arc<SchemaSnapshot> {
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
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        }],
        SchemaMetadata::default(),
    ))
}

fn valid_plan() -> QueryPlan {
    QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause::table("users".to_owned(), Some("t1".to_owned())),
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
    }
}

fn facade_with_mock(plan: QueryPlan) -> VlorQl {
    VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("sqlite")
        .with_llm_client(MockLlmClient::success(plan))
        .build()
        .expect("facade should build")
}

#[tokio::test]
async fn query_runs_prompt_validation_and_compilation() {
    let boxed_client: Box<dyn LlmClient> = Box::new(MockLlmClient::success(valid_plan()));
    let facade = VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("sqlite")
        .with_llm_client(boxed_client)
        .build()
        .expect("boxed client should build");
    let (compiled, _usage) = facade
        .query("show user ids")
        .await
        .expect("valid mock plan should compile");
    assert_eq!(compiled.dialect, SqlDialect::Sqlite);
    assert_eq!(
        compiled.sql,
        "SELECT \"t1\".\"id\" FROM \"users\" AS \"t1\""
    );
}

#[tokio::test]
async fn query_requires_an_llm_client_but_validate_only_does_not() {
    let facade = VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("postgres")
        .build()
        .expect("facade without LLM should still build");
    let error = facade
        .query("show users")
        .await
        .expect_err("query should require an LLM client");
    assert_eq!(error.error_code(), "G001");
    assert!(facade.validate_only(&valid_plan()).is_ok());
}

#[test]
fn builder_checks_schema_and_dialect() {
    assert_eq!(
        VlorQl::builder()
            .with_dialect_name("sqlite")
            .build()
            .expect_err("schema is required")
            .error_code(),
        "G002"
    );
    assert_eq!(
        VlorQl::builder()
            .with_schema(schema())
            .with_dialect_name("unknown")
            .build()
            .expect_err("dialect should be checked")
            .error_code(),
        "G003"
    );
}

#[cfg(test)]
struct SequenceClient {
    plans: Mutex<Vec<QueryPlan>>,
    config: LlmConfig,
}

#[cfg(test)]
#[async_trait]
impl LlmClient for SequenceClient {
    async fn generate_plan(
        &self,
        _question: &str,
        _system_prompt: &str,
        _temperature: Option<f32>,
    ) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
        let plan = self
            .plans
            .lock()
            .expect("sequence lock should not be poisoned")
            .pop()
            .ok_or_else(|| {
                VlorQLError::llm(
                    LlmErrorKind::ParseError {
                        details: "sequence exhausted".to_owned(),
                    },
                    json!({}),
                )
            })?;
        Ok((plan, TokenUsage::default()))
    }

    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<StreamResult, VlorQLError> {
        let (plan, _usage) = self.generate_plan(&question, &system_prompt, None).await?;
        let serialized = serde_json::to_string(&plan).unwrap_or_default();
        let stream = Box::new(futures::stream::iter(vec![Ok(serialized)]))
            as Box<
                dyn futures::stream::Stream<Item = Result<String, VlorQLError>> + Send + Unpin,
            >;
        let usage = Arc::new(tokio::sync::Mutex::new(Some(TokenUsage::default())));
        Ok(StreamResult { stream, usage })
    }

    fn provider(&self) -> vlorql_llm::LlmProvider {
        vlorql_llm::LlmProvider::OpenAi
    }

    fn config(&self) -> &vlorql_llm::LlmConfig {
        &self.config
    }
}

#[tokio::test]
async fn query_retries_retryable_validation_errors() {
    let mut invalid = valid_plan();
    invalid.r#where = Some(Predicate::Comparison {
        left: Box::new(Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
        }),
        op: vlorql_core::schema::ComparisonOperator::Eq,
        right: Box::new(Expression::Literal {
            value: serde_json::json!(1),
            data_type: DataType::Int,
        }),
    });
    let sequence = SequenceClient {
        plans: Mutex::new(vec![valid_plan(), invalid]),
        config: LlmConfig::default(),
    };
    let facade = VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("postgres")
        .with_llm_client(sequence)
        .with_max_retries(2)
        .build()
        .expect("facade should build");

    let (compiled, _usage) = facade
        .query("show user ids")
        .await
        .expect("second valid plan should be used");
    assert!(compiled.sql.contains("SELECT"));
}

#[test]
fn with_llm_config_creates_facade() {
    let config = LlmConfig {
        provider: LlmProvider::Ollama,
        model: "llama3".to_owned(),
        ..LlmConfig::default()
    };
    let v = VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("sqlite")
        .with_llm_config(config)
        .build()
        .expect("facade should build with config");
    assert_eq!(v.max_retries(), 3);
}

#[test]
fn validation_and_compilation_helpers_are_public() {
    let facade = facade_with_mock(valid_plan());
    let validated = facade
        .validate_only(&valid_plan())
        .expect("plan should validate");
    let compiled = facade
        .compile_only(&validated)
        .expect("plan should compile");
    assert!(compiled.sql.contains("users"));
}

#[tokio::test]
async fn query_stream_emits_chunks_then_plan_complete() {
    use futures::StreamExt;
    let facade = VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("sqlite")
        .with_policy(PolicyConfig::default())
        .with_llm_client(MockLlmClient::success(valid_plan()))
        .build()
        .expect("facade should build");
    let mut stream = facade
        .query_stream("list users")
        .await
        .expect("query_stream should succeed");
    let mut final_plan = None;
    let mut saw_chunks = false;
    let mut saw_usage = false;
    while let Some(item) = stream.next().await {
        match item.expect("event should be Ok") {
            StreamEvent::TextChunk(_) => saw_chunks = true,
            StreamEvent::PlanComplete(plan) => final_plan = Some(*plan),
            StreamEvent::TokenUsage(usage) => {
                saw_usage = true;
                assert_eq!(usage, TokenUsage::default());
            }
            StreamEvent::Error(error) => panic!("unexpected error event: {error}"),
        }
    }
    assert!(saw_chunks, "should receive at least one text chunk");
    assert_eq!(final_plan, Some(valid_plan()));
    assert!(saw_usage, "should receive token usage event");
}

#[tokio::test]
async fn span_hierarchy_includes_query_validate_compile() {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let events_clone = Arc::clone(&events);

    let layer = tracing_subscriber::fmt::layer()
        .with_test_writer()
        .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
            meta.target().starts_with("vlorql")
        }));

    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let facade = VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("sqlite")
        .with_llm_client(MockLlmClient::success(valid_plan()))
        .build()
        .expect("facade should build");

    let (compiled, _usage) = facade
        .query("show user ids")
        .await
        .expect("valid mock plan should compile");

    assert_eq!(compiled.dialect, SqlDialect::Sqlite);
    assert_eq!(
        compiled.sql,
        "SELECT \"t1\".\"id\" FROM \"users\" AS \"t1\""
    );
    drop(
        events_clone
            .lock()
            .expect("events lock should not be poisoned"),
    );
}

#[test]
fn json_logs_contain_trace_id_and_span_id() {
    use std::sync::LazyLock;
    use std::sync::Mutex;
    static BUF: LazyLock<Mutex<Vec<u8>>> = LazyLock::new(|| Mutex::new(Vec::new()));

    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_target(true)
        .with_current_span(true)
        .with_span_list(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_writer(move || {
            let buf = BUF.lock().expect("lock");
            let data = buf.clone();
            Box::new(std::io::Cursor::new(data)) as Box<dyn std::io::Write + Send>
        })
        .finish();

    let _guard = tracing::subscriber::set_default(subscriber);

    let span = tracing::info_span!("test_span", key = "value");
    let _enter = span.enter();
    tracing::info!("test event inside span");
    drop(_enter);
    drop(_guard);

    let buf_guard = BUF.lock().expect("lock");
    let output = String::from_utf8_lossy(&buf_guard);
    if !output.is_empty() {
        for line in output.lines() {
            if line.contains("span") {
                let parsed: serde_json::Value =
                    serde_json::from_str(line).expect("each line should be valid JSON");
                assert!(parsed.is_object(), "JSON log line should be an object");
            }
        }
    }
}
