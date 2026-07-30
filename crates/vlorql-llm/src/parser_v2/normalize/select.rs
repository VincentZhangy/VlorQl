//! SELECT clause structure normalization.
//!
//! Ensures:
//! - `select` is always an array of projection objects
//! - Each projection has a valid `type` tag
//! - Invalid projections are removed
//! - A default `select` is injected when missing but `from` exists
//! - String `"*"` is converted to `{"type": "star"}`

/// Fields that are valid projection types in QueryPlan.
const VALID_PROJECTION_TYPES: &[&str] = &["column_ref", "expr", "star"];

/// Expression-like types that the LLM may emit directly in `select`
/// instead of wrapping them in an `expr` projection.
/// When detected, these are auto-converted to `{"type": "expr", "expression": {...}}`.
const EXPRESSION_LIKE_TYPES: &[&str] = &[
    "function_call",
    "FunctionCall",
    "binary_op",
    "BinaryOp",
    "literal",
    "Literal",
    "subquery",
    "SubQuery",
];

/// Inject a basic `[{"type": "star"}]` select when `select` is missing
/// but `from` exists.
///
/// Some small LLMs (e.g. Qwen2.5) omit the `select` field in
/// subqueries.
#[must_use]
pub fn inject_default_select(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    if !obj.contains_key("select") && obj.contains_key("from") {
        obj.insert("select".to_owned(), serde_json::json!([{"type": "star"}]));
        return true;
    }
    false
}

/// Inject missing `type` tags for items that look like ColumnRef
/// (have `column` and optionally `table`, but no `type`).
///
/// Returns `true` if any item was modified.
#[must_use]
pub fn inject_missing_type(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(arr) = obj.get_mut("select").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for item in arr.iter_mut() {
        if let Some(item_obj) = item.as_object_mut()
            && !item_obj.contains_key("type")
            && item_obj.contains_key("column")
        {
            item_obj.insert(
                "type".to_owned(),
                serde_json::Value::String("column_ref".to_owned()),
            );
            changed = true;
        }
    }
    changed
}

