//! Integration tests for the full V2 pipeline: recover → normalize → build.
//!
//! Tests the end-to-end flow from raw LLM output to typed QueryPlan.

use vlorql_llm::parser_v2::builder::query_builder;
use vlorql_llm::parser_v2::normalize::pipeline;
use vlorql_llm::parser_v2::recover::extract_json_content;

/// Run the full V2 pipeline: recover → normalize → build.
fn run_pipeline(raw: &str) -> Result<vlorql_core::schema::QueryPlan, Box<dyn std::error::Error>> {
    // Stage 1: Recover
    let json_str = extract_json_content(raw);
    // Stage 2: Normalize
    let mut value: serde_json::Value = serde_json::from_str(json_str)?;
    let _ = pipeline::normalize(&mut value);
    // Stage 3: Build
    let plan = query_builder::build_plan(&value)?;
    Ok(plan)
}

fn run_pipeline_str(
    raw: &str,
) -> Result<vlorql_core::schema::QueryPlan, Box<dyn std::error::Error>> {
    run_pipeline(raw)
}

// ── OpenAI-style output ───────────────────────────────────────────

#[test]
fn openai_style_full_pipeline() {
    let raw = r#"{
        "select": [{"type": "column_ref", "table": "users", "column": "name"}],
        "from": {"table": "users"},
        "where": {"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}}
    }"#;
    let plan = run_pipeline_str(raw).unwrap();
    assert_eq!(plan.select.len(), 1);
    assert_eq!(plan.from.table, "users");
    assert!(plan.r#where.is_some());
}

// ── DeepSeek-style output (filter instead of where) ───────────────

#[test]
fn deepseek_style_full_pipeline() {
    let raw = r#"{
        "select": [{"type": "star"}],
        "from": {"table": "orders"},
        "filter": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}
    }"#;
    let plan = run_pipeline_str(raw).unwrap();
    assert_eq!(plan.select.len(), 1);
    assert!(plan.r#where.is_some(), "filter should become where");
}

// ── Qwen-style output (array wrapper, string projections) ─────────

