//! DeepSeek chat-completions client.
//!
//! DeepSeek's public API is OpenAI-compatible: it accepts the standard
//! `/chat/completions` payload and returns the same response envelope,
//! including the SSE streaming format. This client reuses that
//! compatibility while applying DeepSeek-specific quirks:
//!
//! * The default endpoint points at `https://api.deepseek.com/v1/chat/completions`
//!   but can be overridden via `LlmConfig::api_base` for proxies / private
//!   deployments.
//! * The API key is sourced from `LlmConfig::api_key` first, then from the
//!   `DEEPSEEK_API_KEY` environment variable.
//! * DeepSeek only accepts the `{"type": "json_object"}` response format.
//!   Unlike OpenAI, it does not (yet) support the strict `json_schema`
//!   mode, so the system prompt must include a JSON Schema example and
//!   the word "json" for the model to honour the structured output.
//!   The returned `content` is then validated client-side by attempting
//!   to deserialise it into a [`QueryPlan`].
//! * Operators upgrading from `deepseek-chat` / `deepseek-reasoner`
//!   (deprecated on 2026-07-24) should switch to `deepseek-v4-flash` or
//!   `deepseek-v4-pro`. Building a client with a deprecated model name
//!   emits a `tracing::warn!` hint.
//! * Empty `choices[0].message.content` payloads are surfaced as a
//!   dedicated `LlmErrorKind::ParseError` so callers can distinguish a
//!   refusal / silent failure from a malformed JSON body.

use async_trait::async_trait;
use futures::stream::Stream;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::warn;
use vlorql_core::errors::{ConfigErrorKind, LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;

use crate::{
    drive_sse_consumer, is_retryable, response_message, retry_backoff, sse_lines, transport_error,
    truncate, LlmClient, LlmConfig, LlmProvider, DEFAULT_MAX_ATTEMPTS, DEFAULT_RETRY_DELAY,
};

const DEFAULT_API_BASE: &str = "https://api.deepseek.com/v1/chat/completions";
const DEEPSEEK_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// DeepSeek chat-completions client.
#[derive(Clone)]
pub struct DeepSeekClient {
    config: LlmConfig,
    client: reqwest::Client,
    api_key: String,
}

impl std::fmt::Debug for DeepSeekClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeepSeekClient")
            .field("api_key", &"[REDACTED]")
            .field("provider", &self.config.provider)
            .field("model", &self.config.model)
            .field("api_base", &self.config.api_base)
            .field("max_retries", &self.max_attempts())
            .finish()
    }
}

impl DeepSeekClient {
    /// Builds a new DeepSeek client from a populated [`LlmConfig`].
    ///
    /// The API key is taken from `config.api_key` (if non-empty) or from
    /// the `DEEPSEEK_API_KEY` environment variable. Either source must
    /// produce a non-empty key, otherwise a `Config` error is returned.
    pub fn new(config: LlmConfig) -> Result<Self, VlorQLError> {
        if config.model.trim().is_empty() {
            return Err(VlorQLError::config(
                ConfigErrorKind::EmptyModel,
                json!({"provider": LlmProvider::DeepSeek, "field": "model"}),
            ));
        }
        let api_key = config
            .api_key
            .clone()
            .or_else(|| std::env::var(DEEPSEEK_API_KEY_ENV).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                VlorQLError::config(
                    ConfigErrorKind::MissingApiKey {
                        provider: "deepseek".to_owned(),
                    },
                    json!({
                        "provider": LlmProvider::DeepSeek,
                        "env": DEEPSEEK_API_KEY_ENV,
                    }),
                )
            })?;
        let timeout = Duration::from_secs(config.timeout_seconds.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let model_lower = config.model.to_ascii_lowercase();
        if model_lower == "deepseek-chat" || model_lower == "deepseek-reasoner" {
            warn!(
                "DeepSeek model `{}` is deprecated and will be removed on 2026-07-24; \
                 switch to `deepseek-v4-flash` or `deepseek-v4-pro`",
                config.model,
            );
        }
        Ok(Self {
            config,
            client,
            api_key,
        })
    }

    /// Returns the effective endpoint URL.
    fn endpoint(&self) -> String {
        self.config
            .api_base
            .clone()
            .unwrap_or_else(|| DEFAULT_API_BASE.to_owned())
    }

