//! Array structure normalization.
//!
//! Ensures fields that should be arrays are always arrays, even when
//! the LLM emits a single value or object.  Also removes null/empty
//! entries from array fields.

/// Ensures the specified field is always a JSON array.
///
/// If the field is a single object, it is wrapped in an array.
/// Returns `true` if any change was made.
#[must_use]
pub fn ensure_array_field(
    val: &mut serde_json::Value,
    field: &str,
) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(field_val) = obj.get(field) else {
        return false;
    };

    if field_val.is_array() {
        return false;
    }

    if field_val.is_null() || field_val.is_object() {
        // Wrap single object or null in an array.
        // For null, wrap as empty array (will be removed later if empty).
        let wrapped = if field_val.is_null() {
            serde_json::json!([])
        } else {
            serde_json::json!([field_val.clone()])
        };
        obj.insert(field.to_owned(), wrapped);
        return true;
    }

    false
}

/// Ensures multiple fields are always arrays.
#[must_use]
pub fn ensure_array_fields(
    val: &mut serde_json::Value,
    fields: &[&str],
) -> bool {
    let mut changed = false;
    for field in fields {
        changed |= ensure_array_field(val, field);
    }
    changed
}

/// Removes null entries from an array field.
/// Returns `true` if any entry was removed.
#[must_use]
pub fn remove_nulls(
    val: &mut serde_json::Value,
    field: &str,
) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(arr) = obj.get_mut(field).and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let before = arr.len();
    arr.retain(|v| !v.is_null());
    if arr.len() != before {
        if arr.is_empty() {
            obj.remove(field);
        }
        return true;
    }
    false
}

/// Removes empty array fields (sets them to None).
/// Returns `true` if any field was removed.
#[must_use]
pub fn remove_empty_arrays(
    val: &mut serde_json::Value,
    fields: &[&str],
) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for field in fields {
        if let Some(arr) = obj.get(*field).and_then(|v| v.as_array()) {
            if arr.is_empty() {
                obj.remove(*field);
                changed = true;
            }
        }
    }
    changed
}

/// Flattens `{"type": "array", "items": [...]}` wrapper objects that
/// the LLM sometimes emits instead of a bare array.
///
/// Returns `true` if any field was flattened.
#[must_use]
pub fn flatten_array_wrapper(
    val: &mut serde_json::Value,
    field: &str,
) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(field_val) = obj.get(field) else {
        return false;
    };

    // Check if this is a {"type": "array", "items": [...]} wrapper.
    if let Some(map) = field_val.as_object() {
        if map.get("type").and_then(|t| t.as_str()) == Some("array") {
            if let Some(items) = map.get("items").and_then(|v| v.as_array()) {
                obj.insert(field.to_owned(), serde_json::Value::Array(items.clone()));
                return true;
            }
        }
    }
    false
}

/// Full array normalization for a query plan value.
///
/// 1. Ensure select, group_by, order_by, joins, ctes are arrays
/// 2. Remove nulls from group_by, order_by
/// 3. Remove empty arrays
/// 4. Flatten array wrappers
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let mut changed = false;

    // Fields that should always be arrays.
    changed |= ensure_array_fields(val, &["select", "group_by", "order_by", "joins", "ctes"]);

    // Remove null entries from array fields.
    changed |= remove_nulls(val, "group_by");
    changed |= remove_nulls(val, "order_by");

    // Remove empty arrays.
    changed |= remove_empty_arrays(val, &["group_by", "order_by", "joins", "ctes"]);

    // Flatten array wrapper objects.
    changed |= flatten_array_wrapper(val, "group_by");
    changed |= flatten_array_wrapper(val, "order_by");

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ensure_select_single_object() {
        let mut val = json!({"select": {"type": "star"}, "from": {"table": "users"}});
        assert!(ensure_array_field(&mut val, "select"));
        let arr = val.get("select").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("type").and_then(|v| v.as_str()), Some("star"));
    }

    #[test]
    fn ensure_select_already_array() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
        assert!(!ensure_array_field(&mut val, "select"));
    }

    #[test]
    fn ensure_joins_single_object() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "joins": {"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison"}}});
        assert!(ensure_array_field(&mut val, "joins"));
        assert!(val.get("joins").unwrap().is_array());
    }

    #[test]
    fn ensure_ctes_single_object() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "ctes": {"name": "active", "query": {"select": [{"type": "star"}], "from": {"table": "users"}}}});
        assert!(ensure_array_field(&mut val, "ctes"));
        assert!(val.get("ctes").unwrap().is_array());
    }

    #[test]
    fn ensure_null_field() {
        let mut val = json!({"select": null, "from": {"table": "users"}});
        assert!(ensure_array_field(&mut val, "select"));
        let arr = val.get("select").unwrap().as_array().unwrap();
        assert!(arr.is_empty());
    }

    #[test]
    fn remove_nulls_from_group_by() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "group_by": [{"column": "status"}, null, {"column": "type"}]});
        assert!(remove_nulls(&mut val, "group_by"));
        let arr = val.get("group_by").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn remove_nulls_removes_empty_field() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "group_by": [null]});
        assert!(remove_nulls(&mut val, "group_by"));
        assert!(val.get("group_by").is_none(), "empty group_by should be removed");
    }

    #[test]
    fn remove_empty_arrays_removes_empty() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "group_by": []});
        assert!(remove_empty_arrays(&mut val, &["group_by"]));
        assert!(val.get("group_by").is_none());
    }

    #[test]
    fn flatten_array_wrapper_detects() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "group_by": {"type": "array", "items": [{"column": "status"}]}});
        assert!(flatten_array_wrapper(&mut val, "group_by"));
        let arr = val.get("group_by").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("column").and_then(|v| v.as_str()), Some("status"));
    }

    #[test]
    fn full_normalize() {
        let mut val = json!({
            "select": {"type": "star"},
            "from": {"table": "users"},
            "group_by": [{"column": "status"}, null],
            "order_by": {"type": "array", "items": [{"expr": {"column": "name"}, "descending": true}]}
        });
        assert!(normalize(&mut val));
        assert!(val.get("select").unwrap().is_array());
        assert!(val.get("group_by").is_some());
        assert_eq!(val.get("group_by").unwrap().as_array().unwrap().len(), 1);
        assert!(val.get("order_by").unwrap().is_array());
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"}
        });
        assert!(!normalize(&mut val));
    }
}