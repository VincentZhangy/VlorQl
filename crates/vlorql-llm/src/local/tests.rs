use super::*;
use mockito::{Matcher, Server};
use vlorql_core::schema::{FromClause, Projection, QueryPlan};

fn plan() -> QueryPlan {
    QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause::table("users".to_owned(), Some("t1".to_owned())),
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
        distinct: false,
        distinct_on: None,
        set_operation: None,
    }
}

fn local_config(provider: LlmProvider, model: &str) -> LlmConfig {
    LlmConfig {
        provider,
        api_key: None,
        api_base: Some("http://127.0.0.1:0".to_owned()),
        model: model.to_owned(),
        max_tokens: 1024,
        temperature: 0.0,
        timeout_seconds: 60,
        max_retries: 1,
        extra: std::collections::HashMap::new(),
    }
}

fn vllm_chat_response(plan: &QueryPlan) -> String {
    json!({
        "id": "vllm-1",
        "model": "Qwen2.5-7B-Instruct",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": serde_json::to_string(plan).expect("plan should serialize"),
            },
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
    })
    .to_string()
}

fn ollama_chat_response(plan: &QueryPlan) -> String {
    json!({
        "model": "llama3.2",
        "created_at": "2026-01-01T00:00:00Z",
        "message": {
            "role": "assistant",
            "content": serde_json::to_string(plan).expect("plan should serialize"),
        },
        "done": true,
        "done_reason": "stop",
    })
    .to_string()
}

#[test]
fn resolve_backend_prefers_extra_override() {
    let mut config = local_config(LlmProvider::Ollama, "llama3.2");
    config
        .extra
        .insert("backend".to_owned(), Value::String("vllm".to_owned()));
    let backend = resolve_backend(&config).expect("backend should resolve");
    assert_eq!(backend, LocalBackend::VLLM);

    config
        .extra
        .insert("backend".to_owned(), Value::String("OLLAMA".to_owned()));
    let backend = resolve_backend(&config).expect("backend should resolve");
    assert_eq!(backend, LocalBackend::Ollama);
}

#[test]
fn resolve_backend_falls_back_to_provider() {
    let config = local_config(LlmProvider::Ollama, "llama3.2");
    assert_eq!(
        resolve_backend(&config).expect("ollama"),
        LocalBackend::Ollama
    );

    let config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
    assert_eq!(resolve_backend(&config).expect("vllm"), LocalBackend::VLLM);

    let mut config = local_config(LlmProvider::OpenAi, "gpt-4o-mini");
    config.extra.remove("backend");
    assert_eq!(
        resolve_backend(&config).expect("default vllm"),
        LocalBackend::VLLM
    );
}

#[test]
fn resolve_backend_rejects_unknown_value() {
    let mut config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
    config
        .extra
        .insert("backend".to_owned(), Value::String("llamacpp".to_owned()));
    let error = resolve_backend(&config).expect_err("unknown backend should fail");
    assert_eq!(error.error_code(), "G003");
}

#[test]
fn resolve_base_url_strips_chat_suffix() {
    let mut config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
    config.api_base = Some("http://localhost:8000/v1/chat/completions".to_owned());
    assert_eq!(
        resolve_base_url(&config, LocalBackend::VLLM),
        "http://localhost:8000/v1"
    );

    config.api_base = Some("http://localhost:8000/v1/".to_owned());
    assert_eq!(
        resolve_base_url(&config, LocalBackend::VLLM),
        "http://localhost:8000/v1"
    );

    config.api_base = Some("http://localhost:11434/api/chat".to_owned());
    assert_eq!(
        resolve_base_url(&config, LocalBackend::Ollama),
        "http://localhost:11434"
    );
}

#[test]
fn resolve_base_url_uses_backend_default_when_unset() {
    let mut config = local_config(LlmProvider::Vllm, "m");
    config.api_base = None;
    assert_eq!(
        resolve_base_url(&config, LocalBackend::VLLM),
        DEFAULT_VLLM_BASE_URL
    );
    assert_eq!(
        resolve_base_url(&config, LocalBackend::Ollama),
        DEFAULT_OLLAMA_BASE_URL
    );
}

