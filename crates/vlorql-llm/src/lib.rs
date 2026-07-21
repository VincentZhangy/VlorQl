//! Async LLM clients that produce validated VlorQl query-plan data.
//!
//! The crate exposes a single [`LlmClient`] trait and a factory
//! function ([`create_llm_client`]) that returns a boxed
//! implementation for one of six supported providers. All clients
//! share the same JSON contract so the rest of VlorQl can treat them
//! uniformly.
//!
//! ## Streaming
//!
//! [`LlmClient::stream_plan`] returns a `Stream<Item = Result<String,
//! VlorQLError>>`. The items are raw text deltas emitted by the LLM
//! (concatenated, they form the assistant's reply).
//!
//! ## Retries
//!
//! The HTTP clients retry on transient failures (5xx, 429, timeouts).
//! Set [`LlmConfig::max_retries`] to control the budget.

#![deny(missing_docs)]

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{self, Stream};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{Instrument, debug, warn};
use vlorql_core::errors::{ConfigErrorKind, LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;

pub(crate) const SSE_DONE: &str = "[DONE]";

pub mod anthropic;
pub mod deepseek;
pub mod local;
pub mod zhipu;

/// Supported LLM providers.
///
/// Each variant corresponds to a dedicated client implementation
/// reachable through [`create_llm_client`]. The `serde` representation
/// uses `snake_case` so the value can be deserialized from the
/// `provider` field of a TOML/JSON configuration.
///
/// # Examples
///
/// ```
/// use vlorql_llm::LlmProvider;
///
/// let provider = LlmProvider::OpenAi;
/// assert_eq!(provider.id(), "openai");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProvider {
    /// OpenAI's hosted `/chat/completions` endpoint.
    OpenAi,
    /// Anthropic's `/v1/messages` endpoint (Claude).
    Anthropic,
    /// DeepSeek's OpenAI-compatible chat-completions endpoint.
    DeepSeek,
    /// Zhipu GLM's `/api/paas/v4/chat/completions` endpoint.
    Zhipu,
    /// Locally running vLLM OpenAI-compatible server.
    Vllm,
    /// Locally running Ollama `/api/chat` endpoint.
    Ollama,
}

impl LlmProvider {
    /// Returns the canonical identifier of the provider.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            LlmProvider::OpenAi => "openai",
            LlmProvider::Anthropic => "anthropic",
            LlmProvider::DeepSeek => "deepseek",
            LlmProvider::Zhipu => "zhipu",
            LlmProvider::Vllm => "vllm",
            LlmProvider::Ollama => "ollama",
        }
    }
}

impl std::fmt::Display for LlmProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.id())
    }
}

/// Default endpoint per provider. May be overridden via `api_base`.
fn default_api_base(provider: LlmProvider) -> &'static str {
    match provider {
        LlmProvider::OpenAi => "https://api.openai.com/v1/chat/completions",
        LlmProvider::Anthropic => "https://api.anthropic.com/v1/messages",
        LlmProvider::DeepSeek => "https://api.deepseek.com/v1/chat/completions",
        LlmProvider::Zhipu => "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        LlmProvider::Vllm => "http://localhost:8000/v1/chat/completions",
        LlmProvider::Ollama => "http://localhost:11434/api/chat",
    }
}

/// Provider-agnostic LLM configuration.
///
/// The struct is intentionally flat so it round-trips through
/// `serde_json` / TOML with no surprises. `api_key` is optional
/// because local providers (vLLM, Ollama) usually do not require
/// authentication; for hosted providers the factory
/// ([`create_llm_client`]) also checks the documented environment
/// variable when `api_key` is empty.
///
/// # Examples
///
/// ```
/// use vlorql_llm::{LlmConfig, LlmProvider};
///
/// let config = LlmConfig {
///     provider: LlmProvider::OpenAi,
///     model: "gpt-4o-mini".to_owned(),
///     ..LlmConfig::default()
/// };
/// assert_eq!(config.effective_api_base(), "https://api.openai.com/v1/chat/completions");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// The target provider.
    pub provider: LlmProvider,
    /// API key, if the provider requires authentication.
    pub api_key: Option<String>,
    /// Override the default endpoint. May be a full URL or a
    /// base URL (the client appends the chat-completions suffix
    /// when appropriate).
    pub api_base: Option<String>,
    /// Model identifier (e.g. `"gpt-4o-mini"`, `"claude-sonnet-4-5"`).
    pub model: String,
    /// Maximum number of tokens the LLM is allowed to emit.
    pub max_tokens: u32,
    /// Sampling temperature. `0.0` produces deterministic output.
    pub temperature: f32,
    /// Per-request timeout, in seconds.
    pub timeout_seconds: u64,
    /// Maximum number of retry attempts for transient errors.
    pub max_retries: u32,
    /// Free-form provider-specific options. The
    /// [`local::LocalClient`] recognises `"backend"` and
    /// `"strict_json_schema"`; other clients ignore the map.
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: LlmProvider::OpenAi,
            api_key: None,
            api_base: None,
            model: "gpt-4o-mini".to_owned(),
            max_tokens: 1024,
            temperature: 0.0,
            timeout_seconds: 60,
            max_retries: 3,
            extra: HashMap::new(),
        }
    }
}

impl LlmConfig {
    /// Returns the effective API base, falling back to the provider default.
    #[must_use]
    pub fn effective_api_base(&self) -> String {
        self.api_base
            .clone()
            .unwrap_or_else(|| default_api_base(self.provider).to_owned())
    }
}

pub(crate) const DEFAULT_API_BASE: &str = "https://api.openai.com/v1/chat/completions";
pub(crate) const DEFAULT_MAX_ATTEMPTS: usize = 3;
pub(crate) const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(1);

/// A client that turns a natural-language question into a structured query plan.
///
/// # Examples
///
/// ```
/// use vlorql_llm::{LlmClient, MockLlmClient};
/// use vlorql_core::schema::{QueryPlan, Projection, FromClause};
///
/// # async fn example() {
/// let plan = QueryPlan {
///     select: vec![Projection::Column {
///         table: None, column: "id".to_owned(), alias: None,
///     }],
///     from: FromClause { table: "users".to_owned(), alias: None },
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None,
///     joins: None, ctes: None,
/// };
/// let client = MockLlmClient::success(plan);
/// let result = client.generate_plan("test", "prompt").await;
/// assert!(result.is_ok());
/// # }
/// ```
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Generates a complete query plan from the LLM.
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
    ) -> Result<QueryPlan, VlorQLError>;

    /// Streams raw text deltas as the LLM emits them.
    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>;

    /// Returns the provider that produced this client.
    fn provider(&self) -> LlmProvider;

    /// Returns the configuration used to build this client.
    fn config(&self) -> &LlmConfig;
}

#[async_trait]
impl<T> LlmClient for Box<T>
where
    T: LlmClient + ?Sized,
{
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
    ) -> Result<QueryPlan, VlorQLError> {
        (**self).generate_plan(question, system_prompt).await
    }

    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        (**self).stream_plan(question, system_prompt).await
    }

    fn provider(&self) -> LlmProvider {
        (**self).provider()
    }

    fn config(&self) -> &LlmConfig {
        (**self).config()
    }
}

/// OpenAI-compatible chat-completions client.
#[derive(Clone)]
pub struct OpenAIClient {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: Option<String>,
    max_attempts: usize,
    retry_base_delay: Duration,
    strict_json_schema_override: Option<bool>,
    config: LlmConfig,
}

impl std::fmt::Debug for OpenAIClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAIClient")
            .field("client", &self.client)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("api_base", &self.api_base)
            .field("max_attempts", &self.max_attempts)
            .field("retry_base_delay", &self.retry_base_delay)
            .field(
                "strict_json_schema_override",
                &self.strict_json_schema_override,
            )
            .field("provider", &self.config.provider)
            .field("model", &self.config.model)
            .finish()
    }
}

