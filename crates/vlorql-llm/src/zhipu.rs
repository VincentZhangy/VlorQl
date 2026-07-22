//! Zhipu GLM chat-completions client.
//!
//! Zhipu's public API is OpenAI-compatible: it accepts the standard
//! `/chat/completions` payload and returns the same response envelope,
//! including the SSE streaming format. This client reuses that
//! compatibility while applying a few Zhipu-specific defaults:
//!
//! * The default endpoint points at `https://open.bigmodel.cn/api/paas/v4/chat/completions`
//!   but can be overridden via `LlmConfig::api_base` for proxies / private
//!   deployments.
//! * The API key is sourced from `LlmConfig::api_key` first, then from the
//!   `ZHIPU_API_KEY` environment variable.
//! * GLM-4.7+ natively supports `{"type": "json_schema", ...}` payloads.
//!   Older GLM models (e.g. `glm-4`, `glm-3-turbo`) only accept the
//!   looser `{"type": "json_object"}`; the client picks the appropriate
//!   shape based on the configured model name.
//! * GLM-4.7 supports a 200K context window, so the client raises the
//!   `max_tokens` default to 4 096 when the operator leaves it at the
//!   crate-wide 1 024 placeholder.

use async_trait::async_trait;
use futures::stream::Stream;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::warn;
use vlorql_core::errors::{ConfigErrorKind, LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;

use crate::{
    DEFAULT_MAX_ATTEMPTS, DEFAULT_RETRY_DELAY, LlmClient, LlmConfig, LlmProvider,
    compact_query_plan_schema, drive_sse_consumer, is_retryable, response_message, retry_backoff,
    sse_lines, transport_error, truncate,
};

const DEFAULT_API_BASE: &str = "https://open.bigmodel.cn/api/paas/v4/chat/completions";
const ZHIPU_API_KEY_ENV: &str = "ZHIPU_API_KEY";
const DEFAULT_MAX_TOKENS: u32 = 4_096;
const MIN_MAX_TOKENS: u32 = 256;

/// Zhipu GLM chat-completions client.
#[derive(Clone)]
pub struct ZhipuClient {
    config: LlmConfig,
    client: reqwest::Client,
    api_key: String,
}

impl std::fmt::Debug for ZhipuClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ZhipuClient")
            .field("api_key", &"[REDACTED]")
            .field("provider", &self.config.provider)
            .field("model", &self.config.model)
            .field("api_base", &self.config.api_base)
            .field("max_retries", &self.max_attempts())
            .finish()
    }
}