#[test]
fn local_client_endpoint_appends_chat_suffix() {
    let mut config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
    config.api_base = Some("http://localhost:8000/v1".to_owned());
    let client = LocalClient::new(config).expect("client should build");
    assert_eq!(
        client.endpoint(),
        "http://localhost:8000/v1/chat/completions"
    );

    let mut config = local_config(LlmProvider::Ollama, "llama3.2");
    config.api_base = Some("http://localhost:11434".to_owned());
    let client = LocalClient::new(config).expect("client should build");
    assert_eq!(client.endpoint(), "http://localhost:11434/api/chat");
}

#[test]
fn local_client_provider_returns_active_backend() {
    let mut config = local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct");
    config.api_base = None;
    let client = LocalClient::new(config).expect("client should build");
    assert_eq!(client.provider(), LlmProvider::Vllm);

    let mut config = local_config(LlmProvider::Ollama, "llama3.2");
    config.api_base = None;
    let client = LocalClient::new(config).expect("client should build");
    assert_eq!(client.provider(), LlmProvider::Ollama);
}

#[test]
fn local_client_rejects_empty_model() {
    let mut config = local_config(LlmProvider::Vllm, "placeholder");
    config.model = "   ".to_owned();
    config.api_base = None;
    let error = LocalClient::new(config).expect_err("empty model should fail");
    assert_eq!(error.error_code(), "G005");
}

#[tokio::test]
async fn vllm_client_uses_json_schema_response_format() {
    let mut server = Server::new_async().await;
    let expected = plan();
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::Regex(
            r#""model":"Qwen2\.5-7B-Instruct".*"response_format".*"json_schema""#.to_owned(),
        ))
        .with_status(200)
        .with_body(vllm_chat_response(&expected))
        .create_async()
        .await;

    let config = LlmConfig {
        api_base: Some(format!("{}/v1", server.url())),
        ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
    };
    let client = LocalClient::new(config).expect("client should build");
    let request_body = client.build_request_body("show users", "system", false, None);
    assert_eq!(request_body["model"], "Qwen2.5-7B-Instruct");
    assert_eq!(request_body["response_format"]["type"], "json_schema");
    assert_eq!(
        request_body["response_format"]["json_schema"]["name"],
        "QueryPlan"
    );
    assert!(
        request_body["response_format"]["json_schema"]
            .get("schema")
            .is_some()
    );
    assert_eq!(request_body["temperature"], 0.0);
    assert_eq!(request_body["max_tokens"], 1024);
    let messages = request_body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "show users");

    let (actual, _usage) = client
        .generate_plan("show users", "system", None)
        .await
        .expect("plan should parse");
    assert_eq!(actual, expected);
    assert_eq!(client.provider(), LlmProvider::Vllm);
    assert_eq!(client.config().model, "Qwen2.5-7B-Instruct");
    mock.assert_async().await;
}

#[tokio::test]
async fn vllm_client_sends_bearer_when_api_key_set() {
    let mut server = Server::new_async().await;
    let expected = plan();
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header("authorization", "Bearer test-key")
        .with_status(200)
        .with_body(vllm_chat_response(&expected))
        .create_async()
        .await;

    let config = LlmConfig {
        api_key: Some("test-key".to_owned()),
        api_base: Some(format!("{}/v1", server.url())),
        ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
    };
    let client = LocalClient::new(config).expect("client should build");
    let (actual, _usage) = client
        .generate_plan("q", "s", None)
        .await
        .expect("plan should parse");
    assert_eq!(actual, expected);
    mock.assert_async().await;
}

#[tokio::test]
async fn vllm_client_falls_back_to_json_object_on_400() {
    let mut server = Server::new_async().await;
    let expected = plan();
    let failure = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::Regex(
            r#""response_format".*"json_schema""#.to_owned(),
        ))
        .with_status(400)
        .with_body(r#"{"error":{"message":"structured output backend unavailable"}}"#)
        .create_async()
        .await;
    let success = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::Regex(
            r#""response_format":\{"type":"json_object"\}"#.to_owned(),
        ))
        .with_status(200)
        .with_body(vllm_chat_response(&expected))
        .create_async()
        .await;

    let config = LlmConfig {
        api_base: Some(format!("{}/v1", server.url())),
        max_retries: 3,
        ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
    };
    let client = LocalClient::new(config).expect("client should build");
    let (actual, _usage) = client
        .generate_plan("q", "s", None)
        .await
        .expect("fallback should succeed");
    assert_eq!(actual, expected);
    failure.assert_async().await;
    success.assert_async().await;
}

