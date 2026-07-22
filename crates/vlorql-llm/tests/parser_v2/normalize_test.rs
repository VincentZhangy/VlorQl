//! Integration tests for the `parser_v2::normalize` module.
//!
//! Tests the new normalize pipeline's alias resolution and field-name
//! normalization.  These tests verify the new behaviour directly rather
//! than comparing with the old `parse::canonicalize::repair_query_plan_json`,
//! because the old pipeline also does structural repairs (type injection,
//! missing-field filling) that are not part of P2.

use vlorql_llm::parser_v2::normalize::aliases;
use vlorql_llm::parser_v2::normalize::pipeline;

// ── Alias resolution ──────────────────────────────────────────────

#[test]
fn resolve_filter_to_where() {
    assert_eq!(aliases::resolve_alias("filter"), Some("where"));
}

#[test]
fn resolve_projection_to_select() {
    assert_eq!(aliases::resolve_alias("projection"), Some("select"));
}

#[test]
fn resolve_source_to_from() {
    assert_eq!(aliases::resolve_alias("source"), Some("from"));
}

#[test]
fn resolve_sort_to_order_by() {
    assert_eq!(aliases::resolve_alias("sort"), Some("order_by"));
}

#[test]
fn resolve_group_to_group_by() {
    assert_eq!(aliases::resolve_alias("group"), Some("group_by"));
}

#[test]
fn resolve_join_to_joins() {
    assert_eq!(aliases::resolve_alias("join"), Some("joins"));
}

#[test]
fn resolve_cte_to_ctes() {
    assert_eq!(aliases::resolve_alias("cte"), Some("ctes"));
}

#[test]
fn resolve_max_rows_to_limit() {
    assert_eq!(aliases::resolve_alias("max_rows"), Some("limit"));
}

#[test]
fn resolve_skip_to_offset() {
    assert_eq!(aliases::resolve_alias("skip"), Some("offset"));
}

#[test]
fn resolve_kind_to_type() {
    assert_eq!(aliases::resolve_alias("kind"), Some("type"));
}

#[test]
fn resolve_field_to_column() {
    assert_eq!(aliases::resolve_alias("field"), Some("column"));
}

#[test]
fn resolve_table_name_to_table() {
    assert_eq!(aliases::resolve_alias("table_name"), Some("table"));
}

#[test]
fn resolve_operator_to_op() {
    assert_eq!(aliases::resolve_alias("operator"), Some("op"));
}

#[test]
fn resolve_desc_to_descending() {
    assert_eq!(aliases::resolve_alias("desc"), Some("descending"));
}

#[test]
fn resolve_canonical_returns_none() {
    assert_eq!(aliases::resolve_alias("where"), None);
    assert_eq!(aliases::resolve_alias("select"), None);
    assert_eq!(aliases::resolve_alias("from"), None);
    assert_eq!(aliases::resolve_alias("order_by"), None);
}

#[test]
fn resolve_unknown_returns_none() {
    assert_eq!(aliases::resolve_alias("nonexistent"), None);
}

// ── Field name normalization ──────────────────────────────────────

#[test]
fn normalize_filter_to_where() {
    let mut val = serde_json::json!({"filter": {"type": "comparison"}});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("where").is_some(), "filter → where");
    assert!(val.get("filter").is_none(), "filter removed");
}

#[test]
fn normalize_projection_to_select() {
    let mut val = serde_json::json!({"projection": [{"type": "star"}], "from": {"table": "users"}});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("select").is_some());
    assert!(val.get("projection").is_none());
}

#[test]
fn normalize_sort_to_order_by() {
    let mut val = serde_json::json!({"sort": [{"expr": {"column": "name"}, "descending": true}], "select": [{"type": "star"}], "from": {"table": "users"}});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("order_by").is_some());
    assert!(val.get("sort").is_none());
}

#[test]
fn normalize_kind_to_type() {
    let mut val = serde_json::json!({"kind": "column_ref", "table": "users", "column": "id"});
    assert!(aliases::normalize_field_names(&mut val));
    assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("column_ref"));
    assert!(val.get("kind").is_none());
}

