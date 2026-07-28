# LLM Token Recording Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Pipe LLM token usage from provider responses through to OTLP metrics counters and facade return values.

**Architecture:** `TokenUsage` struct + `StreamResult` at vlorql-llm level; `LlmClient` trait returns usage alongside plan; facade records metrics and passes usage to caller.

**Tech Stack:** Rust, serde_json, opentelemetry

## Global Constraints

- Missing/invalid `usage` field → `TokenUsage::default()` (0 tokens), never propagate error
- Provider field names: OpenAI/DeepSeek/Zhipu → `usage.prompt_tokens`/`completion_tokens`; Anthropic → `usage.input_tokens`/`output_tokens`; Ollama → `prompt_eval_count`/`eval_count`; vLLM → `usage.prompt_tokens`/`completion_tokens`
- Do NOT add new dependencies
- `TokenUsage` derives `Debug, Clone, Copy, PartialEq, Default`
- All tests must pass: `cargo test --workspace`

---

### Task 1: `TokenUsage`, `StreamResult`, and `LlmClient` trait change

**Files:**
- Modify: `crates/vlorql-llm/src/lib.rs`

**Interfaces:**
- Produces: `TokenUsage { prompt_tokens: u64, completion_tokens: u64 }`
- Produces: `StreamResult { stream: ..., usage: Arc<Mutex<Option<TokenUsage>>> }`
- Produces: Updated `LlmClient::generate_plan` returns `Result<(QueryPlan, TokenUsage), VlorQLError>`
- Produces: Updated `LlmClient::stream_plan` returns `Result<StreamResult, VlorQLError>`

- [ ] **Step 1: Add `TokenUsage` struct + public exports**

In `crates/vlorql-llm/src/lib.rs`, add above the trait:

```rust
/// Token usage returned by an LLM provider.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}
```

In the `pub use` block, add `TokenUsage` to the re-exports.

- [ ] **Step 2: Add `StreamResult` struct**

```rust
/// Result of `stream_plan()`. The stream emits text deltas; after the
/// stream ends, `usage` contains the provider's token usage (if any).
pub struct StreamResult {
    pub stream: Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>,
    pub usage: Arc<tokio::sync::Mutex<Option<TokenUsage>>>,
}
```

Add the `use` imports: `use std::sync::Arc;` and `use tokio::sync::Mutex;` (check if already imported).

- [ ] **Step 3: Update `LlmClient` trait**

```rust
pub trait LlmClient: Send + Sync {
    async fn generate_plan(
        &self,
        question: &str,
        system_prompt: &str,
        temperature: Option<f32>,
    ) -> Result<(QueryPlan, TokenUsage), VlorQLError>;

    async fn stream_plan(
        &self,
        question: String,
        system_prompt: String,
    ) -> Result<StreamResult, VlorQLError>;

    fn provider(&self) -> LlmProvider;
    fn config(&self) -> &LlmConfig;
}
```

- [ ] **Step 4: Update blanket `impl<T> LlmClient for Box<T>`**

```rust
impl<T> LlmClient for Box<T>
where
    T: LlmClient + ?Sized,
{
    async fn generate_plan(&self, ...) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
        (**self).generate_plan(question, system_prompt, temperature).await
    }
    async fn stream_plan(&self, ...) -> Result<StreamResult, VlorQLError> {
        (**self).stream_plan(question, system_prompt).await
    }
    // provider(), config() — unchanged
}
```

- [ ] **Step 5: Verify compilation (will fail — expected)**

Run: `cargo check -p vlorql-llm 2>&1 | head -20`
Expected: compile errors in provider implementations (trait mismatch) — this is expected.

- [ ] **Step 6: Commit**

```bash
git add crates/vlorql-llm/src/lib.rs
git commit -m "feat(llm): add TokenUsage, StreamResult, update LlmClient trait"
```

---

### Task 2: `RetryableHttpClient` — extract usage from response

**Files:**
- Modify: `crates/vlorql-llm/src/retry_client.rs`

**Interfaces:**
- Consumes: `TokenUsage`, updated `generate_plan` / `stream_plan` signatures from Task 1
- Produces: Usage extracted from raw JSON response in `execute_with_retry`

- [ ] **Step 1: Add helper to parse usage from JSON**

