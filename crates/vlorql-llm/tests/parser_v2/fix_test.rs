//! Integration tests for the `parser_v2::fix` module.
//!
//! Tests the auto-fix engine in the context of the full V2 pipeline
//! (recover → normalize → build → fix → validate).

use vlorql_llm::parser_v2::builder::query_builder;
use vlorql_llm::parser_v2::fix::fixer;
use vlorql_llm::parser_v2::normalize::pipeline;
use vlorql_llm::parser_v2::recover::extract_json_content;
use vlorql_llm::parser_v2::validate::validator;

fn build_and_fix(raw: &str) -> Result<vlorql_core::schema::QueryPlan, Box<dyn std::error::Error>> {
    let json_str = extract_json_content(raw);
    let mut value: serde_json::Value = serde_json::from_str(json_str)?;
    let _ = pipeline::normalize(&mut value);
    let mut plan = query_builder::build_plan(&value)?;
    let _ = fixer::fix_plan(&mut plan);
    Ok(plan)
}

// ── Fix: limit zero ──────────────────────────────────────────────

#[test]
fn fix_limit_zero_in_pipeline() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}, "limit": 0}"#;
    let plan = build_and_fix(raw).unwrap();
    assert_eq!(plan.limit, None, "limit 0 should be removed");
    // Should pass validation after fix.
    assert!(validator::validate_plan(&plan).is_ok());
}

// ── Fix: missing alias ───────────────────────────────────────────

#[test]
fn fix_missing_alias_in_pipeline() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}}"#;
    let plan = build_and_fix(raw).unwrap();
    assert_eq!(
        plan.from.alias,
        Some("t1".to_owned()),
        "missing alias should be generated"
    );
}

#[test]
fn fix_missing_alias_in_join() {
    let raw = r#"{
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "joins": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison", "left": {"type": "column_ref", "column": "user_id"}, "op": "eq", "right": {"type": "column_ref", "column": "id"}}}]
    }"#;
    let plan = build_and_fix(raw).unwrap();
    assert_eq!(plan.from.alias, Some("t1".to_owned()));
    assert_eq!(
        plan.joins.unwrap()[0].right_table.alias,
        Some("t2".to_owned())
    );
}

// ── Fix: empty select (should not happen in normal pipeline,
//     but the fix layer handles it) ────────────────────────────────

#[test]
fn fix_empty_select_in_pipeline() {
    // This is a synthetic case — the builder requires select.
    // We test the fix layer directly on a plan with empty select.
    let mut plan = vlorql_core::schema::QueryPlan {
        select: vec![],
        from: vlorql_core::schema::FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
    };
    assert!(fixer::fix_plan(&mut plan));
    assert_eq!(plan.select.len(), 1);
    assert!(validator::validate_plan(&plan).is_ok());
}

// ── Full pipeline: Normalize → Build → Fix → Validate ────────────

#[test]
fn full_pipeline_with_fix() {
    let raw = r#"{"projection": [{"type": "star"}], "source": "users", "filter": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18, "data_type": "integer"}}, "limit": 0}"#;
    let plan = build_and_fix(raw).unwrap();
    // Normalize: projection → select, source → from, filter → where
    assert_eq!(plan.select.len(), 1);
    assert_eq!(plan.from.table, "users");
    assert!(plan.r#where.is_some());
    // Fix: limit 0 removed
    assert_eq!(plan.limit, None, "limit 0 should be removed by fix");
    // Fix: missing alias added
    assert_eq!(plan.from.alias, Some("t1".to_owned()));
    // Validate: should pass
    assert!(
        validator::validate_plan(&plan).is_ok(),
        "fixed plan should be valid"
    );
}

#[test]
fn valid_plan_unchanged_by_fix() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users", "alias": "u"}}"#;
    let plan = build_and_fix(raw).unwrap();
    // Already has alias, should not be changed.
    assert_eq!(plan.from.alias, Some("u".to_owned()));
    assert!(validator::validate_plan(&plan).is_ok());
}

// ── DeepSeek-style with fix ───────────────────────────────────────

#[test]
fn deepseek_style_with_fix() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "orders"}, "filter": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}}"#;
    let plan = build_and_fix(raw).unwrap();
    // Normalize: filter → where
    assert!(plan.r#where.is_some());
    // Fix: missing alias added
    assert_eq!(plan.from.alias, Some("t1".to_owned()));
    // Validate: should pass
    assert!(validator::validate_plan(&plan).is_ok());
}
