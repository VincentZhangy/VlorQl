//! Integration tests for the `parser_v2::optimize` module.
//!
//! Tests the optimizer in the context of the full V2 pipeline
//! (recover → normalize → build → fix → validate → optimize).

use vlorql_llm::parser_v2::builder::query_builder;
use vlorql_llm::parser_v2::normalize::pipeline;
use vlorql_llm::parser_v2::optimize::optimize;
use vlorql_llm::parser_v2::recover::extract_json_content;

fn build_plan(raw: &str) -> Result<vlorql_core::schema::QueryPlan, Box<dyn std::error::Error>> {
    let json_str = extract_json_content(raw);
    let mut value: serde_json::Value = serde_json::from_str(json_str)?;
    pipeline::normalize(&mut value);
    let plan = query_builder::build_plan(&value)?;
    Ok(plan)
}

// ── Predicate simplification ──────────────────────────────────────

#[test]
fn simplify_and_true_in_where() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}, "where": {"type": "and", "left": {"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}}, "right": {"type": "comparison", "left": {"type": "literal", "value": true, "data_type": "boolean"}, "op": "eq", "right": {"type": "literal", "value": true, "data_type": "boolean"}}}}"#;
    let mut plan = build_plan(raw).unwrap();
    assert!(optimize(&mut plan));
    // AND TRUE should be removed
    assert!(matches!(
        plan.r#where.unwrap(),
        vlorql_core::schema::Predicate::Comparison { .. }
    ));
}

#[test]
fn simplify_not_not() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}, "where": {"type": "not", "child": {"type": "not", "child": {"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}}}}}"#;
    let mut plan = build_plan(raw).unwrap();
    assert!(optimize(&mut plan));
    // NOT NOT should be eliminated
    assert!(matches!(
        plan.r#where.unwrap(),
        vlorql_core::schema::Predicate::Comparison { .. }
    ));
}

// ── Projection pruning ────────────────────────────────────────────

#[test]
fn remove_duplicate_columns() {
    let raw = r#"{"select": [{"type": "column_ref", "column": "id"}, {"type": "column_ref", "column": "name"}, {"type": "column_ref", "column": "id"}], "from": {"table": "users"}}"#;
    let mut plan = build_plan(raw).unwrap();
    assert_eq!(plan.select.len(), 3, "before optimization: 3 items");
    assert!(optimize(&mut plan));
    assert_eq!(plan.select.len(), 2, "after optimization: 2 unique items");
}

// ── Full pipeline ─────────────────────────────────────────────────

#[test]
fn full_pipeline_with_optimize() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}, "filter": {"type": "and", "left": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}, "right": {"type": "comparison", "left": {"type": "literal", "value": true, "data_type": "boolean"}, "op": "eq", "right": {"type": "literal", "value": true, "data_type": "boolean"}}}}"#;
    let mut plan = build_plan(raw).unwrap();
    // After normalize: filter → where, AND TRUE should be simplified
    assert!(optimize(&mut plan));
    assert!(matches!(
        plan.r#where.unwrap(),
        vlorql_core::schema::Predicate::Comparison { .. }
    ));
}

#[test]
fn canonical_plan_no_optimization() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}, "where": {"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}}}"#;
    let mut plan = build_plan(raw).unwrap();
    assert!(
        !optimize(&mut plan),
        "canonical plan should not need optimization"
    );
}

// ── DeepSeek-style ────────────────────────────────────────────────

#[test]
fn deepseek_style_with_optimize() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "orders"}, "filter": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}}"#;
    let mut plan = build_plan(raw).unwrap();
    // After normalize: filter → where, no simplification needed
    assert!(!optimize(&mut plan));
    assert!(plan.r#where.is_some());
}