```rust
fn parse_usage_from_response(text: &str) -> TokenUsage {
    use serde_json::Value;
    match serde_json::from_str::<Value>(text) {
        Ok(val) => {
            let usage = val.get("usage").or_else(|| val.get("message").and_then(|m| m.get("usage")));
            match usage {
                Some(u) => TokenUsage {
                    prompt_tokens: u.get("prompt_tokens").or_else(|| u.get("input_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
                    completion_tokens: u.get("completion_tokens").or_else(|| u.get("output_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
                },
                None => TokenUsage::default(),
            }
        }
        Err(_) => TokenUsage::default(),
    }
}
```

Note: This handles both OpenAI-style (`prompt_tokens`/`completion_tokens`) and Anthropic-style (`input_tokens`/`output_tokens`). Ollama is handled separately in the Local provider.

- [ ] **Step 2: Update `execute_with_retry` signature**

Change return type from `Result<QueryPlan, VlorQLError>` to `Result<(QueryPlan, TokenUsage), VlorQLError>`.

In the success branch (line 99: `return self.parse_response(&text);`), change to:
```rust
let plan = self.parse_response(&text)?;
let usage = parse_usage_from_response(&text);
return Ok((plan, usage));
```

- [ ] **Step 3: Run test**

Run: `cargo test -p vlorql-llm -- retry_client`
Expected: compile errors (providers not yet updated) — expected.

- [ ] **Step 4: Commit**

```bash
git commit -a -m "feat(llm): extract TokenUsage from response in RetryableHttpClient"
```

---

### Task 3: Update all 5 providers to return TokenUsage

**Files:**
- Modify: `crates/vlorql-llm/src/openai.rs`
- Modify: `crates/vlorql-llm/src/deepseek.rs`
- Modify: `crates/vlorql-llm/src/anthropic.rs`
- Modify: `crates/vlorql-llm/src/zhipu.rs`
- Modify: `crates/vlorql-llm/src/local.rs`

**Interfaces:**
- Consumes: `TokenUsage`, `StreamResult`, updated `LlmClient` trait from Task 1
- Consumes: Updated `RetryableHttpClient` from Task 2
- Produces: Each provider implements updated `generate_plan` and `stream_plan`

- [ ] **Step 1: Update `OpenAIClient`**

`generate_plan`: delegates to `RetryableHttpClient::execute_with_retry` whose return type is already `Result<(QueryPlan, TokenUsage), ...>`. Just propagate.

`stream_plan`: create `StreamResult`, pass to `stream_with_sse`. In the SSE handler, when a data line contains `"usage"`, parse it and store in `usage`.

```rust
// In OpenAIClient
async fn generate_plan(&self, ...) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
    let (endpoint, body) = self.build_request(question, system_prompt, temperature);
    self.execute_with_retry(&endpoint, &body).await
}

async fn stream_plan(&self, ...) -> Result<StreamResult, VlorQLError> {
    let endpoint = format!("{}/chat/completions", self.config.base_url());
    let body = self.build_stream_body(question, system_prompt);
    let usage = Arc::new(Mutex::new(None));
    let usage_clone = Arc::clone(&usage);
    let extract = move |data: &Value| {
        // If this SSE event contains usage, store it
        if let Some(u) = data.get("usage") {
            let mut guard = usage_clone.lock().unwrap();
            *guard = Some(TokenUsage {
                prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                completion_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            });
        }
        // Extract delta text as before
        data.pointer("/choices/0/delta/content").and_then(|v| v.as_str().map(String::from))
    };
    let stream = self.stream_with_sse(&endpoint, &body, extract).await?;
    Ok(StreamResult { stream, usage })
}
```

- [ ] **Step 2: Update `DeepSeekClient`**

Same pattern as OpenAI (compatible API).

- [ ] **Step 3: Update `AnthropicClient`**

`generate_plan`: For Anthropic, the usage is in `response.usage` with fields `input_tokens`/`output_tokens`. The `parse_usage_from_response` helper in `retry_client.rs` already handles this via `or_else(|| u.get("input_tokens"))`. Just delegate to `execute_with_retry`.

`stream_plan`: Anthropic SSE format is different (uses `message_start`/`content_block_delta` events). The usage is in the `message_delta` event: `data: {"type": "message_delta", "usage": {"output_tokens": N}}`. The `input_tokens` comes from the `message_start` event. Need to accumulate both.

- [ ] **Step 4: Update `ZhipuClient`**

Same pattern as OpenAI.

- [ ] **Step 5: Update `LocalClient`**

`generate_plan`: Ollama uses `prompt_eval_count` and `eval_count` as top-level fields (not nested under `usage`). Check for these first, fall back to `usage.prompt_tokens` for vLLM compatibility.

`stream_plan`: For Ollama SSE, the last event contains the full response object with `prompt_eval_count`/`eval_count`. For vLLM SSE, similar to OpenAI.