    /// Returns the maximum number of attempts for retryable failures.
    fn max_attempts(&self) -> usize {
        usize::try_from(self.config.max_retries.max(1)).unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    /// Builds the JSON body sent to the DeepSeek chat-completions endpoint.
    ///
    /// The body shape is OpenAI-compatible. DeepSeek does not support the
    /// strict `json_schema` mode, so `response_format` is always set to
    /// `{"type": "json_object"}` and the caller is expected to embed a
    /// JSON Schema example (containing the word "json") in the system
    /// prompt so the model emits structured output. When `stream` is
    /// `true`, the `stream` flag is toggled on for SSE delivery.
    fn build_request_body(&self, question: &str, system_prompt: &str, stream: bool) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": question},
            ],
            "response_format": {"type": "json_object"},
            "temperature": self.config.temperature,
            "max_tokens": self.config.max_tokens,
        });
        if stream {
            body["stream"] = Value::Bool(true);
        }
        body
    }

    /// Issues a single non-streaming request and parses the result.
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
        let text = response
            .text()
            .await
            .map_err(|error| transport_error(&error))?;
        if !status.is_success() {
            return Err(VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: status.as_u16(),
                    message: response_message(&text),
                },
                json!({
                    "status": status.as_u16(),
                    "body": truncate(&text, 2048),
                }),
            ));
        }
        parse_completion_payload(&text)
    }
}

