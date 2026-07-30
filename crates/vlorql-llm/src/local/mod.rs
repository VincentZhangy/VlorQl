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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::warn;
use vlorql_core::errors::{ConfigErrorKind, LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;

use crate::schema::compact_query_plan_schema;
use crate::sse::{
    extract_delta_content, is_retryable, response_message, retry_backoff, sse_lines,
    transport_error, truncate,
};
use crate::{
    DEFAULT_MAX_ATTEMPTS, DEFAULT_RETRY_DELAY, LlmClient, LlmConfig, LlmProvider,
    RetryableHttpClient, StreamResult, TokenUsage, detect_template_leak,
};

pub(crate) mod ollama;
pub(crate) use ollama::drive_ollama_ndjson_consumer;

#[cfg(test)]
mod tests;

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
        // Ollama 的本地小模型（4B-7B）对完整 JSON Schema 支持不佳，
        // 默认关闭严格模式，回退到 format = "json"（宽松模式）。
        // 可通过 extra["strict_json_schema"] = true 手动开启。
        self.backend != LocalBackend::Ollama
    }

    /// Builds the JSON body sent to a vLLM `/chat/completions` endpoint.
    fn build_vllm_body(
        &self,
        question: &str,
        system_prompt: &str,
        stream: bool,
        temperature: Option<f32>,
    ) -> Value {
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
            "temperature": temperature.unwrap_or(self.config.temperature),
            "max_tokens": self.config.max_tokens,
        });
        if stream {
            body["stream"] = Value::Bool(true);
        }
        body
    }

    /// Builds the JSON body sent to an Ollama `/api/chat` endpoint.
    fn build_ollama_body(
        &self,
        question: &str,
        system_prompt: &str,
        stream: bool,
        temperature: Option<f32>,
    ) -> Value {
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
                "temperature": temperature.unwrap_or(self.config.temperature),
                "num_predict": self.config.max_tokens,
            },
        })
    }

    /// Builds the request body for the active backend.
    fn build_request_body(
        &self,
        question: &str,
        system_prompt: &str,
        stream: bool,
        temperature: Option<f32>,
    ) -> Value {
        match self.backend {
            LocalBackend::VLLM => {
                self.build_vllm_body(question, system_prompt, stream, temperature)
            }
            LocalBackend::Ollama => {
                self.build_ollama_body(question, system_prompt, stream, temperature)
            }
        }
    }

    /// Builds a degraded request body that drops strict-schema output.
    ///
    /// Used as a fallback when the engine rejects the JSON Schema payload
    /// (typically HTTP 400 or 422). The fallback uses
    /// `response_format.type = "json_object"` for vLLM and `format = "json"`
    /// for Ollama.
    fn build_fallback_body(
        &self,
        question: &str,
        system_prompt: &str,
        stream: bool,
        temperature: Option<f32>,
    ) -> Value {
        match self.backend {
            LocalBackend::VLLM => {
                let mut body = json!({
                    "model": self.config.model,
                    "messages": [
                        {"role": "system", "content": system_prompt},
                        {"role": "user", "content": question},
                    ],
                    "response_format": {"type": "json_object"},
                    "temperature": temperature.unwrap_or(self.config.temperature),
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
                    "temperature": temperature.unwrap_or(self.config.temperature),
                    "num_predict": self.config.max_tokens,
                },
            }),
        }
    }

    /// Sends the request with the appropriate auth header.
    async fn send_request(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<reqwest::Response, VlorQLError> {
        let mut builder = self.client.post(endpoint).json(body);
        if matches!(self.backend, LocalBackend::VLLM) {
            if let Some(key) = self.config.api_key.as_deref().filter(|k| !k.is_empty()) {
                builder = builder.bearer_auth(key);
            }
            builder = builder.header("accept", "text/event-stream");
        }
        builder
            .send()
            .await
            .map_err(|error| transport_error(&error))
    }
}

#[async_trait]
impl RetryableHttpClient for LocalClient {
    fn max_attempts(&self) -> usize {
        usize::try_from(self.config.max_retries.max(1)).unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    fn provider_label(&self) -> &'static str {
        "local"
    }

