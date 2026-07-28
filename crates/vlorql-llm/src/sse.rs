use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::debug;
use vlorql_core::errors::{LlmErrorKind, VlorQLError};

pub(crate) const SSE_DONE: &str = "[DONE]";

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
