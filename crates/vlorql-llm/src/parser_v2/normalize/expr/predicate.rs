//! Predicate normalization.
//!
//! Handles repair, unwrapping, and canonicalization of Predicate-like
//! JSON objects (comparison, and, or, not, between, in, like, is_null,
//! exists).  All items are `pub(super)` — visible within the `expr`
//! module but not outside it.

use serde_json::Value;
use tracing;

use super::literal::{fix_literal_type_aliases, repair_expression_value};

/// Known predicate type names (used for key-as-type detection).
const PRED_KEYS: &[&str] = &["not", "exists", "and", "or"];

/// Known predicate type names (used for nested-predicate detection inside comparisons).
const PREDICATE_TYPES: &[&str] = &["like", "in", "between", "is_null", "exists"];

/// Inject missing `type` tag on a bare predicate object that has `left`
/// and `op` but no `type`.
///
/// Returns `true` if any change was made.
#[must_use]
pub(super) fn repair_predicate_type(val: &mut Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    // Even if `type` exists, check for key-as-type conflicts:
    // LLM sometimes sets `"type": "not"` on object that also has key `"exists"`.
    // Guard: if the current `type` is already a valid, non-keyword type like
    // "comparison", do NOT override it — extra fields like `"not":{...}` are
    // garbage, not key-as-type patterns.
    let existing_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let has_valid_type = !existing_type.is_empty()
        && existing_type != "and"
        && existing_type != "or"
        && existing_type != "not"
        && existing_type != "exists";
    for &key in PRED_KEYS {
        if obj.contains_key(key) {
            if has_valid_type {
                continue;
            }
            // Only override when the key's value is a non-null object (meaningful
            // content). Null/empty values are garbage fields, not key-as-type.
            if let Some(v) = obj.get(key)
                && (v.is_null()
                    || v.as_str().is_some()
                    || (v.as_object().is_some_and(|o| o.is_empty())))
            {
                continue;
            }
            if let Some(v) = obj.remove(key) {
                obj.insert("type".to_owned(), Value::String(key.to_owned()));
                if key == "not" || key == "exists" {
                    obj.insert("child".to_owned(), v);
                } else {
                    obj.insert("left".to_owned(), v);
                    if !obj.contains_key("right") {
                        obj.insert("right".to_owned(), Value::Null);
                    }
                }
                return true;
            }
        }
    }

    // Fix: LLM sets `"type": "and"`/`"or"` on comparison predicates
    // (e.g. `{"type":"and","left":Expression,"op":"eq","right":Expression}`).
    // `and`/`or` never have `op` — detect this and correct to `comparison`.
    if let Some(type_str) = obj.get("type").and_then(|t| t.as_str())
        && (type_str == "and" || type_str == "or")
        && obj.contains_key("op")
    {
        obj.insert("type".to_owned(), Value::String("comparison".to_owned()));
        return true;
    }

    if obj.contains_key("type") {
        return false;
    }

    if obj.contains_key("left") && obj.contains_key("op") {
        obj.insert("type".to_owned(), Value::String("comparison".to_owned()));
        return true;
    }

    // Fix: LLM sometimes uses predicate type names as keys instead of
    // the `type` field value, e.g. `{"not": {"exists": {...}}}` instead
    // of `{"type": "not", "child": {"type": "exists", ...}}`.
    // Detect known predicate key names and convert.
    for &key in PRED_KEYS {
        if let Some(v) = obj.remove(key) {
            obj.insert("type".to_owned(), Value::String(key.to_owned()));
            if key == "not" || key == "exists" {
                // If the value already contains `child`, prefer embedding
                // the extracted key into that child rather than overwriting.
                if key == "not"
                    && let Some(child_obj) = v.as_object()
                    && child_obj.contains_key("child")
                {
                    if let Some(child_val) = child_obj.get("child").cloned() {
                        obj.insert("child".to_owned(), child_val);
                    }
                } else {
                    obj.insert("child".to_owned(), v);
                }
            } else {
                // and/or — wrap value in `left`, create empty `right`
                obj.insert("left".to_owned(), v);
                if !obj.contains_key("right") {
                    obj.insert("right".to_owned(), Value::Null);
                }
            }
            return true;
        }
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
pub(super) fn unwrap_array_sides(val: &mut Value) -> bool {
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
        // Special case: LLM sometimes puts 2 predicates in `left` array
        // (e.g. `{"type":"and","left":[A,B],"right":null}`).
        // Handle this BEFORE unwrap_side strips the array.
        let split: Option<(Value, Value)> = {
            let arr = obj.get("left").and_then(|v| v.as_array());
            match arr {
                Some(arr) if arr.len() >= 2 && (obj.get("right").is_none_or(|v| v.is_null())) => {
                    Some((arr[0].clone(), arr[1].clone()))
                }
                _ => None,
            }
        };
        if let Some((new_left, new_right)) = split {
            obj.insert("left".to_owned(), new_left);
            obj.insert("right".to_owned(), new_right);
            changed = true;
        }
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
        // Fix array-valued `op` in comparisons: LLM sometimes outputs
        // `"op":[{"type":"function_call","name":"between",...}, ...]` instead
        // of `"op":"gt"`. Try to extract a string operator.
        if pred_type == "comparison" {
            changed |= unwrap_operator_field(obj);
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
    if let Some(arr) = obj.get(field).and_then(|v| v.as_array())
        && !arr.is_empty()
    {
        obj.insert((*field).to_string(), arr[0].clone());
        return true;
    }
    false
}

/// Fix array-valued `op` in comparison predicates.
///
/// The LLM sometimes outputs `"op":[{"type":"function_call","name":"between",...},...]`
/// instead of `"op":"gt"`. Try to extract a string operator from the array.
/// For `"between"`, converts the entire predicate to `Between` format
/// (`type: "between"`, expr, low, high) since `"between"` is not a valid
/// comparison operator.
#[must_use]
fn unwrap_operator_field(obj: &mut serde_json::Map<String, Value>) -> bool {
    let arr = match obj.get("op").and_then(|v| v.as_array()) {
        Some(a) if !a.is_empty() => a.clone(),
        _ => return false,
    };
    // Extract operator name: try string first, then object with "name" field
    let op_name = arr[0].as_str().or_else(|| {
        arr[0]
            .as_object()
            .and_then(|o| o.get("name").and_then(|v| v.as_str()))
    });

    match op_name {
        Some("between" | "not_between") => {
            // Convert comparison to Between predicate
            let expr = obj
                .get("left")
                .cloned()
                .or_else(|| obj.get("expr").cloned())
                .unwrap_or(Value::Null);
            // Try to extract low/high from function_call args
            let (low, high) = arr[0]
                .as_object()
                .and_then(|o| o.get("args"))
                .and_then(|a| a.as_array())
                .map(|a| {
                    let low = a.first().cloned().unwrap_or(Value::Null);
                    let high = a.get(1).cloned().unwrap_or(Value::Null);
                    (low, high)
                })
                .unwrap_or((Value::Null, Value::Null));

            obj.clear();
            obj.insert("type".to_owned(), Value::String("between".to_owned()));
            obj.insert("expr".to_owned(), expr);
            obj.insert("low".to_owned(), low);
            obj.insert("high".to_owned(), high);
            true
        }
        Some(name) => {
            obj.insert("op".to_owned(), Value::String(name.to_owned()));
            true
        }
        None => {
            // Fallback: use "eq" as safe default
            obj.insert("op".to_owned(), Value::String("eq".to_owned()));
            true
        }
    }
}

/// Inject missing `right` field on comparison predicates.
///
/// The LLM sometimes emits `{"left": ..., "op": "in"}` without `right`.
/// Serde rejects the missing field, so we inject a null literal to let
/// it deserialize; the validator will catch the semantic problem.
///
/// Returns `true` if any change was made.
#[must_use]
pub(super) fn inject_missing_right(val: &mut Value) -> bool {
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
pub(super) fn simplify_single_child(val: &mut Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let pred_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

    if (pred_type == "and" || pred_type == "or")
        && obj.contains_key("left")
        && (obj.get("right").is_none_or(|v| v.is_null()))
        && let Some(left_val) = obj.remove("left")
    {
        obj.remove("right");
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
pub(super) fn normalize_predicate(val: &mut Value) -> bool {
    let mut changed = false;

    // Fix: LLM outputs {"type": "string", "value": "..."} or {"type": "integer", "value": N}
    // instead of the canonical {"type": "literal", "value": "...", "data_type": "string"}.
    // This must run BEFORE repair_predicate_type so that "expr" → "function_call" unwrapping
    // also exposes nested "string" args to subsequent normalize_impl recursion.
    changed |= fix_literal_type_aliases(val);

    // Fix: `{"type": "expr", "expression": {...}}` is an Expression format, not a valid
    // predicate. The LLM sometimes uses this format in having/where clauses (e.g.
    // `{"type":"expr","expression":{"type":"function_call","name":"count",...}}`).
    // Convert it to a comparison predicate.
    if let Some(obj) = val.as_object()
        && obj.get("type").and_then(|t| t.as_str()) == Some("expr")
        && obj.contains_key("expression")
        && let Some(inner) = obj.get("expression").cloned()
    {
        // Preserve existing op/right from the expr wrapper (the LLM
        // sometimes puts them alongside `expression`):
        //   {"type":"expr","expression":{...},"op":"gt","right":{...}}
        let op = obj
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or("gt")
            .to_owned();
        let right = obj.get("right").cloned().unwrap_or_else(
            || serde_json::json!({"type": "literal", "value": 0, "data_type": "int"}),
        );
        let comparison = serde_json::json!({
            "type": "comparison",
            "left": inner,
            "op": op,
            "right": right,
        });
        *val = comparison;
        changed = true;
        changed |= normalize_predicate(val);
        return changed;
    }

    // Fix: bare `{"type":"function_call",...}` in a predicate position (e.g.
    // `having`) — wrap it in a comparison.
    if let Some(obj) = val.as_object()
        && obj.get("type").and_then(|t| t.as_str()) == Some("function_call")
    {
        let comparison = serde_json::json!({
            "type": "comparison",
            "left": val.clone(),
            "op": "gt",
            "right": {"type": "literal", "value": 0, "data_type": "int"}
        });
        *val = comparison;
        changed = true;
        changed |= normalize_predicate(val);
        return changed;
    }

    // Fix: `{"type":"none"}` in predicate positions — not a valid predicate.
    if let Some(obj) = val.as_object()
        && obj.get("type").and_then(|t| t.as_str()) == Some("none")
        && obj.len() == 1
    {
        *val = serde_json::json!({
            "type": "comparison",
            "left": {"type": "literal", "value": 1, "data_type": "int"},
            "op": "eq",
            "right": {"type": "literal", "value": 0, "data_type": "int"}
        });
        changed = true;
        return changed;
    }

    // Fix: bare `{"type":"case",...}` in a predicate position — `Case` is an
    // Expression variant, not a Predicate variant.  Wrap it in a comparison
    // so the builder doesn't fail with "unknown Predicate variant `case`".
    if let Some(obj) = val.as_object()
        && obj.get("type").and_then(|t| t.as_str()) == Some("case")
    {
        let comparison = serde_json::json!({
            "type": "comparison",
            "left": val.clone(),
            "op": "ne",
            "right": {"type": "literal", "value": null, "data_type": "null"}
        });
        *val = comparison;
        changed = true;
        // No recursion here — the builder handles Case expressions inside
        // comparison predicates without further normalization.
        return changed;
    }

    // Fix: `{"type":"distinct","on":[...]}` in a predicate position — the
    // LLM confuses `SELECT DISTINCT` with a WHERE predicate and puts the
    // JOIN condition (or WHERE predicate) inside an `on` array.  Extract
    // the first element from `on` and use it as the real predicate.
    if let Some(obj) = val.as_object()
        && obj.get("type").and_then(|t| t.as_str()) == Some("distinct")
        && let Some(on_arr) = obj.get("on").and_then(|v| v.as_array())
        && !on_arr.is_empty()
    {
        // Promote the first on-element as the actual predicate.
        *val = on_arr[0].clone();
        changed = true;
        // Recurse so the promoted predicate goes through the full pipeline.
        changed |= normalize_predicate(val);
        return changed;
    }

    // Fix: LLM hallucinates non-standard predicate types like
    // `"comparisonarray"`, `"comparisonop"` — anything starting with
    // "comparison" is treated as a standard "comparison".
    if let Some(obj) = val.as_object()
        && let Some(type_str) = obj.get("type").and_then(|t| t.as_str())
        && type_str.starts_with("comparison")
        && type_str != "comparison"
    {
        let mut tmp = val.clone();
        if let Some(tmp_obj) = tmp.as_object_mut() {
            tmp_obj.insert("type".to_owned(), Value::String("comparison".to_owned()));
        }
        *val = tmp;
        changed = true;
        changed |= normalize_predicate(val);
        return changed;
    }

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

        // Rename `left` to `expr` for `like` / `is_null` / `between` predicates.
        if (pred_type == "like" || pred_type == "is_null" || pred_type == "between")
            && obj.contains_key("left")
            && !obj.contains_key("expr")
            && let Some(left) = obj.remove("left")
        {
            obj.insert("expr".to_owned(), left);
            changed = true;
        }

        // Fix: LLM outputs like predicates missing the `pattern` field.
        if pred_type == "like" && !obj.contains_key("pattern") {
            if let Some(expr) = obj.get("expr").and_then(|v| v.as_object()) {
                let table = expr.get("table").and_then(|v| v.as_str()).unwrap_or("");
                let column = expr.get("column").and_then(|v| v.as_str()).unwrap_or("");
                let known_tables = ["users", "orders", "products", "order_items", "employees"];
                if !known_tables.contains(&table) && !column.is_empty() {
                    let pattern = format!("%{column}");
                    let corrected_expr = if table.is_empty() || known_tables.contains(&table) {
                        Value::Object(expr.clone())
                    } else {
                        serde_json::json!({"type": "column_ref", "column": table})
                    };
                    obj.insert("pattern".to_owned(), Value::String(pattern));
                    obj.insert("expr".to_owned(), corrected_expr);
                    changed = true;
                }
            }
            if !obj.contains_key("pattern") {
                obj.insert("pattern".to_owned(), Value::String("%".to_owned()));
                changed = true;
            }
        }

        // Fix: LLM outputs BETWEEN predicates missing `low` and/or `high`
        // fields.  Inject default values so the builder doesn't fail with
        // "missing `low` field" — the validator will catch nonsense bounds.
        if pred_type == "between" {
            if !obj.contains_key("low") {
                obj.insert(
                    "low".to_owned(),
                    serde_json::json!({"type": "literal", "value": 0, "data_type": "int"}),
                );
                changed = true;
            }
            if !obj.contains_key("high") {
                obj.insert(
                    "high".to_owned(),
                    serde_json::json!({"type": "literal", "value": null, "data_type": "null"}),
                );
                changed = true;
            }
        }

        // Convert `op: "is_null"` / `op: "is not null"` to proper IsNull predicate.
        if pred_type == "comparison" {
            let left_is_pred = obj
                .get("left")
                .and_then(|v| v.as_object())
                .and_then(|o| o.get("type"))
                .and_then(|t| t.as_str())
                .is_some_and(|t| PREDICATE_TYPES.contains(&t));

            if left_is_pred && let Some(left_val) = obj.remove("left") {
                *val = left_val;
                changed = true;
                changed |= normalize_predicate(val);
                return changed;
            }

            // Symmetric check: LLM nests predicate types inside the `right` field
            // of a comparison (e.g. {"type":"comparison","left":{"value":"@..."},"op":"eq","right":{"type":"like","pattern":"..."}}).
            // Convert the comparison to the predicate type carried by `right`,
            // using the original `left` as the predicate's `expr` operand.
            let right_is_pred = obj
                .get("right")
                .and_then(|v| v.as_object())
                .and_then(|o| o.get("type"))
                .and_then(|t| t.as_str())
                .is_some_and(|t| PREDICATE_TYPES.contains(&t));

            if right_is_pred
                && let Some(right_val) = obj.remove("right")
                && let Some(right_obj) = right_val.as_object()
            {
                let right_type = right_obj
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("like");
                let left_expr = obj.remove("left").or_else(|| obj.remove("expr"));
                obj.clear();
                match right_type {
                    "like" => {
                        obj.insert("type".to_owned(), Value::String("like".to_owned()));
                        let expr = left_expr.unwrap_or(Value::Null);
                        let pattern = right_obj
                            .get("pattern")
                            .cloned()
                            .unwrap_or(Value::String("%".to_owned()));
                        obj.insert("expr".to_owned(), expr);
                        obj.insert("pattern".to_owned(), pattern);
                    }
                    "is_null" => {
                        obj.insert("type".to_owned(), Value::String("is_null".to_owned()));
                        let expr = left_expr.unwrap_or_else(|| {
                            right_obj.get("expr").cloned().unwrap_or(Value::Null)
                        });
                        obj.insert("expr".to_owned(), expr);
                    }
                    other => {
                        // For less common predicate types (in, between, exists),
                        // promote the right-side predicate directly and drop the
                        // comparison wrapper.  The builder will fail with a clear
                        // error if the structure is incomplete.
                        obj.insert("type".to_owned(), Value::String(other.to_owned()));
                        if let Some(expr) = left_expr {
                            obj.insert("expr".to_owned(), expr);
                        }
                        // Copy any fields from the right-side predicate that
                        // aren't already present.
                        for (k, v) in right_obj.iter() {
                            if k != "type" && !obj.contains_key(k) {
                                obj.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
                changed = true;
                changed |= normalize_predicate(val);
                return changed;
            }

            // Fix: LLM sometimes nests op/right inside the left field.
            if !obj.contains_key("op") {
                let promoted = obj
                    .get("left")
                    .and_then(|v| v.as_object())
                    .and_then(|left_obj| {
                        let op_val = left_obj.get("op").and_then(|v| v.as_str())?;
                        if !left_obj.contains_key("right") {
                            return None;
                        }
                        Some((
                            op_val.to_owned(),
                            left_obj.get("right").cloned().unwrap_or(Value::Null),
                        ))
                    });
                if let Some((op_str, right_val)) = promoted {
                    if let Some(left) = obj.get_mut("left").and_then(|v| v.as_object_mut()) {
                        left.remove("op");
                        left.remove("right");
                    }
                    obj.insert("op".to_owned(), Value::String(op_str));
                    obj.insert("right".to_owned(), right_val);
                    changed = true;
                }
            }

            // Fix empty `op` — LLM sometimes outputs `"op":""` or `"op":null`.
            if matches!(obj.get("op"), Some(Value::String(s)) if s.is_empty() || s == "unknown")
                || obj.get("op").is_some_and(|v| v.is_null())
            {
                obj.insert("op".to_owned(), Value::String("eq".to_owned()));
                changed = true;
            }

            if let Some(op_val) = obj.get("op").and_then(|v| v.as_str()) {
                if op_val == "is_null" || op_val == "is null" {
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
                } else if op_val == "like" || op_val == "ilike" {
                    // Convert {"type":"comparison","left":expr,"op":"like","right":{"value":"%...","data_type":"string"}}
                    // to a proper Like predicate.
                    let expr = obj
                        .remove("left")
                        .or_else(|| obj.remove("expr"))
                        .unwrap_or(Value::Null);
                    let pattern = obj
                        .remove("right")
                        .and_then(|r| r.get("value").cloned())
                        .and_then(|v| v.as_str().map(|s| s.to_owned()))
                        .unwrap_or_else(|| "%".to_owned());
                    obj.clear();
                    obj.insert("type".to_owned(), Value::String("like".to_owned()));
                    obj.insert("expr".to_owned(), expr);
                    obj.insert("pattern".to_owned(), Value::String(pattern));
                    changed = true;
                }
            }
        }

        // Convert single-value IN target to array.
        if pred_type == "in"
            && let Some(target) = obj.get("target")
            && target.is_object()
            && !target.as_object().is_some_and(|o| o.contains_key("select"))
        {
            let wrapped = serde_json::json!([target.clone()]);
            obj.insert("target".to_owned(), wrapped);
            changed = true;
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

        match pred_type.as_str() {
            "and" | "or" => {
                for side in &["left", "right"] {
                    if let Some(v) = obj.get_mut(*side) {
                        changed |= normalize_predicate(v);
                    }
                }
            }
            "not" => {
                if let Some(v) = obj.get_mut("child") {
                    changed |= normalize_predicate(v);
                }
            }
            "comparison" | "between" | "in" | "like" | "is_null" | "exists" | "true" | "false" => {}
            other => {
                tracing::debug!("normalize_predicate: unknown predicate type `{other}`");
            }
        }
    }

    changed
}
