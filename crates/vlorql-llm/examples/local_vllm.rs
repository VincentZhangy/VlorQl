//! Local vLLM example for `vlorql-llm`.
//!
//! Demonstrates how to configure [`vlorql_llm::LocalClient`] against a
//! locally running vLLM instance and stream a `QueryPlan` response.
//!
//! vLLM exposes an OpenAI-compatible `/chat/completions` endpoint and
//! supports structured outputs (via xgrammar, guidance, outlines, or
//! lm-format-enforcer) through the `response_format` field. The example
//! enables `json_schema` mode by default and falls back to
//! `json_object` if the engine rejects the schema.
//!
//! Start a vLLM server before running this example, for instance:
//!
//! ```bash
//! vllm serve Qwen/Qwen2.5-7B-Instruct \
//!     --port 8000 \
//!     --guided-decoding-backend xgrammar
//! ```
//!
//! Then run the example:
//!
//! ```bash
//! export VLLM_BASE_URL=http://localhost:8000/v1   # optional, this is the default
//! export VLLM_MODEL=Qwen/Qwen2.5-7B-Instruct     # optional, default shown
//! cargo run -p vlorql-llm --example local_vllm -- "List user ids"
//! ```
//!
//! Pass `--build-only` to construct the client without issuing a
//! network request; useful for verifying the configuration on a
//! machine where vLLM is not yet running.

use std::env;
use std::error::Error;

use futures::StreamExt;
use vlorql_llm::{create_llm_client, LlmConfig, LlmProvider};

const DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";
const DEFAULT_MODEL: &str = "Qwen/Qwen2.5-7B-Instruct";
const SYSTEM_PROMPT: &str = "You are a SQL assistant. Reply with a JSON QueryPlan.";

fn build_config() -> LlmConfig {
    let base_url = env::var("VLLM_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
    let model = env::var("VLLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
    LlmConfig {
        provider: LlmProvider::Vllm,
        // vLLM does not require auth by default; an empty key is fine.
        api_key: env::var("VLLM_API_KEY").ok(),
        api_base: Some(base_url),
        model,
        max_tokens: 4096,
        temperature: 0.0,
        timeout_seconds: 60,
        max_retries: 3,
        // Enable strict JSON Schema by default; the client will retry
        // with `json_object` if vLLM rejects the schema.
        extra: Default::default(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let build_only = args.iter().any(|arg| arg == "--build-only");
    args.retain(|arg| arg != "--build-only");

    let config = build_config();
    eprintln!(
        "Building vLLM client against {}",
        config.api_base.clone().unwrap_or_default()
    );
    let client = create_llm_client(config)?;
    eprintln!("Provider id: {}", client.provider());

    if build_only {
        eprintln!("--build-only: skipping network call");
        return Ok(());
    }

    let question = args
        .first()
        .cloned()
        .unwrap_or_else(|| "Show user ids".to_owned());

    eprintln!("Streaming response for: {question}");
    let mut stream = client
        .stream_plan(question.clone(), SYSTEM_PROMPT.to_owned())
        .await?;
    let mut combined = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item?;
        combined.push_str(&chunk);
        print!("{chunk}");
    }
    println!();
    eprintln!("Stream produced {} characters", combined.len());
    Ok(())
}
