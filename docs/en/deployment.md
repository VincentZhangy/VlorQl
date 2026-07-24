# VlorQl Deployment Guide

This document covers how to deploy VlorQl in real environments:
running local model servers, sizing production deployments, and
tuning the LLM/validator knobs for the workloads you care about.

For language-level configuration (LlmConfig, DialectProfile,
PolicyConfig), see [`guide.md`](./guide.md).

---

## 1. Running a local LLM

VlorQl supports two local inference engines out of the box: vLLM
(OpenAI-compatible HTTP) and Ollama (native `/api/chat`).

### 1.1 vLLM

vLLM is a high-throughput OpenAI-compatible server. The structured
output is what makes it a first-class VlorQl backend — the model is
constrained to emit JSON that matches the `QueryPlan` schema.

**Install and launch (Linux, single GPU):**

```bash
pip install vllm

# Serve a model with guided decoding (recommended backend: xgrammar)
vllm serve Qwen/Qwen2.5-7B-Instruct \
    --host 0.0.0.0 \
    --port 8000 \
    --gpu-memory-utilization 0.85 \
    --max-model-len 8192 \
    --guided-decoding-backend xgrammar
```

The VlorQl client enables `response_format.json_schema` by default
and falls back to `response_format.json_object` if the engine
rejects the schema (HTTP 4xx). For tighter guarantees, pin the
backend explicitly:

| Model family          | Recommended `--guided-decoding-backend` |
|-----------------------|------------------------------------------|
| Qwen 2.5 / 3 / 3.5    | `xgrammar`                               |
| Llama 3 / 3.1 / 3.3   | `xgrammar` or `outlines`                  |
| Mistral 7B / 22B      | `guidance`                                |
| Yi / DeepSeek / GLM   | `xgrammar`                                |

**Verify the deployment:**

```bash
curl -s http://localhost:8000/v1/models | jq .
```

You should see `Qwen/Qwen2.5-7B-Instruct` listed.

**Connect VlorQl:**

```rust
use vlorql::{LlmConfig, LlmProvider, VlorQl};
use vlorql_llm::create_llm_client;

let config = LlmConfig {
    provider: LlmProvider::Vllm,
    api_key: None,
    api_base: Some("http://gpu-host.internal:8000/v1".to_owned()),
    model: "Qwen/Qwen2.5-7B-Instruct".to_owned(),
    max_tokens: 4096,
    ..LlmConfig::default()
};
let client = create_llm_client(config)?;
```

### 1.2 Ollama

Ollama is a single-binary model server. Its `/api/chat` endpoint
accepts a JSON Schema object in the `format` parameter, which is
exactly what VlorQl uses to constrain output.

**Install and launch (macOS / Linux):**

```bash
# Install
curl -fsSL https://ollama.com/install.sh | sh

# Download a model
ollama pull llama3.2

# Start the server (default port 11434)
ollama serve
```

**Verify:**

```bash
curl -s http://localhost:11434/api/tags | jq .
```

**Connect VlorQl:**

```rust
use std::collections::HashMap;
use serde_json::json;
use vlorql::{LlmConfig, LlmProvider, VlorQl};
use vlorql_llm::create_llm_client;

let mut extra = HashMap::new();
extra.insert("backend".to_owned(), json!("ollama"));
extra.insert("strict_json_schema".to_owned(), json!(true));

let config = LlmConfig {
    provider: LlmProvider::Ollama,
    api_key: None,
    api_base: Some("http://localhost:11434".to_owned()),
    model: "llama3.2".to_owned(),
    max_tokens: 4096,
    extra,
    ..LlmConfig::default()
};
let client = create_llm_client(config)?;
```

> **Note on compatibility:** Some Ollama builds (notably older
> Qwen 3.5/3.6) ignore the JSON Schema in `format` and only honor
> the looser `format: "json"` mode. If you see plans that don't
> match the schema, set `extra["strict_json_schema"] = false` in
> the `LlmConfig` to fall back to `format: "json"`. The system
> prompt always inlines the schema as a textual fallback so the
> model still produces valid output.

### 1.3 Hosted providers

Hosted APIs require only the API key as an environment variable:

