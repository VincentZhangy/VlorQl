//! Expression normalization.
//!
//! Normalizes expression structures and predicate shapes:
//!
//! - Injects missing `type` discriminator for Expression-like objects
//!   (ColumnRef, Literal, FunctionCall)
//! - Injects missing `type` discriminator for Predicate-like objects
//!   (Comparison)
//! - Fixes array-valued `left`/`right`/`child` in predicates
//! - Injects missing `right` field on comparison predicates
//! - Simplifies single-child `and`/`or` predicates

use serde_json::Value;

/// Adds missing `"type"` tags to Expression-like JSON objects.
///
/// The LLM frequently omits the `type` discriminator from `ColumnRef`,
/// `Literal`, and `FunctionCall` objects.  This function infers the
/// correct tag from the present fields so that serde can deserialize
/// the value as an [`Expression`](vlorql_core::schema::Expression).
///
/// Returns `true` if any change was made.
#[must_use]
pub fn repair_expression_value(val: &mut Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    if obj.contains_key("type") {
        return false;
    }

    // ColumnRef: has `column` (and optionally `table`)
    if obj.contains_key("column") {
        obj.insert("type".to_owned(), Value::String("column_ref".to_owned()));
        return true;
    }

    // Literal: has `value`
    if obj.contains_key("value") {
        obj.insert("type".to_owned(), Value::String("literal".to_owned()));
        return true;
    }

    // FunctionCall: has `name` and `args`
    if obj.contains_key("name") && obj.contains_key("args") {
        obj.insert("type".to_owned(), Value::String("function_call".to_owned()));
        return true;
    }

    false
}

/// Inject missing `type` tag on a bare predicate object that has `left`
/// and `op` but no `type`.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn repair_predicate_type(val: &mut Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    if obj.contains_key("type") {
        return false;
    }

    if obj.contains_key("left") && obj.contains_key("op") {
        obj.insert("type".to_owned(), Value::String("comparison".to_owned()));
        return true;
    }

    false
}

/// Fix array-valued `left`/`right`/`child` fields in predicates.
///
/// The LLM sometimes emits `"left": [{...}]` (array wrapping a single
/// predicate) instead of `"left": {...}`.  This unwraps the first
/// element.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn unwrap_array_sides(val: &mut Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let pred_type = obj
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_owned();

    let mut changed = false;

    // Fix array-valued sides in and/or
    if pred_type == "and" || pred_type == "or" {
        for side in &["left", "right"] {
            changed |= unwrap_side(obj, side);
        }
    }

    // Fix array-valued `child` in `not`
    if pred_type == "not" {
        changed |= unwrap_side(obj, "child");
    }

    // Fix array-valued expression fields in comparison/between/in/like/is_null
    if pred_type == "comparison"
        || pred_type == "between"
        || pred_type == "in"
        || pred_type == "like"
        || pred_type == "is_null"
    {
        for field in &["left", "right", "expr", "low", "high"] {
            changed |= unwrap_array_field(obj, field);
        }
    }

    changed
}

/// Unwrap a predicate side from array to single value.
fn unwrap_side(obj: &mut serde_json::Map<String, Value>, side: &str) -> bool {
    if let Some(arr) = obj.get(side).and_then(|v| v.as_array()) {
        if arr.is_empty() {
            obj.remove(side);
            true
        } else {
            obj.insert(side.to_string(), arr[0].clone());
            true
        }
    } else {
        false
    }
}

/// Unwrap an expression field from array to single value.
fn unwrap_array_field(obj: &mut serde_json::Map<String, Value>, field: &str) -> bool {
    if let Some(arr) = obj.get(field).and_then(|v| v.as_array()) {
        if !arr.is_empty() {
            obj.insert((*field).to_string(), arr[0].clone());
            return true;
        }
    }
    false
}

/// Inject missing `right` field on comparison predicates.
///
/// The LLM sometimes emits `{"left": ..., "op": "in"}` without `right`.
/// Serde rejects the missing field, so we inject a null literal to let
/// it deserialize; the validator will catch the semantic problem.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn inject_missing_right(val: &mut Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let pred_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

    if pred_type == "comparison"
        && !obj.contains_key("right")
        && obj.contains_key("left")
        && obj.contains_key("op")
    {
        obj.insert(
            "right".to_owned(),
            serde_json::json!({"type": "literal", "value": null, "data_type": "null"}),
        );
        return true;
    }

    false
}