#[test]
fn qwen_style_full_pipeline() {
    // Qwen sometimes uses string projections and non-standard field names.
    let raw = r#"{"projection": ["id", "name"], "source": "users", "filter": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}}"#;
    let plan = run_pipeline_str(raw).unwrap();
    assert_eq!(plan.select.len(), 2);
    assert_eq!(plan.from.table, "users");
    assert!(plan.r#where.is_some());
}

// ── Llama-style output (markdown fence, where as array) ───────────

#[test]
fn llama_style_full_pipeline() {
    let raw = "```json\n{\"select\": [{\"type\": \"star\"}], \"from\": {\"table\": \"products\"}, \"where\": [{\"type\": \"comparison\", \"left\": {\"column\": \"price\"}, \"op\": \"lt\", \"right\": {\"value\": 100}}], \"sort\": [{\"expr\": {\"column\": \"name\"}, \"descending\": true}]}\n```";
    let plan = run_pipeline_str(raw).unwrap();
    assert_eq!(plan.select.len(), 1);
    assert_eq!(plan.from.table, "products");
    assert!(plan.r#where.is_some());
    assert!(plan.order_by.is_some());
}

// ── GLM-style output (conditions, fields aliases) ─────────────────

#[test]
fn glm_style_full_pipeline() {
    let raw = r#"{"fields": [{"type": "column_ref", "column": "id"}], "from": {"table": "users"}, "conditions": [{"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}]}"#;
    let plan = run_pipeline_str(raw).unwrap();
    assert_eq!(plan.select.len(), 1);
    assert!(plan.r#where.is_some());
}

// ── Messy output with multiple issues ─────────────────────────────

#[test]
fn messy_multi_model_full_pipeline() {
    let raw = "some text ```json\n{\"projection\": [{\"column\": \"name\"}, {\"column\": \"email\"}], \"source\": \"employees\", \"filter\": [{\"type\": \"comparison\", \"left\": {\"column\": \"salary\"}, \"operator\": \">\", \"right\": {\"value\": 50000, \"data_type\": \"integer\"}}], \"sort\": [{\"expr\": {\"column\": \"name\"}, \"descending\": true}]}\n``` more text";
    let plan = run_pipeline_str(raw).unwrap();
    assert_eq!(plan.select.len(), 2);
    assert_eq!(plan.from.table, "employees");
    assert!(plan.r#where.is_some());
    assert!(plan.order_by.is_some());
}

// ── Minimal plan ──────────────────────────────────────────────────

#[test]
fn minimal_star_plan() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}}"#;
    let plan = run_pipeline_str(raw).unwrap();
    assert_eq!(plan.select.len(), 1);
    assert_eq!(plan.from.table, "users");
    assert!(plan.r#where.is_none());
    assert!(plan.group_by.is_none());
    assert!(plan.order_by.is_none());
    assert!(plan.limit.is_none());
    assert!(plan.offset.is_none());
    assert!(plan.joins.is_none());
    assert!(plan.ctes.is_none());
}

// ── Full plan with joins and CTEs ─────────────────────────────────

#[test]
fn full_plan_with_joins_and_ctes() {
    let raw = r#"{
        "select": [{"type": "column_ref", "column": "u.name"}, {"type": "column_ref", "column": "o.total"}],
        "from": {"table": "users", "alias": "u"},
        "joins": [{"join_type": "inner", "right_table": {"table": "orders", "alias": "o"}, "on": {"type": "comparison", "left": {"type": "column_ref", "column": "u.id"}, "op": "eq", "right": {"type": "column_ref", "column": "o.user_id"}}}],
        "where": {"type": "comparison", "left": {"type": "column_ref", "column": "o.status"}, "op": "eq", "right": {"type": "literal", "value": "completed", "data_type": "string"}},
        "order_by": [{"expr": {"type": "column_ref", "column": "o.total"}, "descending": true}],
        "limit": 10,
        "ctes": [{"name": "recent_orders", "query": {"select": [{"type": "star"}], "from": {"table": "orders"}, "where": {"type": "comparison", "left": {"type": "column_ref", "column": "created_at"}, "op": "gt", "right": {"type": "literal", "value": "2024-01-01", "data_type": "string"}}}}]
    }"#;
    let plan = run_pipeline_str(raw).unwrap();
    assert_eq!(plan.select.len(), 2);
    assert_eq!(plan.from.table, "users");
    assert_eq!(plan.from.alias, Some("u".to_owned()));
    assert!(plan.r#where.is_some());
    assert!(plan.order_by.is_some());
    assert_eq!(plan.limit, Some(10));
    assert!(plan.joins.is_some());
    assert_eq!(plan.joins.unwrap().len(), 1);
    assert!(plan.ctes.is_some());
    assert_eq!(plan.ctes.unwrap().len(), 1);
}

// ── Error handling ────────────────────────────────────────────────

#[test]
fn error_on_missing_from() {
    let raw = r#"{"select": [{"type": "star"}]}"#;
    let result = run_pipeline_str(raw);
    assert!(result.is_err(), "should fail on missing from");
}

#[test]
fn error_on_invalid_json() {
    let raw = "this is not json at all";
    let result = run_pipeline_str(raw);
    assert!(result.is_err(), "should fail on invalid JSON");
}

// ── Idempotent pipeline (normalize twice) ─────────────────────────

#[test]
fn double_normalize_roundtrip() {
    let raw = r#"{"projection": [{"type": "star"}], "source": "users", "filter": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18, "data_type": "integer"}}}"#;
    let json_str = extract_json_content(raw);
    let mut value: serde_json::Value = serde_json::from_str(json_str).unwrap();
    // First normalize
    let _ = pipeline::normalize(&mut value);
    // Second normalize — should be no-op
    assert!(
        !pipeline::normalize(&mut value),
        "normalize should be idempotent"
    );
    // Build
    let plan = query_builder::build_plan(&value).unwrap();
    assert_eq!(plan.select.len(), 1);
    assert_eq!(plan.from.table, "users");
    assert!(plan.r#where.is_some());
}
