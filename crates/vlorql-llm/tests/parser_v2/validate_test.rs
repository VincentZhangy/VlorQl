//! Integration tests for the `parser_v2::validate` module.
//!
//! Tests the validate module in the context of the full V2 pipeline
//! (recover → normalize → build → validate).

use vlorql_llm::parser_v2::builder::query_builder;
use vlorql_llm::parser_v2::normalize::pipeline;
use vlorql_llm::parser_v2::recover::extract_json_content;
use vlorql_llm::parser_v2::validate::validator;

fn build_plan(raw: &str) -> Result<vlorql_core::schema::QueryPlan, Box<dyn std::error::Error>> {
    let json_str = extract_json_content(raw);
    let mut value: serde_json::Value = serde_json::from_str(json_str)?;
    let _ = pipeline::normalize(&mut value);
    let plan = query_builder::build_plan(&value)?;
    Ok(plan)
}

// ── Valid plans ───────────────────────────────────────────────────

#[test]
fn valid_star_plan() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}}"#;
    let plan = build_plan(raw).unwrap();
    let result = validator::validate_plan(&plan);
    assert!(
        result.is_ok(),
        "valid star plan should pass: {:?}",
        result.err()
    );
}

#[test]
fn valid_full_plan() {
    let raw = r#"{
        "select": [{"type": "column_ref", "column": "id"}, {"type": "column_ref", "column": "name"}],
        "from": {"table": "users"},
        "where": {"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}},
        "order_by": [{"expr": {"type": "column_ref", "column": "name"}, "descending": true}],
        "limit": 10
    }"#;
    let plan = build_plan(raw).unwrap();
    let result = validator::validate_plan(&plan);
    assert!(
        result.is_ok(),
        "valid full plan should pass: {:?}",
        result.err()
    );
}

#[test]
fn valid_plan_with_joins() {
    let raw = r#"{
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "joins": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison", "left": {"type": "column_ref", "column": "user_id"}, "op": "eq", "right": {"type": "column_ref", "column": "id"}}}]
    }"#;
    let plan = build_plan(raw).unwrap();
    let result = validator::validate_plan(&plan);
    assert!(
        result.is_ok(),
        "valid plan with joins should pass: {:?}",
        result.err()
    );
}

#[test]
fn valid_plan_with_cross_join() {
    let raw = r#"{
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "joins": [{"join_type": "cross", "right_table": {"table": "orders"}}]
    }"#;
    let plan = build_plan(raw).unwrap();
    let result = validator::validate_plan(&plan);
    assert!(
        result.is_ok(),
        "cross join without on should pass: {:?}",
        result.err()
    );
}

#[test]
fn valid_plan_with_cte() {
    let raw = r#"{
        "select": [{"type": "star"}],
        "from": {"table": "active_users"},
        "ctes": [{"name": "active_users", "query": {"select": [{"type": "star"}], "from": {"table": "users"}, "where": {"type": "comparison", "left": {"type": "column_ref", "column": "status"}, "op": "eq", "right": {"type": "literal", "value": "active", "data_type": "string"}}}}]
    }"#;
    let plan = build_plan(raw).unwrap();
    let result = validator::validate_plan(&plan);
    assert!(
        result.is_ok(),
        "valid plan with CTE should pass: {:?}",
        result.err()
    );
}

// ── Invalid plans ─────────────────────────────────────────────────

#[test]
fn detects_limit_zero() {
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}, "limit": 0}"#;
    let plan = build_plan(raw).unwrap();
    let result = validator::validate_plan(&plan);
    assert!(result.is_err(), "limit 0 should fail");
}

#[test]
fn missing_from_fails_build() {
    // The builder requires `from` — this fails before validation.
    let raw = r#"{"select": [{"type": "star"}]}"#;
    let result = build_plan(raw);
    assert!(result.is_err(), "should fail on missing from");
}

#[test]
fn missing_join_condition_fails_build() {
    // The builder requires `on` for non-cross joins — this fails
    // before validation.  The validator's semantic test covers the
    // case where a dummy ON is injected (e.g. by auto-fix layer).
    let raw = r#"{
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "joins": [{"join_type": "inner", "right_table": {"table": "orders"}}]
    }"#;
    let result = build_plan(raw);
    // The normalize layer now infers a default ON condition from FK
    // conventions, so a missing `on` is no longer a build error.
    assert!(
        result.is_ok(),
        "inner join without on is now repaired by normalize layer: {:?}",
        result.err()
    );
}

// ── Full pipeline validation test ─────────────────────────────────

#[test]
fn messy_deepseek_plan_ends_up_valid() {
    // DeepSeek-style output with filter instead of where.
    let raw = r#"{"select": [{"type": "star"}], "from": {"table": "orders"}, "filter": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}}"#;
    let plan = build_plan(raw).unwrap();
    let result = validator::validate_plan(&plan);
    assert!(
        result.is_ok(),
        "DeepSeek-style plan should be valid after normalize: {:?}",
        result.err()
    );
}

#[test]
fn messy_qwen_plan_ends_up_valid() {
    // Qwen-style output with string projections and non-standard names.
    let raw = r#"{"projection": ["id", "name"], "source": "users", "filter": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}}"#;
    let plan = build_plan(raw).unwrap();
    let result = validator::validate_plan(&plan);
    assert!(
        result.is_ok(),
        "Qwen-style plan should be valid after normalize: {:?}",
        result.err()
    );
}

// ── Error handling ────────────────────────────────────────────────

#[test]
fn invalid_json_has_no_plan() {
    let raw = "this is not json at all";
    let result = build_plan(raw);
    assert!(result.is_err(), "should fail on invalid JSON");
}

#[test]
fn missing_from_fails_validation() {
    // The normalize pipeline does NOT inject a default `from`.
    let raw = r#"{"select": [{"type": "star"}]}"#;
    let result = build_plan(raw);
    assert!(result.is_err(), "should fail on missing from");
}
