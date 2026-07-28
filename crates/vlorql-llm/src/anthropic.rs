//! Anthropic Claude Messages API client.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use vlorql_core::errors::{ConfigErrorKind, LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;

use crate::schema::compact_query_plan_schema;
use crate::sse::{transport_error, truncate};
use crate::{LlmClient, LlmConfig, LlmProvider, RetryableHttpClient, StreamResult, TokenUsage};

const DEFAULT_API_BASE: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const DEFAULT_MAX_ATTEMPTS: usize = 3;

/// Anthropic Claude messages-API client.
#[derive(Clone)]
pub struct AnthropicClient {
    config: LlmConfig,
    client: reqwest::Client,
    api_key: String,
}

impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicClient")
            .field("api_key", &"[REDACTED]")
            .field("provider", &self.config.provider)
            .field("model", &self.config.model)
            .field("api_base", &self.config.api_base)
            .field("max_retries", &self.max_attempts())
            .finish()
    }
}

impl AnthropicClient {
    /// Creates a new Anthropic client from the given configuration.
    pub fn new(config: LlmConfig) -> Result<Self, VlorQLError> {
        let api_key = config
            .api_key
            .clone()
            .or_else(|| std::env::var(ANTHROPIC_API_KEY_ENV).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                VlorQLError::config(
                    ConfigErrorKind::MissingApiKey {
                        provider: "anthropic".to_owned(),
                    },
                    json!({
                        "provider": "anthropic",
                        "field": "api_key",
                        "env": ANTHROPIC_API_KEY_ENV,
                    }),
                )
            })?;
        let timeout = Duration::from_secs(config.timeout_seconds.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Ok(Self {
            config,
            client,
            api_key,
        })
    }

    fn endpoint(&self) -> String {
        self.config
            .api_base
            .clone()
            .unwrap_or_else(|| DEFAULT_API_BASE.to_owned())
    }

    fn build_request_body(
        &self,
        question: &str,
        system_prompt: &str,
        stream: bool,
        temperature: Option<f32>,
    ) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "system": system_prompt,
            "messages": [
                {
                    "role": "user",
                    "content": question,
                }
            ],
            "temperature": temperature.unwrap_or(self.config.temperature),
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": compact_query_plan_schema(),
                }
            },
        });
        if stream {
            body["stream"] = Value::Bool(true);
        }
        body
    }
}

#[async_trait]
impl RetryableHttpClient for AnthropicClient {
    fn max_attempts(&self) -> usize {
        usize::try_from(self.config.max_retries.max(1)).unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    fn provider_label(&self) -> &'static str {
        "anthropic"
    }

    fn parse_response(&self, body: &str) -> Result<QueryPlan, VlorQLError> {
        parse_completion_payload(body)
    }

    async fn send_request(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<reqwest::Response, VlorQLError> {
        self.client
            .post(endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|error| transport_error(&error))
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
        temperature: Option<f32>,
    ) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
        let endpoint = self.endpoint();
        let body = self.build_request_body(question, system_prompt, false, temperature);
        self.generate_with_retry(&endpoint, &body).await
    }

    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<StreamResult, VlorQLError> {
        let endpoint = self.endpoint();
        let body = self.build_request_body(&question, &system_prompt, true, None);
        let usage = Arc::new(tokio::sync::Mutex::new(None));
        let usage_clone = Arc::clone(&usage);
        let extract = move |data: &Value| {
            // Anthropic: input_tokens in message_start, output_tokens in message_delta
            if let Some(u) = data.get("usage")
                && let Ok(mut guard) = usage_clone.try_lock()
            {
                let current = guard.get_or_insert(TokenUsage::default());
                current.prompt_tokens = u
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(current.prompt_tokens);
                current.completion_tokens = u
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(current.completion_tokens);
            }
            if let Some(msg) = data.get("message")
                && let Some(u) = msg.get("usage")
                && let Ok(mut guard) = usage_clone.try_lock()
            {
                let current = guard.get_or_insert(TokenUsage::default());
                current.prompt_tokens = u
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(current.prompt_tokens);
                current.completion_tokens = u
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(current.completion_tokens);
            }
            extract_delta_text(data)
        };
        let stream = self.stream_with_sse(&endpoint, &body, extract).await?;
        Ok(StreamResult { stream, usage })
    }