/// Remove items from `select` that have invalid or missing `type` tags.
///
/// Returns `true` if any item was removed or converted.
#[must_use]
pub fn remove_invalid(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(arr) = obj.get_mut("select").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;

    // First pass: convert expression-like items (function_call, binary_op, etc.)
    // to `{"type": "expr", "expression": {...}}` wrapper.
    for item in arr.iter_mut() {
        if let Some(item_obj) = item.as_object_mut()
            && let Some(type_str) = item_obj.get("type").and_then(|t| t.as_str())
            && EXPRESSION_LIKE_TYPES.contains(&type_str)
        {
            // Collect alias before moving expression.
            let alias = item_obj.remove("alias").filter(|v| !v.is_null());
            // Wrap the entire item as expression.
            let expression = serde_json::Value::Object(std::mem::take(item_obj));
            let mut wrapper = serde_json::Map::new();
            wrapper.insert(
                "type".to_owned(),
                serde_json::Value::String("expr".to_owned()),
            );
            wrapper.insert("expression".to_owned(), expression);
            if let Some(alias_val) = alias {
                wrapper.insert("alias".to_owned(), alias_val);
            }
            *item = serde_json::Value::Object(wrapper);
            changed = true;
        }
    }

    // Second pass: remove items that still have invalid types.
    let before = arr.len();
    arr.retain(|v| {
        v.as_object()
            .and_then(|o| o.get("type"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| VALID_PROJECTION_TYPES.contains(&t))
    });
    if arr.len() != before {
        if arr.is_empty() {
            obj.remove("select");
        }
        changed = true;
    }
    changed
}

/// Normalize a single projection item (string → object).
///
/// If the projection is a plain string like `"id"`, convert it to
/// `{"type": "column_ref", "column": "id"}`.
/// If the string is `"*"`, convert it to `{"type": "star"}`.
/// Strings containing JSON corruption artifacts (quotes, colons, etc.)
/// are converted to `null` so the caller can remove them.
#[must_use]
pub fn normalize_projection_item(item: &mut serde_json::Value) -> bool {
    if let Some(s) = item.as_str() {
        // Detect JSON corruption artifacts: strings from LLM output that
        // contain quotes, colons, or `alias:` patterns are clearly not
        // valid column names.  Set to null so the caller drops them.
        if s.contains('\'') || s.contains('\"') || s.contains(':') || s.contains("alias") {
            *item = serde_json::Value::Null;
            return true;
        }
        if s == "*" {
            *item = serde_json::json!({"type": "star"});
        } else {
            *item = serde_json::json!({
                "type": "column_ref",
                "column": s
            });
        }
        return true;
    }
    false
}

/// Normalize all projection items.
#[must_use]
pub fn normalize_projection_items(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(arr) = obj.get_mut("select").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for item in arr.iter_mut() {
        changed |= normalize_projection_item(item);
    }
    changed
}

/// Normalize string items in the `group_by` array.
///
/// Converts `["status"]` to `[{"type": "column_ref", "column": "status"}]`.
#[must_use]
pub fn normalize_group_by_strings(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(arr) = obj.get_mut("group_by").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for item in arr.iter_mut() {
        if let Some(s) = item.as_str() {
            *item = serde_json::json!({"type": "column_ref", "column": s});
            changed = true;
        }
    }
    changed
}

/// Full SELECT structure normalization.
///
/// 1. Normalize string projection items to objects
/// 2. Inject missing `type` tags
/// 3. Remove invalid items
/// 4. Inject default select when missing
/// 5. Normalize group_by strings
/// 6. Remove Star projections when GROUP BY is present
/// 7. Remove aggregate function calls from GROUP BY
#[must_use]
/// 8. Unwrap `items` wrapper from select.
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let mut changed = false;

    // 0. Unwrap `{"select": {"items": [...]}}` — LLM sometimes wraps
    //     the select list in an `items` field.
    changed |= unwrap_select_items(val);

    // 1. Normalize string projection items.
    changed |= normalize_projection_items(val);

    // 2. Inject missing `type` tags.
    changed |= inject_missing_type(val);

    // 3. Remove invalid items.
    changed |= remove_invalid(val);

    // 4. Inject default select when missing.
    changed |= inject_default_select(val);

    // 5. Normalize group_by strings.
    changed |= normalize_group_by_strings(val);

    // 6. Remove Star projections when GROUP BY is present.
    changed |= remove_star_with_group_by(val);

    // 7. Remove aggregate function calls from GROUP BY.
    changed |= remove_aggregates_from_group_by(val);

    changed
}

/// Unwrap `{"select": {"items": [...]}}` or `{"select": {"projections": [...]}}`
/// to `{"select": [...]}`.  The LLM sometimes wraps the select list in a
/// wrapper object instead of emitting a bare array.
#[must_use]
fn unwrap_select_items(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let select_val = match obj.get("select") {
        Some(v) if v.is_object() => v,
        _ => return false,
    };
    let Some(select_obj) = select_val.as_object() else {
        return false;
    };
    // Try `items` or `projections` as the actual array.
    let items = select_obj
        .get("items")
        .or_else(|| select_obj.get("projections"))
        .and_then(|v| v.as_array())
        .cloned();
    if let Some(arr) = items {
        obj.insert("select".to_owned(), serde_json::Value::Array(arr));
        return true;
    }
    false
}

/// Remove aggregate function call items from the `group_by` array.
///
/// The LLM frequently puts aggregate functions (`string_agg`, `sum`,
/// `count`, `avg`, etc.) in the `group_by` list, but SQL only allows
/// column references (or non-aggregate expressions) in `GROUP BY`.
/// These misplaced aggregates are silently removed so the validator
/// doesn't reject the plan.
#[must_use]
fn remove_aggregates_from_group_by(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(arr) = obj.get_mut("group_by").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    // Collect items to remove (function calls / aggregate shorthands)
    // before mutating, so we can add them to `select` afterwards.
    let before = arr.len();
    let mut removed: Vec<serde_json::Value> = Vec::new();
    arr.retain(|item| {
        let type_ = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        // Direct function_call match.
        if type_ == "function_call" {
            removed.push(item.clone());
            return false;
        }
        // `expr` wrapper containing a function_call.
        if type_ == "expr"
            && let Some(inner) = item.get("expression")
            && inner.get("type").and_then(|t| t.as_str()) == Some("function_call")
        {
            removed.push(inner.clone());
            return false;
        }
        // Also remove items that look like aggregate shorthands:
        // {"type": "sum", "args": [...]} (non-canonical aggregate form)
        if !type_.is_empty() && type_ != "column_ref" && type_ != "literal"
            && item.get("args").is_some()
        {
            removed.push(item.clone());
            return false;
        }
        true
    });
    let changed = arr.len() != before;
    // If group_by is now empty, remove the field entirely.
    if arr.is_empty() {
        obj.remove("group_by");
    }

    // Add removed aggregates to `select` if they aren't already there.
    if !removed.is_empty()
        && let Some(select_arr) = obj.get_mut("select").and_then(|v| v.as_array_mut())
    {
        for agg in &removed {
            // Check if an equivalent item already exists in select.
            let already_present = select_arr.iter().any(|s| s == agg);
            if !already_present {
                select_arr.push(agg.clone());
            }
        }
    }

    changed
}

