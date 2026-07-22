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
        if let Some(item_obj) = item.as_object_mut() {
            if !item_obj.contains_key("type") && item_obj.contains_key("column") {
                item_obj.insert(
                    "type".to_owned(),
                    serde_json::Value::String("column_ref".to_owned()),
                );
                changed = true;
            }
        }
    }
    changed
}

/// Remove items from `select` that have invalid or missing `type` tags.
///
/// Returns `true` if any item was removed.
#[must_use]
pub fn remove_invalid(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(arr) = obj.get_mut("select").and_then(|v| v.as_array_mut()) else {
        return false;
    };
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
        return true;
    }
    false
}

/// Normalize a single projection item (string → object).
///
/// If the projection is a plain string like `"id"`, convert it to
/// `{"type": "column_ref", "column": "id"}`.
/// If the string is `"*"`, convert it to `{"type": "star"}`.
#[must_use]
pub fn normalize_projection_item(item: &mut serde_json::Value) -> bool {
    if let Some(s) = item.as_str() {
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
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let mut changed = false;

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
}
