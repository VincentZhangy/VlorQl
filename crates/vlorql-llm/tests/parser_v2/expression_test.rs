//! Integration tests for the P4 expression/operator normalizer.
//!
//! Tests the full expression/operator normalization pipeline on
//! realistic multi-model LLM output patterns.

use vlorql_llm::parser_v2::normalize::pipeline;

fn normalize_val(val: &mut serde_json::Value) -> bool {
    pipeline::normalize(val)
}

// ── Operator normalization ────────────────────────────────────────

#[test]
fn normalizes_operators_in_where() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "comparison",
            "left": {"column": "age"},
            "op": "=",
            "right": {"value": 18, "data_type": "int"}
        }
    });
    assert!(normalize_val(&mut val));
    assert_eq!(
        val.pointer("/where/op").and_then(|v| v.as_str()),
        Some("eq")
    );
}

#[test]
fn normalizes_multiple_operators() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "and",
            "left": {"type": "comparison", "left": {"column": "age"}, "op": ">=", "right": {"value": 18, "data_type": "int"}},
            "right": {"type": "comparison", "left": {"column": "status"}, "op": "!=", "right": {"value": "deleted", "data_type": "text"}}
        }
    });
    assert!(normalize_val(&mut val));
    assert_eq!(
        val.pointer("/where/left/op").and_then(|v| v.as_str()),
        Some("gte")
    );
    assert_eq!(
        val.pointer("/where/right/op").and_then(|v| v.as_str()),
        Some("ne")
    );
}

// ── Data type normalization ───────────────────────────────────────

#[test]
fn normalizes_data_types_in_literals() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "comparison",
            "left": {"column": "age"},
            "op": "gt",
            "right": {"type": "literal", "value": 18, "data_type": "integer"}
        }
    });
    assert!(normalize_val(&mut val));
    assert_eq!(
        val.pointer("/where/right/data_type")
            .and_then(|v| v.as_str()),
        Some("int")
    );
}

#[test]
fn normalizes_data_types_in_multiple_literals() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "and",
            "left": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "integer"}},
            "right": {"type": "comparison", "left": {"column": "name"}, "op": "eq", "right": {"type": "literal", "value": "Alice", "data_type": "varchar"}}
        }
    });
    assert!(normalize_val(&mut val));
    assert_eq!(
        val.pointer("/where/left/right/data_type")
            .and_then(|v| v.as_str()),
        Some("int")
    );
    assert_eq!(
        val.pointer("/where/right/right/data_type")
            .and_then(|v| v.as_str()),
        Some("string")
    );
}

// ── Expression type tag injection ─────────────────────────────────

#[test]
fn injects_missing_expression_types_in_where() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "comparison",
            "left": {"column": "age"},
            "op": "gt",
            "right": {"value": 18, "data_type": "int"}
        }
    });
    assert!(normalize_val(&mut val));
    assert_eq!(
        val.pointer("/where/left/type").and_then(|v| v.as_str()),
        Some("column_ref")
    );
    assert_eq!(
        val.pointer("/where/right/type").and_then(|v| v.as_str()),
        Some("literal")
    );
}

#[test]
fn injects_missing_predicate_type() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "left": {"column": "age"},
            "op": ">",
            "right": {"value": 18}
        }
    });
    assert!(normalize_val(&mut val));
    assert_eq!(
        val.pointer("/where/type").and_then(|v| v.as_str()),
        Some("comparison")
    );
}

// ── Predicate array unwrapping ────────────────────────────────────

#[test]
fn unwraps_array_wrapped_predicate() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "and",
            "left": [{"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}],
            "right": [{"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}]
        }
    });
    assert!(normalize_val(&mut val));
    let where_obj = val.get("where").unwrap().as_object().unwrap();
    assert!(
        where_obj.get("left").unwrap().is_object(),
        "left should be unwrapped from array"
    );
    assert!(
        where_obj.get("right").unwrap().is_object(),
        "right should be unwrapped from array"
    );
}

// ── Missing right field injection ─────────────────────────────────