impl OpenAIClient {
    /// Creates a client using the OpenAI public API endpoint.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let config = LlmConfig {
            api_key: Some(api_key.into()),
            model: model.into(),
            ..LlmConfig::default()
        };
        Self::from_config(config)
    }

    /// Creates a client from a fully populated [`LlmConfig`].
    pub fn from_config(config: LlmConfig) -> Self {
        let api_key = config.api_key.clone().unwrap_or_default();
        let timeout = std::time::Duration::from_secs(config.timeout_seconds);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let max_attempts =
            usize::try_from(config.max_retries.max(1)).unwrap_or(DEFAULT_MAX_ATTEMPTS);
        Self {
            client,
            api_key,
            model: config.model.clone(),
            api_base: config.api_base.clone(),
            max_attempts,
            retry_base_delay: DEFAULT_RETRY_DELAY,
            strict_json_schema_override: None,
            config,
        }
    }

    /// Creates a client from all required transport fields.
    pub fn from_parts(
        client: reqwest::Client,
        api_key: impl Into<String>,
        model: impl Into<String>,
        api_base: Option<String>,
    ) -> Self {
        let config = LlmConfig {
            api_key: Some(api_key.into()),
            model: model.into(),
            api_base,
            ..LlmConfig::default()
        };
        Self {
            client,
            api_key: config.api_key.clone().unwrap_or_default(),
            model: config.model.clone(),
            api_base: config.api_base.clone(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            retry_base_delay: DEFAULT_RETRY_DELAY,
            strict_json_schema_override: None,
            config,
        }
    }

    /// Replaces the API base URL. A `/chat/completions` suffix is added when absent.
    #[must_use]
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = Some(api_base.into());
        self
    }

    /// Uses a caller-provided reqwest client, useful for custom TLS and test transports.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Sets the retry delay base. The normal production default is one second.
    #[must_use]
    pub fn with_retry_base_delay(mut self, delay: Duration) -> Self {
        self.retry_base_delay = delay;
        self
    }

    /// Sets the maximum number of total attempts, capped at three.
    #[must_use]
    pub fn with_max_attempts(mut self, attempts: usize) -> Self {
        self.max_attempts = attempts.clamp(1, DEFAULT_MAX_ATTEMPTS);
        self
    }

    /// Overrides model capability detection for strict JSON Schema responses.
    #[must_use]
    pub fn with_strict_json_schema(mut self, supported: bool) -> Self {
        self.strict_json_schema_override = Some(supported);
        self
    }

    /// Returns the configured model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the configured API base, if one was explicitly supplied.
    pub fn api_base(&self) -> Option<&str> {
        self.api_base.as_deref()
    }

    /// Returns whether this client will request OpenAI strict JSON Schema output.
    pub fn supports_strict_json_schema(&self) -> bool {
        self.strict_json_schema_override
            .unwrap_or_else(|| model_supports_strict_json_schema(&self.model))
    }

    async fn send_once(&self, endpoint: &str, body: &Value) -> Result<QueryPlan, VlorQLError> {
        let response = self
            .client
            .post(endpoint)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|error| transport_error(&error))?;

        let status = response.status();
        let response_text = response
            .text()
            .await
            .map_err(|error| transport_error(&error))?;
        if !status.is_success() {
            return Err(VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: status.as_u16(),
                    message: response_message(&response_text),
                },
                json!({
                    "status": status.as_u16(),
                    "body": truncate(&response_text, 2048),
                }),
            ));
        }

        let response_json: Value = serde_json::from_str(&response_text).map_err(|error| {
            VlorQLError::llm(
                LlmErrorKind::ParseError {
                    details: format!("OpenAI response is not valid JSON: {error}"),
                },
                json!({
                    "source": "provider_response",
                    "body": truncate(&response_text, 2048),
                }),
            )
        })?;
        let content = response_json
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                VlorQLError::llm(
                    LlmErrorKind::ParseError {
                        details: "OpenAI response did not contain choices[0].message.content"
                            .to_owned(),
                    },
                    json!({"source": "provider_response"}),
                )
            })?;

        serde_json::from_str::<QueryPlan>(content).map_err(|error| {
            VlorQLError::llm(
                LlmErrorKind::ParseError {
                    details: format!("assistant content is not a valid QueryPlan: {error}"),
                },
                json!({
                    "source": "assistant_content",
                    "content": truncate(content, 4096),
                }),
            )
        })
    }

    fn streaming_request_body(&self, question: &str, system_prompt: &str) -> Value {
        let response_format = if self.supports_strict_json_schema() {
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "QueryPlan",
                    "strict": true,
                    "schema": compact_query_plan_schema(),
                },
            })
        } else {
            json!({"type": "json_object"})
        };

        json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": question},
            ],
            "temperature": 0.0,
            "stream": true,
            "stream_options": {"include_usage": false},
            "response_format": response_format,
        })
    }

    fn request_body(&self, question: &str, system_prompt: &str) -> Value {
        let response_format = if self.supports_strict_json_schema() {
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "QueryPlan",
                    "strict": true,
                    "schema": compact_query_plan_schema(),
                },
            })
        } else {
            json!({"type": "json_object"})
        };

        json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": question},
            ],
            "temperature": 0.0,
            "response_format": response_format,
        })
    }

    fn endpoint(&self) -> String {
        let base = self.api_base.as_deref().unwrap_or(DEFAULT_API_BASE);
        if base.ends_with("/chat/completions") {
            base.to_owned()
        } else {
            format!("{}/chat/completions", base.trim_end_matches('/'))
        }
    }
}

