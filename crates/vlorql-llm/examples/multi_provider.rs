//! Multi-provider example for `vlorql-llm`.
//!
//! Demonstrates how to switch between hosted LLM providers (Anthropic,
//! DeepSeek, Zhipu, OpenAI) using the [`vlorql_llm::create_llm_client`]
//! factory. Every provider exposes the same [`vlorql_llm::LlmClient`]
//! trait, so the only thing that changes is the [`vlorql_llm::LlmConfig`]
//! struct that drives construction.
//!
//! Set the `LLM_PROVIDER` environment variable to pick the active
//! provider and the corresponding `*_API_KEY` environment variable to
//! provide credentials. Supported values for `LLM_PROVIDER` are
//! `anthropic`, `deepseek`, `zhipu` and `openai`.
//!
//! ```bash
//! export LLM_PROVIDER=anthropic
//! export ANTHROPIC_API_KEY=sk-ant-...
//! cargo run -p vlorql-llm --example multi_provider -- "Show user ids"
//! ```
//!
//! Run with `--build-only` to construct every client without issuing a
//! network request. This is useful for verifying that the
//! configuration is correct (e.g. during CI smoke tests).

use std::env;
use std::error::Error;

use vlorql_llm::{create_llm_client, LlmClient, LlmConfig, LlmProvider};

/// The system prompt used for every provider. In a real deployment you
/// would inject the database schema and policies; for brevity this
/// example only shows the bare-minimum prompt.
const SYSTEM_PROMPT: &str = "You are a SQL assistant. Reply with a JSON QueryPlan.";

/// Builds the configuration for each supported hosted provider.
fn provider_configs() -> Vec<(&'static str, LlmConfig)> {
    vec![
        (
            "anthropic",
            LlmConfig {
                provider: LlmProvider::Anthropic,
                api_key: env::var("ANTHROPIC_API_KEY").ok(),
                api_base: None,
                model: "claude-sonnet-4-5".to_owned(),
                max_tokens: 4096,
                temperature: 0.0,
                timeout_seconds: 60,
                max_retries: 3,
                extra: Default::default(),
            },
        ),
        (
            "deepseek",
            LlmConfig {
                provider: LlmProvider::DeepSeek,
                api_key: env::var("DEEPSEEK_API_KEY").ok(),
                api_base: None,
                model: "deepseek-v4-pro".to_owned(),
                max_tokens: 4096,
                temperature: 0.0,
                timeout_seconds: 60,
                max_retries: 3,
                extra: Default::default(),
            },
        ),
        (
            "zhipu",
            LlmConfig {
                provider: LlmProvider::Zhipu,
                api_key: env::var("ZHIPU_API_KEY").ok(),
                api_base: None,
                model: "glm-4.7".to_owned(),
                max_tokens: 4096,
                temperature: 0.0,
                timeout_seconds: 60,
                max_retries: 3,
                extra: Default::default(),
            },
        ),
        (
            "openai",
            LlmConfig {
                provider: LlmProvider::OpenAi,
                api_key: env::var("OPENAI_API_KEY").ok(),
                api_base: None,
                model: "gpt-4o-mini".to_owned(),
                max_tokens: 4096,
                temperature: 0.0,
                timeout_seconds: 60,
                max_retries: 3,
                extra: Default::default(),
            },
        ),
    ]
}

/// Validates that every supported provider can be constructed using
/// only its documented environment variable, and reports the result on
/// stderr. Returns a list of `(label, config)` tuples for providers
/// that have credentials available.
fn list_buildable() -> Vec<(String, LlmConfig)> {
    let mut out = Vec::new();
    for (label, config) in provider_configs() {
        match create_llm_client(config.clone()) {
            Ok(_) => {
                eprintln!("[ok]   {label}: client built");
                out.push((label.to_owned(), config));
            }
            Err(error) => {
                eprintln!("[skip] {label}: {error}");
            }
        }
    }
    out
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let build_only = args.iter().any(|arg| arg == "--build-only");
    args.retain(|arg| arg != "--build-only");

    eprintln!("Building every supported provider…");
    let buildable = list_buildable();
    if buildable.is_empty() {
        return Err("no provider has its API key configured".into());
    }

    if build_only {
        eprintln!("--build-only: skipping network call");
        return Ok(());
    }

    let provider_label = env::var("LLM_PROVIDER").unwrap_or_else(|_| "anthropic".to_owned());
    let question = args
        .first()
        .cloned()
        .unwrap_or_else(|| "Show user ids".to_owned());

    let (_, config) = buildable
        .into_iter()
        .find(|(label, _)| label == &provider_label)
        .ok_or_else(|| {
            format!(
                "provider `{provider_label}` is not configured (set LLM_PROVIDER and the matching *_API_KEY)"
            )
        })?;

    eprintln!("Calling {} with model `{}`…", config.provider, config.model);
    let client = create_llm_client(config)?;
    eprintln!("Provider id: {}", client.provider());
    let started = std::time::Instant::now();
    let plan = client.generate_plan(&question, SYSTEM_PROMPT).await?;
    eprintln!("Plan received in {:?}", started.elapsed());
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}
