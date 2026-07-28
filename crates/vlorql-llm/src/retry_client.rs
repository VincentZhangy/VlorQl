//! Retryable HTTP client trait — common retry loop and SSE streaming logic.
//!
//! Provides a [`RetryableHttpClient`] trait with two default methods
//! (`generate_with_retry` and `stream_with_sse`) that eliminate the
//! near-identical retry / SSE boilerplate across provider clients.

use async_trait::async_trait;
use futures::stream::Stream;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::warn;
use vlorql_core::errors::{LlmErrorKind, VlorQLError};
use vlorql_core::schema::QueryPlan;

use crate::{
    DEFAULT_RETRY_DELAY, drive_sse_consumer_with, is_retryable, response_message, retry_backoff,
    sse_lines, transport_error, truncate,
};

/// A retryable HTTP client that can send requests to an LLM provider.
///
/// Each provider implements [`send_request`] to customise auth / headers,
/// and the trait provides default `generate_with_retry` and `stream_with_sse`
/// methods that handle the common retry loop and SSE streaming pipeline.
#[async_trait]
pub(crate) trait RetryableHttpClient: Send + Sync {
    /// Maximum number of attempts for retryable failures.
    fn max_attempts(&self) -> usize;

    /// Human-readable provider label used in log messages.
    fn provider_label(&self) -> &'static str;

    /// Send an HTTP POST request to `endpoint` with the given JSON `body`.
    ///
    /// Implementors are responsible for setting the provider-specific
    /// authentication headers, content-type, and any other transport-level
    /// Configuration.
    async fn send_request(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<reqwest::Response, VlorQLError>;

    /// Parse a successful HTTP response body into a [`QueryPlan`].
    ///
    /// The input is the raw response text (the full JSON body returned by
    /// the provider). Implementors should first extract the assistant's
    /// message content from the provider-specific JSON envelope and then
    /// use [`crate::parse_llm_response`] to convert it to a query plan.
    fn parse_response(&self, body: &str) -> Result<QueryPlan, VlorQLError>;

    /// Send a non-streaming request with automatic retries on transient
    /// failures, and parse the successful response into a [`QueryPlan`].
    async fn generate_with_retry(
        &self,
        endpoint: &str,
        body: &Value,
    ) -> Result<QueryPlan, VlorQLError> {
        let max_attempts = self.max_attempts();
        let label = self.provider_label();
        let mut last_error: Option<VlorQLError> = None;

        for attempt in 0..max_attempts {
            let response = self.send_request(endpoint, body).await;
            match response {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp
                        .text()
                        .await
                        .map_err(|error| transport_error(&error))?;
                    if !status.is_success() {
                        let error = VlorQLError::llm(
                            LlmErrorKind::ApiError {
                                status: status.as_u16(),
                                message: response_message(&text),
                            },
                            json!({
                                "status": status.as_u16(),
                                "body": truncate(&text, 2048),
                            }),
                        );
                        let can_retry = is_retryable(&error) && attempt + 1 < max_attempts;
                        if !can_retry {
                            return Err(error);
                        }
                        let delay = retry_backoff(DEFAULT_RETRY_DELAY, attempt);
                        warn!(
                            attempt = attempt + 1,
                            max_attempts,
                            ?delay,
                            "{label} request failed; retrying",
                        );
                        last_error = Some(error);
                        sleep(delay).await;
                    } else {
                        return self.parse_response(&text);
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
                        ?delay,
                        "{label} request failed; retrying",
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
                    message: format!("{label} request did not produce a result"),
                },
                json!({"source": format!("{label}_client")}),
            )
        }))
    }

    /// Send a streaming SSE request and return a stream of text deltas.
    ///
    /// `extract_delta` is a closure that extracts the delta text from a
    /// parsed SSE JSON value (provider-specific event shapes).
    async fn stream_with_sse<F>(
        &self,
        endpoint: &str,
        body: &Value,
        extract_delta: F,
    ) -> Result<
        Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>,
        VlorQLError,
    >
    where
        F: Fn(&Value) -> Option<String> + Send + 'static,
    {
        let label = self.provider_label();
        let response = self.send_request(endpoint, body).await?;
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
            if !drive_sse_consumer_with(
                line_stream,
                tx,
                max_attempts,
                retry_base,
                extract_delta,
            )
            .await
            {
                warn!("{label} SSE consumer ended before producing content");
            }
        });
        let output = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        Ok(Box::new(Box::pin(output)))
    }
}