| Provider   | Env var             | Notes                                                  |
|------------|---------------------|--------------------------------------------------------|
| Anthropic  | `ANTHROPIC_API_KEY` | Use `claude-sonnet-4-5` for the best cost/quality      |
| DeepSeek   | `DEEPSEEK_API_KEY`  | Use `deepseek-v4-pro` (the `deepseek-chat` model is deprecated 2026-07-24) |
| Zhipu      | `ZHIPU_API_KEY`     | Use `glm-4.7` or later; older models accept only `json_object` mode |
| OpenAI     | `OPENAI_API_KEY`    | Default model `gpt-4o-mini`                            |

Set the env var in your deployment system (Kubernetes Secret,
systemd `EnvironmentFile=`, etc.) and the `create_llm_client`
factory will read it automatically.

---

## 2. Production deployment

### 2.1 Share state with `Arc`

Every component VlorQl hands you is cheap to clone and can be
shared across threads:

* [`SchemaSnapshot`] is internally reference-counted; cloning is
  an atomic increment.
* [`DialectProfile`] is `Clone + Send + Sync`.
* [`PolicyConfig`] is `Clone + Send + Sync`.
* [`VlorQl`] is `Send + Sync`; the `Arc<dyn LlmClient>` and
  `ArcSchemaSnapshot` it owns are cheap to share.

A typical setup constructs **one** `VlorQl` value at startup and
shares it across request handlers:

```rust
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    vlorql: Arc<VlorQl>,
}
```

### 2.2 Use the streaming API for user-facing endpoints

`VlorQl::query_stream` is preferable to `VlorQl::query` for any
human-facing flow. It starts emitting text chunks within a few
hundred milliseconds while the LLM is still generating, which
materially improves perceived latency.

For batch jobs, prefer `VlorQl::query` because it skips the
streaming channel and surfaces the final plan in a single call.

### 2.3 Bound the retry budget

`max_retries` is a per-facade setting (default `2`). The LLM client
also retries on transient HTTP errors via `LlmConfig::max_retries`
(default `3`). Multiply the two when sizing your request budget:
an `LlmConfig::max_retries = 3` plus a `VlorQl` with `with_max_retries(2)` allows up to 12 LLM calls per user request.

```rust
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_policy(PolicyConfig::default())
    .with_llm_config(LlmConfig {
        max_retries: 2,
        ..LlmConfig::default()
    })
    .with_max_retries(1)
    .build()?;
```

### 2.4 Cap the LLM token budget

`LlmConfig::max_tokens` is the upper bound the model is allowed to
emit. Most `QueryPlan` JSON bodies fit comfortably in 2,000 tokens
even for wide schemas. Set `max_tokens: 2048` unless you have a
specific reason to allow more.

### 2.5 Configure request timeouts

`LlmConfig::timeout_seconds` controls the per-request HTTP timeout
in `reqwest`. The default is 60 s. For local servers on the same
host, drop it to 15 s; for cloud providers with cold-start
latency, raise it to 90 s. The LLM client retries on `Timeout`
errors, so a too-low value just wastes retry budget.

### 2.6 Logging

VlorQl uses `tracing` for everything. Connect a subscriber to get
visibility into validation retries, fallback behaviour, and SSE
stream consumption:

```rust
use tracing_subscriber::{fmt, EnvFilter};

tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,vlorql=debug")))
    .init();
```

Useful events:

* `vlorql_client::local` — emitted when a vLLM structured-output
  request is rejected and the client falls back to
  `response_format: json_object`.
* `vlorql_client::deepseek` / `zhipu` / `anthropic` — emitted on
  every retry with the attempt number, the configured budget, and
  the backoff delay.
* `vlorql_client::local` — emitted when an SSE stream ends before
  producing any content (the consumer is "best-effort" and
  reconnects on transient errors).

### 2.7 Surface the error response

Wrap `VlorQl::query` in your own error type and serialize the
`VlorQLError` to a stable wire format. The error helpers are designed
to be promptable for the LLM retry loop and to be human-readable in
logs:

```rust
let response = error.to_error_response();
serde_json::to_value(response)?;  // round-trips through serde
```

`ValidationErrorKind::InvalidJson` (`V001`) is the most common
retryable error; the rest of the `V*` codes are also retryable.
`P*` / `S*` / `C*` / `G*` codes are not — surface them to the
caller with the operator's original `details` payload.

---

## 3. Performance tuning

