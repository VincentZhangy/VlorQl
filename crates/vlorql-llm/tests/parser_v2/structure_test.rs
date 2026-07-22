//! Integration tests for the P3 structure normalizer.
//!
//! Tests the full structure normalization pipeline on realistic
//! multi-model LLM output patterns.

use vlorql_llm::parser_v2::normalize::pipeline;

fn normalize_val(val: &mut serde_json::Value) -> bool {
    pipeline::normalize(val)
}

// ── Realistic LLM output patterns ─────────────────────────────────

#[test]
fn openai_style_output() {
    // OpenAI typically outputs clean JSON that needs minimal normalization.
    let mut val = serde_json::json!({
        "select": [{"type": "column_ref", "table": "users", "column": "name"}],
        "from": {"table": "users"},
        "where": {"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}}
    });
    assert!(
        !normalize_val(&mut val),
        "OpenAI output should already be canonical"
    );
}

#[test]
fn deepseek_style_output() {
    // DeepSeek sometimes uses "filter" instead of "where".
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "orders"},
        "filter": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}
    });
    assert!(normalize_val(&mut val));
    assert!(val.get("where").is_some(), "filter → where");
    assert!(val.get("filter").is_none());
}

#[test]
fn qwen_style_output() {
    // Qwen sometimes wraps the plan in array-ish structures and
    // uses string projections.
    let mut val = serde_json::json!({
        "projection": ["id", "name", "email"],
        "source": "users",
        "filter": {"type": "comparison", "column": "status", "op": "eq", "right": {"value": "active"}}
    });
    assert!(normalize_val(&mut val));
    // Aliases
    assert!(val.get("select").is_some());
    assert!(val.get("from").is_some());
    assert!(val.get("where").is_some());
    // String projections → objects
    let select = val.get("select").unwrap().as_array().unwrap();
    for item in select {
        assert!(
            item.get("type").is_some(),
            "each select item should have type"
        );
        assert!(
            item.get("column").is_some(),
            "each select item should have column"
        );
    }
    // String source → object
    assert!(val.get("from").unwrap().is_object());
}

#[test]
fn llama_style_output() {
    // Llama 3.2 sometimes emits where as an array with garbage.
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "products"},
        "where": [
            {"type": "comparison", "left": {"column": "price"}, "op": "lt", "right": {"value": 100}},
            "extra text",
            42
        ],
        "order_by": [{"expr": {"column": "name"}, "descending": true}]
    });
    assert!(normalize_val(&mut val));
    // Where array → object
    let where_obj = val.get("where").unwrap();
    assert!(where_obj.is_object(), "where should be a single object");
    assert_eq!(
        where_obj.get("type").and_then(|v| v.as_str()),
        Some("comparison")
    );
}

#[test]
fn glm_style_output() {
    // GLM sometimes uses "conditions" for where and "fields" for select.
    let mut val = serde_json::json!({
        "fields": [{"type": "column_ref", "column": "id"}, {"type": "column_ref", "column": "name"}],
        "from": {"table": "users"},
        "conditions": [{"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}]
    });
    assert!(normalize_val(&mut val));
    assert!(val.get("select").is_some(), "fields → select");
    assert!(val.get("where").is_some(), "conditions → where");
    assert!(val.get("fields").is_none());
    assert!(val.get("conditions").is_none());
}

#[test]
fn misplaced_top_level_fields_in_where() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "where": {
            "type": "comparison",
            "left": {"column": "age"},
            "op": "gt",
            "right": {"value": 18},
            "order_by": [{"expr": {"column": "name"}, "descending": true}],
            "limit": 10,
            "offset": 5
        }
    });
    assert!(normalize_val(&mut val));
    // Fields should be extracted to top level.
    assert!(val.get("order_by").is_some());
    assert_eq!(val.get("limit").and_then(|v| v.as_u64()), Some(10));
    assert_eq!(val.get("offset").and_then(|v| v.as_u64()), Some(5));
    // Where should still be a valid predicate.
    let where_obj = val.get("where").unwrap().as_object().unwrap();
    assert!(where_obj.get("order_by").is_none());
    assert!(where_obj.get("limit").is_none());
    assert!(where_obj.get("offset").is_none());
    assert!(where_obj.get("type").is_some());
}

