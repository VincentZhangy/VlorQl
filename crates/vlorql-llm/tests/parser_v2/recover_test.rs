//! Integration tests for the `parser_v2::recover` module.
//!
//! Verifies that the new recover pipeline handles edge cases correctly.

use vlorql_llm::parser_v2::recover::extract_json_content;

// ── New behaviour (array wrapping) ─────────────────────────────────

#[test]
fn extracts_from_array_wrapper() {
    let input = r#"[{"select":[{"type":"star"}],"from":{"table":"users"}}]"#;
    let result = extract_json_content(input);
    let parsed: serde_json::Value = serde_json::from_str(result).unwrap();
    assert!(
        parsed.is_object(),
        "should extract object from array: {result}"
    );
    assert!(parsed.get("select").is_some(), "should have select field");
    assert!(parsed.get("from").is_some(), "should have from field");
}

// ── Edge cases ─────────────────────────────────────────────────────

#[test]
fn empty_input() {
    assert_eq!(extract_json_content(""), "");
}

#[test]
fn whitespace_only() {
    assert_eq!(extract_json_content("   "), "");
}

#[test]
fn only_braces_no_content() {
    let result = extract_json_content("{}");
    assert_eq!(result, "{}");
}

#[test]
fn only_array_brackets() {
    assert_eq!(extract_json_content("[]"), "[]");
}

#[test]
fn nested_array_wrapping() {
    let input = r#"[[{"a":1}]]"#;
    let result = extract_json_content(input);
    assert_eq!(
        result, r#"{"a":1}"#,
        "should extract object from nested array"
    );
    let parsed: serde_json::Value = serde_json::from_str(result).unwrap();
    assert!(parsed.is_object());
    assert_eq!(parsed.get("a").and_then(|v| v.as_i64()), Some(1));
}

#[test]
fn multiple_objects_picks_first() {
    let input = r#"some text {"a":1} and then {"b":2}"#;
    let result = extract_json_content(input);
    assert_eq!(result, r#"{"a":1}"#);
}

// ── Realistic LLM output patterns ──────────────────────────────────

#[test]
fn realistic_openai_response() {
    let input = r#"Here is the query plan:

{
  "select": [{"type": "column_ref", "table": "users", "column": "name"}],
  "from": {"table": "users"}
}"#;
    let result = extract_json_content(input);
    let parsed: serde_json::Value = serde_json::from_str(result).unwrap();
    assert!(parsed.is_object());
}

#[test]
fn realistic_deepseek_response() {
    let input = "```json
{
  \"select\": [{\"type\": \"star\"}],
  \"from\": {\"table\": \"orders\"},
  \"where\": {\"type\": \"comparison\", \"left\": {\"type\": \"column_ref\", \"column\": \"status\"}, \"op\": \"eq\", \"right\": {\"type\": \"literal\", \"value\": \"active\", \"data_type\": \"text\"}}
}
```";
    let result = extract_json_content(input);
    assert!(result.starts_with('{'), "should extract JSON object");
}

#[test]
fn realistic_qwen_response() {
    let input = "Based on the query, here is the plan:
[
  {
    \"select\": [{\"type\": \"star\"}],
    \"from\": {\"table\": \"products\"}
  }
]
Let me know if you need modifications.";
    let result = extract_json_content(input);
    let parsed: serde_json::Value = serde_json::from_str(result).unwrap();
    assert!(parsed.is_object(), "should extract object from array");
}

#[test]
fn realistic_markdown_fence() {
    let fenced = "```json\n{\"a\":1}\n```";
    assert_eq!(extract_json_content(fenced), "{\"a\":1}");
}

#[test]
fn realistic_prefix_text() {
    let with_prefix = "Here is the JSON: {\"a\":1} end";
    assert_eq!(extract_json_content(with_prefix), "{\"a\":1}");
}

#[test]
fn realistic_nested_braces() {
    let nested = "text {\"outer\": {\"inner\": 1}} trailing";
    assert_eq!(extract_json_content(nested), "{\"outer\": {\"inner\": 1}}");
}

#[test]
fn returns_original_when_no_json_found() {
    let no_json = "this is not json at all";
    assert_eq!(extract_json_content(no_json), no_json);
}

#[test]
fn passes_through_valid_json() {
    let valid = r#"{"select":[{"type":"star"}],"from":{"table":"users"}}"#;
    assert_eq!(extract_json_content(valid), valid);
}