### 3.1 Validator knobs

| Knob                                | Where it lives                  | Effect                                                                              |
|-------------------------------------|---------------------------------|-------------------------------------------------------------------------------------|
| `max_joins`                         | `DialectProfile`                | Reject plans that exceed the join budget. `None` = unlimited.                        |
| `max_group_by_columns`              | `DialectProfile`                | Reject plans that group by too many expressions.                                      |
| `allowed_join_types`                | `DialectProfile`                | Restrict the set of JOIN types the LLM may emit.                                      |
| `allowed_functions` / `denied_functions` | `DialectProfile`          | Constrain the SQL functions the LLM may call. An empty allowlist means "all non-denied". |
| `allow_distinct`                    | `DialectProfile`                | Toggle `DISTINCT` in function calls.                                                  |
| `supports_offset` / `supports_fetch` | `DialectProfile`               | Toggle pagination clauses.                                                            |
| `supports_cte`                      | `DialectProfile`                | Toggle `WITH` clauses.                                                                |
| `max_tokens`                        | `LlmConfig`                     | Cap the LLM's output length.                                                          |
| `max_retries`                       | `LlmConfig` + `VlorQl` builder  | Cap the retry budget.                                                                  |

### 3.1.5 TOML/YAML dialect configs

When you need a custom dialect that does not match PostgreSQL, SQLite,
or MySQL exactly, use `DialectConfig::from_toml` or `DialectConfig::from_yaml`
to load the configuration from a file.  The file defines identifier
quoting, placeholder syntax, pagination templates, type mappings,
and feature flags.  Example at `examples/custom-dialect.toml`.

```rust
use vlorql_core::compile::{ConfigCompiler, DialectConfig};
use std::sync::Arc;

let config = DialectConfig::from_toml("path/to/dialect.toml")?;
let compiler = ConfigCompiler(Arc::new(config));
```

Rewrite rules can be loaded the same way:

```rust
use vlorql_core::compile::RewriteEngine;

let engine = RewriteEngine::load_toml("examples/rewrite-rules.toml")?;
```

### 3.2 Prompt size

The [`PromptBuilder`] renders the schema, policy, dialect, JSON
Schema, and an example section into every system prompt. With
defaults the prompt is well under 10 KB. To shrink it further:

* **Disable examples** when the deployment is well-tested:
  `PromptBuilder::with_examples(false)` (the CLI exposes this via
  `VlorQl::builder().with_examples_disabled()`-style configuration
  in a future release).
* **Hide globally denied columns** (the default already does
  this).
* **Tighten `max_description_chars`** by editing the schema —
  long prose descriptions are the largest contributor after the
  JSON Schema itself.

### 3.3 SQL compilation

The [`QueryBuilder`] is allocation-light:

* Identifiers are pushed through `std::fmt::Write` instead of
  building intermediate `String`s.
* Parameters are appended to a `Vec<Parameter>` in textual order
  via `add_parameter`. The PostgreSQL path tracks a counter;
  SQLite/MySQL emit `"?"` for every slot.
* Identifier validation rejects empty or non-ASCII names at
  compile time (when `quote_style = Never`), so an attacker-controlled
  LLM cannot inject SQL fragments through identifiers.

If you compile thousands of plans per second, profile
[`crate::compile::QueryBuilder::build`] and consider caching the
compiled SQL on the JSON-of-plan hash.

### 3.4 Caching

The pipeline is pure for a given `(schema, policy, plan, dialect)`
tuple. Common patterns:

| Cache key                                       | Cached value              |
|-------------------------------------------------|---------------------------|
| `serde_json::to_string(plan)` + `dialect`         | `CompiledQuery`           |
| `(user_question, schema_hash, policy_hash)`     | `QueryPlan`               |
| `schema_hash`                                   | `SchemaSnapshot` (Arc)    |

`CompiledQuery` caching is safe because the parameter list is
already in textual order and the SQL string is pure text. The
cache is invalidated automatically when the schema, policy, or
dialect version changes.

### 3.5 LLM-side tuning

* **Use strict `response_format`** whenever the provider supports
  it. The DeepSeek client enables it by default; the OpenAI
  client auto-enables it for models that support strict JSON Schema
  (GPT-4o, o1/o3/o4, GPT-4.1, …).