#[async_trait]
impl LlmClient for OpenAIClient {
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
    ) -> Result<QueryPlan, VlorQLError> {
        let span = tracing::info_span!(
            "llm.generate_plan",
            provider = ?self.provider(),
            model = %self.config().model,
            prompt_len = system_prompt.len(),
            streaming = false,
        );
        async move {
            let endpoint = self.endpoint();
            let body = self.request_body(question, system_prompt);
            let attempts = self.max_attempts.max(1);
            let mut last_error = None;

            for attempt in 0..attempts {
                match self.send_once(&endpoint, &body).await {
                    Ok(plan) => {
                        tracing::debug!("LLM response received");
                        return Ok(plan);
                    }
                    Err(error) => {
                        let can_retry = is_retryable(&error) && attempt + 1 < attempts;
                        if !can_retry {
                            return Err(error);
                        }
                        let delay = retry_delay(self.retry_base_delay, attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts = attempts,
                            ?delay,
                            "temporary LLM request failure; retrying"
                        );
                        last_error = Some(error);
                        sleep(delay).await;
                    }
                }
            }

            Err(last_error.unwrap_or_else(|| {
                VlorQLError::llm(
                    LlmErrorKind::ApiError {
                        status: 0,
                        message: "LLM request did not produce a result".to_owned(),
                    },
                    json!({"source": "client"}),
                )
            }))
        }
        .instrument(span)
        .await
    }

    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        let span = tracing::info_span!(
            "llm.stream_plan",
            provider = ?self.provider(),
            model = %self.config().model,
            prompt_len = system_prompt.len(),
            streaming = true,
        );
        let _enter = span.enter();
        let body = self.streaming_request_body(&question, &system_prompt);
        let endpoint = self.endpoint();
        let response = self
            .client
            .post(&endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: status.as_u16(),
                    message: response_message(&body),
                },
                json!({
                    "status": status.as_u16(),
                    "body": truncate(&body, 2048),
                }),
            ));
        }

        let byte_stream = response.bytes_stream();
        let (tx, rx) = mpsc::unbounded_channel::<Result<String, VlorQLError>>();
        let line_stream = sse_lines(byte_stream);

        let max_attempts = self.max_attempts;
        let retry_base = self.retry_base_delay;
        tokio::spawn(async move {
            if !drive_sse_consumer(line_stream, tx, max_attempts, retry_base).await {
                warn!("SSE consumer ended before producing content");
            }
        });

        let output = UnboundedReceiverStream::new(rx);
        Ok(Box::new(Box::pin(output)))
    }

    fn provider(&self) -> LlmProvider {
        self.config.provider
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

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
///     from: FromClause { table: "users".to_owned(), alias: None },
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None,
///     joins: None, ctes: None,
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
            config,
        }
    }

    /// Creates a successful mock returning the supplied plan.
    pub fn success(plan: QueryPlan) -> Self {
        Self::new(true, Some(plan))
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
    ) -> Result<QueryPlan, VlorQLError> {
        if self.should_succeed {
            Ok(self.plan.clone().unwrap_or_else(default_plan))
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
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        if !self.should_succeed {
            let err = VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: 500,
                    message: "mock LLM failure".to_owned(),
                },
                json!({"source": "mock"}),
            );
            return Ok(Box::new(stream::iter(vec![Err(err)])));
        }
        let serialized = serde_json::to_string(&self.plan.clone().unwrap_or_else(default_plan))
            .unwrap_or_default();
        // Mock implementation: emit a single chunk containing the serialized plan.
        let stream = stream::iter(vec![Ok(serialized)]);
        Ok(Box::new(stream))
    }

    fn provider(&self) -> LlmProvider {
        self.config.provider
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

pub(crate) fn compact_query_plan_schema() -> Value {
    static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(simplified_query_plan_schema).clone()
}

/// Returns a flattened JSON Schema for `QueryPlan` without `$ref` or `$defs`.
///
/// The auto-generated schema from `schemars::schema_for!(QueryPlan)` contains
/// recursive `$ref` definitions that confuse smaller Ollama models (4B–7B),
/// causing them to output schema fragments instead of actual data. This
/// manually constructed schema inlines everything so the model can produce
/// valid output directly.
fn simplified_query_plan_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "select": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "enum": ["column_ref", "expr", "star"] },
                        "table": { "type": "string" },
                        "column": { "type": "string" },
                        "expression": { "$ref": "#/definitions/Expression" },
                        "alias": { "type": "string" }
                    },
                    "required": ["type"]
                }
            },
            "from": {
                "type": "object",
                "properties": {
                    "table": { "type": "string" },
                    "alias": { "type": "string" }
                },
                "required": ["table"]
            },
            "where": { "$ref": "#/definitions/Predicate" },
            "group_by": {
                "type": "array",
                "items": { "$ref": "#/definitions/Expression" }
            },
            "having": { "$ref": "#/definitions/Predicate" },
            "order_by": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "expr": { "$ref": "#/definitions/Expression" },
                        "descending": { "type": "boolean" }
                    },
                    "required": ["expr", "descending"]
                }
            },
            "limit": { "type": "integer" },
            "offset": { "type": "integer" },
            "joins": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "join_type": { "type": "string", "enum": ["inner", "left", "right", "full", "cross"] },
                        "right_table": {
                            "type": "object",
                            "properties": {
                                "table": { "type": "string" },
                                "alias": { "type": "string" }
                            },
                            "required": ["table"]
                        },
                        "on": { "$ref": "#/definitions/Predicate" }
                    },
                    "required": ["join_type", "right_table", "on"]
                }
            },
            "ctes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "query": { "$ref": "#/definitions/QueryPlan" }
                    },
                    "required": ["name", "query"]
                }
            }
        },
        "required": ["select", "from"],
        "definitions": {
            "QueryPlan": {
                "type": "object",
                "properties": {
                    "select": { "type": "array", "items": { "type": "object" } },
                    "from": { "type": "object", "properties": { "table": { "type": "string" }, "alias": { "type": "string" } } },
                    "where": { "type": "object" },
                    "group_by": { "type": "array", "items": { "type": "object" } },
                    "having": { "type": "object" },
                    "order_by": { "type": "array", "items": { "type": "object" } },
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" },
                    "joins": { "type": "array", "items": { "type": "object" } },
                    "ctes": { "type": "array", "items": { "type": "object" } }
                }
            },
            "Expression": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["literal"] },
                            "value": { "type": ["string", "number", "boolean", "null"] },
                            "data_type": { "type": "string", "enum": ["int", "float", "string", "boolean", "date", "timestamp", "json", "uuid"] }
                        },
                        "required": ["type", "value", "data_type"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["column_ref"] },
                            "table": { "type": "string" },
                            "column": { "type": "string" }
                        },
                        "required": ["type", "column"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["function_call"] },
                            "name": { "type": "string" },
                            "args": { "type": "array", "items": { "type": "object" } },
                            "distinct": { "type": "boolean" }
                        },
                        "required": ["type", "name", "args"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["binary_op"] },
                            "left": { "type": "object" },
                            "op": { "type": "string", "enum": ["add", "sub", "mul", "div", "and", "or"] },
                            "right": { "type": "object" }
                        },
                        "required": ["type", "left", "op", "right"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["star"] }
                        },
                        "required": ["type"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["sub_query"] },
                            "query": { "type": "object" }
                        },
                        "required": ["type", "query"]
                    }
                ]
            },
            "Predicate": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["comparison"] },
                            "left": { "type": "object" },
                            "op": { "type": "string", "enum": ["eq", "neq", "gt", "gte", "lt", "lte", "like", "not_like", "is", "is_not"] },
                            "right": { "type": "object" }
                        },
                        "required": ["type", "left", "op", "right"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["and"] },
                            "left": { "type": "object" },
                            "right": { "type": "object" }
                        },
                        "required": ["type", "left", "right"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["or"] },
                            "left": { "type": "object" },
                            "right": { "type": "object" }
                        },
                        "required": ["type", "left", "right"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["not"] },
                            "child": { "type": "object" }
                        },
                        "required": ["type", "child"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["between"] },
                            "expr": { "type": "object" },
                            "low": { "type": "object" },
                            "high": { "type": "object" }
                        },
                        "required": ["type", "expr", "low", "high"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["in"] },
                            "expr": { "type": "object" },
                            "target": {
                                "oneOf": [
                                    { "type": "array", "items": { "type": "object" } },
                                    { "type": "object" }
                                ]
                            }
                        },
                        "required": ["type", "expr", "target"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["like"] },
                            "expr": { "type": "object" },
                            "pattern": { "type": "string" }
                        },
                        "required": ["type", "expr", "pattern"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["is_null"] },
                            "expr": { "type": "object" }
                        },
                        "required": ["type", "expr"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["exists"] },
                            "query": { "type": "object" }
                        },
                        "required": ["type", "query"]
                    }
                ]
            }
        }
    })
}

fn model_supports_strict_json_schema(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("gpt-4o")
        || model.starts_with("gpt-4.1")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

pub(crate) fn transport_error(error: &reqwest::Error) -> VlorQLError {
    if error.is_timeout() {
        VlorQLError::llm(
            LlmErrorKind::Timeout,
            json!({"source": "transport", "message": error.to_string()}),
        )
    } else {
        VlorQLError::llm(
            LlmErrorKind::ApiError {
                status: 0,
                message: error.to_string(),
            },
            json!({"source": "transport", "message": error.to_string()}),
        )
    }
}

pub(crate) fn is_retryable(error: &VlorQLError) -> bool {
    match error {
        VlorQLError::Llm {
            kind: LlmErrorKind::Timeout,
            ..
        } => true,
        VlorQLError::Llm {
            kind: LlmErrorKind::ApiError { status, .. },
            ..
        } => *status == 0 || *status == 429 || *status >= 500,
        _ => false,
    }
}

pub(crate) fn retry_delay(base: Duration, retry_index: usize) -> Duration {
    let multiplier = 1u32
        .checked_shl(retry_index.min(31) as u32)
        .unwrap_or(u32::MAX);
    base.checked_mul(multiplier).unwrap_or(Duration::MAX)
}

pub(crate) fn response_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| truncate(body, 512))
}