#[test]
fn string_table_name_in_from() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": "users"
    });
    assert!(normalize_val(&mut val));
    let from = val.get("from").unwrap();
    assert!(from.is_object(), "from should be an object");
    assert_eq!(from.get("table").and_then(|v| v.as_str()), Some("users"));
}

#[test]
fn string_join_right_table() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "joins": [{"join_type": "inner", "right_table": "orders", "on": {"type": "comparison"}}]
    });
    assert!(normalize_val(&mut val));
    let joins = val.get("joins").unwrap().as_array().unwrap();
    let rt = joins[0].get("right_table").unwrap();
    assert!(rt.is_object(), "right_table should be an object");
    assert_eq!(rt.get("table").and_then(|v| v.as_str()), Some("orders"));
}

#[test]
fn single_select_wrapped_to_array() {
    let mut val = serde_json::json!({
        "select": {"type": "star"},
        "from": {"table": "users"}
    });
    assert!(normalize_val(&mut val));
    assert!(val.get("select").unwrap().is_array());
}

#[test]
fn missing_select_gets_default() {
    let mut val = serde_json::json!({
        "from": {"table": "users"}
    });
    assert!(normalize_val(&mut val));
    assert!(
        val.get("select").is_some(),
        "default select should be injected"
    );
    let select = val.get("select").unwrap().as_array().unwrap();
    assert_eq!(select[0].get("type").and_then(|v| v.as_str()), Some("star"));
}

#[test]
fn unknown_top_level_fields_stripped() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "right": {"column": "id"},
        "left": {"column": "name"},
        "op": "eq",
        "child": {"type": "comparison"}
    });
    assert!(normalize_val(&mut val));
    assert!(val.get("right").is_none());
    assert!(val.get("left").is_none());
    assert!(val.get("op").is_none());
    assert!(val.get("child").is_none());
}

#[test]
fn plan_level_fields_in_joins() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "joins": [{
            "join_type": "inner",
            "right_table": {"table": "orders"},
            "on": {"type": "comparison"},
            "where": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}},
            "limit": 10
        }]
    });
    assert!(normalize_val(&mut val));
    // Plan-level fields should be extracted from joins.
    assert!(val.get("where").is_some());
    assert_eq!(val.get("limit").and_then(|v| v.as_u64()), Some(10));
    // Join should no longer have these fields.
    let joins = val.get("joins").unwrap().as_array().unwrap();
    let join_obj = joins[0].as_object().unwrap();
    assert!(join_obj.get("where").is_none());
    assert!(join_obj.get("limit").is_none());
}

#[test]
fn empty_group_by_removed() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "group_by": []
    });
    assert!(normalize_val(&mut val));
    assert!(
        val.get("group_by").is_none(),
        "empty group_by should be removed"
    );
}

#[test]
fn group_by_with_nulls() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "group_by": [{"column": "status"}, null, {"column": "type"}]
    });
    assert!(normalize_val(&mut val));
    let group_by = val.get("group_by").unwrap().as_array().unwrap();
    assert_eq!(group_by.len(), 2);
}

#[test]
fn top_level_expr_descending_to_order_by() {
    let mut val = serde_json::json!({
        "select": [{"type": "star"}],
        "from": {"table": "users"},
        "expr": {"column": "name"},
        "descending": true
    });
    assert!(normalize_val(&mut val));
    assert!(val.get("order_by").is_some());
    assert!(val.get("expr").is_none());
    assert!(val.get("descending").is_none());
}