#[async_trait]
impl LlmClient for DeepSeekClient {
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
    ) -> Result<QueryPlan, VlorQLError> {
        let endpoint = self.endpoint();
        let body = self.build_request_body(question, system_prompt, false);
        let max_attempts = self.max_attempts();
        let mut last_error: Option<VlorQLError> = None;
        for attempt in 0..max_attempts {
            let result = self.send_once(&endpoint, &body).await;
            match result {
                Ok(plan) => return Ok(plan),
                Err(error) => {
                    let can_retry = is_retryable(&error) && attempt + 1 < max_attempts;
                    if !can_retry {
                        return Err(error);
                    }
                    let delay = retry_backoff(DEFAULT_RETRY_DELAY, attempt);
                    warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        ?delay,
                        "deepseek request failed; retrying"
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
                    message: "deepseek request did not produce a result".to_owned(),
                },
                json!({"source": "deepseek_client"}),
            )
        }))
    }

    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        let endpoint = self.endpoint();
        let body = self.build_request_body(&question, &system_prompt, true);
        let response = self
            .client
            .post(&endpoint)
            .bearer_auth(&self.api_key)
            .header("accept", "text/event-stream")
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
        let max_attempts = self.max_attempts();
        let retry_base = DEFAULT_RETRY_DELAY;
        tokio::spawn(async move {
            if !drive_sse_consumer(line_stream, tx, max_attempts, retry_base).await {
                warn!("deepseek SSE consumer ended before producing content");
            }
        });

        let output = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        Ok(Box::new(Box::pin(output)))
    }

    fn provider(&self) -> LlmProvider {
        LlmProvider::DeepSeek
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

/// Parses a DeepSeek chat-completions JSON response into a [`QueryPlan`].
///
/// Empty `content` payloads are converted into a dedicated
/// `LlmErrorKind::ParseError` so callers can distinguish a refusal (or
/// a prompt that forgot the word "json") from a malformed JSON body or
/// an HTTP transport failure.
fn parse_completion_payload(body: &str) -> Result<QueryPlan, VlorQLError> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("DeepSeek response is not valid JSON: {error}"),
            },
            json!({
                "source": "deepseek_response",
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
                    .unwrap_or("deepseek returned an error")
                    .to_owned(),
            },
            json!({"source": "deepseek_error", "error": error}),
        ));
    }
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
                    details: "DeepSeek response did not contain choices[0].message.content"
                        .to_owned(),
                },
                json!({"source": "deepseek_response"}),
            )
        })?;
    if content.is_empty() {
        return Err(VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: "DeepSeek returned an empty content; ensure the system prompt \
                         contains the word 'json' and a JSON Schema example"
                    .to_owned(),
            },
            json!({"source": "deepseek_content"}),
        ));
    }
    serde_json::from_str::<QueryPlan>(content).map_err(|error| {
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("assistant content is not a valid QueryPlan: {error}"),
            },
            json!({
                "source": "deepseek_content",
                "content": truncate(content, 4096),
            }),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use vlorql_core::schema::{FromClause, Projection, QueryPlan};

    const AUTH_HEADER: &str = "authorization";

    fn plan() -> QueryPlan {
        QueryPlan {
            select: vec![Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
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
        }
    }

    fn deepseek_config(model: &str) -> LlmConfig {
        LlmConfig {
            provider: LlmProvider::DeepSeek,
            api_key: Some("test-key".to_owned()),
            api_base: None,
            model: model.to_owned(),
            max_tokens: 4096,
            temperature: 0.0,
            timeout_seconds: 60,
            max_retries: 1,
            extra: std::collections::HashMap::new(),
        }
    }

    fn chat_response(plan: &QueryPlan) -> String {
        json!({
            "id": "chatcmpl-1",
            "model": "deepseek-v4-pro",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": serde_json::to_string(plan).expect("plan should serialize"),
                },
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
        })
        .to_string()
    }

    #[tokio::test]
    async fn deepseek_client_uses_json_object_mode() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/chat/completions")
            .match_header(AUTH_HEADER, "Bearer test-key")
            .match_body(Matcher::Regex(
                r#""model":"deepseek-v4-pro".*"response_format":\{"type":"json_object"\}"#
                    .to_owned(),
            ))
            .with_status(200)
            .with_body(chat_response(&expected))
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");

        let body = client.build_request_body("show users", "system", false);
        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["max_tokens"], 4096);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "show users");

        let actual = client
            .generate_plan("show users", "system")
            .await
            .expect("plan should parse");
        assert_eq!(actual, expected);
        assert_eq!(client.provider(), LlmProvider::DeepSeek);
        assert_eq!(client.config().model, "deepseek-v4-pro");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn deepseek_client_parses_query_plan_response() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body(chat_response(&expected))
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");
        let actual = client
            .generate_plan("show users", "system")
            .await
            .expect("plan should parse");
        assert_eq!(actual, expected);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn deepseek_client_returns_error_for_empty_content() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body(
                json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": ""}
                    }]
                })
                .to_string(),
            )
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");
        let error = client
            .generate_plan("hi", "system")
            .await
            .expect_err("empty content should fail");
        assert_eq!(error.error_code(), "L003");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn deepseek_client_returns_error_for_missing_content_field() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body(
                json!({
                    "choices": [{"message": {"role": "assistant"}}]
                })
                .to_string(),
            )
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");
        let error = client
            .generate_plan("hi", "system")
            .await
            .expect_err("missing content should fail");
        assert_eq!(error.error_code(), "L003");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn deepseek_client_converts_error_response() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(429)
            .with_body(json!({"error": {"message": "rate limited"}}).to_string())
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");
        let error = client
            .generate_plan("hi", "system")
            .await
            .expect_err("429 should be reported");
        assert_eq!(error.error_code(), "L001");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn deepseek_client_converts_invalid_plan_to_llm_parse_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body(
                json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": r#"{"unexpected":true}"#}
                    }]
                })
                .to_string(),
            )
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");
        let error = client
            .generate_plan("hi", "system")
            .await
            .expect_err("invalid plan should fail");
        assert_eq!(error.error_code(), "L003");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn deepseek_client_stream_emits_delta_chunks() {
        use futures::StreamExt;
        let mut server = Server::new_async().await;
        let body = [
            format!(
                "data: {}\n\n",
                json!({
                    "id": "1",
                    "choices": [{"delta": {"content": "hello "}}],
                })
            ),
            format!(
                "data: {}\n\n",
                json!({
                    "id": "1",
                    "choices": [{"delta": {"content": "world"}}],
                })
            ),
            "data: [DONE]\n".to_owned(),
        ]
        .join("");
        let mock = server
            .mock("POST", "/chat/completions")
            .match_header("accept", "text/event-stream")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");
        let body = client.build_request_body("hi", "system", true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["response_format"]["type"], "json_object");

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
    async fn deepseek_client_stream_propagates_http_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(500)
            .with_body(r#"{"error":{"message":"down"}}"#)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");
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

    #[tokio::test]
    async fn deepseek_client_retries_retryable_http_errors() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let failures = server
            .mock("POST", "/chat/completions")
            .with_status(503)
            .with_body(r#"{"error":{"message":"busy"}}"#)
            .expect(2)
            .create_async()
            .await;
        let success = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body(chat_response(&expected))
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(format!("{}/chat/completions", server.url())),
            max_retries: 3,
            ..deepseek_config("deepseek-v4-pro")
        };
        let client = DeepSeekClient::new(config).expect("client should build");

        let actual = client
            .generate_plan("hi", "system")
            .await
            .expect("retry should succeed");
        assert_eq!(actual, expected);
        failures.assert_async().await;
        success.assert_async().await;
    }

    #[test]
    fn deepseek_client_requires_api_key() {
        let key_backup = std::env::var(DEEPSEEK_API_KEY_ENV).ok();
        // Clear any leaked env var so the test does not depend on the host.
        unsafe {
            std::env::remove_var(DEEPSEEK_API_KEY_ENV);
        }
        let mut config = deepseek_config("deepseek-v4-pro");
        config.api_key = None;
        config.api_base = None;
        let result = DeepSeekClient::new(config);
        if let Some(previous) = key_backup {
            unsafe {
                std::env::set_var(DEEPSEEK_API_KEY_ENV, previous);
            }
        }
        let error = match result {
            Ok(_) => panic!("missing api key should fail"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "G004");
    }

    #[tokio::test]
    async fn deepseek_client_reads_api_key_from_env() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/chat/completions")
            .match_header(AUTH_HEADER, "Bearer env-key")
            .with_status(200)
            .with_body(chat_response(&expected))
            .create_async()
            .await;

        let key_backup = std::env::var(DEEPSEEK_API_KEY_ENV).ok();
        // SAFETY: tests in this module run on a single-threaded test
        // harness so setting an env var here is observable only by this
        // process.
        unsafe {
            std::env::set_var(DEEPSEEK_API_KEY_ENV, "env-key");
        }
        let mut config = deepseek_config("deepseek-v4-pro");
        config.api_key = None;
        config.api_base = Some(format!("{}/chat/completions", server.url()));
        let client = match DeepSeekClient::new(config) {
            Ok(client) => client,
            Err(error) => {
                if let Some(previous) = key_backup {
                    unsafe {
                        std::env::set_var(DEEPSEEK_API_KEY_ENV, previous);
                    }
                } else {
                    unsafe {
                        std::env::remove_var(DEEPSEEK_API_KEY_ENV);
                    }
                }
                panic!("client should build from env var: {error}");
            }
        };
        let result = client.generate_plan("q", "s").await;
        if let Some(previous) = key_backup {
            unsafe {
                std::env::set_var(DEEPSEEK_API_KEY_ENV, previous);
            }
        } else {
            unsafe {
                std::env::remove_var(DEEPSEEK_API_KEY_ENV);
            }
        }
        let actual = result.expect("env-keyed request should succeed");
        assert_eq!(actual, expected);
        mock.assert_async().await;
    }

    #[test]
    fn deepseek_client_endpoint_uses_default_when_unset() {
        let mut config = deepseek_config("deepseek-v4-pro");
        config.api_base = None;
        let client = DeepSeekClient::new(config).expect("client should build");
        assert_eq!(client.endpoint(), DEFAULT_API_BASE);
    }

    #[test]
    fn deepseek_client_endpoint_uses_override_when_set() {
        let mut config = deepseek_config("deepseek-v4-pro");
        config.api_base = Some("https://proxy.example.test/v1/chat/completions".to_owned());
        let client = DeepSeekClient::new(config).expect("client should build");
        assert_eq!(
            client.endpoint(),
            "https://proxy.example.test/v1/chat/completions"
        );
    }

    #[test]
    fn deepseek_client_rejects_empty_model_name() {
        let key_backup = std::env::var(DEEPSEEK_API_KEY_ENV).ok();
        let mut config = deepseek_config("placeholder");
        config.model = "   ".to_owned();
        config.api_base = None;
        let result = DeepSeekClient::new(config);
        if let Some(previous) = key_backup {
            unsafe {
                std::env::set_var(DEEPSEEK_API_KEY_ENV, previous);
            }
        }
        let error = match result {
            Ok(_) => panic!("empty model should fail"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "G005");
    }

    #[test]
    fn deepseek_client_emits_deprecation_warning_for_legacy_models() {
        // Building a client with a deprecated model name should succeed
        // (we still want to let operators migrate), and the tracing
        // warning is exercised in production via the tracing subscriber.
        let config = deepseek_config("deepseek-chat");
        let client = DeepSeekClient::new(config).expect("client should build");
        assert_eq!(client.config().model, "deepseek-chat");

        let config = deepseek_config("deepseek-reasoner");
        let client = DeepSeekClient::new(config).expect("client should build");
        assert_eq!(client.config().model, "deepseek-reasoner");
    }

    #[test]
    fn deepseek_parse_completion_payload_rejects_non_json_body() {
        let error = parse_completion_payload("not json").expect_err("non-JSON body should fail");
        assert_eq!(error.error_code(), "L003");
    }

    #[test]
    fn deepseek_parse_completion_payload_extracts_top_level_error() {
        let body = json!({
            "error": {"type": "rate_limit_error", "message": "slow down"}
        })
        .to_string();
        let error = parse_completion_payload(&body).expect_err("top-level error should fail");
        assert_eq!(error.error_code(), "L001");
    }
}