#[tokio::test]
async fn vllm_client_emits_sse_delta_chunks() {
    use futures::StreamExt;
    let mut server = Server::new_async().await;
    let body = [
        format!(
            "data: {}\n\n",
            json!({
                "id": "1",
                "choices": [{"delta": {"content": "hello "}}],
            })
        ),
        format!(
            "data: {}\n\n",
            json!({
                "id": "1",
                "choices": [{"delta": {"content": "world"}}],
            })
        ),
        "data: [DONE]\n".to_owned(),
    ]
    .join("");
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header("accept", "text/event-stream")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let config = LlmConfig {
        api_base: Some(format!("{}/v1", server.url())),
        ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
    };
    let client = LocalClient::new(config).expect("client should build");
    let body = client.build_request_body("hi", "system", true, None);
    assert_eq!(body["stream"], true);

    let mut result = client
        .stream_plan("hi".to_owned(), "system".to_owned())
        .await
        .expect("stream should be produced");
    let mut combined = String::new();
    while let Some(chunk) = result.stream.next().await {
        combined.push_str(&chunk.expect("chunk should be Ok"));
    }
    assert_eq!(combined, "hello world");
    mock.assert_async().await;
}

#[tokio::test]
async fn vllm_client_stream_propagates_http_error() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(503)
        .with_body(r#"{"error":{"message":"unavailable"}}"#)
        .create_async()
        .await;
    let config = LlmConfig {
        api_base: Some(format!("{}/v1", server.url())),
        ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
    };
    let client = LocalClient::new(config).expect("client should build");
    let outcome = client
        .stream_plan("hi".to_owned(), "system".to_owned())
        .await;
    let err = match outcome {
        Ok(_) => panic!("503 should produce an error"),
        Err(error) => error,
    };
    assert_eq!(err.error_code(), "L001");
    mock.assert_async().await;
}

#[tokio::test]
async fn ollama_client_uses_format_field_with_schema() {
    let mut server = Server::new_async().await;
    let expected = plan();
    let mock = server
        .mock("POST", "/api/chat")
        .match_body(Matcher::Any)
        .with_status(200)
        .with_body(ollama_chat_response(&expected))
        .create_async()
        .await;

    let mut extra = std::collections::HashMap::new();
    extra.insert(
        "strict_json_schema".to_owned(),
        serde_json::Value::Bool(true),
    );
    let config = LlmConfig {
        api_base: Some(server.url()),
        extra,
        ..local_config(LlmProvider::Ollama, "llama3.2")
    };
    let client = LocalClient::new(config).expect("client should build");
    let request_body = client.build_request_body("show users", "system", false, None);
    assert_eq!(request_body["model"], "llama3.2");
    assert_eq!(request_body["stream"], false);
    assert!(
        request_body["format"].is_object(),
        "format should be a JSON Schema object"
    );
    assert_eq!(request_body["options"]["temperature"], 0.0);
    assert_eq!(request_body["options"]["num_predict"], 1024);

    let (actual, _usage) = client
        .generate_plan("show users", "system", None)
        .await
        .expect("plan should parse");
    assert_eq!(actual, expected);
    assert_eq!(client.provider(), LlmProvider::Ollama);
    mock.assert_async().await;
}

#[tokio::test]
async fn ollama_client_uses_json_string_when_schema_disabled() {
    let mut server = Server::new_async().await;
    let expected = plan();
    let mock = server
        .mock("POST", "/api/chat")
        .match_body(Matcher::Regex(
            r#""format":"json".*"stream":false"#.to_owned(),
        ))
        .with_status(200)
        .with_body(ollama_chat_response(&expected))
        .create_async()
        .await;

    let mut config = LlmConfig {
        api_base: Some(server.url()),
        ..local_config(LlmProvider::Ollama, "llama3.2")
    };
    config
        .extra
        .insert("strict_json_schema".to_owned(), Value::Bool(false));
    let client = LocalClient::new(config).expect("client should build");
    let request_body = client.build_request_body("q", "s", false, None);
    assert_eq!(request_body["format"], "json");
    let (actual, _usage) = client
        .generate_plan("q", "s", None)
        .await
        .expect("plan should parse");
    assert_eq!(actual, expected);
    mock.assert_async().await;
}

