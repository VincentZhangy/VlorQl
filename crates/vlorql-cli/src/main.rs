//! Command-line interface for the VlorQl query orchestration framework.

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use vlorql::{CompiledQuery, VlorQl};
use vlorql_core::errors::ValidationErrors;
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{DialectProfile, QueryPlan, SchemaSnapshot};
use vlorql_llm::{LlmConfig, LlmProvider, create_llm_client};

const DEFAULT_CONFIG_PATH: &str = "vlorql.toml";

type LlmOverrides = (
    LlmProvider,
    Option<String>,
    Option<String>,
    Option<String>,
    usize,
);

#[derive(Debug, Parser)]
#[command(
    name = "vlorql",
    version,
    about = "Safe, policy-driven SQL query orchestration"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a query plan with an LLM, validate it, and print compiled SQL.
    Query {
        /// Natural-language question to answer.
        #[arg(short, long)]
        question: String,
        /// TOML or JSON configuration file.
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH, env = "VLORQL_CONFIG")]
        config: PathBuf,
        /// Override the dialect from the configuration file.
        #[arg(long)]
        dialect: Option<String>,
        /// LLM provider (openai, anthropic, deepseek, zhipu, vllm, ollama).
        #[arg(long, default_value = "openai")]
        provider: String,
        /// API key.
        #[arg(long, env = "LLM_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// Model name.
        #[arg(long, env = "LLM_MODEL")]
        model: Option<String>,
        /// API base URL.
        #[arg(long, env = "LLM_API_BASE")]
        api_base: Option<String>,
        /// Number of validation retries after the initial plan.
        #[arg(long, default_value_t = 2)]
        max_retries: usize,
    },
    /// Validate a QueryPlan file without calling an LLM.
    Validate {
        /// JSON or TOML query plan file.
        #[arg(long, value_name = "FILE")]
        plan_file: PathBuf,
        /// TOML or JSON configuration file.
        #[arg(short, long, default_value = DEFAULT_CONFIG_PATH, env = "VLORQL_CONFIG")]
        config: PathBuf,
        /// Override the dialect from the configuration file.
        #[arg(long)]
        dialect: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    schema: SchemaSnapshot,
    #[serde(default)]
    dialect: DialectProfile,
    #[serde(default)]
    policy: PolicyConfig,
    #[serde(default)]
    llm: LlmSettings,
}

#[derive(Debug, Default, Deserialize)]
struct LlmSettings {
    provider: Option<LlmProvider>,
    model: Option<String>,
    api_base: Option<String>,
    api_key_env: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Query {
            question,
            config,
            dialect,
            provider,
            api_key,
            model,
            api_base,
            max_retries,
        } => {
            let file_config = load_config(&config)?;
            let provider = parse_provider(&provider)?;
            let facade = build_facade(
                file_config,
                dialect.as_deref(),
                Some((provider, api_key, model, api_base, max_retries)),
            )?;
            let compiled = facade
                .query(&question)
                .await
                .context("query execution failed")?;
            print_compiled_query(&compiled);
        }
        Command::Validate {
            plan_file,
            config,
            dialect,
        } => {
            let file_config = load_config(&config)?;
            let plan = load_plan(&plan_file)?;
            let facade = build_facade(file_config, dialect.as_deref(), None)?;
            validate_plan(&facade, &plan)?;
        }
    }
    Ok(())
}

fn load_config(path: &Path) -> Result<FileConfig> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file `{}`", path.display()))?;
    parse_document(path, &contents).with_context(|| format!("failed to parse `{}`", path.display()))
}

fn load_plan(path: &Path) -> Result<QueryPlan> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read plan file `{}`", path.display()))?;
    parse_plan_document(path, &contents)
        .with_context(|| format!("failed to parse plan `{}`", path.display()))
}

fn parse_document(path: &Path, contents: &str) -> Result<FileConfig> {
    match extension(path) {
        Some("json") => serde_json::from_str(contents).map_err(Into::into),
        Some("toml") | None => toml::from_str(contents).map_err(Into::into),
        Some(other) => Err(anyhow!(
            "unsupported config extension `.{other}`; use .toml or .json"
        )),
    }
}

fn parse_plan_document(path: &Path, contents: &str) -> Result<QueryPlan> {
    match extension(path) {
        Some("json") => serde_json::from_str(contents).map_err(Into::into),
        Some("toml") => toml::from_str(contents).map_err(Into::into),
        Some(other) => Err(anyhow!(
            "unsupported plan extension `.{other}`; use .toml or .json"
        )),
        None => {
            serde_json::from_str(contents).or_else(|_| toml::from_str(contents).map_err(Into::into))
        }
    }
}