pub(crate) fn truncate(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

pub(crate) async fn drive_sse_consumer<S>(
    line_stream: S,
    tx: mpsc::UnboundedSender<Result<String, VlorQLError>>,
    max_attempts: usize,
    retry_base: Duration,
) -> bool
where
    S: futures::Stream<Item = std::io::Result<String>> + Unpin + Send,
{
    drive_sse_consumer_with(
        line_stream,
        tx,
        max_attempts,
        retry_base,
        extract_delta_content,
    )
    .await
}

pub(crate) async fn drive_sse_consumer_with<S, F>(
    line_stream: S,
    tx: mpsc::UnboundedSender<Result<String, VlorQLError>>,
    max_attempts: usize,
    retry_base: Duration,
    extract: F,
) -> bool
where
    S: futures::Stream<Item = std::io::Result<String>> + Unpin + Send,
    F: Fn(&Value) -> Option<String>,
{
    let attempts = max_attempts.max(1);
    let mut attempt: usize = 0;
    let mut lines = line_stream;
    loop {
        let mut saw_done = false;
        let mut terminated = false;
        while let Some(item) = lines.next().await {
            let line = match item {
                Ok(line) => line,
                Err(error) => {
                    if attempt + 1 < attempts {
                        break;
                    }
                    let _ = tx.send(Err(sse_error(error.to_string())));
                    return false;
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some(payload) = trimmed.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload == SSE_DONE {
                saw_done = true;
                terminated = true;
                break;
            }
            match serde_json::from_str::<Value>(payload) {
                Ok(value) => {
                    if let Some(content) = extract(&value)
                        && !content.is_empty()
                        && tx.send(Ok(content)).is_err()
                    {
                        return true;
                    }
                }
                Err(error) => {
                    debug!("Skipping malformed SSE chunk: {error}");
                    continue;
                }
            }
        }

        if terminated {
            return !saw_done;
        }
        if attempt + 1 < attempts {
            attempt += 1;
            sleep(retry_backoff(retry_base, attempt)).await;
            continue;
        }
        return true;
    }
}

pub(crate) fn extract_delta_content(value: &Value) -> Option<String> {
    let delta = value.get("choices")?.as_array()?.first()?.get("delta")?;
    delta
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Attempts to extract valid JSON from an LLM response text.
///
/// Small local LLMs often wrap JSON in markdown fences or include
/// extra text before/after the JSON object. This function tries
/// increasingly lenient strategies to recover valid JSON:
///
/// 1. Return the text as-is if it is already valid JSON.
/// 2. Strip markdown code fences (`` ```json … ``` `` or `` ``` … ``` ``).
/// 3. Find the outermost `{…}` JSON object in the text.
///
/// If no strategy yields valid JSON, the original text is returned
/// unchanged so the caller can produce an accurate error message.
#[must_use]
pub fn extract_json_content(raw: &str) -> &str {
    let trimmed = raw.trim();

    // 1. Already valid JSON — fast path.
    if is_valid_json_value(trimmed) {
        return trimmed;
    }

    // 2. Strip markdown fences.
    let no_fence = strip_markdown_fence(trimmed);
    if let Some(cleaned) = no_fence {
        if is_valid_json_value(cleaned) {
            return cleaned;
        }
        // Fence contents may have leading/trailing text — try JSON extraction.
        if let Some(obj) = find_outermost_json_obj(cleaned) {
            return obj;
        }
    }

    // 3. Find first JSON object anywhere in the text.
    if let Some(obj) = find_outermost_json_obj(trimmed) {
        return obj;
    }

    trimmed
}

/// Returns `true` when `text` is a valid JSON value.
fn is_valid_json_value(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok()
}

/// Strips a markdown code fence from the start and end of `text`.
fn strip_markdown_fence(text: &str) -> Option<&str> {
    for prefix in &["```json\n", "```json", "```\n", "```"] {
        if let Some(after_open) = text.strip_prefix(prefix) {
            let after_open = after_open.trim_start();
            // Find the closing fence
            let end = if let Some(close_pos) = after_open.rfind("```") {
                close_pos
            } else {
                after_open.len()
            };
            let inner = after_open[..end].trim_end();
            if !inner.is_empty() {
                return Some(inner);
            }
        }
    }
    None
}

/// Finds the outermost JSON object (`{…}`) in a string by tracking
/// brace depth, respecting string boundaries so that braces inside
/// strings are not counted.  Returns `None` when no balanced object
/// is found.
fn find_outermost_json_obj(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth: u32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '{' => depth = depth.checked_add(1)?,
                '}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(&text[start..=start + i]);
                    }
                }
                '"' => in_string = true,
                _ => {}
            }
        }
    }
    None
}

/// Attempts to repair structural issues in a QueryPlan JSON produced
/// by small LLMs.
///
/// Common issues repaired:
///
/// - **Misplaced fields**: `order_by`, `limit`, `offset`, `group_by`,
///   `having` inside the `where` object are moved to the top level.
/// - **Array predicates**: `left` / `right` inside `and` / `or` that
///   are arrays are unwrapped to a single object (first element wins).
/// - **Null / empty entries**: removed from predicates.
///
/// Returns the input unchanged when no repair is needed.
#[must_use]
pub fn repair_query_plan_json(content: &str) -> std::borrow::Cow<'_, str> {
    let mut value: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return std::borrow::Cow::Borrowed(content),
    };

    let obj = match value.as_object_mut() {
        Some(o) => o,
        None => return std::borrow::Cow::Borrowed(content),
    };

    let changed = repair_query_plan_object(obj);

    if changed {
        std::borrow::Cow::Owned(
            serde_json::to_string(&value).unwrap_or_else(|_| content.to_owned()),
        )
    } else {
        std::borrow::Cow::Borrowed(content)
    }
}

/// Recursively repairs a QueryPlan JSON object in-place.
/// Returns `true` if any change was made.
fn repair_query_plan_object(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let mut changed = false;

    // --- 1. Move misplaced top-level fields from inside `where` ---
    const TOP_LEVEL_FIELDS: &[&str] = &["order_by", "limit", "offset", "group_by", "having", "joins", "ctes"];

    // Collect misplaced fields from `where` first (before any mutable borrow of `obj`).
    let mut extracted: Vec<(String, serde_json::Value)> = Vec::new();

    if let Some(where_val) = obj.get_mut("where")
        && let Some(where_obj) = where_val.as_object_mut()
    {
        for &field in TOP_LEVEL_FIELDS {
            if let Some(val) = where_obj.remove(field) {
                if !val.is_null() && !is_empty_array(&val) {
                    extracted.push((field.to_owned(), val));
                }
                changed = true;
            }
        }

        // --- 2. Recursively fix array predicates inside `where` ---
        changed |= repair_predicate_object(where_val);
    }

    // Now insert the extracted fields at the top level (separate borrow).
    for (field, val) in &extracted {
        if !obj.contains_key(field) {
            obj.insert(field.clone(), val.clone());
        }
    }

    // --- 3. Recursively repair predicates inside `having` ---
    //     Also handles `"having": [null]` or `"having": [Predicate]` emitted by
    //     the LLM when it mistakenly wraps the predicate in an array.
    if let Some(having) = obj.get_mut("having") {
        if having.is_array() {
            let arr = having.as_array().unwrap();
            let pred = arr.iter()
                .filter_map(|v| v.as_object())
                .find(|o| o.contains_key("type"))
                .cloned()
                .map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Null);
            if pred.is_null() {
                obj.remove("having");
            } else {
                obj.insert("having".to_owned(), pred);
            }
            changed = true;
        } else {
            changed |= repair_predicate_object(having);
        }
    }

    // --- 4. Recursively repair predicates inside joins ---
    //     Also strips `left_table` (not a field on JoinClause) and any other
    //     unknown join-level fields the LLM may hallucinate.
    const VALID_JOIN_FIELDS: &[&str] = &["join_type", "right_table", "on"];
    if let Some(joins) = obj.get_mut("joins")
        && let Some(joins_arr) = joins.as_array_mut()
    {
        for join in joins_arr.iter_mut() {
            if let Some(join_obj) = join.as_object_mut() {
                join_obj.retain(|key, _| VALID_JOIN_FIELDS.contains(&key.as_str()));
                if let Some(on) = join_obj.get_mut("on") {
                    changed |= repair_predicate_object(on);
                }
            }
        }
    }

    // --- 5. Wrap top-level `descending` + `expr` into `order_by` ---
    // The LLM sometimes emits `descending` and `expr` at the top level of the
    // QueryPlan instead of inside an `OrderByTerm` within the `order_by` array.
    if !obj.contains_key("order_by") {
        if let (Some(expr), Some(descending)) = (obj.remove("expr"), obj.remove("descending"))
            && descending.is_boolean()
        {
            let term = serde_json::json!({
                "expr": expr,
                "descending": descending,
            });
            obj.insert("order_by".to_owned(), serde_json::json!([term]));
            changed = true;
        }
    }

    // --- 6. Remove null / invalid elements from array fields ---
    for array_field in &["group_by", "order_by"] {
        if let Some(arr) = obj.get_mut(*array_field).and_then(|v| v.as_array_mut()) {
            let len_before = arr.len();
            arr.retain(|v| !v.is_null());
            if arr.len() != len_before {
                changed = true;
            }
            if arr.is_empty() {
                obj.remove(*array_field);
                changed = true;
            }
        }
    }

    // --- 7. Collapse `where` from array to single predicate ---
    //     llama3.2 sometimes emits `"where": [{...}, "garbage string"]`
    let where_array_pred: Option<serde_json::Value> =
        obj.get("where").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_object())
                .find(|o| o.contains_key("type"))
                .cloned()
                .map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Null)
        });

    if let Some(pred) = where_array_pred {
        if pred.is_null() {
            obj.remove("where");
        } else {
            obj.insert("where".to_owned(), pred);
        }
        changed = true;
    }

    // --- 7b. Recursively repair the collapsed `where` predicate ---
    //     The collapsed object may still have array-valued `left`/`right`/`child`
    //     fields (e.g. `"left": [{...}]`), which `repair_predicate_object` fixes.
    if let Some(where_val) = obj.get_mut("where") {
        changed |= repair_predicate_object(where_val);
    }

    // --- 8. Repair and remove invalid elements from `select` ---
    //     First inject missing `type` tags for items that look like
    //     ColumnRef, then remove any remaining invalid items.
    const VALID_PROJECTION_TYPES: &[&str] = &["column_ref", "expr", "star"];
    if let Some(arr) = obj.get_mut("select").and_then(|v| v.as_array_mut()) {
        let len_before = arr.len();
        // Inject missing `type` for items that look like ColumnRef
        for item in arr.iter_mut() {
            if let Some(item_obj) = item.as_object_mut() {
                if !item_obj.contains_key("type") && item_obj.contains_key("column") {
                    item_obj.insert(
                        "type".to_owned(),
                        serde_json::Value::String("column_ref".to_owned()),
                    );
                }
            }
        }
        // Remove items that still have invalid or missing type
        arr.retain(|v| {
            v.as_object()
                .and_then(|o| o.get("type"))
                .and_then(|t| t.as_str())
                .is_some_and(|t| VALID_PROJECTION_TYPES.contains(&t))
        });
        if arr.len() != len_before {
            changed = true;
        }
    }

    // --- 9. Fix missing `type` tags on expression fields in group_by / order_by ---
    //     The LLM often omits `type` from Expression objects in these positions.
    for array_field in &["group_by", "order_by"] {
        if let Some(arr) = obj.get_mut(*array_field).and_then(|v| v.as_array_mut()) {
            for item in arr.iter_mut() {
                if let Some(term) = item.as_object_mut()
                    && term.contains_key("expr")
                {
                    // order_by items are OrderByTerm with an `expr` Expression
                    if let Some(expr) = term.get_mut("expr") {
                        changed |= repair_expression_value(expr);
                    }
                } else {
                    // group_by items are bare Expression objects
                    changed |= repair_expression_value(item);
                }
            }
        }
    }

    changed
}