#[test]
fn injects_missing_right_in_comparison() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "comparison",
            "left": {"column": "age"},
            "op": "gt"
        }
    });
    assert!(normalize_val(&mut val));
    assert!(
        val.pointer("/where/right").is_some(),
        "missing right should be injected"
    );
}

// ── Full multi-stage P4 pipeline ──────────────────────────────────

#[test]
fn full_p4_pipeline_expression_normalize() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "products"},
        "where": {
            "left": {"column": "price"},
            "op": ">=",
            "right": [{"value": 100, "data_type": "integer"}]
        }
    });
    assert!(normalize_val(&mut val));
    // Predicate type injected
    assert_eq!(
        val.pointer("/where/type").and_then(|v| v.as_str()),
        Some("comparison"),
        "missing predicate type should be injected"
    );
    // Operator normalized
    assert_eq!(
        val.pointer("/where/op").and_then(|v| v.as_str()),
        Some("gte"),
        ">= should become gte"
    );
    // Right unwrapped from array
    assert!(
        val.pointer("/where/right")
            .and_then(|v| v.as_object())
            .is_some(),
        "right should be unwrapped from array"
    );
    // Expression type injected
    assert_eq!(
        val.pointer("/where/left/type").and_then(|v| v.as_str()),
        Some("column_ref"),
        "left should have column_ref type"
    );
    assert_eq!(
        val.pointer("/where/right/type").and_then(|v| v.as_str()),
        Some("literal"),
        "right should have literal type"
    );
    // Data type normalized
    assert_eq!(
        val.pointer("/where/right/data_type")
            .and_then(|v| v.as_str()),
        Some("int"),
        "integer should become int"
    );
}

// ── DeepSeek-style output ─────────────────────────────────────────

#[test]
fn deepseek_style_with_operator_and_type_issues() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "orders"},
        "where": {
            "type": "comparison",
            "left": {"column": "status"},
            "op": "=",
            "right": {"value": "active", "data_type": "varchar"}
        }
    });
    assert!(normalize_val(&mut val));
    assert_eq!(
        val.pointer("/where/op").and_then(|v| v.as_str()),
        Some("eq")
    );
    assert_eq!(
        val.pointer("/where/right/data_type")
            .and_then(|v| v.as_str()),
        Some("string")
    );
}

// ── Qwen-style output (missing expression types) ──────────────────

#[test]
fn qwen_style_missing_expression_types() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "and",
            "left": [{"left": {"column": "age"}, "op": ">", "right": {"value": 18}}],
            "right": [{"left": {"column": "status"}, "op": "=", "right": {"value": "active"}}]
        }
    });
    assert!(normalize_val(&mut val));
    // Array sides unwrapped
    let where_obj = val.get("where").unwrap().as_object().unwrap();
    assert!(where_obj.get("left").unwrap().is_object());
    assert!(where_obj.get("right").unwrap().is_object());
    // Predicate types injected
    assert_eq!(
        val.pointer("/where/left/type").and_then(|v| v.as_str()),
        Some("comparison")
    );
    assert_eq!(
        val.pointer("/where/right/type").and_then(|v| v.as_str()),
        Some("comparison")
    );
    // Expression types injected
    assert_eq!(
        val.pointer("/where/left/left/type")
            .and_then(|v| v.as_str()),
        Some("column_ref")
    );
    assert_eq!(
        val.pointer("/where/left/right/type")
            .and_then(|v| v.as_str()),
        Some("literal")
    );
    // Operators normalized
    assert_eq!(
        val.pointer("/where/left/op").and_then(|v| v.as_str()),
        Some("gt")
    );
    assert_eq!(
        val.pointer("/where/right/op").and_then(|v| v.as_str()),
        Some("eq")
    );
}

// ── Idempotency ───────────────────────────────────────────────────

#[test]
fn idempotent_after_full_pipeline() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "comparison",
            "left": {"type": "column_ref", "column": "age"},
            "op": "gt",
            "right": {"type": "literal", "value": 18, "data_type": "int"}
        }
    });
    // First pass: no change (already canonical).
    assert!(!normalize_val(&mut val));
    // Second pass: still no change.
    assert!(!normalize_val(&mut val));
}