fn build_facade(
    config: FileConfig,
    dialect_override: Option<&str>,
    llm_overrides: Option<LlmOverrides>,
) -> Result<VlorQl> {
    let FileConfig {
        schema,
        dialect,
        policy,
        llm,
    } = config;
    let mut builder = VlorQl::builder()
        .with_schema(Arc::new(schema))
        .with_dialect(dialect)
        .with_policy(policy);

    if let Some(dialect_name) = dialect_override {
        builder = builder.with_dialect_name(dialect_name);
    }

    if let Some((provider, cli_api_key, cli_model, cli_api_base, max_retries)) = llm_overrides {
        let api_key_env = llm.api_key_env.as_deref().unwrap_or("LLM_API_KEY");
        let api_key = cli_api_key
            .or_else(|| env::var(api_key_env).ok())
            .filter(|key| !key.trim().is_empty());
        let model = cli_model.or(llm.model);
        let api_base = cli_api_base.or(llm.api_base);
        let llm_provider = llm.provider.unwrap_or(provider);

        let api_key = api_key.or_else(|| {
            let default_env = "LLM_API_KEY";
            env::var(default_env).ok().filter(|k| !k.trim().is_empty())
        });
        let llm_config = LlmConfig {
            provider: llm_provider,
            api_key,
            api_base,
            model: model.unwrap_or_else(|| "gpt-4o-mini".to_owned()),
            ..LlmConfig::default()
        };
        let client = create_llm_client(llm_config).map_err(|e| anyhow!(e))?;
        builder = builder
            .with_llm_client(client)
            .with_max_retries(max_retries);
    }

    builder.build().map_err(Into::into)
}

fn parse_provider(s: &str) -> Result<LlmProvider> {
    match s.to_lowercase().as_str() {
        "openai" => Ok(LlmProvider::OpenAi),
        "anthropic" => Ok(LlmProvider::Anthropic),
        "deepseek" => Ok(LlmProvider::DeepSeek),
        "zhipu" => Ok(LlmProvider::Zhipu),
        "vllm" => Ok(LlmProvider::Vllm),
        "ollama" => Ok(LlmProvider::Ollama),
        _ => Err(anyhow!(
            "unknown provider '{s}'; expected one of: openai, anthropic, deepseek, zhipu, vllm, ollama"
        )),
    }
}

fn validate_plan(facade: &VlorQl, plan: &QueryPlan) -> Result<()> {
    match facade.validate_only(plan) {
        Ok(_) => {
            println!("Validation succeeded.");
            Ok(())
        }
        Err(errors) => {
            print_validation_errors(&errors);
            std::process::exit(1);
        }
    }
}

fn print_validation_errors(errors: &ValidationErrors) {
    eprintln!("Validation failed:");
    for error in errors.as_slice() {
        let response = error.to_error_response();
        eprintln!("- [{}] {}", response.code, response.message);
        if let Some(suggestion) = response.suggestion {
            eprintln!("  suggestion: {suggestion}");
        }
    }
}

fn print_compiled_query(compiled: &CompiledQuery) {
    println!("SQL:");
    println!("{}", compiled.sql);
    println!("Dialect: {:?}", compiled.dialect);
    if compiled.parameters.is_empty() {
        println!("Parameters: []");
        return;
    }

    println!("Parameters:");
    for (index, parameter) in compiled.parameters.iter().enumerate() {
        let value = serde_json::to_string(&parameter.value)
            .unwrap_or_else(|_| "<unserializable value>".to_owned());
        println!(
            "  ${}: value={}, type={:?}",
            index + 1,
            value,
            parameter.data_type
        );
    }
}

fn extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|extension| extension.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_query_and_validate_subcommands() {
        let cli = Cli::try_parse_from([
            "vlorql",
            "query",
            "--question",
            "show users",
            "--config",
            "config.toml",
        ])
        .expect("query arguments should parse");
        assert!(matches!(cli.command, Command::Query { .. }));

        let cli = Cli::try_parse_from([
            "vlorql",
            "validate",
            "--plan-file",
            "plan.json",
            "--config",
            "config.toml",
        ])
        .expect("validate arguments should parse");
        assert!(matches!(cli.command, Command::Validate { .. }));
    }

    #[test]
    fn extension_is_case_sensitive_by_design() {
        assert_eq!(extension(Path::new("config.toml")), Some("toml"));
        assert_eq!(extension(Path::new("plan")), None);
    }
}
