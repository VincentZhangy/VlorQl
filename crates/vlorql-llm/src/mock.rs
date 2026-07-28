use async_trait::async_trait;
use futures::stream;
use serde_json::json;
use std::sync::Arc;
use vlorql_core::errors::{LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;
use crate::{LlmClient, LlmConfig, LlmProvider, StreamResult, TokenUsage};

/// A deterministic client for unit and integration tests.
///
/// The mock returns [`MockLlmClient::plan`] from
/// [`LlmClient::generate_plan`] when `should_succeed` is `true` and a
/// canned `LlmErrorKind::ApiError` (status 500) otherwise. The stream
/// counterpart emits the serialized plan as a single chunk.
///
/// # Examples
///
/// ```
/// use vlorql_llm::MockLlmClient;
/// use vlorql_core::schema::{QueryPlan, Projection, FromClause};
///
/// let plan = QueryPlan {
///     select: vec![Projection::Column {
///         table: None, column: "id".to_owned(), alias: None,
///     }],
///     from: FromClause::table("users".to_owned(), None),
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None,
///     joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
/// };
/// let client = MockLlmClient::success(plan);
/// assert!(client.should_succeed);
/// assert!(client.plan.is_some());
/// ```
#[derive(Debug, Clone)]
pub struct MockLlmClient {
    /// When `true`, `generate_plan` returns `self.plan`; when `false`,
    /// it returns a synthetic LLM error.
    pub should_succeed: bool,
    /// The plan to return on success.
    pub plan: Option<QueryPlan>,
    /// Token usage to return on success.
    pub usage: TokenUsage,
    /// The configuration exposed via [`LlmClient::config`].
    pub config: LlmConfig,
}

impl MockLlmClient {
    /// Creates a mock with explicit success behavior and optional plan.
    pub fn new(should_succeed: bool, plan: Option<QueryPlan>) -> Self {
        let config = LlmConfig {
            provider: LlmProvider::OpenAi,
            model: "mock".to_owned(),
            ..LlmConfig::default()
        };
        Self {
            should_succeed,
            plan,
            usage: TokenUsage::default(),
            config,
        }
    }

    /// Creates a successful mock returning the supplied plan with default usage.
    pub fn success(plan: QueryPlan) -> Self {
        Self::new(true, Some(plan))
    }

    /// Creates a successful mock returning the supplied plan and usage.
    pub fn with_usage(plan: QueryPlan, usage: TokenUsage) -> Self {
        Self {
            should_succeed: true,
            plan: Some(plan),
            usage,
            config: LlmConfig {
                provider: LlmProvider::OpenAi,
                model: "mock".to_owned(),
                ..LlmConfig::default()
            },
        }
    }

    /// Creates a failed mock returning a deterministic provider error.
    pub fn failure() -> Self {
        Self::new(false, None)
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn generate_plan(
        &self,
        _question: &str,
        _system_prompt: &str,
        _temperature: Option<f32>,
    ) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
        if self.should_succeed {
            Ok((self.plan.clone().unwrap_or_else(default_plan), self.usage))
        } else {
            Err(VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: 500,
                    message: "mock LLM failure".to_owned(),
                },
                json!({"source": "mock"}),
            ))
        }
    }

    async fn stream_plan(
        &self,
        _question: String,
        _system_prompt: String,
    ) -> Result<StreamResult, VlorQLError> {
        let usage = Arc::new(tokio::sync::Mutex::new(Some(self.usage)));
        if !self.should_succeed {
            let err = VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: 500,
                    message: "mock LLM failure".to_owned(),
                },
                json!({"source": "mock"}),
            );
            let stream = Box::new(stream::iter(vec![Err(err)]))
                as Box<dyn futures::stream::Stream<Item = Result<String, VlorQLError>> + Send + Unpin>;
            return Ok(StreamResult { stream, usage });
        }
        let serialized = serde_json::to_string(&self.plan.clone().unwrap_or_else(default_plan))
            .unwrap_or_default();
        let stream = Box::new(stream::iter(vec![Ok(serialized)]))
            as Box<dyn futures::stream::Stream<Item = Result<String, VlorQLError>> + Send + Unpin>;
        Ok(StreamResult { stream, usage })
    }

    fn provider(&self) -> LlmProvider {
        self.config.provider
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

fn default_plan() -> QueryPlan {
    QueryPlan {
        select: vec![vlorql_core::schema::Projection::Star { table: None }],
        distinct: false,
        distinct_on: None,
        from: vlorql_core::schema::FromClause::table("placeholder".to_owned(), None),
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
        set_operation: None,
    }
}