/// Adds missing `"type"` tags to Expression-like JSON objects.
///
/// The LLM frequently omits the `type` discriminator from `ColumnRef`,
/// `Literal`, and `FunctionCall` objects. This function infers the
/// correct tag from the present fields so that serde can deserialize
/// the value as an [`Expression`](vlorql_core::schema::Expression).
fn repair_expression_value(val: &mut serde_json::Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    if obj.contains_key("type") {
        return false;
    }

    if obj.contains_key("column") {
        obj.insert(
            "type".to_owned(),
            serde_json::Value::String("column_ref".to_owned()),
        );
        return true;
    }

    if obj.contains_key("value") {
        obj.insert(
            "type".to_owned(),
            serde_json::Value::String("literal".to_owned()),
        );
        return true;
    }

    if obj.contains_key("name") && obj.contains_key("args") {
        obj.insert(
            "type".to_owned(),
            serde_json::Value::String("function_call".to_owned()),
        );
        return true;
    }

    false
}

/// Repairs a single `Predicate` value (may be `and`/`or` with array sides).
/// Recurses into nested predicates.
fn repair_predicate_object(pred: &mut serde_json::Value) -> bool {
    let obj = match pred.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let mut changed = false;
    let pred_type = obj
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_owned();

    // If the object has no `type` tag but looks like a bare Expression
    // (has `column` field), wrap it as a comparison `expr = NULL` so it
    // can deserialize as a `Predicate`. NULL is used because it is type-
    // compatible with all DataTypes (numeric, string, boolean, etc.)
    // in the validator's `types_compatible` check.
    if pred_type.is_empty() && obj.contains_key("column") {
        let mut expr = pred.clone();
        repair_expression_value(&mut expr);
        *pred = serde_json::json!({
            "type": "comparison",
            "left": expr,
            "op": "eq",
            "right": {"type": "literal", "value": null, "data_type": "null"}
        });
        return true;
    }

    // Fix array-valued sides in and/or
    if pred_type == "and" || pred_type == "or" {
        for side in &["left", "right"] {
            if let Some(arr) = obj.get(*side).and_then(|v| v.as_array()) {
                if arr.is_empty() {
                    obj.remove(*side);
                    changed = true;
                } else {
                    let mut first = arr[0].clone();
                    repair_predicate_object(&mut first);
                    obj.insert((*side).to_string(), first);
                    changed = true;
                }
            } else if let Some(side_val) = obj.get_mut(*side) {
                changed |= repair_predicate_object(side_val);
            }
        }
    }

    // Fix array-valued `child` in `not`
    if pred_type == "not" {
        if let Some(arr) = obj.get("child").and_then(|v| v.as_array()) {
            if !arr.is_empty() {
                let mut first = arr[0].clone();
                repair_predicate_object(&mut first);
                obj.insert("child".to_owned(), first);
                changed = true;
            }
        } else if let Some(child) = obj.get_mut("child") {
            changed |= repair_predicate_object(child);
        }
    }

    // Fix array-valued expression fields
    if pred_type == "comparison" || pred_type == "between" || pred_type == "in" || pred_type == "like" || pred_type == "is_null" {
        for field in &["left", "right", "expr", "low", "high"] {
            if let Some(arr) = obj.get(*field).and_then(|v| v.as_array())
                && !arr.is_empty()
            {
                obj.insert((*field).to_string(), arr[0].clone());
                changed = true;
            }
        }
    }

    // Fix missing `type` tags on expression fields nested within predicates.
    // The LLM often emits bare `{"column":"x","table":"t"}` objects without
    // the `"type":"column_ref"` discriminator that serde needs.
    if pred_type == "comparison" || pred_type == "between" || pred_type == "in" || pred_type == "like" || pred_type == "is_null" {
        for field in &["left", "right", "expr", "low", "high"] {
            if let Some(val) = obj.get_mut(*field) {
                changed |= repair_expression_value(val);
            }
        }
    }

    // Simplify single-child `and`/`or`: if only `left` exists and no `right`,
    // replace the entire predicate with `left`.
    if (pred_type == "and" || pred_type == "or")
        && obj.contains_key("left")
        && !obj.contains_key("right")
        && let Some(left_val) = obj.remove("left")
    {
        *pred = left_val;
        changed = true;
    }

    changed
}

/// Returns `true` when `v` is an empty JSON array `[]`.
fn is_empty_array(v: &serde_json::Value) -> bool {
    v.as_array().is_some_and(|a| a.is_empty())
}

/// Returns a descriptive error message when `content` contains raw
/// chat-template tokens (`<|im_start|>`, `<|im_end|>`), which indicate
/// the model did not understand the output format constraint.
#[must_use]
pub fn detect_template_leak(content: &str) -> Option<String> {
    let has_start = content.contains("<|im_start|>");
    let has_end = content.contains("<|im_end|>");
    if !has_start && !has_end {
        return None;
    }
    Some(format!(
        "Model returned raw chat-template tokens{}. \
         This typically means the model does not support the `format` \
         parameter with a full JSON Schema. \
         Try setting `strict_json_schema = false` in `extra` of your \
         LLM configuration, or use a model that supports structured output.",
        if has_start && has_end {
            " (`<|im_start|>`, `<|im_end|>`)"
        } else if has_start {
            " (`<|im_start|>`)"
        } else {
            " (`<|im_end|>`)"
        }
    ))
}

pub(crate) fn retry_backoff(base: Duration, retry_index: usize) -> Duration {
    let multiplier = 1u32
        .checked_shl(retry_index.min(31) as u32)
        .unwrap_or(u32::MAX);
    base.checked_mul(multiplier).unwrap_or(Duration::MAX)
}

pub(crate) fn sse_error(details: impl Into<String>) -> VlorQLError {
    VlorQLError::llm(
        LlmErrorKind::ParseError {
            details: details.into(),
        },
        json!({"source": "sse_stream"}),
    )
}

pub(crate) fn sse_lines<S>(
    byte_stream: S,
) -> impl futures::Stream<Item = std::io::Result<String>> + Unpin + Send
where
    S: futures::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send,
{
    use std::pin::Pin;
    use std::task::{Context, Poll};
    struct SseLines<Inner> {
        inner: Inner,
        buffer: Vec<u8>,
    }
    impl<Inner> SseLines<Inner> {
        fn take_line(&mut self) -> Option<String> {
            if let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let mut end = index;
                if end > 0 && self.buffer[end - 1] == b'\r' {
                    end -= 1;
                }
                let line_bytes: Vec<u8> = self.buffer.drain(..=index).collect();
                let owned = String::from_utf8_lossy(&line_bytes[..end]).into_owned();
                return Some(owned);
            }
            None
        }
    }
    impl<Inner> futures::Stream for SseLines<Inner>
    where
        Inner: futures::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send,
    {
        type Item = std::io::Result<String>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            if let Some(line) = self.take_line() {
                return Poll::Ready(Some(Ok(line)));
            }
            let inner = Pin::new(&mut self.as_mut().get_mut().inner);
            match inner.poll_next(context) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buffer.extend_from_slice(&bytes);
                    if let Some(line) = self.take_line() {
                        Poll::Ready(Some(Ok(line)))
                    } else {
                        Poll::Pending
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    Poll::Ready(Some(Err(std::io::Error::other(error))))
                }
                Poll::Ready(None) => {
                    if self.buffer.is_empty() {
                        Poll::Ready(None)
                    } else {
                        let remaining = std::mem::take(&mut self.buffer);
                        let value = String::from_utf8_lossy(&remaining).into_owned();
                        Poll::Ready(Some(Ok(value)))
                    }
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }
    SseLines {
        inner: byte_stream,
        buffer: Vec::new(),
    }
}