* **Set `temperature: 0.0`** (the `LlmConfig::default()`) for
  reproducible plan output. Non-zero temperature is only useful
  when you intentionally want to explore plan variants.
* **Use guided decoding** on local models — xgrammar in vLLM,
  built-in JSON mode in Ollama. The VlorQl prompt always includes
  a JSON Schema section, so guided decoding slots in cleanly.

---

## 4. Observability

The crate emits structured `tracing` events for every HTTP request
and every retry. To see them, install a subscriber with the
`vlorql` target enabled:

```bash
RUST_LOG=info,vlorql=debug cargo run …
```

The relevant events:

* `vlorql_client::local` — `"structured-output request rejected;
  retrying with json_object mode"` when vLLM returns HTTP 4xx for
  the schema request.
* `vlorql_client::deepseek` / `zhipu` / `anthropic` — emitted on
  every retry, with the attempt number, the configured max
  attempts, and the backoff delay.
* `vlorql::query` — emitted when a validation retry is triggered
  by an LLM-emitted plan that violated one or more policy or
  schema rules.

For production, layer these on top of your existing metrics
pipeline. A simple OpenTelemetry exporter over `tracing` is
sufficient; no custom instrumentation is required.

---

## 5. Security checklist

Run through this list before promoting a deployment:

* [ ] `LlmConfig::api_key` is read from a secret store (env var,
      Kubernetes Secret, Vault, …) and is never logged.
* [ ] The `LlmConfig::api_key` is `None` for vLLM/Ollama when the
      server is unauthenticated. Setting an arbitrary key does not
      break the request, but you should not configure one in
      public environments.
* [ ] All LLM output is treated as untrusted input. The
      `validate_only` pipeline is the only place that touches a
      `QueryPlan`; the LLM never has a path to the database
      driver.
* [ ] Every query goes through `PolicyEngine` with the production
      `PolicyConfig`. The integration tests in
      `crates/vlorql/tests/security/` cover the common
      policy-bypass attempts.
* [ ] The compiled SQL is reviewed for the safety-critical queries
      in your workload. `QueryBuilder` always emits placeholders
      for literals, but you should still audit the schema, the
      policy, and the SQL the LLM chose to emit.
* [ ] The deployment runs the security test suite
      (`cargo test -p vlorql --test security_*`) as part of CI
      so that the SQL-injection and policy-bypass regressions
      fail loudly.

---

## 6. Troubleshooting

### "vLLM returned 400: structured output backend unavailable"

The engine rejected the strict schema. The client retries once
with `response_format: json_object`. If that retry also fails, the
LlmError carries the full HTTP body in `details.body`. Re-launch
vLLM with a structured-decoding backend:

```bash
vllm serve ... --guided-decoding-backend xgrammar
```

### "Anthropic returned 401"

The `ANTHROPIC_API_KEY` env var is missing or invalid. The
`create_llm_client` factory reads it lazily on the first request;
double-check the deployment manifest. The `LlmErrorKind::ApiError`
with status 401 also surfaces the `suggestion` "Check the LLM
provider credentials and permissions." which is logged by the
facade.

### "Validation error: dialect feature `cte` is disabled"

The `DialectProfile` says `supports_cte = false` but the LLM
emitted a CTE. The error code is `V007`. Two fixes:

* Enable CTEs (`DialectProfile::builder().supports_cte(true)`).
* Tighten the prompt or the system message so the LLM is steered
  away from CTEs. (The default prompt already lists disabled
  features under **## Dialect Constraints**.)

### "QueryPlan round-trip fails: unknown field `extra`"

The `QueryPlan` schema uses `#[serde(deny_unknown_fields)]` to
reject extra keys. The most common cause is an LLM that
hallucinates a key. The error code is `V001` (or `L003` if the
parse fails earlier). The error message names the offending
field, which the LLM retry prompt echoes back to the model.

### Tests pass locally but fail in CI

Most likely cause: the `DEEPSEEK_API_KEY` / `ZHIPU_API_KEY` /
`ANTHROPIC_API_KEY` env var leaks from a developer shell into the
test process. The integration tests in
`crates/vlorql-llm/tests/integration/multi_provider.rs` now use a
Drop-style guard to clean up env mutations. If you write your own
env-mutating tests, follow the same pattern.
