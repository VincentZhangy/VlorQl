use vlorql_llm::{create_llm_client, LlmConfig, LlmProvider};

#[test]
fn create_llm_client_rejects_missing_api_key_for_hosted_providers() {
    for provider in [
        LlmProvider::OpenAi,
        LlmProvider::Anthropic,
        LlmProvider::DeepSeek,
        LlmProvider::Zhipu,
    ] {
        let config = LlmConfig {
            provider,
            api_key: None,
            api_base: None,
            model: "m".to_owned(),
            ..LlmConfig::default()
        };
        let error = match create_llm_client(config) {
            Ok(_) => panic!("missing key should fail for {provider}"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "G004", "provider={provider}");
    }
}

#[test]
fn create_llm_client_rejects_empty_model() {
    for provider in [
        LlmProvider::OpenAi,
        LlmProvider::Anthropic,
        LlmProvider::DeepSeek,
        LlmProvider::Zhipu,
        LlmProvider::Vllm,
        LlmProvider::Ollama,
    ] {
        let config = LlmConfig {
            provider,
            api_key: Some("sk-test".to_owned()),
            api_base: None,
            model: "  ".to_owned(),
            ..LlmConfig::default()
        };
        let error = match create_llm_client(config) {
            Ok(_) => panic!("empty model should fail for {provider}"),
            Err(error) => error,
        };
        assert_eq!(error.error_code(), "G005", "provider={provider}");
    }
}

#[test]
fn create_llm_client_requires_api_key_for_openai() {
    let config = LlmConfig {
        provider: LlmProvider::OpenAi,
        api_key: None,
        ..LlmConfig::default()
    };
    let error = match create_llm_client(config) {
        Ok(_) => panic!("api key should be required"),
        Err(error) => error,
    };
    assert_eq!(error.error_code(), "G004");
}

#[test]
fn create_llm_client_allows_local_providers_without_key() {
    for provider in [LlmProvider::Vllm, LlmProvider::Ollama] {
        let config = LlmConfig {
            provider,
            api_key: None,
            model: "llama3".to_owned(),
            ..LlmConfig::default()
        };
        let client = match create_llm_client(config) {
            Ok(client) => client,
            Err(error) => panic!("{provider} client should build: {error}"),
        };
        assert_eq!(client.provider(), provider);
    }
}

#[test]
fn create_llm_client_allows_claude_with_anthropic_config() {
    let config = LlmConfig {
        provider: LlmProvider::Anthropic,
        api_key: Some("sk-ant-test".to_owned()),
        model: "claude-sonnet-4-5".to_owned(),
        ..LlmConfig::default()
    };
    let client = create_llm_client(config).expect("anthropic config should build");
    assert_eq!(client.provider(), LlmProvider::Anthropic);
}

#[test]
fn create_llm_client_rejects_zhipu_without_key() {
    let config = LlmConfig {
        provider: LlmProvider::Zhipu,
        api_key: None,
        model: "glm-4".to_owned(),
        ..LlmConfig::default()
    };
    let error = match create_llm_client(config) {
        Ok(_) => panic!("zhipu requires an api key"),
        Err(error) => error,
    };
    assert_eq!(error.error_code(), "G004");
}

#[test]
fn llm_client_report_provider_variant() {
    let openai = create_llm_client(LlmConfig {
        provider: LlmProvider::OpenAi,
        api_key: Some("sk-test".to_owned()),
        model: "gpt-4o-mini".to_owned(),
        ..LlmConfig::default()
    })
    .expect("openai client");
    assert_eq!(openai.provider(), LlmProvider::OpenAi);

    let vllm = create_llm_client(LlmConfig {
        provider: LlmProvider::Vllm,
        api_key: None,
        model: "llama3".to_owned(),
        ..LlmConfig::default()
    })
    .expect("vllm client");
    assert_eq!(vllm.provider(), LlmProvider::Vllm);
}