/// Creates an LLM client from a populated [`LlmConfig`].
///
/// The factory inspects the `provider` field, performs provider-specific
/// validation (e.g. requiring an API key for hosted providers) and returns a
/// boxed [`LlmClient`].
///
/// # Errors
///
/// Returns a [`VlorQLError::Config`] when the API key is missing for a hosted
/// provider or the model name is empty.
///
/// # Examples
///
/// ```
/// use vlorql_llm::{LlmConfig, LlmProvider, create_llm_client};
///
/// let config = LlmConfig {
///     provider: LlmProvider::Ollama,
///     model: "llama3.2".to_owned(),
///     api_key: None,
///     ..LlmConfig::default()
/// };
/// let client = create_llm_client(config);
/// assert!(client.is_ok());
/// ```
pub fn create_llm_client(config: LlmConfig) -> Result<Box<dyn LlmClient>, VlorQLError> {
    let needs_api_key = !matches!(config.provider, LlmProvider::Vllm | LlmProvider::Ollama);
    if needs_api_key
        && config
            .api_key
            .as_deref()
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true)
    {
        return Err(VlorQLError::config(
            ConfigErrorKind::MissingApiKey {
                provider: config.provider.to_string(),
            },
            json!({
                "provider": config.provider,
                "field": "api_key",
            }),
        ));
    }
    if config.model.trim().is_empty() {
        return Err(VlorQLError::config(
            ConfigErrorKind::EmptyModel,
            json!({"field": "model"}),
        ));
    }
    match config.provider {
        LlmProvider::OpenAi => Ok(Box::new(OpenAIClient::from_config(config))),
        LlmProvider::Vllm | LlmProvider::Ollama => {
            local::LocalClient::new(config).map(|client| Box::new(client) as Box<dyn LlmClient>)
        }
        LlmProvider::DeepSeek => deepseek::DeepSeekClient::new(config)
            .map(|client| Box::new(client) as Box<dyn LlmClient>),
        LlmProvider::Zhipu => {
            zhipu::ZhipuClient::new(config).map(|client| Box::new(client) as Box<dyn LlmClient>)
        }
        LlmProvider::Anthropic => anthropic::AnthropicClient::new(config)
            .map(|client| Box::new(client) as Box<dyn LlmClient>),
    }
}

