# LLM Token Recording Design Spec

> **Date:** 2026-07-28
> **Status:** Draft

## Goal

Record LLM token usage (prompt tokens + completion tokens) from provider responses, pipe through to OTLP metrics counters and return to the caller alongside compiled queries.

## Architecture

```
Provider API response (JSON)
    ↓ parse usage field
LlmClient::generate_plan() → Result<(QueryPlan, TokenUsage), VlorQLError>
    ↓
VlorQl::query()            → Result<(CompiledQuery, TokenUsage), VlorQLError>
    ↓ record counters
VlorqMetrics::llm_prompt_tokens / llm_completion_tokens
```

For streaming, usage arrives in the last SSE event. The provider captures it in shared state accessible after the stream ends.

```
Stream SSE events → capture usage in last event → store in Arc<Mutex<Option<TokenUsage>>>
    ↓ stream ends
run_stream_with_retry() reads usage → emits StreamEvent::TokenUsage after PlanComplete
```

## Tech Stack

Rust, `vlorql-llm`, `vlorql`, `vlorql-core` (metrics), serde_json

## Data Structures

### `TokenUsage`

```rust
// crates/vlorql-llm/src/lib.rs (or new usage.rs module)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}
```

Re-exported from `vlorql-llm` and re-exported from `vlorql`.

### `StreamResult`

```rust
// crates/vlorql-llm/src/lib.rs
pub struct StreamResult {
    pub stream: Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>,
    pub usage: Arc<tokio::sync::Mutex<Option<TokenUsage>>>,
}
```

The provider's `stream_plan` implementation stores usage data in `usage` when it encounters the final SSE event containing token counts.

### `StreamEvent` (extended)

```rust
// crates/vlorql/src/lib.rs
pub enum StreamEvent {
    TextChunk(String),
    PlanComplete(Box<QueryPlan>),
    TokenUsage(TokenUsage),   // ← new
    Error(VlorQLError),
}
```

## Trait Changes

### `LlmClient`

```rust
pub trait LlmClient: Send + Sync {
    // Return type now carries usage data
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
        temperature: Option<f32>,
    ) -> Result<(QueryPlan, TokenUsage), VlorQLError>;

    // Return type now wraps usage alongside the stream
    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<StreamResult, VlorQLError>;

    // provider(), config() — unchanged
}
```

### Facade

```rust
// VlorQl::query() now returns token usage
pub async fn query(&self, question: &str)
    -> Result<(CompiledQuery, TokenUsage), VlorQLError>;

// query_stream() unchanged signature, but final event includes TokenUsage
```

## Provider Changes

Each provider must parse usage from the API response. Field names differ across providers:

| Provider | Response field | prompt_tokens | completion_tokens |
|----------|---------------|---------------|-------------------|
| OpenAI | `response.usage` | `prompt_tokens` | `completion_tokens` |
| DeepSeek | `response.usage` | `prompt_tokens` | `completion_tokens` |
| Anthropic | `response.usage` | `input_tokens` | `output_tokens` |
| Zhipu | `response.usage` | `prompt_tokens` | `completion_tokens` |
| Ollama | `response` | `prompt_eval_count` | `eval_count` |
| vLLM | `response.usage` | `prompt_tokens` | `completion_tokens` |

All providers: if the usage field is absent or parsing fails → return `TokenUsage::default()` (0 tokens) — never propagate a parse error.

### Streaming: SSE usage extraction

Each provider's SSE handler already reads event data line by line. When a data line contains a usage object (typically the last event before the done signal), the handler stores it in the shared `usage: Arc<Mutex<Option<TokenUsage>>>`.

Example (OpenAI SSE stream):
```
data: {"choices":[{"delta":{...}}]}
data: {"choices":[{"delta":{}}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}
data: [DONE]
```

The handler stores `Some(TokenUsage { prompt_tokens: 10, completion_tokens: 5 })` on the 2nd-to-last event.

## Metrics Recording

In `VlorQl::query()`, after `generate_plan` returns successfully:

```rust
if let Some(ref m) = self.metrics {
    m.llm_prompt_tokens.add(usage.prompt_tokens, &[]);
    m.llm_completion_tokens.add(usage.completion_tokens, &[]);
}
```

In `run_stream_with_retry()`, after the stream ends and usage is available:

```rust
if let Some(ref m) = self.metrics {
    if let Some(usage) = *stream_result.usage.lock().unwrap() {
        m.llm_prompt_tokens.add(usage.prompt_tokens, &[]);
        m.llm_completion_tokens.add(usage.completion_tokens, &[]);
    }
}
```

Note: `run_stream_with_retry` doesn't currently have access to `self.metrics`. This function is free-standing. Option: pass `Option<Arc<VlorqMetrics>>` as a parameter, or record metrics in `query_stream()` after `run_stream_with_retry` returns.

Actually, `run_stream_with_retry` returns via the channel, so the metrics can be recorded in `query_stream()` after the spawned task completes. Since the spawned task is fire-and-forget, the metrics recording for streaming happens inside the spawned task (pass metrics handle to `run_stream_with_retry`).

## Error Handling

- Missing `usage` field → `TokenUsage::default()` (no error, no warning)
- Provider returned explicit 0 tokens → record 0 (correct — may be billing-free tier)
- Parse failure (unexpected shape) → `TokenUsage::default()`, `tracing::warn!` once

## File Changes

| File | Change |
|------|--------|
| `crates/vlorql-llm/src/lib.rs` | Add `TokenUsage` struct, `StreamResult` struct, update `LlmClient` trait signatures, update blanket `impl<T>` |
| `crates/vlorql-llm/src/mock.rs` | Update `MockLlmClient` to return `TokenUsage` |
| `crates/vlorql-llm/src/openai.rs` | Parse `usage` in `generate_plan`, capture in SSE stream |
| `crates/vlorql-llm/src/deepseek.rs` | Same |
| `crates/vlorql-llm/src/anthropic.rs` | Same (Anthropic field names: `input_tokens`/`output_tokens`) |
| `crates/vlorql-llm/src/zhipu.rs` | Same |
| `crates/vlorql-llm/src/local.rs` | Same (Ollama: `prompt_eval_count`/`eval_count`, vLLM: `usage.prompt_tokens`/`completion_tokens`) |
| `crates/vlorql-llm/src/retry_client.rs` | Extract usage from response JSON in `execute_with_retry` |
| `crates/vlorql/src/lib.rs` | Update `VlorQl::query()` return type, record metrics, add `StreamEvent::TokenUsage`, update `query_stream()` for streaming usage |
| `crates/vlorql/src/retry.rs` | Accept `StreamResult`, extract usage after stream, emit `StreamEvent::TokenUsage` after `PlanComplete` |

## Testing

- Provider unit tests: verify usage parsing from mock responses (already have `"usage"` in test fixtures)
- `MockLlmClient`: return predictable `TokenUsage` values
- Facade tests: verify `query()` returns correct `TokenUsage`, verify `query_stream()` emits `TokenUsage` event
- Metrics counters: use `opentelemetry_sdk::testing::MetricsExporter` (already available in dev-deps)
