//! Local LLM clients (`vLLM` and `Ollama`).
//!
//! Both engines speak an OpenAI-inspired chat-completion dialect. This
//! module unifies them behind a single [`LocalClient`] whose request and
//! response shapes are selected at runtime via the [`LocalBackend`] enum.
//!
//! ## Backend selection
//!
//! The backend is taken from `LlmConfig::extra["backend"]` when present
//! (values: `"vllm"` or `"ollama"`). When absent, the backend defaults to
//! the configured provider (`Vllm` -> [`LocalBackend::VLLM`],
//! `Ollama` -> [`LocalBackend::Ollama`]) and finally falls back to
//! [`LocalBackend::VLLM`] for any other provider.
//!
//! ## Endpoints
//!
//! * **vLLM** – `{base_url}/chat/completions`, default base URL
//!   `http://localhost:8000/v1`. Structured output is requested via
//!   `response_format.type = "json_schema"` with a JSON Schema payload.
//!   vLLM >= 0.5 supports several structured-output backends
//!   (xgrammar, guidance, outlines, lm-format-enforcer). If the engine
//!   rejects the schema with HTTP 4xx the client falls back once to the
//!   looser `{"type": "json_object"}` mode.
//! * **Ollama** – `{base_url}/api/chat`, default base URL
//!   `http://localhost:11434`. Structured output is requested via the
//!   `format` parameter (a JSON Schema object). `temperature` and
//!   `num_predict` are nested under `options`. The streaming response is
//!   newline-delimited JSON (NDJSON), so a dedicated consumer extracts
//!   `message.content` from each chunk.
//!
//! Both engines are unauthenticated by default; an `api_key` configured
//! on the [`LlmConfig`] is sent as a bearer token for vLLM (operators
//! commonly front vLLM with an auth proxy) and silently ignored for
//! Ollama, which does not implement the bearer scheme.

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
    compact_query_plan_schema, detect_template_leak, drive_sse_consumer, extract_delta_content,
    is_retryable, response_message, retry_backoff, sse_error, sse_lines, transport_error, truncate,
};

/// Default base URL for vLLM (without the `/chat/completions` suffix).
const DEFAULT_VLLM_BASE_URL: &str = "http://localhost:8000/v1";

/// Default base URL for Ollama (without the `/api/chat` suffix).
const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Local inference engines supported by [`LocalClient`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalBackend {
    /// vLLM with an OpenAI-compatible `/chat/completions` endpoint.
    VLLM,
    /// Ollama with the native `/api/chat` endpoint.
    Ollama,
}

impl LocalBackend {
    /// Returns the [`LlmProvider`] associated with this backend.
    fn provider(self) -> LlmProvider {
        match self {
            LocalBackend::VLLM => LlmProvider::Vllm,
            LocalBackend::Ollama => LlmProvider::Ollama,
        }
    }

    /// Returns the canonical lowercase label used in `config.extra`.
    fn label(self) -> &'static str {
        match self {
            LocalBackend::VLLM => "vllm",
            LocalBackend::Ollama => "ollama",
        }
    }
}

/// Local LLM client backed by either vLLM or Ollama.
#[derive(Clone)]
pub struct LocalClient {
    config: LlmConfig,
    client: reqwest::Client,
    backend: LocalBackend,
    base_url: String,
}

impl std::fmt::Debug for LocalClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalClient")
            .field("backend", &self.backend)
            .field("base_url", &self.base_url)
            .field("model", &self.config.model)
            .field("max_retries", &self.max_attempts())
            .field("provider", &self.config.provider)
            .finish()
    }
}