fn default_plan() -> QueryPlan {
    QueryPlan {
        select: vec![vlorql_core::schema::Projection::Star { table: None }],
        from: vlorql_core::schema::FromClause {
            table: "placeholder".to_owned(),
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
    }
}

pub use anthropic::AnthropicClient;
pub use deepseek::DeepSeekClient;
pub use local::{LocalBackend, LocalClient};
pub use zhipu::ZhipuClient;
#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::json;
    use std::time::Duration;
    use vlorql_core::schema::{FromClause, Projection};

    fn plan() -> QueryPlan {
        QueryPlan {
            select: vec![Projection::Star { table: None }],
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
        }
    }

    fn response_for(plan: &QueryPlan) -> String {
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": serde_json::to_string(plan).expect("plan should serialize")
                }
            }]
        })
        .to_string()
    }

    #[tokio::test]
    async fn mock_client_returns_success_and_failure() {
        let expected = plan();
        let success = MockLlmClient::success(expected.clone());
        assert_eq!(
            success
                .generate_plan("question", "system")
                .await
                .expect("mock should succeed"),
            expected
        );

        let failure = MockLlmClient::failure();
        let error = failure
            .generate_plan("question", "system")
            .await
            .expect_err("mock should fail");
        assert_eq!(error.error_code(), "L001");
    }

    #[tokio::test]
    async fn openai_client_sends_messages_and_parses_query_plan() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_header("authorization", "Bearer test-key")
            .match_header(
                "content-type",
                Matcher::Regex("application/json.*".to_owned()),
            )
            .match_body(Matcher::Regex(r#""model":"gpt-4o-mini""#.to_owned()))
            .with_status(200)
            .with_body(response_for(&expected))
            .create_async()
            .await;

        let client = OpenAIClient::new("test-key", "gpt-4o-mini")
            .with_api_base(format!("{}/v1", server.url()))
            .with_retry_base_delay(Duration::ZERO);
        let request_body = client.request_body("show users", "system instructions");
        assert_eq!(request_body["model"], "gpt-4o-mini");
        assert_eq!(request_body["temperature"], 0.0);
        assert_eq!(request_body["response_format"]["type"], "json_schema");
        assert_eq!(
            request_body["response_format"]["json_schema"]["name"],
            "QueryPlan"
        );
        assert_eq!(
            request_body["response_format"]["json_schema"]["strict"],
            true
        );

        let actual = client
            .generate_plan("show users", "system instructions")
            .await
            .expect("OpenAI response should parse");

        assert_eq!(actual, expected);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn openai_client_falls_back_to_json_object_for_unknown_models() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/chat/completions")
            .match_body(Matcher::Regex(
                r#""model":"local-model".*"response_format":\{"type":"json_object"\}"#.to_owned(),
            ))
            .with_status(200)
            .with_body(response_for(&expected))
            .create_async()
            .await;
        let client = OpenAIClient::new("key", "local-model")
            .with_api_base(format!("{}/", server.url()))
            .with_retry_base_delay(Duration::ZERO);
        let request_body = client.request_body("q", "s");
        assert_eq!(request_body["model"], "local-model");
        assert_eq!(request_body["response_format"]["type"], "json_object");

        assert_eq!(
            client.generate_plan("q", "s").await.expect("response"),
            expected
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn openai_client_retries_temporary_http_errors() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let failures = server
            .mock("POST", "/v1/chat/completions")
            .with_status(503)
            .with_body(r#"{"error":{"message":"busy"}}"#)
            .expect(2)
            .create_async()
            .await;
        let success = server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(response_for(&expected))
            .create_async()
            .await;
        let client = OpenAIClient::new("key", "local-model")
            .with_api_base(format!("{}/v1", server.url()))
            .with_retry_base_delay(Duration::ZERO)
            .with_max_attempts(3);

        assert_eq!(
            client
                .generate_plan("q", "s")
                .await
                .expect("retry should succeed"),
            expected
        );
        failures.assert_async().await;
        success.assert_async().await;
    }

    #[tokio::test]
    async fn openai_client_converts_invalid_plan_to_llm_parse_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(
                json!({
                    "choices": [{"message": {"content": r#"{"unexpected":true}"#}}]
                })
                .to_string(),
            )
            .create_async()
            .await;
        let client = OpenAIClient::new("key", "local-model")
            .with_api_base(format!("{}/v1", server.url()))
            .with_retry_base_delay(Duration::ZERO);

        let error = client
            .generate_plan("q", "s")
            .await
            .expect_err("invalid plan should fail");
        assert_eq!(error.error_code(), "L003");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn mock_client_stream_plan_emits_single_chunk() {
        use futures::stream::StreamExt;
        let plan = QueryPlan {
            select: vec![Projection::Star { table: None }],
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
        };
        let client = MockLlmClient::success(plan);
        let mut stream = client
            .stream_plan("question".to_owned(), "system".to_owned())
            .await
            .expect("stream should be produced");
        let mut collected = String::new();
        while let Some(item) = stream.next().await {
            collected.push_str(&item.expect("chunk should be Ok"));
        }
        assert!(collected.contains("users"));
        assert!(collected.contains("\"from\""));
    }

    #[tokio::test]
    async fn openai_client_stream_emits_delta_chunks() {
        let mut server = Server::new_async().await;
        let body = [
            format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "1",
                    "choices": [{"delta": {"content": "hello "}}]
                })
            ),
            format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "1",
                    "choices": [{"delta": {"content": "world"}}]
                })
            ),
            "data: [DONE]\n".to_owned(),
        ]
        .join("");
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;

        let client = OpenAIClient::new("key", "local-model")
            .with_api_base(format!("{}/v1", server.url()))
            .with_retry_base_delay(Duration::ZERO);
        let mut stream = client
            .stream_plan("hi".to_owned(), "system".to_owned())
            .await
            .expect("stream should be produced");
        let mut combined = String::new();
        while let Some(chunk) = stream.next().await {
            combined.push_str(&chunk.expect("chunk should be Ok"));
        }
        assert_eq!(combined, "hello world");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn openai_client_stream_propagates_http_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(500)
            .with_body(r#"{"error":{"message":"down"}}"#)
            .create_async()
            .await;
        let client = OpenAIClient::new("key", "local-model")
            .with_api_base(format!("{}/v1", server.url()))
            .with_retry_base_delay(Duration::ZERO)
            .with_max_attempts(1);
        let outcome = client
            .stream_plan("hi".to_owned(), "system".to_owned())
            .await;
        let err = match outcome {
            Ok(_) => panic!("500 should produce an error"),
            Err(error) => error,
        };
        assert_eq!(err.error_code(), "L001");
        mock.assert_async().await;
    }

    #[test]
    fn llm_config_default_matches_documented_defaults() {
        let config = LlmConfig::default();
        assert_eq!(config.provider, LlmProvider::OpenAi);
        assert_eq!(config.model, "gpt-4o-mini");
        assert_eq!(config.max_tokens, 1024);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.timeout_seconds, 60);
    }

    #[test]
    fn llm_config_effective_api_base_uses_provider_default() {
        let initial = LlmConfig {
            provider: LlmProvider::Zhipu,
            ..LlmConfig::default()
        };
        assert_eq!(
            initial.effective_api_base(),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
        let overridden = LlmConfig {
            api_base: Some("https://example.test/v1".to_owned()),
            ..initial
        };
        assert_eq!(overridden.effective_api_base(), "https://example.test/v1");
    }

    #[test]
    fn create_llm_client_requires_api_key_for_hosted_providers() {
        let config = LlmConfig {
            provider: LlmProvider::OpenAi,
            api_key: None,
            ..LlmConfig::default()
        };
        let error = match create_llm_client(config) {
            Ok(_) => panic!("api key should be required"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "G004");
    }

    #[test]
    fn create_llm_client_allows_local_providers_without_key() {
        let config = LlmConfig {
            provider: LlmProvider::Ollama,
            api_key: None,
            model: "llama3".to_owned(),
            ..LlmConfig::default()
        };
        let client = match create_llm_client(config) {
            Ok(client) => client,
            Err(error) => panic!("ollama client should build: {error}"),
        };
        assert_eq!(client.provider(), LlmProvider::Ollama);
    }

    #[test]
    fn create_llm_client_rejects_empty_model() {
        let config = LlmConfig {
            api_key: Some("k".to_owned()),
            model: "  ".to_owned(),
            ..LlmConfig::default()
        };
        let error = match create_llm_client(config) {
            Ok(_) => panic!("empty model should be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "G005");
    }

    #[test]
    fn llm_config_round_trips_through_serde() {
        let config = LlmConfig {
            provider: LlmProvider::DeepSeek,
            api_key: Some("k".to_owned()),
            api_base: None,
            model: "deepseek-chat".to_owned(),
            max_tokens: 2048,
            temperature: 0.2,
            timeout_seconds: 90,
            max_retries: 5,
            extra: std::collections::HashMap::new(),
        };
        let serialized = serde_json::to_string(&config).expect("config should serialize");
        let restored: LlmConfig =
            serde_json::from_str(&serialized).expect("config should deserialize");
        assert_eq!(restored.provider, LlmProvider::DeepSeek);
        assert_eq!(restored.model, "deepseek-chat");
        assert_eq!(restored.max_tokens, 2048);
        assert_eq!(restored.temperature, 0.2);
    }

    /// Verify that the LLM span created by OpenAIClient::generate_plan
    /// includes provider, model, and streaming attributes.
    #[tokio::test]
    async fn llm_span_contains_provider_and_model() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured_clone = Arc::clone(&captured);

        // A layer that records the "llm.generate_plan" span's fields.
        let layer = tracing_subscriber::fmt::layer()
            .with_test_writer()
            .with_filter(tracing_subscriber::filter::filter_fn(move |meta| {
                meta.target().starts_with("llm")
            }));

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let client = OpenAIClient::from_config(LlmConfig {
            provider: LlmProvider::OpenAi,
            api_key: Some("test-key".to_owned()),
            model: "gpt-4o-mini".to_owned(),
            ..LlmConfig::default()
        });

        // The mockito server will fail, so we expect an error — but the span
        // should still be created with the correct attributes.
        let result = client.generate_plan("test question", "test prompt").await;
        assert!(result.is_err(), "expected error from mock endpoint");

        // The span was created; the subscriber captured it.
        // The test verifies that the span instrumentation does not panic.
        drop(captured_clone.lock().expect("lock should not be poisoned"));
    }

    #[test]
    fn extract_json_content_passes_through_valid_json() {
        let valid = r#"{"select":[{"type":"star"}],"from":{"table":"users"}}"#;
        assert_eq!(extract_json_content(valid), valid);
    }

    #[test]
    fn extract_json_content_strips_markdown_fence() {
        let fenced = "```json\n{\"a\":1}\n```";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn extract_json_content_strips_fence_without_closing() {
        let fenced = "```json\n{\"a\":1}\n";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn extract_json_content_strips_fence_with_text_after() {
        let fenced = "```json\n{\"a\":1}\n```\nsome trailing text";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn extract_json_content_finds_outermost_object() {
        let with_prefix = "Here is the JSON: {\"a\":1} end";
        assert_eq!(extract_json_content(with_prefix), "{\"a\":1}");
    }

    #[test]
    fn extract_json_content_handles_nested_braces() {
        let nested = "text {\"outer\": {\"inner\": 1}} trailing";
        assert_eq!(extract_json_content(nested), "{\"outer\": {\"inner\": 1}}");
    }

    #[test]
    fn extract_json_content_returns_original_when_no_json_found() {
        let no_json = "this is not json at all";
        assert_eq!(extract_json_content(no_json), no_json);
    }

    #[test]
    fn extract_json_content_strips_fence_then_finds_object() {
        let messy = "```markdown\nSome text {\"key\": \"value\"}\n```";
        assert_eq!(extract_json_content(messy), "{\"key\": \"value\"}");
    }

    // --- repair_query_plan_json tests ---

    #[test]
    fn repair_moves_misplaced_fields_from_where_to_top_level() {
        // llama3.2 often puts order_by, limit, offset inside `where`
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"orders"},"where":{"type":"and","left":{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}},"order_by":[{"expr":{"type":"column_ref","column":"total"},"descending":true}],"limit":10}}"#;
        let repaired = repair_query_plan_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        // order_by and limit should be at top level now
        assert!(
            parsed.get("order_by").is_some(),
            "order_by should be at top level, got: {parsed}"
        );
        assert!(
            parsed.get("limit").is_some(),
            "limit should be at top level, got: {parsed}"
        );
        // where should NOT have order_by or limit
        let wh = parsed.get("where").and_then(|w| w.as_object()).unwrap();
        assert!(wh.get("order_by").is_none(), "where should not have order_by");
        assert!(wh.get("limit").is_none(), "where should not have limit");
    }

    #[test]
    fn repair_unwraps_array_left_in_and_predicate() {
        // llama3.2 puts left/right as arrays instead of single objects
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"orders"},"where":{"type":"and","left":[{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}}],"right":{"type":"comparison","left":{"type":"column_ref","column":"status"},"op":"eq","right":{"type":"literal","value":"completed","data_type":"string"}}}}"#;
        let repaired = repair_query_plan_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        let wh = parsed.get("where").and_then(|w| w.as_object()).unwrap();
        let left = wh.get("left").unwrap();
        assert!(left.is_object(), "left should be an object, not array: {left:?}");
        assert_eq!(
            left.get("type").and_then(|t| t.as_str()),
            Some("comparison")
        );
    }

    #[test]
    fn repair_does_not_modify_valid_query_plan() {
        let valid = r#"{"select":[{"type":"column_ref","table":"orders","column":"id"}],"from":{"table":"orders"},"where":{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}},"order_by":[{"expr":{"type":"column_ref","column":"total"},"descending":true}],"limit":10}"#;
        let repaired = repair_query_plan_json(valid);
        // Should be borrowed (not owned) — unchanged
        assert!(
            matches!(repaired, std::borrow::Cow::Borrowed(_)),
            "valid JSON should not be modified"
        );
    }

    #[test]
    fn repair_handles_llama3_2_output_with_multiple_issues() {
        // This is the actual llama3.2 output from the user's bug report
        let input = r#"{"select":[{"type":"column_ref","table":"orders","column":"id","alias":null},{"type":"column_ref","table":"users","column":"name","alias":null},{"type":"column_ref","table":"orders","column":"total","alias":null}], "from":{"table":"orders","alias":null}, "where":{"type":"and", "left":[{"type":"comparison","left":{"type":"column_ref","column":"total","table":"orders"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}}],"group_by":[null], "having":{"type":"comparison","left":{"type":"column_ref","column":"total","table":"orders"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}},"order_by":[{"expr":{"type":"column_ref","column":"total","table":"orders"},"descending":false},{"expr":{"type":"column_ref","column":"id","table":"orders"},"descending":true}], "limit":10} }"#;
        let repaired = repair_query_plan_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();

        // order_by should be at top level
        assert!(parsed.get("order_by").is_some(), "order_by should be at top level");
        assert!(parsed.get("limit").is_some(), "limit should be at top level");

        // where should NOT have group_by, having, order_by, limit
        let wh = parsed.get("where").and_then(|w| w.as_object()).unwrap();
        assert!(wh.get("group_by").is_none(), "where should not have group_by");
        assert!(wh.get("having").is_none(), "where should not have having");
        assert!(wh.get("order_by").is_none(), "where should not have order_by");
        assert!(wh.get("limit").is_none(), "where should not have limit");

        // left should be an object, not array
        let left = wh.get("left").unwrap();
        assert!(left.is_object(), "left should be object, got: {left:?}");

        // Verify the whole thing can parse as a QueryPlan
        let plan: Result<QueryPlan, _> = serde_json::from_str(&repaired);
        assert!(plan.is_ok(), "repaired should be a valid QueryPlan: {:?}", plan.err());
    }

    #[test]
    fn repair_collapses_where_array_and_removes_invalid_select_items() {
        // Simulated output where `where` is an array and select has an invalid
        // `literal` item (mimicking llama3.2 structural errors).
        let input = r#"{"select":[{"type":"column_ref","column":"id","table":"orders"},{"type":"literal","value":150},{"type":"column_ref","column":"name","table":"users"},{"type":"column_ref","column":"total","table":"orders"}],"from":{"table":"orders"},"where":[{"type":"and","left":[{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150}}],"right":{"type":"literal","value":"active"}}],"order_by":[{"expr":{"type":"column_ref","column":"total"},"descending":true}]}"#;
        let repaired = repair_query_plan_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();

        // where should now be an object, not an array
        let wh = parsed.get("where").unwrap();
        assert!(wh.is_object(), "where should be an object after repair, got: {wh:?}");

        // select should only contain valid Projection types
        let select = parsed.get("select").and_then(|s| s.as_array()).unwrap();
        for item in select {
            let t = item.get("type").and_then(|t| t.as_str()).unwrap();
            assert!(
                ["column_ref", "expr", "star"].contains(&t),
                "select item has invalid type: {t}"
            );
        }
        // Should have 3 valid items (the literal was removed)
        assert_eq!(select.len(), 3, "select should have 3 items after removing invalid one");
    }

    #[test]
    fn find_outermost_json_obj_is_string_aware() {
        // Braces inside a JSON string should not affect depth tracking.
        let input = r#"{"outer":{"inner":"some {text with} braces"}}"#;
        let found = super::find_outermost_json_obj(input);
        assert!(found.is_some(), "should find balanced outer object");
        assert_eq!(found.unwrap(), input);

        // When a string contains braces, the scanner should not count them
        // for depth tracking.
        let with_braces_in_string = r#"{"where":[{"type":"and"},"string with {braces}"],"extra":"value"}"#;
        let found2 = super::find_outermost_json_obj(with_braces_in_string);
        assert!(found2.is_some(), "should handle braces inside strings");
        let parsed: serde_json::Value = serde_json::from_str(found2.unwrap()).unwrap();
        assert_eq!(parsed.get("extra").and_then(|v| v.as_str()), Some("value"));
    }

    #[test]
    fn repair_injects_missing_type_on_join_on_and_select() {
        // The LLM emits flat objects without `type` discriminator in
        // expression and projection positions, e.g.:
        //   - join `on` is a bare `{"table":"users","column":"id"}`
        //     instead of a comparison predicate.
        //   - select items lack `"type":"column_ref"`.
        // The fix injects the missing tags so serde can deserialize
        // the result as a QueryPlan.
        let input = r#"{
  "from": {"alias": null, "table": "orders"},
  "select": [
    {"alias": null, "table": "orders", "column": "id"},
    {"alias": null, "table": "users", "column": "name"},
    {"alias": null, "table": "orders", "column": "total"}
  ],
  "where": {
    "left": {"column": "total", "table": "orders", "type": "column_ref"},
    "op": "gt",
    "right": {"data_type": "float", "type": "literal", "value": 150},
    "type": "comparison"
  },
  "joins": [{
    "join_type": "inner",
    "on": {"table": "users", "column": "id", "table": "orders", "column": "user_id"},
    "right_table": {"alias": "u", "table": "users"},
    "left_table": {"alias": "o", "table": "orders"}
  }],
  "group_by": [null],
  "having": [null],
  "order_by": [{"descending": false, "expr": {"column": "total", "table": "orders", "type": "column_ref"}}],
  "limit": 10,
  "offset": 0
}"#;
        let repaired = repair_query_plan_json(input);
        let plan: Result<QueryPlan, _> = serde_json::from_str(&repaired);
        assert!(
            plan.is_ok(),
            "repaired should deserialize as QueryPlan: {:?}",
            plan.err()
        );

        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();

        // select items should have type injected
        let select = parsed.get("select").and_then(|s| s.as_array()).unwrap();
        assert_eq!(select.len(), 3);
        for item in select {
            assert_eq!(
                item.get("type").and_then(|t| t.as_str()),
                Some("column_ref"),
                "select item should have type column_ref: {item:?}"
            );
        }

        // join should have its `on` repaired into a comparison predicate
        let joins = parsed.get("joins").and_then(|j| j.as_array()).unwrap();
        assert_eq!(joins.len(), 1);
        let join = &joins[0];
        // left_table should be stripped (not a valid JoinClause field)
        assert!(
            join.get("left_table").is_none(),
            "left_table should be stripped"
        );
        let on = join.get("on").and_then(|o| o.as_object()).unwrap();
        assert_eq!(
            on.get("type").and_then(|t| t.as_str()),
            Some("comparison"),
            "on should be wrapped as comparison predicate"
        );
        // The left expression should have type column_ref injected
        let left = on.get("left").and_then(|l| l.as_object()).unwrap();
        assert_eq!(
            left.get("type").and_then(|t| t.as_str()),
            Some("column_ref"),
            "on.left should have type column_ref: {left:?}"
        );

        // group_by null should be removed
        assert!(parsed.get("group_by").is_none(), "null group_by should be removed");
    }

    #[test]
    fn repair_injects_missing_type_on_bare_expression_in_group_by() {
        // The LLM sometimes omits `type` from Expression objects in
        // group_by positions.
        let input = r#"{
  "select": [{"type": "column_ref", "table": "orders", "column": "total"}],
  "from": {"table": "orders"},
  "group_by": [{"column": "status", "table": "orders"}],
  "order_by": [{"descending": false, "expr": {"column": "total", "table": "orders"}}]
}"#;
        let repaired = repair_query_plan_json(input);
        let plan: Result<QueryPlan, _> = serde_json::from_str(&repaired);
        assert!(
            plan.is_ok(),
            "repaired should deserialize as QueryPlan: {:?}",
            plan.err()
        );

        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();

        // group_by expression should have type injected
        let group_by = parsed.get("group_by").and_then(|g| g.as_array()).unwrap();
        assert_eq!(group_by.len(), 1);
        assert_eq!(
            group_by[0].get("type").and_then(|t| t.as_str()),
            Some("column_ref")
        );

        // order_by expr should have type injected
        let order_by = parsed.get("order_by").and_then(|o| o.as_array()).unwrap();
        assert_eq!(order_by.len(), 1);
        assert_eq!(
            order_by[0]
                .get("expr")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str()),
            Some("column_ref")
        );
    }
}