#[tokio::test]
async fn ollama_client_parses_message_content_field() {
    let mut server = Server::new_async().await;
    let expected = plan();
    let raw = serde_json::to_string(&expected).expect("plan should serialize");
    let body = json!({
        "model": "llama3.2",
        "message": {"role": "assistant", "content": raw},
        "done": true,
    })
    .to_string();
    let mock = server
        .mock("POST", "/api/chat")
        .with_status(200)
        .with_body(body)
        .create_async()
        .await;
    let config = LlmConfig {
        api_base: Some(server.url()),
        ..local_config(LlmProvider::Ollama, "llama3.2")
    };
    let client = LocalClient::new(config).expect("client should build");
    let (actual, _usage) = client
        .generate_plan("q", "s", None)
        .await
        .expect("ollama plan should parse");
    assert_eq!(actual, expected);
    mock.assert_async().await;
}

#[tokio::test]
async fn ollama_client_returns_error_for_empty_content() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/api/chat")
        .with_status(200)
        .with_body(
            json!({
                "model": "llama3.2",
                "message": {"role": "assistant", "content": ""},
                "done": true,
            })
            .to_string(),
        )
        .create_async()
        .await;
    let config = LlmConfig {
        api_base: Some(server.url()),
        ..local_config(LlmProvider::Ollama, "llama3.2")
    };
    let client = LocalClient::new(config).expect("client should build");
    let error = client
        .generate_plan("q", "s", None)
        .await
        .expect_err("empty content should fail");
    assert_eq!(error.error_code(), "L003");
    mock.assert_async().await;
}

#[tokio::test]
async fn ollama_client_converts_error_response() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/api/chat")
        .with_status(500)
        .with_body(r#"{"error":"model not loaded"}"#)
        .create_async()
        .await;
    let config = LlmConfig {
        api_base: Some(server.url()),
        ..local_config(LlmProvider::Ollama, "llama3.2")
    };
    let client = LocalClient::new(config).expect("client should build");
    let error = client
        .generate_plan("q", "s", None)
        .await
        .expect_err("500 should be reported");
    assert_eq!(error.error_code(), "L001");
    mock.assert_async().await;
}

#[tokio::test]
async fn ollama_client_stream_emits_ndjson_chunks() {
    use futures::StreamExt;
    let mut server = Server::new_async().await;
    let body = [
        json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": "hello "},
            "done": false,
        })
        .to_string(),
        json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": "world"},
            "done": false,
        })
        .to_string(),
        json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": ""},
            "done": true,
        })
        .to_string(),
    ]
    .join("\n");
    let mock = server
        .mock("POST", "/api/chat")
        .match_body(Matcher::Regex(r#""stream":true"#.to_owned()))
        .with_status(200)
        .with_body(body)
        .create_async()
        .await;
    let config = LlmConfig {
        api_base: Some(server.url()),
        ..local_config(LlmProvider::Ollama, "llama3.2")
    };
    let client = LocalClient::new(config).expect("client should build");
    let mut result = client
        .stream_plan("hi".to_owned(), "system".to_owned())
        .await
        .expect("stream should be produced");
    let mut combined = String::new();
    while let Some(chunk) = result.stream.next().await {
        combined.push_str(&chunk.expect("chunk should be Ok"));
    }
    assert_eq!(combined, "hello world");
    mock.assert_async().await;
}

#[tokio::test]
async fn ollama_client_stream_propagates_http_error() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/api/chat")
        .with_status(500)
        .with_body(r#"{"error":"down"}"#)
        .create_async()
        .await;
    let config = LlmConfig {
        api_base: Some(server.url()),
        ..local_config(LlmProvider::Ollama, "llama3.2")
    };
    let client = LocalClient::new(config).expect("client should build");
    let outcome = client
        .stream_plan("hi".to_owned(), "system".to_owned())
        .await;
    let err = match outcome {
        Ok(_) => panic!("500 should produce an error"),
        Err(error) => error,
    };
    assert_eq!(err.error_code(), "L001");
    mock.assert_async().await;
}

#[tokio::test]
async fn local_client_translates_connection_failure_into_timeout_or_api_error() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(vllm_chat_response(&plan()))
        .create_async()
        .await;
    let url = server.url();
    drop(mock);
    drop(server);
    let config = LlmConfig {
        api_base: Some(format!("{url}/v1")),
        timeout_seconds: 1,
        ..local_config(LlmProvider::Vllm, "Qwen2.5-7B-Instruct")
    };
    let client = LocalClient::new(config).expect("client should build");
    let error = client
        .generate_plan("q", "s", None)
        .await
        .expect_err("dead endpoint should fail");
    assert!(
        error.error_code() == "L001" || error.error_code() == "L002",
        "expected transport/timeout error, got {}",
        error.error_code()
    );
}