impl LocalClient {
    /// Builds a new local client from a populated [`LlmConfig`].
    ///
    /// The backend is taken from `LlmConfig::extra["backend"]` when set
    /// (`"vllm"` or `"ollama"`). When unset the backend follows the
    /// configured provider, defaulting to vLLM. The base URL is read
    /// from `api_base`; any trailing `/chat/completions` or `/api/chat`
    /// suffix is stripped before re-appending the backend-appropriate
    /// chat endpoint.
    pub fn new(config: LlmConfig) -> Result<Self, VlorQLError> {
        if config.model.trim().is_empty() {
            return Err(VlorQLError::config(
                ConfigErrorKind::EmptyModel,
                json!({"provider": config.provider, "field": "model"}),
            ));
        }
        let backend = resolve_backend(&config)?;
        let base_url = resolve_base_url(&config, backend);
        let timeout = Duration::from_secs(config.timeout_seconds.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Ok(Self {
            config,
            client,
            backend,
            base_url,
        })
    }

    /// Returns the maximum number of attempts for retryable failures.
    fn max_attempts(&self) -> usize {
        usize::try_from(self.config.max_retries.max(1)).unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    /// Returns the effective chat endpoint for the active backend.
    fn endpoint(&self) -> String {
        let suffix = match self.backend {
            LocalBackend::VLLM => "chat/completions",
            LocalBackend::Ollama => "api/chat",
        };
        let trimmed = self.base_url.trim_end_matches('/');
        format!("{trimmed}/{suffix}")
    }

    /// Returns whether the configured backend/model supports strict JSON
    /// Schema output.
    ///
    /// Operators can force the choice via
    /// `LlmConfig::extra["strict_json_schema"]` (boolean). The default is
    /// `true` for both backends: vLLM uses
    /// `response_format.json_schema`, Ollama uses the JSON Schema object
    /// form of the `format` parameter. Models with known
    /// JSON-Schema compatibility issues (e.g. some Qwen 3.5/3.6 builds
    /// for Ollama) should opt out via
    /// `extra["strict_json_schema"] = false`, which falls back to
    /// `{"type": "json_object"}` for vLLM and `format = "json"` for
    /// Ollama. The system prompt should always inline the schema as a
    /// textual fallback so the model can produce valid output regardless.
    fn supports_strict_json_schema(&self) -> bool {
        if let Some(override_value) = self.config.extra.get("strict_json_schema")
            && let Some(flag) = override_value.as_bool()
        {
            return flag;
        }
        true
    }

    /// Builds the JSON body sent to a vLLM `/chat/completions` endpoint.
    fn build_vllm_body(&self, question: &str, system_prompt: &str, stream: bool) -> Value {
        let response_format = if self.supports_strict_json_schema() {
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "QueryPlan",
                    "schema": compact_query_plan_schema(),
                },
            })
        } else {
            json!({"type": "json_object"})
        };
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": question},
            ],
            "response_format": response_format,
            "temperature": self.config.temperature,
            "max_tokens": self.config.max_tokens,
        });
        if stream {
            body["stream"] = Value::Bool(true);
        }
        body
    }

    /// Builds the JSON body sent to an Ollama `/api/chat` endpoint.
    fn build_ollama_body(&self, question: &str, system_prompt: &str, stream: bool) -> Value {
        let format_value = if self.supports_strict_json_schema() {
            compact_query_plan_schema()
        } else {
            Value::String("json".to_owned())
        };
        json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": question},
            ],
            "format": format_value,
            "stream": stream,
            "options": {
                "temperature": self.config.temperature,
                "num_predict": self.config.max_tokens,
            },
        })
    }

    /// Builds the request body for the active backend.
    fn build_request_body(&self, question: &str, system_prompt: &str, stream: bool) -> Value {
        match self.backend {
            LocalBackend::VLLM => self.build_vllm_body(question, system_prompt, stream),
            LocalBackend::Ollama => self.build_ollama_body(question, system_prompt, stream),
        }
    }

    /// Builds a degraded request body that drops strict-schema output.
    ///
    /// Used as a fallback when the engine rejects the JSON Schema payload
    /// (typically HTTP 400 or 422). The fallback uses
    /// `response_format.type = "json_object"` for vLLM and `format = "json"`
    /// for Ollama.
    fn build_fallback_body(&self, question: &str, system_prompt: &str, stream: bool) -> Value {
        match self.backend {
            LocalBackend::VLLM => {
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
            LocalBackend::Ollama => json!({
                "model": self.config.model,
                "messages": [
                    {"role": "system", "content": system_prompt},
                    {"role": "user", "content": question},
                ],
                "format": "json",
                "stream": stream,
                "options": {
                    "temperature": self.config.temperature,
                    "num_predict": self.config.max_tokens,
                },
            }),
        }
    }

    /// Sends the request with the appropriate auth header.
    async fn send_request(&self, body: &Value) -> Result<reqwest::Response, VlorQLError> {
        let mut builder = self.client.post(self.endpoint()).json(body);
        if matches!(self.backend, LocalBackend::VLLM)
            && let Some(key) = self.config.api_key.as_deref().filter(|k| !k.is_empty())
        {
            builder = builder.bearer_auth(key);
        }
        builder
            .send()
            .await
            .map_err(|error| transport_error(&error))
    }

    /// Issues a single non-streaming request and parses the result.
    async fn send_once(&self, body: &Value) -> Result<QueryPlan, VlorQLError> {
        let response = self.send_request(body).await?;
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
                    "backend": self.backend.label(),
                    "body": truncate(&text, 2048),
                }),
            ));
        }
        parse_completion_payload(&text, self.backend)
    }
}