/// Remove `Star` projections from `select` when the plan has a `group_by`.
///
/// `SELECT * ... GROUP BY col` is invalid SQL in most dialects — the `*`
/// cannot be resolved against grouped columns.  When the LLM emits both,
/// we remove the `Star` items so the builder can produce a valid query.
/// Individual column references and aggregate expressions are kept.
///
/// Returns `true` if any `Star` was removed.
#[must_use]
fn remove_star_with_group_by(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    // Check for GROUP BY at plan level OR nested inside `where`.
    let has_group_by = obj
        .get("group_by")
        .and_then(|v| v.as_array())
        .is_some_and(|arr| !arr.is_empty())
        // Also check inside `where` — `select::normalize` runs before
        // `where_::normalize`, so `group_by` may not yet be extracted.
        || obj.get("where")
            .and_then(|w| w.as_object())
            .and_then(|w| w.get("group_by"))
            .and_then(|v| v.as_array())
            .is_some_and(|arr| !arr.is_empty());
    if !has_group_by {
        return false;
    }
    let Some(select_arr) = obj.get_mut("select").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let before = select_arr.len();
    select_arr.retain(|item| {
        item.get("type").and_then(|t| t.as_str()) != Some("star")
    });
    let changed = select_arr.len() != before;
    // If removing Star left the select list empty, inject a default
    // column_ref so the builder doesn't fail.
    if select_arr.is_empty() {
        select_arr.push(serde_json::json!({"type": "column_ref", "column": "id"}));
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inject_missing_type_on_column_ref() {
        let mut val =
            json!({"select": [{"column": "name", "table": "users"}], "from": {"table": "users"}});
        assert!(inject_missing_type(&mut val));
        let item = &val.get("select").unwrap().as_array().unwrap()[0];
        assert_eq!(
            item.get("type").and_then(|v| v.as_str()),
            Some("column_ref")
        );
    }

    #[test]
    fn inject_missing_type_noop_when_already_present() {
        let mut val = json!({"select": [{"type": "column_ref", "column": "name"}], "from": {"table": "users"}});
        assert!(!inject_missing_type(&mut val));
    }

    #[test]
    fn remove_invalid_items() {
        let mut val = json!({"select": [{"type": "star"}, {"type": "invalid_type"}, "bare string", 42], "from": {"table": "users"}});
        assert!(remove_invalid(&mut val));
        let arr = val.get("select").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("type").and_then(|v| v.as_str()), Some("star"));
    }

    #[test]
    fn remove_invalid_removes_empty_select() {
        let mut val = json!({"select": [{"type": "invalid"}], "from": {"table": "users"}});
        assert!(remove_invalid(&mut val));
        assert!(
            val.get("select").is_none(),
            "empty select should be removed"
        );
    }

    #[test]
    fn inject_default_select_when_missing() {
        let mut val = json!({"from": {"table": "users"}});
        assert!(inject_default_select(&mut val));
        let select = val.get("select").unwrap().as_array().unwrap();
        assert_eq!(select[0].get("type").and_then(|v| v.as_str()), Some("star"));
    }

    #[test]
    fn inject_default_select_noop_when_select_exists() {
        let mut val =
            json!({"select": [{"type": "column_ref", "column": "id"}], "from": {"table": "users"}});
        assert!(!inject_default_select(&mut val));
    }

    #[test]
    fn inject_default_select_noop_when_no_from() {
        let mut val = json!({"where": {"type": "comparison"}});
        assert!(!inject_default_select(&mut val));
    }

    #[test]
    fn normalize_projection_string_to_object() {
        let mut val = json!({"select": ["id", "name"], "from": {"table": "users"}});
        assert!(normalize_projection_items(&mut val));
        let arr = val.get("select").unwrap().as_array().unwrap();
        assert_eq!(
            arr[0].get("type").and_then(|v| v.as_str()),
            Some("column_ref")
        );
        assert_eq!(arr[0].get("column").and_then(|v| v.as_str()), Some("id"));
        assert_eq!(arr[1].get("column").and_then(|v| v.as_str()), Some("name"));
    }

    #[test]
    fn normalize_star_string_to_star_object() {
        let mut val = json!({"select": ["*"], "from": {"table": "users"}});
        assert!(normalize_projection_items(&mut val));
        let arr = val.get("select").unwrap().as_array().unwrap();
        assert_eq!(arr[0].get("type").and_then(|v| v.as_str()), Some("star"));
        assert!(arr[0].get("column").is_none());
    }

    #[test]
    fn test_normalize_group_by_strings() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "group_by": ["status", "type"]});
        assert!(normalize_group_by_strings(&mut val));
        let arr = val.get("group_by").unwrap().as_array().unwrap();
        assert_eq!(
            arr[0].get("type").and_then(|v| v.as_str()),
            Some("column_ref")
        );
        assert_eq!(
            arr[0].get("column").and_then(|v| v.as_str()),
            Some("status")
        );
        assert_eq!(arr[1].get("column").and_then(|v| v.as_str()), Some("type"));
    }

    #[test]
    fn full_normalize_select() {
        let mut val = json!({
            "select": ["id", {"column": "name"}],
            "from": {"table": "users"}
        });
        assert!(normalize(&mut val));
        let arr = val.get("select").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(
            arr[0].get("type").and_then(|v| v.as_str()),
            Some("column_ref")
        );
        assert_eq!(arr[0].get("column").and_then(|v| v.as_str()), Some("id"));
        assert_eq!(
            arr[1].get("type").and_then(|v| v.as_str()),
            Some("column_ref")
        );
        assert_eq!(arr[1].get("column").and_then(|v| v.as_str()), Some("name"));
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
        assert!(!normalize(&mut val));
    }

    #[test]
    fn remove_star_when_group_by_present() {
        // Regression: `SELECT orders.*, users.id, users.name, SUM(total) ... GROUP BY users.id`
        // is invalid SQL.  The Star must be removed when GROUP BY exists.
        let mut val = json!({
            "select": [
                {"type": "star", "table": "orders"},
                {"type": "column_ref", "table": "users", "column": "id"},
                {"type": "column_ref", "table": "users", "column": "name"}
            ],
            "from": {"type": "table", "table": "users"},
            "group_by": [{"type": "column_ref", "table": "users", "column": "id"}]
        });
        assert!(normalize(&mut val));
        let select = val.get("select").unwrap().as_array().unwrap();
        // Star must be removed; individual columns must remain.
        for item in select.iter() {
            let type_ = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            assert_ne!(type_, "star", "Star must be removed when GROUP BY is present");
        }
        assert!(!select.is_empty(), "select must not be empty after star removal");
    }

    #[test]
    fn star_not_removed_when_no_group_by() {
        // Sanity: SELECT * without GROUP BY must be left intact.
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"type": "table", "table": "users"}
        });
        assert!(!normalize(&mut val));
        assert_eq!(
            val.pointer("/select/0/type").and_then(|t| t.as_str()),
            Some("star")
        );
    }

    #[test]
    fn star_removed_leaves_fallback_column_when_empty() {
        // When Star is the only projection and GROUP BY is present,
        // the removed Star must be replaced with a default column_ref.
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"type": "table", "table": "users"},
            "group_by": [{"type": "column_ref", "column": "status"}]
        });
        assert!(normalize(&mut val));
        let select = val.get("select").unwrap().as_array().unwrap();
        assert_eq!(select.len(), 1);
        assert_eq!(
            select[0].get("type").and_then(|t| t.as_str()),
            Some("column_ref")
        );
    }
}
