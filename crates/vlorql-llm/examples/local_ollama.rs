//! Local Ollama example for `vlorql-llm`.
//!
//! Demonstrates how to configure [`vlorql_llm::LocalClient`] against a
//! locally running Ollama instance and generate a `QueryPlan`.
//!
//! Ollama exposes a native `/api/chat` endpoint that accepts a JSON
//! Schema object in its `format` parameter. The example enables the
//! strict JSON Schema by default; the system prompt should always
//! inline the schema as a textual fallback because some Ollama models
//! (notably older Qwen 3.5/3.6 builds) do not validate `format`
//! payloads at the engine level.
//!
//! Start an Ollama server before running this example, for instance:
//!
//! ```bash
//! ollama serve                       # listens on http://localhost:11434
//! ollama pull llama3.2               # download the default model
//! ```
//!
//! Then run the example:
//!
//! ```bash
//! export OLLAMA_BASE_URL=http://localhost:11434  # optional, this is the default
//! export OLLAMA_MODEL=llama3.2                   # optional, default shown
//! cargo run -p vlorql-llm --example local_ollama -- "List user ids"
//! ```
//!
//! Pass `--build-only` to construct the client without issuing a
//! network request; useful for verifying the configuration on a
//! machine where Ollama is not yet running.

use std::env;
use std::error::Error;

use vlorql_llm::{LlmConfig, LlmProvider, create_llm_client};

const DEFAULT_BASE_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "llama3.2";
const SYSTEM_PROMPT: &str = "You are a SQL assistant. Reply with a JSON QueryPlan.";

fn build_config() -> LlmConfig {
    let base_url = env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
    let model = env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
    LlmConfig {
        provider: LlmProvider::Ollama,
        // Ollama does not require an API key.
        api_key: None,
        api_base: Some(base_url),
        model,
        max_tokens: 4096,
        temperature: 0.0,
        timeout_seconds: 60,
        max_retries: 3,
        // The `extra` map accepts backend selection and per-model
        // overrides. We pin the backend to `ollama` here so the
        // example keeps working when the default provider is
        // something else.
        extra: [("backend".to_owned(), serde_json::json!("ollama"))]
            .into_iter()
            .collect(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let build_only = args.iter().any(|arg| arg == "--build-only");
    args.retain(|arg| arg != "--build-only");

    let config = build_config();
    eprintln!(
        "Building Ollama client against {}",
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

    eprintln!("Generating plan for: {question}");
    let plan = client.generate_plan(&question, SYSTEM_PROMPT).await?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}