/// Returns `true` for HTTP errors that suggest the engine rejected the
/// JSON Schema payload (and that a fallback to `json_object` / `"json"`
/// may succeed).
fn should_fallback_to_json_object(error: &VlorQLError) -> bool {
    matches!(
        error,
        VlorQLError::Llm {
            kind: LlmErrorKind::ApiError { status, .. },
            ..
        } if *status == 400 || *status == 415 || *status == 422
    )
}

#[async_trait]
impl LlmClient for LocalClient {
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
    ) -> Result<QueryPlan, VlorQLError> {
        let primary = self.build_request_body(question, system_prompt, false);
        let max_attempts = self.max_attempts();
        let mut last_error: Option<VlorQLError> = None;
        let mut body = primary;
        let mut fallback_used = false;

        for attempt in 0..max_attempts {
            let result = self.send_once(&body).await;
            match result {
                Ok(plan) => return Ok(plan),
                Err(error) => {
                    let mut did_fallback = false;
                    if !fallback_used && should_fallback_to_json_object(&error) {
                        warn!(
                            backend = self.backend.label(),
                            "structured-output request rejected; retrying with json_object mode"
                        );
                        body = self.build_fallback_body(question, system_prompt, false);
                        fallback_used = true;
                        did_fallback = true;
                    }
                    if !did_fallback {
                        let can_retry = is_retryable(&error) && attempt + 1 < max_attempts;
                        if !can_retry {
                            return Err(error);
                        }
                        let delay = retry_backoff(DEFAULT_RETRY_DELAY, attempt);
                        warn!(
                            attempt = attempt + 1,
                            max_attempts,
                            backend = self.backend.label(),
                            ?delay,
                            "local request failed; retrying"
                        );
                        last_error = Some(error);
                        sleep(delay).await;
                    } else {
                        last_error = Some(error);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            VlorQLError::llm(
                LlmErrorKind::ApiError {
                    status: 0,
                    message: "local request did not produce a result".to_owned(),
                },
                json!({"source": "local_client", "backend": self.backend.label()}),
            )
        }))
    }

    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        let body = self.build_request_body(&question, &system_prompt, true);
        let endpoint = self.endpoint();
        let mut builder = self.client.post(&endpoint).json(&body);
        if matches!(self.backend, LocalBackend::VLLM) {
            if let Some(key) = self.config.api_key.as_deref().filter(|k| !k.is_empty()) {
                builder = builder.bearer_auth(key);
            }
            builder = builder.header("accept", "text/event-stream");
        }
        let response = builder
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
                    "backend": self.backend.label(),
                    "body": truncate(&body, 2048),
                }),
            ));
        }

        let byte_stream = response.bytes_stream();
        let (tx, rx) = mpsc::unbounded_channel::<Result<String, VlorQLError>>();
        let line_stream = sse_lines(byte_stream);

        match self.backend {
            LocalBackend::VLLM => {
                let max_attempts = self.max_attempts();
                let retry_base = DEFAULT_RETRY_DELAY;
                tokio::spawn(async move {
                    if !drive_sse_consumer(line_stream, tx, max_attempts, retry_base).await {
                        warn!("vLLM SSE consumer ended before producing content");
                    }
                });
            }
            LocalBackend::Ollama => {
                tokio::spawn(async move {
                    if !drive_ollama_ndjson_consumer(line_stream, tx).await {
                        warn!("Ollama NDJSON consumer ended before producing content");
                    }
                });
            }
        }

        let output = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        Ok(Box::new(Box::pin(output)))
    }

    fn provider(&self) -> LlmProvider {
        self.backend.provider()
    }

    fn config(&self) -> &LlmConfig {
        &self.config
    }
}

