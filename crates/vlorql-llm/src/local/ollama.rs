use serde_json::Value;
use tokio::sync::mpsc;
use vlorql_core::errors::VlorQLError;

use crate::detect_template_leak;
use crate::sse::{extract_delta_content, sse_error};

/// Consumes a stream of newline-delimited JSON lines emitted by Ollama
/// and forwards `message.content` deltas through `tx`.
///
/// Ollama's `/api/chat` stream is NDJSON: each line is a self-contained
/// JSON object. The terminal line carries `"done": true` and an empty
/// `message.content`; we surface the completion sentinel but do not
/// forward an empty delta.
pub(crate) async fn drive_ollama_ndjson_consumer<S>(
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