impl ZhipuClient {
    /// Builds a new Zhipu client from a populated [`LlmConfig`].
    ///
    /// The API key is taken from `config.api_key` (if non-empty) or from
    /// the `ZHIPU_API_KEY` environment variable. Either source must
    /// produce a non-empty key, otherwise a `Config` error is returned.
    pub fn new(config: LlmConfig) -> Result<Self, VlorQLError> {
        if config.model.trim().is_empty() {
            return Err(VlorQLError::config(
                ConfigErrorKind::EmptyModel,
                json!({"provider": LlmProvider::Zhipu, "field": "model"}),
            ));
        }
        let api_key = config
            .api_key
            .clone()
            .or_else(|| std::env::var(ZHIPU_API_KEY_ENV).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                VlorQLError::config(
                    ConfigErrorKind::MissingApiKey {
                        provider: "zhipu".to_owned(),
                    },
                    json!({
                        "provider": LlmProvider::Zhipu,
                        "env": ZHIPU_API_KEY_ENV,
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

    /// Returns the `max_tokens` value sent on the wire.
    ///
    /// Zhipu's GLM-4.7+ supports a 200K context window, so the default
    /// 1 024 budget shipped with `LlmConfig` is raised to 4 096 unless
    /// the operator explicitly set a different value.
    fn effective_max_tokens(&self) -> u32 {
        if self.config.max_tokens == 0 {
            DEFAULT_MAX_TOKENS
        } else if self.config.max_tokens < MIN_MAX_TOKENS {
            MIN_MAX_TOKENS
        } else {
            self.config.max_tokens
        }
    }

    /// Returns whether the configured model supports Zhipu's strict
    /// `json_schema` response format.
    ///
    /// GLM-4.7 (and later) and the GLM-5 family accept the full
    /// `{ "type": "json_schema", "json_schema": { ... } }` payload. Older
    /// models only accept `{ "type": "json_object" }` and rely on the
    /// system prompt to coerce the JSON shape. Operators can also force
    /// the choice via `LlmConfig::extra["strict_json_schema"]` (boolean).
    fn supports_strict_json_schema(&self) -> bool {
        if let Some(override_value) = self.config.extra.get("strict_json_schema")
            && let Some(flag) = override_value.as_bool()
        {
            return flag;
        }
        let model = self.config.model.to_ascii_lowercase();
        model.starts_with("glm-4.7")
            || model.starts_with("glm-4.8")
            || model.starts_with("glm-4.9")
            || model.starts_with("glm-5")
            || model.starts_with("glm-6")
    }

    /// Builds the JSON body sent to the Zhipu chat-completions endpoint.
    ///
    /// The body shape is OpenAI-compatible. `response_format` is selected
    /// based on [`Self::supports_strict_json_schema`]. When `stream` is
    /// `true`, `stream` is set on the body and `tool_stream` is enabled
    /// for the GLM-5 family (Zhipu's documented flag for streaming tool
    /// calls). Function calling is not used in this codebase, but the
    /// flag is wired through so callers can opt in via the model name.
    fn build_request_body(&self, question: &str, system_prompt: &str, stream: bool) -> Value {
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
        let model = self.config.model.to_ascii_lowercase();
        let supports_tool_stream = model.starts_with("glm-5") || model.starts_with("glm-6");
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": question},
            ],
            "temperature": self.config.temperature,
            "max_tokens": self.effective_max_tokens(),
            "response_format": response_format,
        });
        if stream {
            body["stream"] = Value::Bool(true);
            if supports_tool_stream {
                body["tool_stream"] = Value::Bool(true);
            }
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
impl LlmClient for ZhipuClient {
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
                        "zhipu request failed; retrying"
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
                    message: "zhipu request did not produce a result".to_owned(),
                },
                json!({"source": "zhipu_client"}),
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
                warn!("zhipu SSE consumer ended before producing content");
            }
        });

        let output = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        Ok(Box::new(Box::pin(output)))
    }