/// Resolves the active [`LocalBackend`] from the supplied [`LlmConfig`].
///
/// Reads `extra["backend"]` first (case-insensitive `"vllm"` or
/// `"ollama"`); falls back to the configured [`LlmProvider`]; finally
/// defaults to vLLM.
fn resolve_backend(config: &LlmConfig) -> Result<LocalBackend, VlorQLError> {
    if let Some(value) = config.extra.get("backend")
        && let Some(label) = value.as_str()
    {
        let lowered = label.trim().to_ascii_lowercase();
        return match lowered.as_str() {
            "vllm" => Ok(LocalBackend::VLLM),
            "ollama" => Ok(LocalBackend::Ollama),
            other => Err(VlorQLError::config(
                ConfigErrorKind::InvalidDialect {
                    dialect: format!("unknown local backend `{other}`"),
                },
                json!({
                    "field": "extra.backend",
                    "value": other,
                }),
            )),
        };
    }
    Ok(match config.provider {
        LlmProvider::Ollama => LocalBackend::Ollama,
        _ => LocalBackend::VLLM,
    })
}

/// Resolves the chat-completions-free base URL for the active backend.
///
/// Strips a trailing `/chat/completions` or `/api/chat` suffix when
/// present so that callers can pass either a base URL or a full endpoint
/// via `LlmConfig::api_base`.
fn resolve_base_url(config: &LlmConfig, backend: LocalBackend) -> String {
    let fallback = match backend {
        LocalBackend::VLLM => DEFAULT_VLLM_BASE_URL,
        LocalBackend::Ollama => DEFAULT_OLLAMA_BASE_URL,
    };
    let raw = config
        .api_base
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned());
    let trimmed = raw.trim().trim_end_matches('/');
    let trimmed = trimmed
        .strip_suffix("/chat/completions")
        .or_else(|| trimmed.strip_suffix("/api/chat"))
        .unwrap_or(trimmed);
    trimmed.to_owned()
}

/// Parses a non-streaming chat-completion response into a [`QueryPlan`].
///
/// vLLM responses follow the OpenAI shape (`choices[0].message.content`);
/// Ollama responses use a flatter envelope (`message.content`).
fn parse_completion_payload(body: &str, backend: LocalBackend) -> Result<QueryPlan, VlorQLError> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("{} response is not valid JSON: {error}", backend.label()),
            },
            json!({
                "source": "local_response",
                "backend": backend.label(),
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
                    .unwrap_or("local engine returned an error")
                    .to_owned(),
            },
            json!({
                "source": "local_error",
                "backend": backend.label(),
                "error": error,
            }),
        ));
    }
    let content = match backend {
        LocalBackend::VLLM => value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                VlorQLError::llm(
                    LlmErrorKind::ParseError {
                        details: "vLLM response did not contain choices[0].message.content"
                            .to_owned(),
                    },
                    json!({"source": "local_response", "backend": backend.label()}),
                )
            })?,
        LocalBackend::Ollama => value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                VlorQLError::llm(
                    LlmErrorKind::ParseError {
                        details: "Ollama response did not contain message.content".to_owned(),
                    },
                    json!({"source": "local_response", "backend": backend.label()}),
                )
            })?,
    };
    if content.is_empty() {
        return Err(VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!(
                    "{} returned an empty content; the model likely refused the prompt",
                    backend.label()
                ),
            },
            json!({"source": "local_content", "backend": backend.label()}),
        ));
    }
    if let Some(details) = detect_template_leak(content) {
        return Err(VlorQLError::llm(
            LlmErrorKind::ParseError { details },
            json!({
                "source": "local_content",
                "backend": backend.label(),
                "content": truncate(content, 4096),
            }),
        ));
    }
    serde_json::from_str::<QueryPlan>(content).map_err(|error| {
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("assistant content is not a valid QueryPlan: {error}"),
            },
            json!({
                "source": "local_content",
                "backend": backend.label(),
                "content": truncate(content, 4096),
            }),
        )
    })
}