/// Simplify single-child `and`/`or`: if only `left` exists and no
/// `right`, replace the entire predicate with `left`.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn simplify_single_child(val: &mut Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let pred_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

    if (pred_type == "and" || pred_type == "or")
        && obj.contains_key("left")
        && !obj.contains_key("right")
        && let Some(left_val) = obj.remove("left")
    {
        *val = left_val;
        return true;
    }

    false
}

/// Full expression normalization for a predicate tree.
///
/// 1. Inject missing predicate type tag
/// 2. Unwrap array sides
/// 3. Repair expression type tags on left/right/expr
/// 4. Inject missing right field
/// 5. Simplify single-child and/or
#[must_use]
pub fn normalize_predicate(val: &mut Value) -> bool {
    let mut changed = false;

    // Inject missing type tag.
    changed |= repair_predicate_type(val);

    // Unwrap array sides.
    changed |= unwrap_array_sides(val);

    // Repair expression type tags on known fields.
    if let Some(obj) = val.as_object_mut() {
        let pred_type = obj
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();

        if pred_type == "comparison"
            || pred_type == "between"
            || pred_type == "in"
            || pred_type == "like"
            || pred_type == "is_null"
        {
            for field in &["left", "right", "expr", "low", "high"] {
                if let Some(v) = obj.get_mut(*field) {
                    changed |= repair_expression_value(v);
                }
            }
        }

        // Convert `op: "is_null"` / `op: "is not null"` to proper IsNull predicate.
        // The LLM sometimes uses {"type":"comparison","op":"is_null","left":...,"right":null}
        // instead of {"type":"is_null","expr":...}.
        if pred_type == "comparison" {
            if let Some(op_val) = obj.get("op").and_then(|v| v.as_str()) {
                if op_val == "is_null" || op_val == "is null" {
                    // Extract the expression from `left` or `expr`.
                    let expr = obj
                        .remove("left")
                        .or_else(|| obj.remove("expr"))
                        .unwrap_or(Value::Null);
                    obj.clear();
                    obj.insert("type".to_owned(), Value::String("is_null".to_owned()));
                    obj.insert("expr".to_owned(), expr);
                    changed = true;
                } else if op_val == "is_not_null"
                    || op_val == "is not null"
                    || op_val == "isnotnull"
                {
                    // Convert to NOT(IsNull).
                    let expr = obj
                        .remove("left")
                        .or_else(|| obj.remove("expr"))
                        .unwrap_or(Value::Null);
                    obj.clear();
                    obj.insert("type".to_owned(), Value::String("not".to_owned()));
                    obj.insert(
                        "child".to_owned(),
                        serde_json::json!({
                            "type": "is_null",
                            "expr": expr
                        }),
                    );
                    changed = true;
                }
            }
        }

        // Convert single-value IN target to array.
        // {"type":"in","expr":...,"target":{"value":"active"}} → {"type":"in","expr":...,"target":[{"value":"active"}]}
        if pred_type == "in" {
            if let Some(target) = obj.get("target") {
                if target.is_object()
                    && !target
                        .as_object()
                        .map_or(false, |o| o.contains_key("select"))
                {
                    // Single value object — wrap in array.
                    let wrapped = serde_json::json!([target.clone()]);
                    obj.insert("target".to_owned(), wrapped);
                    changed = true;
                }
            }
        }

        // Inject missing right.
        changed |= inject_missing_right(val);

        // Simplify single-child and/or.
        changed |= simplify_single_child(val);
    }

    // Recurse into sub-predicates.
    if let Some(obj) = val.as_object_mut() {
        let pred_type = obj
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();

        if pred_type == "and" || pred_type == "or" {
            for side in &["left", "right"] {
                if let Some(v) = obj.get_mut(*side) {
                    changed |= normalize_predicate(v);
                }
            }
        }

        if pred_type == "not" {
            if let Some(v) = obj.get_mut("child") {
                changed |= normalize_predicate(v);
            }
        }
    }

    changed
}