#[test]
fn normalize_field_to_column() {
    let mut val = serde_json::json!({"field": "name", "table": "users"});
    assert!(aliases::normalize_field_names(&mut val));
    assert_eq!(val.get("column").and_then(|v| v.as_str()), Some("name"));
    assert!(val.get("field").is_none());
}

#[test]
fn normalize_table_name_to_table() {
    let mut val = serde_json::json!({"table_name": "users"});
    assert!(aliases::normalize_field_names(&mut val));
    assert_eq!(val.get("table").and_then(|v| v.as_str()), Some("users"));
    assert!(val.get("table_name").is_none());
}

#[test]
fn normalize_operator_to_op() {
    let mut val = serde_json::json!({"operator": "eq"});
    assert!(aliases::normalize_field_names(&mut val));
    assert_eq!(val.get("op").and_then(|v| v.as_str()), Some("eq"));
    assert!(val.get("operator").is_none());
}

#[test]
fn normalize_desc_to_descending() {
    let mut val = serde_json::json!({"desc": true});
    assert!(aliases::normalize_field_names(&mut val));
    assert_eq!(val.get("descending").and_then(|v| v.as_bool()), Some(true));
    assert!(val.get("desc").is_none());
}

#[test]
fn normalize_source_to_from() {
    let mut val = serde_json::json!({"select": [{"type": "star"}], "source": {"table": "users"}});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("from").is_some(), "source → from");
    assert!(val.get("source").is_none());
}

#[test]
fn normalize_group_to_group_by() {
    let mut val = serde_json::json!({"select": [{"type": "star"}], "from": {"table": "users"}, "group": [{"column": "status"}]});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("group_by").is_some(), "group → group_by");
    assert!(val.get("group").is_none());
}