#[test]
fn full_multi_stage_normalize() {
    // Simulate a messy LLM output that needs all stages.
    let mut val = serde_json::json!({
        "projection": ["id", "name", {"type": "star"}],
        "source": "users",
        "filter": {
            "type": "comparison",
            "column": "age",
            "operator": "gt",
            "right": {"value": 18},
            "order_by": [{"expr": {"column": "name"}, "descending": true}],
            "limit": 10
        },
        "right": {"column": "id"},
        "sort": [{"expr": {"column": "email"}, "descending": false}]
    });
    assert!(normalize_val(&mut val));
    // Alias: projection → select (array of objects)
    let select = val.get("select").unwrap().as_array().unwrap();
    assert_eq!(select.len(), 3);
    assert_eq!(
        select[0].get("type").and_then(|v| v.as_str()),
        Some("column_ref")
    );
    assert_eq!(select[0].get("column").and_then(|v| v.as_str()), Some("id"));
    assert_eq!(select[2].get("type").and_then(|v| v.as_str()), Some("star"));
    // Alias: source → from (object)
    assert_eq!(
        val.get("from")
            .and_then(|v| v.get("table"))
            .and_then(|v| v.as_str()),
        Some("users")
    );
    // Alias: filter → where (object)
    let where_obj = val.get("where").unwrap().as_object().unwrap();
    assert_eq!(
        where_obj.get("type").and_then(|v| v.as_str()),
        Some("comparison")
    );
    // Alias: operator → op
    assert_eq!(where_obj.get("op").and_then(|v| v.as_str()), Some("gt"));
    // Structure: order_by extracted from where to top level
    assert!(val.get("order_by").is_some());
    // Structure: limit extracted from where to top level
    assert_eq!(val.get("limit").and_then(|v| v.as_u64()), Some(10));
    // Structure: order_by from sort alias
    let order_by = val.get("order_by").unwrap().as_array().unwrap();
    assert_eq!(order_by.len(), 1, "sort alias provides 1 order_by term");
    // Structure: unknown top-level field stripped
    assert!(val.get("right").is_none());
    // No change on second pass (idempotency)
    assert!(!normalize_val(&mut val), "normalize should be idempotent");
}

#[test]
fn idempotent_normalize() {
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

#[test]
fn messy_llama_deepseek_mix() {
    // Mixed issues from Llama + DeepSeek patterns.
    let mut val = serde_json::json!({
        "projection": [{"column": "name"}, {"column": "email"}],
        "from": "employees",
        "filter": [
            {"type": "comparison", "kind": "comparison", "field": "salary", "operator": "gt", "right": {"value": 50000, "data_type": "int"}},
            null,
            "garbage"
        ],
        "sort": [{"expr": {"column": "name"}, "descending": true}]
    });
    assert!(normalize_val(&mut val));
    // All aliases applied.
    assert!(val.get("select").is_some(), "projection → select");
    assert!(val.get("from").is_some(), "from should exist");
    assert!(val.get("where").is_some(), "filter → where");
    assert!(val.get("order_by").is_some(), "sort → order_by");
    // Structure: select items have type injected.
    let select = val.get("select").unwrap().as_array().unwrap();
    assert_eq!(
        select[0].get("type").and_then(|v| v.as_str()),
        Some("column_ref")
    );
    assert_eq!(
        select[0].get("column").and_then(|v| v.as_str()),
        Some("name")
    );
    // Structure: from string → object.
    assert!(val.get("from").unwrap().is_object());
    assert_eq!(
        val.get("from")
            .and_then(|v| v.get("table"))
            .and_then(|v| v.as_str()),
        Some("employees")
    );
    // Structure: where array → single object.
    assert!(val.get("where").unwrap().is_object());
    let where_obj = val.get("where").unwrap().as_object().unwrap();
    assert_eq!(
        where_obj.get("type").and_then(|v| v.as_str()),
        Some("comparison")
    );
    // Nested aliases: field → column, operator → op
    // Note: `kind` → `type` is skipped because `type` already exists.
    assert_eq!(
        where_obj.get("column").and_then(|v| v.as_str()),
        Some("salary")
    );
    assert_eq!(where_obj.get("op").and_then(|v| v.as_str()), Some("gt"));
    assert!(where_obj.get("field").is_none());
    assert!(where_obj.get("operator").is_none());
    // Idempotent.
    assert!(!normalize_val(&mut val));
}