/// Consumes a stream of newline-delimited JSON lines emitted by Ollama
/// and forwards `message.content` deltas through `tx`.
///
/// Ollama's `/api/chat` stream is NDJSON: each line is a self-contained
/// JSON object. The terminal line carries `"done": true` and an empty
/// `message.content`; we surface the completion sentinel but do not
/// forward an empty delta.
async fn drive_ollama_ndjson_consumer<S>(
    line_stream: S,
    tx: mpsc::UnboundedSender<Result<String, VlorQLError>>,
) -> bool
where
    S: futures::Stream<Item = std::io::Result<String>> + Unpin + Send,
{
    use futures::StreamExt;
    let mut lines = line_stream;
    let mut saw_done = false;
    while let Some(item) = lines.next().await {
        let line = match item {
            Ok(line) => line,
            Err(error) => {
                let _ = tx.send(Err(sse_error(error.to_string())));
                return false;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                let _ = tx.send(Err(sse_error(format!(
                    "Ollama NDJSON chunk is not valid JSON: {error}"
                ))));
                return false;
            }
        };
        let done = value.get("done").and_then(Value::as_bool).unwrap_or(false);
        if done {
            saw_done = true;
            break;
        }
        // Reuse the OpenAI-compatible delta parser when the chunk looks
        // like an OpenAI event (some Ollama versions stream OpenAI-shaped
        // payloads through the same endpoint).
        if let Some(content) = extract_delta_content(&value) {
            if !content.is_empty() && tx.send(Ok(content)).is_err() {
                return true;
            }
            continue;
        }
        if let Some(content) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            && !content.is_empty()
        {
            if let Some(details) = detect_template_leak(content) {
                let _ = tx.send(Err(sse_error(details)));
                return false;
            }
            if tx.send(Ok(content.to_owned())).is_err() {
                return true;
            }
        }
    }
    !saw_done
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

    fn local_config(provider: LlmProvider, model: &str) -> LlmConfig {
        LlmConfig {
            provider,
            api_key: None,
            api_base: Some("http://127.0.0.1:0".to_owned()),
            model: model.to_owned(),
            max_tokens: 1024,
            temperature: 0.0,
            timeout_seconds: 60,
            max_retries: 1,
            extra: std::collections::HashMap::new(),
        }
    }

    fn vllm_chat_response(plan: &QueryPlan) -> String {
        json!({
            "id": "vllm-1",
            "model": "Qwen2.5-7B-Instruct",
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

    fn ollama_chat_response(plan: &QueryPlan) -> String {
        json!({
            "model": "llama3.2",
            "created_at": "2026-01-01T00:00:00Z",
            "message": {
                "role": "assistant",
                "content": serde_json::to_string(plan).expect("plan should serialize"),
            },
            "done": true,
            "done_reason": "stop",
        })
        .to_string()
    }

    #[test]
    fn resolve_backend_prefers_extra_override() {
        let mut config = local_config(LlmProvider::Ollama, "llama3.2");
        config
            .extra
            .insert("backend".to_owned(), Value::String("vllm".to_owned()));
        let backend = resolve_backend(&config).expect("backend should resolve");
        assert_eq!(backend, LocalBackend::VLLM);

        config
            .extra
            .insert("backend".to_owned(), Value::String("OLLAMA".to_owned()));
        let backend = resolve_backend(&config).expect("backend should resolve");
        assert_eq!(backend, LocalBackend::Ollama);
    }

    #[test]
    fn resolve_backend_falls_back_to_provider() {
        let config = local_config(LlmProvider::Ollama, "llama3.2");
        assert_eq!(
            resolve_backend(&config).expect("ollama"),
            LocalBackend::Ollama
        );

        let config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
        assert_eq!(resolve_backend(&config).expect("vllm"), LocalBackend::VLLM);

        let mut config = local_config(LlmProvider::OpenAi, "gpt-4o-mini");
        config.extra.remove("backend");
        assert_eq!(
            resolve_backend(&config).expect("default vllm"),
            LocalBackend::VLLM
        );
    }

    #[test]
    fn resolve_backend_rejects_unknown_value() {
        let mut config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
        config
            .extra
            .insert("backend".to_owned(), Value::String("llamacpp".to_owned()));
        let error = resolve_backend(&config).expect_err("unknown backend should fail");
        assert_eq!(error.error_code(), "G003");
    }

    #[test]
    fn resolve_base_url_strips_chat_suffix() {
        let mut config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
        config.api_base = Some("http://localhost:8000/v1/chat/completions".to_owned());
        assert_eq!(
            resolve_base_url(&config, LocalBackend::VLLM),
            "http://localhost:8000/v1"
        );

        config.api_base = Some("http://localhost:8000/v1/".to_owned());
        assert_eq!(
            resolve_base_url(&config, LocalBackend::VLLM),
            "http://localhost:8000/v1"
        );

        config.api_base = Some("http://localhost:11434/api/chat".to_owned());
        assert_eq!(
            resolve_base_url(&config, LocalBackend::Ollama),
            "http://localhost:11434"
        );
    }

    #[test]
    fn resolve_base_url_uses_backend_default_when_unset() {
        let mut config = local_config(LlmProvider::Vllm, "m");
        config.api_base = None;
        assert_eq!(
            resolve_base_url(&config, LocalBackend::VLLM),
            DEFAULT_VLLM_BASE_URL
        );
        assert_eq!(
            resolve_base_url(&config, LocalBackend::Ollama),
            DEFAULT_OLLAMA_BASE_URL
        );
    }

    #[test]
    fn local_client_endpoint_appends_chat_suffix() {
        let mut config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
        config.api_base = Some("http://localhost:8000/v1".to_owned());
        let client = LocalClient::new(config).expect("client should build");
        assert_eq!(
            client.endpoint(),
            "http://localhost:8000/v1/chat/completions"
        );

        let mut config = local_config(LlmProvider::Ollama, "llama3.2");
        config.api_base = Some("http://localhost:11434".to_owned());
        let client = LocalClient::new(config).expect("client should build");
        assert_eq!(client.endpoint(), "http://localhost:11434/api/chat");
    }

    #[test]
    fn local_client_provider_returns_active_backend() {
        let mut config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
        config.api_base = None;
        let client = LocalClient::new(config).expect("client should build");
        assert_eq!(client.provider(), LlmProvider::Vllm);

        let mut config = local_config(LlmProvider::Ollama, "llama3.2");
        config.api_base = None;
        let client = LocalClient::new(config).expect("client should build");
        assert_eq!(client.provider(), LlmProvider::Ollama);
    }

    #[test]
    fn local_client_rejects_empty_model() {
        let mut config = local_config(LlmProvider::Vllm, "placeholder");
        config.model = "   ".to_owned();
        config.api_base = None;
        let error = LocalClient::new(config).expect_err("empty model should fail");
        assert_eq!(error.error_code(), "G005");
    }

    #[tokio::test]
    async fn vllm_client_uses_json_schema_response_format() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_body(Matcher::Regex(
                r#""model":"Qwen2\.5-7B-Instruct".*"response_format".*"json_schema""#.to_owned(),
            ))
            .with_status(200)
            .with_body(vllm_chat_response(&expected))
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(format!("{}/v1", server.url())),
            ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
        };
        let client = LocalClient::new(config).expect("client should build");
        let request_body = client.build_request_body("show users", "system", false);
        assert_eq!(request_body["model"], "Qwen2.5-7B-Instruct");
        assert_eq!(request_body["response_format"]["type"], "json_schema");
        assert_eq!(
            request_body["response_format"]["json_schema"]["name"],
            "QueryPlan"
        );
        assert!(
            request_body["response_format"]["json_schema"]
                .get("schema")
                .is_some()
        );
        assert_eq!(request_body["temperature"], 0.0);
        assert_eq!(request_body["max_tokens"], 1024);
        let messages = request_body["messages"].as_array().expect("messages");
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
        assert_eq!(client.provider(), LlmProvider::Vllm);
        assert_eq!(client.config().model, "Qwen2.5-7B-Instruct");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn vllm_client_sends_bearer_when_api_key_set() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_header("authorization", "Bearer test-key")
            .with_status(200)
            .with_body(vllm_chat_response(&expected))
            .create_async()
            .await;

        let config = LlmConfig {
            api_key: Some("test-key".to_owned()),
            api_base: Some(format!("{}/v1", server.url())),
            ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
        };
        let client = LocalClient::new(config).expect("client should build");
        let actual = client
            .generate_plan("q", "s")
            .await
            .expect("plan should parse");
        assert_eq!(actual, expected);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn vllm_client_falls_back_to_json_object_on_400() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let failure = server
            .mock("POST", "/v1/chat/completions")
            .match_body(Matcher::Regex(
                r#""response_format".*"json_schema""#.to_owned(),
            ))
            .with_status(400)
            .with_body(r#"{"error":{"message":"structured output backend unavailable"}}"#)
            .create_async()
            .await;
        let success = server
            .mock("POST", "/v1/chat/completions")
            .match_body(Matcher::Regex(
                r#""response_format":\{"type":"json_object"\}"#.to_owned(),
            ))
            .with_status(200)
            .with_body(vllm_chat_response(&expected))
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(format!("{}/v1", server.url())),
            max_retries: 3,
            ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
        };
        let client = LocalClient::new(config).expect("client should build");
        let actual = client
            .generate_plan("q", "s")
            .await
            .expect("fallback should succeed");
        assert_eq!(actual, expected);
        failure.assert_async().await;
        success.assert_async().await;
    }

    #[tokio::test]
    async fn vllm_client_emits_sse_delta_chunks() {
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
            .mock("POST", "/v1/chat/completions")
            .match_header("accept", "text/event-stream")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(format!("{}/v1", server.url())),
            ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
        };
        let client = LocalClient::new(config).expect("client should build");
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
    async fn vllm_client_stream_propagates_http_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(503)
            .with_body(r#"{"error":{"message":"unavailable"}}"#)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(format!("{}/v1", server.url())),
            ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
        };
        let client = LocalClient::new(config).expect("client should build");
        let outcome = client
            .stream_plan("hi".to_owned(), "system".to_owned())
            .await;
        let err = match outcome {
            Ok(_) => panic!("503 should produce an error"),
            Err(error) => error,
        };
        assert_eq!(err.error_code(), "L001");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_client_uses_format_field_with_schema() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/api/chat")
            .match_body(Matcher::Any)
            .with_status(200)
            .with_body(ollama_chat_response(&expected))
            .create_async()
            .await;

        let config = LlmConfig {
            api_base: Some(server.url()),
            ..local_config(LlmProvider::Ollama, "llama3.2")
        };
        let client = LocalClient::new(config).expect("client should build");
        let request_body = client.build_request_body("show users", "system", false);
        assert_eq!(request_body["model"], "llama3.2");
        assert_eq!(request_body["stream"], false);
        assert!(
            request_body["format"].is_object(),
            "format should be a JSON Schema object"
        );
        assert_eq!(request_body["options"]["temperature"], 0.0);
        assert_eq!(request_body["options"]["num_predict"], 1024);

        let actual = client
            .generate_plan("show users", "system")
            .await
            .expect("plan should parse");
        assert_eq!(actual, expected);
        assert_eq!(client.provider(), LlmProvider::Ollama);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_client_uses_json_string_when_schema_disabled() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let mock = server
            .mock("POST", "/api/chat")
            .match_body(Matcher::Regex(
                r#""format":"json".*"stream":false"#.to_owned(),
            ))
            .with_status(200)
            .with_body(ollama_chat_response(&expected))
            .create_async()
            .await;

        let mut config = LlmConfig {
            api_base: Some(server.url()),
            ..local_config(LlmProvider::Ollama, "llama3.2")
        };
        config
            .extra
            .insert("strict_json_schema".to_owned(), Value::Bool(false));
        let client = LocalClient::new(config).expect("client should build");
        let request_body = client.build_request_body("q", "s", false);
        assert_eq!(request_body["format"], "json");
        let actual = client
            .generate_plan("q", "s")
            .await
            .expect("plan should parse");
        assert_eq!(actual, expected);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_client_parses_message_content_field() {
        let mut server = Server::new_async().await;
        let expected = plan();
        let raw = serde_json::to_string(&expected).expect("plan should serialize");
        let body = json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": raw},
            "done": true,
        })
        .to_string();
        let mock = server
            .mock("POST", "/api/chat")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(server.url()),
            ..local_config(LlmProvider::Ollama, "llama3.2")
        };
        let client = LocalClient::new(config).expect("client should build");
        let actual = client
            .generate_plan("q", "s")
            .await
            .expect("ollama plan should parse");
        assert_eq!(actual, expected);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_client_returns_error_for_empty_content() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/chat")
            .with_status(200)
            .with_body(
                json!({
                    "model": "llama3.2",
                    "message": {"role": "assistant", "content": ""},
                    "done": true,
                })
                .to_string(),
            )
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(server.url()),
            ..local_config(LlmProvider::Ollama, "llama3.2")
        };
        let client = LocalClient::new(config).expect("client should build");
        let error = client
            .generate_plan("q", "s")
            .await
            .expect_err("empty content should fail");
        assert_eq!(error.error_code(), "L003");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_client_converts_error_response() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/chat")
            .with_status(500)
            .with_body(r#"{"error":"model not loaded"}"#)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(server.url()),
            ..local_config(LlmProvider::Ollama, "llama3.2")
        };
        let client = LocalClient::new(config).expect("client should build");
        let error = client
            .generate_plan("q", "s")
            .await
            .expect_err("500 should be reported");
        assert_eq!(error.error_code(), "L001");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn ollama_client_stream_emits_ndjson_chunks() {
        use futures::StreamExt;
        let mut server = Server::new_async().await;
        let body = [
            json!({
                "model": "llama3.2",
                "message": {"role": "assistant", "content": "hello "},
                "done": false,
            })
            .to_string(),
            json!({
                "model": "llama3.2",
                "message": {"role": "assistant", "content": "world"},
                "done": false,
            })
            .to_string(),
            json!({
                "model": "llama3.2",
                "message": {"role": "assistant", "content": ""},
                "done": true,
            })
            .to_string(),
        ]
        .join("\n");
        let mock = server
            .mock("POST", "/api/chat")
            .match_body(Matcher::Regex(r#""stream":true"#.to_owned()))
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(server.url()),
            ..local_config(LlmProvider::Ollama, "llama3.2")
        };
        let client = LocalClient::new(config).expect("client should build");
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
    async fn ollama_client_stream_propagates_http_error() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/api/chat")
            .with_status(500)
            .with_body(r#"{"error":"down"}"#)
            .create_async()
            .await;
        let config = LlmConfig {
            api_base: Some(server.url()),
            ..local_config(LlmProvider::Ollama, "llama3.2")
        };
        let client = LocalClient::new(config).expect("client should build");
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
    async fn local_client_translates_connection_failure_into_timeout_or_api_error() {
        let mut server = Server::new_async().await;
        // Bind a mock to a server we then drop to force a connection failure.
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(vllm_chat_response(&plan()))
            .create_async()
            .await;
        let url = server.url();
        drop(mock);
        drop(server);
        let config = LlmConfig {
            api_base: Some(format!("{url}/v1")),
            timeout_seconds: 1,
            ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
        };
        let client = LocalClient::new(config).expect("client should build");
        let error = client
            .generate_plan("q", "s")
            .await
            .expect_err("dead endpoint should fail");
        assert!(
            error.error_code() == "L001" || error.error_code() == "L002",
            "expected transport/timeout error, got {}",
            error.error_code()
        );
    }
}