/// Full expression normalization for a value tree.
///
/// Recursively normalizes all predicates and expressions.
#[must_use]
pub fn normalize(val: &mut Value) -> bool {
    normalize_impl(val)
}

fn normalize_impl(val: &mut Value) -> bool {
    let mut changed = false;
    match val {
        Value::Object(map) => {
            // Check if this is a predicate-like object (has `type` or
            // looks like a comparison with `left` + `op`).
            let pred_type = map.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let is_predicate_like =
                !pred_type.is_empty() || (map.contains_key("left") && map.contains_key("op"));

            if is_predicate_like {
                // This is a predicate-like object — run full predicate normalization.
                let mut tmp = Value::Object(std::mem::take(map));
                changed |= normalize_predicate(&mut tmp);
                if let Value::Object(m) = tmp {
                    *map = m;
                }
            }

            // Fix: LLMs sometimes emit aggregate function names as `type`
            // (e.g. `{"type": "sum", "args": [...]}`) instead of the
            // canonical `{"type": "function_call", "name": "sum", ...}`.
            // Convert them here before the builder rejects them.
            let type_str = map
                .get("type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            if !type_str.is_empty()
                && !matches!(
                    type_str.as_str(),
                    "function_call"
                        | "column_ref"
                        | "literal"
                        | "binary_op"
                        | "star"
                        | "subquery"
                        | "comparison"
                        | "and"
                        | "or"
                        | "not"
                        | "between"
                        | "in"
                        | "like"
                        | "is_null"
                        | "exists"
                )
                && vlorql_core::function::is_known_function(&type_str)
            {
                if let Some(args) = map.remove("args") {
                    map.insert("type".to_owned(), Value::String("function_call".to_owned()));
                    map.insert("name".to_owned(), Value::String(type_str));
                    map.insert("args".to_owned(), args);
                    changed = true;
                }
            }

            // Recurse into children (some may be predicates or expressions).
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in &keys {
                if let Some(v) = map.get_mut(key) {
                    changed |= normalize_impl(v);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                changed |= normalize_impl(v);
            }
        }
        _ => {}
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── repair_expression_value ───────────────────────────────────

    #[test]
    fn injects_column_ref_type() {
        let mut val = json!({"column": "name", "table": "users"});
        assert!(repair_expression_value(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("column_ref"));
    }

    #[test]
    fn injects_literal_type() {
        let mut val = json!({"value": 42, "data_type": "int"});
        assert!(repair_expression_value(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("literal"));
    }

    #[test]
    fn injects_function_call_type() {
        let mut val = json!({"name": "count", "args": [{"type": "star"}]});
        assert!(repair_expression_value(&mut val));
        assert_eq!(
            val.get("type").and_then(|v| v.as_str()),
            Some("function_call")
        );
    }

    #[test]
    fn expression_already_has_type() {
        let mut val = json!({"type": "column_ref", "column": "name"});
        assert!(!repair_expression_value(&mut val));
    }

    #[test]
    fn expression_no_recognizable_fields() {
        let mut val = json!({"unknown": "field"});
        assert!(!repair_expression_value(&mut val));
    }

    // ── repair_predicate_type ─────────────────────────────────────

    #[test]
    fn injects_comparison_type() {
        let mut val = json!({"left": {"column": "age"}, "op": "gt", "right": {"value": 18}});
        assert!(repair_predicate_type(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("comparison"));
    }

    #[test]
    fn predicate_already_has_type() {
        let mut val = json!({"type": "comparison", "left": {"column": "age"}, "op": "gt"});
        assert!(!repair_predicate_type(&mut val));
    }

    // ── unwrap_array_sides ────────────────────────────────────────

    #[test]
    fn unwraps_and_left_array() {
        let mut val = json!({"type": "and", "left": [{"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}], "right": {"type": "comparison", "left": {"column": "b"}, "op": "gt", "right": {"value": 2}}});
        assert!(unwrap_array_sides(&mut val));
        assert!(val.get("left").unwrap().is_object());
    }

    #[test]
    fn unwraps_not_child_array() {
        let mut val = json!({"type": "not", "child": [{"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}]});
        assert!(unwrap_array_sides(&mut val));
        assert!(val.get("child").unwrap().is_object());
    }

    #[test]
    fn unwraps_comparison_left_array() {
        let mut val = json!({"type": "comparison", "left": [{"column": "age"}], "op": "gt", "right": {"value": 18}});
        assert!(unwrap_array_sides(&mut val));
        assert!(val.get("left").unwrap().is_object());
    }

    // ── inject_missing_right ──────────────────────────────────────

    #[test]
    fn injects_missing_right_on_comparison() {
        let mut val = json!({"type": "comparison", "left": {"column": "age"}, "op": "gt"});
        assert!(inject_missing_right(&mut val));
        assert!(val.get("right").is_some());
        assert_eq!(
            val.pointer("/right/type").and_then(|v| v.as_str()),
            Some("literal")
        );
    }

    #[test]
    fn does_not_inject_when_right_exists() {
        let mut val = json!({"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}});
        assert!(!inject_missing_right(&mut val));
    }

    // ── simplify_single_child ─────────────────────────────────────

    #[test]
    fn simplifies_and_without_right() {
        let mut val = json!({"type": "and", "left": {"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}});
        assert!(simplify_single_child(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("comparison"));
    }

    #[test]
    fn does_not_simplify_and_with_both_sides() {
        let mut val =
            json!({"type": "and", "left": {"type": "comparison"}, "right": {"type": "comparison"}});
        assert!(!simplify_single_child(&mut val));
    }

    // ── normalize_predicate ───────────────────────────────────────

    #[test]
    fn full_predicate_normalize() {
        let mut val = json!({
            "left": {"column": "a"},
            "op": "=",
            "right": [{"value": 1}]
        });
        assert!(normalize_predicate(&mut val));
        // Injected type
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("comparison"));
        // Unwrapped right from array
        assert!(val.get("right").unwrap().is_object());
        // Injected expression type on right
        assert_eq!(
            val.pointer("/right/type").and_then(|v| v.as_str()),
            Some("literal")
        );
        // Injected expression type on left
        assert_eq!(
            val.pointer("/left/type").and_then(|v| v.as_str()),
            Some("column_ref")
        );
    }

    #[test]
    fn recursive_predicate_normalize() {
        let mut val = json!({
            "type": "and",
            "left": [{"left": {"column": "a"}, "op": "=", "right": {"value": 1}}],
            "right": [{"left": {"column": "b"}, "op": ">", "right": {"value": 2}}]
        });
        assert!(normalize_predicate(&mut val));
        // Unwrapped array sides
        assert!(val.get("left").unwrap().is_object());
        assert!(val.get("right").unwrap().is_object());
        // Injected types in sub-predicates
        assert_eq!(
            val.pointer("/left/type").and_then(|v| v.as_str()),
            Some("comparison")
        );
        assert_eq!(
            val.pointer("/right/type").and_then(|v| v.as_str()),
            Some("comparison")
        );
    }

    // ── normalize (top-level) ─────────────────────────────────────

    #[test]
    fn full_normalize_tree() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {
                "type": "and",
                "left": [{"left": {"column": "age"}, "op": ">", "right": {"value": 18, "data_type": "integer"}}],
                "right": [{"left": {"column": "status"}, "op": "=", "right": {"value": "active"}}]
            }
        });
        assert!(normalize(&mut val));
        // Where should be unwrapped
        let where_obj = val.get("where").unwrap().as_object().unwrap();
        assert!(where_obj.get("left").unwrap().is_object());
        assert!(where_obj.get("right").unwrap().is_object());
        // Sub-predicates should have types
        assert_eq!(
            val.pointer("/where/left/type").and_then(|v| v.as_str()),
            Some("comparison")
        );
        assert_eq!(
            val.pointer("/where/right/type").and_then(|v| v.as_str()),
            Some("comparison")
        );
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {
                "type": "comparison",
                "left": {"type": "column_ref", "column": "age"},
                "op": "gt",
                "right": {"type": "literal", "value": 18, "data_type": "int"}
            }
        });
        assert!(!normalize(&mut val));
    }
}