#[test]
fn normalize_join_to_joins() {
    let mut val = serde_json::json!({"select": [{"type": "star"}], "from": {"table": "users"}, "join": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison"}}]});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("joins").is_some(), "join → joins");
    assert!(val.get("join").is_none());
}

#[test]
fn normalize_cte_to_ctes() {
    let mut val = serde_json::json!({"select": [{"type": "star"}], "from": {"table": "users"}, "cte": [{"name": "active_users", "query": {"select": [{"type": "star"}], "from": {"table": "users"}}}]});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("ctes").is_some(), "cte → ctes");
    assert!(val.get("cte").is_none());
}

#[test]
fn normalize_max_rows_to_limit() {
    let mut val = serde_json::json!({"select": [{"type": "star"}], "from": {"table": "users"}, "max_rows": 10});
    assert!(aliases::normalize_field_names(&mut val));
    assert_eq!(val.get("limit").and_then(|v| v.as_u64()), Some(10));
    assert!(val.get("max_rows").is_none());
}

#[test]
fn normalize_skip_to_offset() {
    let mut val = serde_json::json!({"select": [{"type": "star"}], "from": {"table": "users"}, "skip": 20});
    assert!(aliases::normalize_field_names(&mut val));
    assert_eq!(val.get("offset").and_then(|v| v.as_u64()), Some(20));
    assert!(val.get("skip").is_none());
}

#[test]
fn normalize_conditions_to_where() {
    let mut val = serde_json::json!({"conditions": [{"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}]});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("where").is_some(), "conditions → where");
    assert!(val.get("conditions").is_none());
}

#[test]
fn normalize_predicate_to_where() {
    let mut val = serde_json::json!({"predicate": {"type": "comparison"}});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("where").is_some());
    assert!(val.get("predicate").is_none());
}

#[test]
fn normalize_columns_to_select() {
    let mut val = serde_json::json!({"columns": [{"type": "column_ref", "table": "users", "column": "id"}], "from": {"table": "users"}});
    assert!(aliases::normalize_field_names(&mut val));
    assert!(val.get("select").is_some());
    assert!(val.get("columns").is_none());
}

#[test]
fn normalize_nested_multi_alias() {
    let mut val = serde_json::json!({
        "select": [{"kind": "star"}],
        "from": {"table": "users"},
        "filter": {
            "kind": "comparison",
            "field": "age",
            "operator": "gt",
            "right": {"value": 18}
        }
    });
    assert!(aliases::normalize_field_names(&mut val));
    // Top-level: "filter" → "where"
    assert!(val.get("where").is_some());
    assert!(val.get("filter").is_none());
    // Inside where: "kind" → "type", "field" → "column", "operator" → "op"
    let where_obj = val.get("where").unwrap();
    assert_eq!(where_obj.get("type").and_then(|v| v.as_str()), Some("comparison"));
    assert_eq!(where_obj.get("column").and_then(|v| v.as_str()), Some("age"));
    assert_eq!(where_obj.get("op").and_then(|v| v.as_str()), Some("gt"));
    assert!(where_obj.get("kind").is_none());
    assert!(where_obj.get("field").is_none());
    assert!(where_obj.get("operator").is_none());
    // Inside select: "kind" → "type"
    let select = val.get("select").unwrap().as_array().unwrap();
    assert_eq!(select[0].get("type").and_then(|v| v.as_str()), Some("star"));
    assert!(select[0].get("kind").is_none());
}

#[test]
fn normalize_already_canonical_no_change() {
    let mut val = serde_json::json!({"select": [{"type": "star"}], "from": {"table": "users"}});
    assert!(!aliases::normalize_field_names(&mut val));
}

#[test]
fn normalize_does_not_overwrite_existing() {
    let mut val = serde_json::json!({"where": {"type": "comparison"}, "filter": {"type": "and"}});
    // "filter" → "where" skipped because "where" already exists
    assert!(!aliases::normalize_field_names(&mut val));
    assert!(val.get("filter").is_some(), "filter stays when where already exists");
    assert_eq!(
        val.get("where").and_then(|v| v.get("type")).and_then(|v| v.as_str()),
        Some("comparison")
    );
}

// ── Pipeline tests ────────────────────────────────────────────────

#[test]
fn pipeline_normalize_applies_aliases() {
    let mut val = serde_json::json!({
        "filter": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}},
        "projection": [{"type": "star"}],
        "source": {"table": "users"},
    });
    assert!(pipeline::normalize(&mut val));
    assert!(val.get("where").is_some());
    assert!(val.get("select").is_some());
    assert!(val.get("from").is_some());
}

#[test]
fn pipeline_normalize_str_owned() {
    let input = r#"{"filter": {"type": "comparison"}}"#;
    let result = pipeline::normalize_str(input);
    assert!(result.contains(r#""where""#));
    assert!(!result.contains(r#""filter""#));
}

#[test]
fn pipeline_normalize_str_borrowed() {
    let input = r#"{"where": {"type": "comparison"}}"#;
    let result = pipeline::normalize_str(input);
    // Should be borrowed (no change).
    if let std::borrow::Cow::Borrowed(s) = &result {
        assert_eq!(*s, input);
    } else {
        panic!("expected borrowed for unchanged input");
    }
}

#[test]
fn pipeline_normalize_str_invalid_json() {
    let input = "not json at all";
    let result = pipeline::normalize_str(input);
    assert_eq!(result.as_ref(), input);
}

#[test]
fn global_aliases_includes_new_entries() {
    let aliases = aliases::global_aliases();
    assert!(aliases.contains(&("source", "from")));
    assert!(aliases.contains(&("group", "group_by")));
    assert!(aliases.contains(&("join", "joins")));
    assert!(aliases.contains(&("cte", "ctes")));
    assert!(aliases.contains(&("max_rows", "limit")));
    assert!(aliases.contains(&("skip", "offset")));
    assert!(aliases.contains(&("filter", "where")));
    assert!(aliases.contains(&("projection", "select")));
    assert!(aliases.contains(&("sort", "order_by")));
    assert!(aliases.contains(&("kind", "type")));
    assert!(aliases.contains(&("field", "column")));
    assert!(aliases.contains(&("operator", "op")));
    assert!(aliases.contains(&("desc", "descending")));
}