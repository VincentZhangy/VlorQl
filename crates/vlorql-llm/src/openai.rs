use crate::schema::compact_query_plan_schema;
use crate::sse::{transport_error, truncate};
use crate::{DEFAULT_API_BASE, DEFAULT_MAX_ATTEMPTS};
use crate::{LlmClient, LlmConfig, LlmProvider, RetryableHttpClient, StreamResult, TokenUsage};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tracing::Instrument;
use vlorql_core::errors::{LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;

/// OpenAI-compatible chat-completions client.
#[derive(Clone)]
pub struct OpenAIClient {
    client: reqwest::Client,
    api_key: String,
    model: String,
    api_base: Option<String>,
    strict_json_schema_override: Option<bool>,
    config: LlmConfig,
}

impl std::fmt::Debug for OpenAIClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAIClient")
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("api_base", &self.api_base)
            .field("max_attempts", &self.max_attempts())
            .field(
                "strict_json_schema_override",
                &self.strict_json_schema_override,
            )
            .field("provider", &self.config.provider)
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
        let timeout = Duration::from_secs(config.timeout_seconds);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            api_key,
            model: config.model.clone(),
            api_base: config.api_base.clone(),
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

    pub(crate) fn request_body(
        &self,
        question: &str,
        system_prompt: &str,
        temperature: Option<f32>,
    ) -> Value {
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
            "temperature": temperature.unwrap_or(self.config.temperature),
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
impl RetryableHttpClient for OpenAIClient {
    fn max_attempts(&self) -> usize {
        usize::try_from(self.config.max_retries.max(1)).unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    fn provider_label(&self) -> &'static str {
        "openai"
    }

    fn parse_response(&self, body: &str) -> Result<QueryPlan, VlorQLError> {
        let value: Value = serde_json::from_str(body).map_err(|error| {
            VlorQLError::llm(
                LlmErrorKind::ParseError {
                    details: format!("OpenAI response is not valid JSON: {error}"),
                },
                json!({
                    "source": "provider_response",
                    "body": truncate(body, 2048),
                }),
            )
        })?;
        let content = value
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
        crate::parse_llm_response(content).map_err(|error| {
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

    async fn send_request(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<reqwest::Response, VlorQLError> {
        self.client
            .post(endpoint)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|error| transport_error(&error))
    }
}

#[async_trait]
impl LlmClient for OpenAIClient {
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
        temperature: Option<f32>,
    ) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
        let span = tracing::info_span!(
            "llm.generate_plan",
            provider = ?self.provider(),
            model = %self.config().model,
            prompt_len = system_prompt.len(),
            streaming = false,
        );
        async move {
            let endpoint = self.endpoint();
            let body = self.request_body(question, system_prompt, temperature);
            self.generate_with_retry(&endpoint, &body).await
        }
        .instrument(span)
        .await
    }

    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<StreamResult, VlorQLError> {
        let span = tracing::info_span!(
            "llm.stream_plan",
            provider = ?self.provider(),
            model = %self.config().model,
            prompt_len = system_prompt.len(),
            streaming = true,
        );
        async move {
            let endpoint = self.endpoint();
            let body = self.streaming_request_body(&question, &system_prompt);
            let usage = Arc::new(tokio::sync::Mutex::new(None));
            let usage_clone = Arc::clone(&usage);
            let extract = move |data: &Value| {
                if let Some(u) = data.get("usage") {
                    if let Ok(mut guard) = usage_clone.try_lock() {
                        *guard = Some(TokenUsage {
                            prompt_tokens: u
                                .get("prompt_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            completion_tokens: u
                                .get("completion_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                        });
                    }
                }
                data.pointer("/choices/0/delta/content")
                    .and_then(|v| v.as_str().map(String::from))
            };
            let stream = self.stream_with_sse(&endpoint, &body, extract).await?;
            Ok(StreamResult { stream, usage })
        }
        .instrument(span)
        .await
    }

    fn provider(&self) -> LlmProvider {
        self.config.provider
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

fn model_supports_strict_json_schema(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("gpt-4o")
        || model.starts_with("gpt-4.1")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}