    fn parse_response(&self, body: &str) -> Result<QueryPlan, VlorQLError> {
        parse_completion_payload(body, self.backend)
    }

    async fn send_request(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<reqwest::Response, VlorQLError> {
        LocalClient::send_request(self, endpoint, body).await
    }
}

/// Parse token usage from a local provider response.
fn parse_local_usage(body: &str, backend: LocalBackend) -> TokenUsage {
    use serde_json::Value;
    match serde_json::from_str::<Value>(body) {
        Ok(val) => match backend {
            LocalBackend::VLLM => val
                .get("usage")
                .map(|u| TokenUsage {
                    prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    completion_tokens: u
                        .get("completion_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                })
                .unwrap_or_default(),
            LocalBackend::Ollama => TokenUsage {
                prompt_tokens: val
                    .get("prompt_eval_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                completion_tokens: val.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0),
            },
        },
        Err(_) => TokenUsage::default(),
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
        temperature: Option<f32>,
    ) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
        let endpoint = self.endpoint();
        let primary = self.build_request_body(question, system_prompt, false, temperature);
        let max_attempts = self.max_attempts();
        let mut last_error: Option<VlorQLError> = None;
        let mut body = primary;
        let mut fallback_used = false;

        for attempt in 0..max_attempts {
            let response = self.send_request(&endpoint, &body).await;
            match response {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.map_err(|error| transport_error(&error))?;
                    if status.is_success() {
                        let plan = parse_completion_payload(&text, self.backend)?;
                        let usage = parse_local_usage(&text, self.backend);
                        return Ok((plan, usage));
                    }
                    let error = VlorQLError::llm(
                        LlmErrorKind::ApiError {
                            status: status.as_u16(),
                            message: response_message(&text),
                        },
                        json!({
                            "status": status.as_u16(),
                            "backend": self.backend.label(),
                            "body": truncate(&text, 2048),
                        }),
                    );

                    let mut did_fallback = false;
                    if !fallback_used && should_fallback_to_json_object(&error) {
                        warn!(
                            backend = self.backend.label(),
                            "structured-output request rejected; retrying with json_object mode"
                        );
                        body =
                            self.build_fallback_body(question, system_prompt, false, temperature);
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
                Err(error) => {
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
    ) -> Result<StreamResult, VlorQLError> {
        let endpoint = self.endpoint();
        let body = self.build_request_body(&question, &system_prompt, true, None);

        match self.backend {
            LocalBackend::VLLM => {
                let usage = Arc::new(tokio::sync::Mutex::new(None));
                let usage_clone = Arc::clone(&usage);
                let extract = move |data: &Value| {
                    if let Some(u) = data.get("usage")
                        && let Ok(mut guard) = usage_clone.try_lock()
                    {
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
                    extract_delta_content(data)
                };
                let stream = self.stream_with_sse(&endpoint, &body, extract).await?;
                Ok(StreamResult { stream, usage })
            }
            LocalBackend::Ollama => {
                let response = self.send_request(&endpoint, &body).await?;
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
                let usage = Arc::new(tokio::sync::Mutex::new(None));
                tokio::spawn(async move {
                    if !drive_ollama_ndjson_consumer(line_stream, tx).await {
                        warn!("Ollama NDJSON consumer ended before producing content");
                    }
                });

                let output = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
                Ok(StreamResult {
                    stream: Box::new(Box::pin(output))
                        as Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>,
                    usage,
                })
            }
        }
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

/// Parses a non-streaming chat-completion response into a [`QueryPlan`](vlorql_core::schema::QueryPlan).
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
                        details:
                            "vLLM response did not contain choices[0].message.content"
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
    let cleaned = crate::extract_json_content(content);
    crate::parse_llm_response(cleaned).map_err(|error| {
        let raw_content = truncate(content, 4096);
        let cleaned_for_debug = if cleaned != content {
            Some(truncate(cleaned, 4096))
        } else {
            None
        };
        VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: format!("assistant content is not a valid QueryPlan: {error}"),
            },
            json!({
                "source": "local_content",
                "backend": backend.label(),
                "content": raw_content,
                "cleaned": cleaned_for_debug,
            }),
        )
    })
}