    fn provider(&self) -> LlmProvider {
        LlmProvider::Zhipu
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

/// Parses a Zhipu chat-completions JSON response into a [`QueryPlan`].
fn parse_completion_payload(body: &str) -> Result<QueryPlan, VlorQLError> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("Zhipu response is not valid JSON: {error}"),
            },
            json!({
                "source": "zhipu_response",
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
                    .unwrap_or("zhipu returned an error")
                    .to_owned(),
            },
            json!({"source": "zhipu_error", "error": error}),
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
                    details: "Zhipu response did not contain choices[0].message.content".to_owned(),
                },
                json!({"source": "zhipu_response"}),
            )
        })?;
    crate::parse_llm_response(content).map_err(|error| {
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("assistant content is not a valid QueryPlan: {error}"),
            },
            json!({
                "source": "zhipu_content",
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

    fn plan() -> QueryPlan {
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
        }
    }

    fn zhipu_config(model: &str) -> LlmConfig {
        LlmConfig {
            provider: LlmProvider::Zhipu,
            api_key: Some("test-key".to_owned()),
            api_base: Some("http://127.0.0.1:0/v4/chat/completions".to_owned()),
            model: model.to_owned(),
            max_tokens: 1024,
            temperature: 0.0,
            timeout_seconds: 60,
            max_retries: 1,
            extra: std::collections::HashMap::new(),
        }
    }

    fn chat_response(plan: &QueryPlan) -> String {
        json!({
            "id": "chatcmpl-1",
            "model": "glm-4.7",
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
    async fn zhipu_client_uses_strict_json_schema_for_glm_4_7() {
        let mut server = Server::new_async().await;
        let expected = plan();
        // Note: mockito's regex matcher parses `\{` as a literal `\{` (the
        // regex crate does not collapse the escape), so the body matcher
        // intentionally omits the brace and relies on the surrounding
        // JSON keys instead. The companion assertions below verify the
        // brace-shaped payload directly.
        let mock = server
            .mock("POST", "/v4/chat/completions")
            .match_body(Matcher::Regex(
                r#""model":"glm-4\.7".*"name":"QueryPlan".*"strict":true"#.to_owned(),
            ))
            .with_status(200)
            .with_body(chat_response(&expected))
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(format!("{}/v4/chat/completions", server.url())),
            ..zhipu_config("glm-4.7")
        };
        let client = ZhipuClient::new(config).expect("client should build");
        let request_body = client.build_request_body("show users", "system", false);
        assert_eq!(request_body["model"], "glm-4.7");
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
            .generate_plan("show users", "system")
            .await
            .expect("zhipu plan should parse");
        assert_eq!(actual, expected);
        assert_eq!(client.provider(), LlmProvider::Zhipu);
        let config_ref = client.config();
        assert_eq!(config_ref.model, "glm-4.7");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn zhipu_client_falls_back_to_json_object_for_older_models() {
        let mut server = Server::new_async().await;
        let expected = plan();
        // The matcher deliberately avoids `\{` because the regex crate
        // matches that as a literal two-character sequence rather than
        // a single `{`. Key-only matching is sufficient for the
        // contract this test exercises.
        let mock = server
            .mock("POST", "/v4/chat/completions")
            .match_body(Matcher::Regex(
                r#""model":"glm-4".*"response_format".*"type":"json_object""#.to_owned(),
            ))
            .with_status(200)
            .with_body(chat_response(&expected))
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(format!("{}/v4/chat/completions", server.url())),
            ..zhipu_config("glm-4")
        };
        let client = ZhipuClient::new(config).expect("client should build");
        let request_body = client.build_request_body("q", "s", false);
        assert_eq!(request_body["model"], "glm-4");
        assert_eq!(request_body["response_format"]["type"], "json_object");

        assert_eq!(
            client.generate_plan("q", "s").await.expect("response"),
            expected
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn zhipu_client_converts_error_response() {
        let mut server = Server::new_async().await;
        let body = json!({
            "error": {"code": "invalid_api_key", "message": "bad key"}
        })
        .to_string();
        let mock = server
            .mock("POST", "/v4/chat/completions")
            .with_status(401)
            .with_body(body)
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(format!("{}/v4/chat/completions", server.url())),
            ..zhipu_config("glm-4.7")
        };
        let client = ZhipuClient::new(config).expect("client should build");
        let error = client
            .generate_plan("hi", "system")
            .await
            .expect_err("401 should be reported");
        assert_eq!(error.error_code(), "L001");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn zhipu_client_converts_invalid_plan_to_llm_parse_error() {
        let mut server = Server::new_async().await;
        let body = json!({
            "choices": [{"message": {"role": "assistant", "content": r#"{"unexpected":true}"#}}]
        })
        .to_string();
        let mock = server
            .mock("POST", "/v4/chat/completions")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/v4/chat/completions", server.url())),
            ..zhipu_config("glm-4.7")
        };
        let client = ZhipuClient::new(config).expect("client should build");
        let error = client
            .generate_plan("hi", "system")
            .await
            .expect_err("invalid plan should fail");
        assert_eq!(error.error_code(), "L003");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn zhipu_client_stream_emits_delta_chunks() {
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
            .mock("POST", "/v4/chat/completions")
            .match_header("accept", "text/event-stream")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/v4/chat/completions", server.url())),
            ..zhipu_config("glm-4.7")
        };
        let client = ZhipuClient::new(config).expect("client should build");
        let body = client.build_request_body("hi", "system", true);
        assert_eq!(body["stream"], true);

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
    async fn zhipu_client_stream_propagates_http_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/v4/chat/completions")
            .with_status(500)
            .with_body(r#"{"error":{"message":"down"}}"#)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/v4/chat/completions", server.url())),
            ..zhipu_config("glm-4.7")
        };
        let client = ZhipuClient::new(config).expect("client should build");
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
    fn zhipu_client_requires_api_key() {
        // Save and clear any pre-existing ZHIPU_API_KEY so the test is
        // deterministic regardless of host environment or sibling tests
        // that mutate the variable.
        let key_backup = std::env::var(ZHIPU_API_KEY_ENV).ok();
        // SAFETY: tests in this module share a single process; we
        // restore the previous value before returning so other tests
        // see the original environment.
        unsafe { std::env::remove_var(ZHIPU_API_KEY_ENV) };
        let mut config = zhipu_config("glm-4.7");
        config.api_key = None;
        config.api_base = None;
        let error = match ZhipuClient::new(config) {
            Ok(client) => {
                if let Some(previous) = key_backup {
                    unsafe { std::env::set_var(ZHIPU_API_KEY_ENV, previous) };
                }
                panic!("missing api key should fail; got client {client:?}");
            }
            Err(error) => {
                if let Some(previous) = key_backup {
                    unsafe { std::env::set_var(ZHIPU_API_KEY_ENV, previous) };
                }
                error
            }
        };
        assert_eq!(error.error_code(), "G004");
    }

    #[tokio::test]
    async fn zhipu_client_reads_api_key_from_env() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/v4/chat/completions")
            .match_header("authorization", "Bearer env-key")
            .with_status(200)
            .with_body(chat_response(&expected))
            .create_async()
            .await;

        let key_backup = std::env::var(ZHIPU_API_KEY_ENV).ok();
        // SAFETY: tests in this module run on a single process; we
        // restore the previous value via the `_guard` drop handle below
        // even on panic.
        struct RestoreEnv(Option<String>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                // SAFETY: see comment above.
                unsafe {
                    match self.0.take() {
                        Some(previous) => std::env::set_var(ZHIPU_API_KEY_ENV, previous),
                        None => std::env::remove_var(ZHIPU_API_KEY_ENV),
                    }
                }
            }
        }
        let _guard = RestoreEnv(key_backup.clone());
        unsafe {
            std::env::set_var(ZHIPU_API_KEY_ENV, "env-key");
        }
        let mut config = zhipu_config("glm-4.7");
        config.api_key = None;
        config.api_base = Some(format!("{}/v4/chat/completions", server.url()));
        let client = ZhipuClient::new(config).expect("client should build from env var");
        let actual = client
            .generate_plan("q", "s")
            .await
            .expect("env-keyed request should succeed");
        assert_eq!(actual, expected);
        mock.assert_async().await;
    }

    #[test]
    fn zhipu_client_effective_max_tokens_floors_low_values() {
        let mut config = zhipu_config("glm-4.7");
        config.max_tokens = 0;
        config.api_base = None;
        let client = ZhipuClient::new(config).expect("client should build");
        assert_eq!(client.effective_max_tokens(), DEFAULT_MAX_TOKENS);

        let mut config = zhipu_config("glm-4.7");
        config.max_tokens = 64;
        config.api_base = None;
        let client = ZhipuClient::new(config).expect("client should build");
        assert_eq!(client.effective_max_tokens(), MIN_MAX_TOKENS);
    }

    #[test]
    fn zhipu_client_endpoint_uses_default_when_unset() {
        let mut config = zhipu_config("glm-4.7");
        config.api_base = None;
        let client = ZhipuClient::new(config).expect("client should build");
        assert_eq!(client.endpoint(), DEFAULT_API_BASE);
    }

    #[test]
    fn zhipu_client_supports_strict_json_schema_detection() {
        let mut config = zhipu_config("glm-3-turbo");
        config.api_base = None;
        let client = ZhipuClient::new(config).expect("client should build");
        assert!(!client.supports_strict_json_schema());

        let mut config = zhipu_config("glm-4");
        config.api_base = None;
        let client = ZhipuClient::new(config).expect("client should build");
        assert!(!client.supports_strict_json_schema());

        let mut config = zhipu_config("glm-4.7");
        config.api_base = None;
        let client = ZhipuClient::new(config).expect("client should build");
        assert!(client.supports_strict_json_schema());

        let mut config = zhipu_config("glm-5-air");
        config.api_base = None;
        let client = ZhipuClient::new(config).expect("client should build");
        assert!(client.supports_strict_json_schema());

        let mut config = zhipu_config("glm-4.6");
        config.api_base = None;
        config
            .extra
            .insert("strict_json_schema".to_owned(), Value::Bool(true));
        let client = ZhipuClient::new(config).expect("client should build");
        assert!(client.supports_strict_json_schema());

        let mut config = zhipu_config("glm-4.7");
        config.api_base = None;
        config
            .extra
            .insert("strict_json_schema".to_owned(), Value::Bool(false));
        let client = ZhipuClient::new(config).expect("client should build");
        assert!(!client.supports_strict_json_schema());
    }

    #[test]
    fn zhipu_query_plan_schema_round_trips_through_parse() {
        // Sanity check: the schema we send to Zhipu must serialise to
        // valid JSON (Zhipu rejects malformed JSON Schema payloads).
        let value = compact_query_plan_schema();
        let serialized = serde_json::to_string(&value).expect("schema should serialize");
        let _: Value = serde_json::from_str(&serialized).expect("schema should parse");
    }
}