- [ ] **Step 6: Compile check**

Run: `cargo check -p vlorql-llm 2>&1 | grep -E "^error" | head -5`
Expected: 0 errors (all providers updated).

- [ ] **Step 7: Run tests**

Run: `cargo test -p vlorql-llm`
Expected: all pass (some tests may need test fixture updates).

- [ ] **Step 8: Fix test fixtures**

Provider tests often include mock response JSON with `"usage"` fields. Ensure all test fixtures that represent successful responses include valid `usage` objects (most already do). For any that don't, add `"usage": {"prompt_tokens": 0, "completion_tokens": 0}` or leave as-is (default will be used).

- [ ] **Step 9: Commit**

```bash
git commit -a -m "feat(llm): update all providers to return TokenUsage"
```

---

### Task 4: Update `MockLlmClient`

**Files:**
- Modify: `crates/vlorql-llm/src/mock.rs`

**Interfaces:**
- Consumes: `TokenUsage`, updated `LlmClient` trait
- Produces: `MockLlmClient::success(plan)` adds usage; `MockLlmClient::with_usage(plan, usage)` new constructor

- [ ] **Step 1: Add usage field to `MockLlmClient`**

```rust
pub struct MockLlmClient {
    pub plan: QueryPlan,
    pub should_succeed: bool,
    pub usage: TokenUsage,
    pub config: LlmConfig,
}
```

Add `with_usage` constructor:
```rust
pub fn with_usage(plan: QueryPlan, usage: TokenUsage) -> Self { ... }
```

Update `MockLlmClient::success(plan)` to set `usage: TokenUsage::default()`.

- [ ] **Step 2: Update trait impl**

```rust
async fn generate_plan(&self, ...) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
    if self.should_succeed {
        Ok((self.plan.clone(), self.usage))
    } else {
        Err(VlorQLError::llm(LlmErrorKind::ApiError { status: 500, message: "mock failure".into() }, json!({})))
    }
}

async fn stream_plan(&self, ...) -> Result<StreamResult, VlorQLError> {
    let usage = Arc::new(Mutex::new(Some(self.usage)));
    let text = serde_json::to_string(&self.plan).unwrap();
    let stream = Box::pin(futures::stream::once(async move {
        Ok(text)
    })) as Box<dyn Stream<Item = Result<String, VlorQLError>> + Send + Unpin>;
    Ok(StreamResult { stream, usage })
}
```

- [ ] **Step 3: Update test usages of MockLlmClient**

Check all `MockLlmClient::success(...)` and `MockLlmClient::failure()` calls. The `success` now uses `TokenUsage::default()` so most call sites won't need changes.

- [ ] **Step 4: Run test**