    fn provider(&self) -> LlmProvider {
        LlmProvider::Anthropic
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

fn parse_completion_payload(body: &str) -> Result<QueryPlan, VlorQLError> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("Anthropic response is not valid JSON: {error}"),
            },
            json!({
                "source": "anthropic_response",
                "body": truncate(body, 1024),
            }),
        )
    })?;
    if let Some(error) = value.get("error") {
        return Err(VlorQLError::llm(
            LlmErrorKind::ApiError {
                status: 0,
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("anthropic returned an error")
                    .to_owned(),
            },
            json!({"source": "anthropic_error", "error": error}),
        ));
    }
    let text = value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find_map(|item| item.get("text").and_then(Value::as_str))
        })
        .ok_or_else(|| {
            VlorQLError::llm(
                LlmErrorKind::ParseError {
                    details: "Anthropic response did not contain content[].text".to_owned(),
                },
                json!({"source": "anthropic_response"}),
            )
        })?;
    crate::parse_llm_response(text).map_err(|error| {
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("assistant content is not a valid QueryPlan: {error}"),
            },
            json!({
                "source": "anthropic_content",
                "text": truncate(text, 2048),
            }),
        )
    })
}

fn extract_delta_text(value: &Value) -> Option<String> {
    // Anthropic streaming events use { type: "content_block_delta", delta: { type: "text_delta", text: "..." } }
    let delta = value.get("delta")?;
    delta.get("text").and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;
    use vlorql_core::schema::{FromClause, Projection, QueryPlan};

    fn query_plan() -> QueryPlan {
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

    fn anthropic_config() -> LlmConfig {
        LlmConfig {
            provider: LlmProvider::Anthropic,
            api_key: Some("test-key".to_owned()),
            model: "claude-sonnet-4-5".to_owned(),
            max_tokens: 4096,
            ..LlmConfig::default()
        }
    }

    #[tokio::test]
    async fn anthropic_client_parses_completion_response() {
        let mut server = Server::new_async().await;
        let plan = query_plan();
        let serialized = serde_json::to_string(&plan).expect("plan should serialize");
        let body = json!({
            "id": "msg_1",
            "content": [{"type": "text", "text": serialized}],
        })
        .to_string();
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "test-key")
            .match_header("anthropic-version", "2023-06-01")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/v1/messages", server.url())),
            ..anthropic_config()
        };
        let client = AnthropicClient::new(config).expect("client should build");
        let (actual, _usage) = client
            .generate_plan("hi", "system", None)
            .await
            .expect("anthropic plan should parse");
        assert_eq!(actual, plan);
        assert_eq!(client.provider(), LlmProvider::Anthropic);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn anthropic_client_converts_error_response() {
        let mut server = Server::new_async().await;
        let body = json!({
            "error": {"type": "rate_limit", "message": "too many requests"}
        })
        .to_string();
        let mock = server
            .mock("POST", "/v1/messages")
            .with_status(429)
            .with_body(body)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/v1/messages", server.url())),
            max_retries: 1,
            ..anthropic_config()
        };
        let client = AnthropicClient::new(config).expect("client should build");
        let error = client
            .generate_plan("hi", "system", None)
            .await
            .expect_err("rate limit should be reported");
        assert_eq!(error.error_code(), "L001");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn anthropic_client_stream_emits_text_chunks() {
        use futures::StreamExt;
        let mut server = Server::new_async().await;
        let first = format!(
            "data: {}\n\n",
            serde_json::json!({
                "type": "content_block_delta",
                "delta": {"type": "text_delta", "text": "hello "}
            })
        );
        let second = format!(
            "data: {}\n\n",
            serde_json::json!({
                "type": "content_block_delta",
                "delta": {"type": "text_delta", "text": "world"}
            })
        );
        let body = [first, second, "data: [DONE]\n".to_owned()].concat();
        let mock = server
            .mock("POST", "/v1/messages")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/v1/messages", server.url())),
            ..anthropic_config()
        };
        let client = AnthropicClient::new(config).expect("client should build");
        let mut result = client
            .stream_plan("hi".to_owned(), "system".to_owned())
            .await
            .expect("stream should be produced");
        let mut combined = String::new();
        while let Some(item) = result.stream.next().await {
            combined.push_str(&item.expect("chunk should be Ok"));
        }
        assert_eq!(combined, "hello world");
        mock.assert_async().await;
    }

    #[test]
    fn anthropic_client_requires_api_key() {
        let config = LlmConfig {
            provider: LlmProvider::Anthropic,
            api_key: None,
            model: "claude-sonnet-4-5".to_owned(),
            ..LlmConfig::default()
        };
        assert!(AnthropicClient::new(config).is_err());
    }
}