Run: `cargo test -p vlorql-llm`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git commit -a -m "feat(llm): update MockLlmClient to return TokenUsage"
```

---

### Task 5: Facade — update `VlorQl::query()` return type + metrics

**Files:**
- Modify: `crates/vlorql/src/lib.rs`

**Interfaces:**
- Consumes: `TokenUsage`, updated `LlmClient` trait
- Produces: `VlorQl::query()` returns `Result<(CompiledQuery, TokenUsage), VlorQLError>`
- Produces: Metrics counters `llm_prompt_tokens` / `llm_completion_tokens` updated

- [ ] **Step 1: Update `VlorQl::query()` return type**

```rust
pub async fn query(&self, question: &str) -> Result<(CompiledQuery, TokenUsage), VlorQLError> {
```

- [ ] **Step 2: Record token metrics + return usage**

In `VlorQl::query()`, wherever `client.generate_plan(...)` is called, destructure to capture usage:

```rust
let (plan, usage) = client
    .generate_plan(&llm_question, &system_prompt, temperature)
    .await?;
if let Some(ref m) = self.metrics {
    m.llm_prompt_tokens.add(usage.prompt_tokens, &[]);
    m.llm_completion_tokens.add(usage.completion_tokens, &[]);
}
```

Replace all existing `client.generate_plan(...)` calls with the destructured form. Then at the final `Ok(compiled)` return site, change to `Ok((compiled, usage))`.

- [ ] **Step 3: Update all callers of `VlorQl::query()` internally**

Search for `self.query(` calls within the lib.rs file (the retry logic). Update those to destructure the tuple.

- [ ] **Step 4: Update the doctest example**

The docstring at line ~105 uses `let compiled = vlorql.query(...).await.unwrap();`. Update to `let (compiled, _usage) = ...`.

- [ ] **Step 5: Run tests**

Run: `cargo test -p vlorql`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git commit -a -m "feat(facade): query() returns TokenUsage, record metrics counters"
```

---

### Task 6: Streaming — `StreamEvent::TokenUsage` + `retry.rs` changes

**Files:**
- Modify: `crates/vlorql/src/lib.rs`
- Modify: `crates/vlorql/src/retry.rs`

- [ ] **Step 1: Add `TokenUsage` variant to `StreamEvent`**

```rust
pub enum StreamEvent {
    TextChunk(String),
    PlanComplete(Box<QueryPlan>),
    TokenUsage(TokenUsage),
    Error(VlorQLError),
}
```

- [ ] **Step 2: Update `run_stream_with_retry`**

Change `llm_client.stream_plan(...)` call to destructure `StreamResult`:

In the `run_stream_with_retry` function, replace:
```rust
let stream = match llm_client.stream_plan(...).await { Ok(stream) => stream, ... };
```
with:
```rust
let stream_result = match llm_client.stream_plan(...).await { Ok(sr) => sr, ... };
let mut stream = stream_result.stream;
```

After the stream loop ends and `process_assembled_text` returns the event, in the success branch (the `_ => { ... return; }` arm after the `match event` block), read usage and send `TokenUsage`:

```rust
_ => {
    let _ = event_tx.send(Ok(event));
    if let Some(usage) = *stream_result.usage.lock().unwrap() {
        let _ = event_tx.send(Ok(StreamEvent::TokenUsage(usage)));
    }
    return;
}
```

- [ ] **Step 3: Update `query_stream` doctest**

Update any doctests/examples that reference `StreamEvent` variants.

- [ ] **Step 4: Update tests**

Update `query_stream_emits_chunks_then_plan_complete` test to also check for `TokenUsage` event.

- [ ] **Step 5: Run tests**

Run: `cargo test -p vlorql`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git commit -a -m "feat(stream): add StreamEvent::TokenUsage, wire StreamResult in retry.rs"
```

---

### Task 7: End-to-end tests

**Files:**
- Modify: `crates/vlorql-core/src/observability/metrics.rs` (add integration test)
- Modify: `crates/vlorql-llm/src/retry_client.rs` (add unit test for `parse_usage_from_response`)
- Modify: `crates/vlorql/tests/integration/observability.rs` (add token metrics test)

- [ ] **Step 1: Test `parse_usage_from_response` in retry_client.rs**

```rust
#[test]
fn parse_usage_from_openai_response() {
    let json = r#"{"id":"...","usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
    let usage = parse_usage_from_response(json);
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
}

#[test]
fn parse_usage_from_anthropic_response() {
    let json = r#"{"content":[...],"usage":{"input_tokens":8,"output_tokens":3}}"#;
    let usage = parse_usage_from_response(json);
    assert_eq!(usage.prompt_tokens, 8);
    assert_eq!(usage.completion_tokens, 3);
}

#[test]
fn parse_usage_missing_field_returns_default() {
    let json = r#"{"id":"...","object":"..."}"#;
    let usage = parse_usage_from_response(json);
    assert_eq!(usage, TokenUsage::default());
}

#[test]
fn parse_usage_invalid_json_returns_default() {
    let usage = parse_usage_from_response("not json");
    assert_eq!(usage, TokenUsage::default());
}
```

- [ ] **Step 2: Test MockLlmClient returns usage**

```rust
#[tokio::test]
async fn mock_returns_usage() {
    let plan = make_plan();
    let expected_usage = TokenUsage { prompt_tokens: 10, completion_tokens: 5 };
    let client = MockLlmClient::with_usage(plan.clone(), expected_usage);
    let (result_plan, result_usage) = client.generate_plan("q", "p", None).await.unwrap();
    assert_eq!(result_plan, plan);
    assert_eq!(result_usage, expected_usage);
}
```

- [ ] **Step 3: Test facade query returns usage**

```rust
#[tokio::test]
async fn query_returns_token_usage() {
    let (vlorql, _plan) = make_facade(MockLlmClient::with_usage(
        make_plan(),
        TokenUsage { prompt_tokens: 10, completion_tokens: 5 },
    ));
    let (_compiled, usage) = vlorql.query("test query").await.expect("query");
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
}
```

- [ ] **Step 4: Run full suite**

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git commit -a -m "test(llm): add token usage parsing and integration tests"
```

---

### Task 8: Verify

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Final commit if fixes needed**

```bash
git commit -a -m "chore: fix clippy warnings"
```
