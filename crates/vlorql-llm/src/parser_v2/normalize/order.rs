//! ORDER BY clause normalization.
//!
//! Ensures `order_by` is always an array of valid order-by terms.
//!
//! Normalizes each item:
//! - `{"column": "name", "descending": true}` → `{"expr": {"type": "column_ref", "column": "name"}, "descending": true}`
//! - `{"expr": {"column": "name"}}` → `{"expr": {"type": "column_ref", "column": "name"}}`

/// Normalize a single order_by item: ensure `expr` is a proper expression object.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn normalize_item(item: &mut serde_json::Value) -> bool {
    let obj = match item.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let mut changed = false;

    // Case 1: `{"column": "name", "descending": true}` — bare column field.
    // Wrap it into `{"expr": {"type": "column_ref", "column": "..."}, "descending": ...}`.
    if !obj.contains_key("expr") {
        if let Some(column_val) = obj.remove("column") {
            if let Some(col_name) = column_val.as_str() {
                obj.insert(
                    "expr".to_owned(),
                    serde_json::json!({"type": "column_ref", "column": col_name}),
                );
                changed = true;
            }
        }
    }

    // Case 2: `{"expr": {"column": "name"}}` — expr has column but no type.
    if let Some(expr_val) = obj.get_mut("expr") {
        if let Some(expr_obj) = expr_val.as_object_mut() {
            if !expr_obj.contains_key("type") && expr_obj.contains_key("column") {
                expr_obj.insert(
                    "type".to_owned(),
                    serde_json::Value::String("column_ref".to_owned()),
                );
                changed = true;
            }
        }
    }

    changed
}

/// Normalize all items in the `order_by` array.
///
/// Returns `true` if any item was modified.
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let mut changed = false;

    // Normalize order_by items (OrderByTerm with expr + descending).
    if let Some(arr) = obj.get_mut("order_by").and_then(|v| v.as_array_mut()) {
        for item in arr.iter_mut() {
            changed |= normalize_item(item);
        }
    }

    // Normalize group_by items: LLMs often emit them as
    // {"expr": {"type": "column_ref", ...}} (order_by format) instead of
    // bare Expression objects.  Unwrap expr → item.
    if let Some(arr) = obj.get_mut("group_by").and_then(|v| v.as_array_mut()) {
        for item in arr.iter_mut() {
            if let Some(obj) = item.as_object_mut()
                && obj.contains_key("expr")
                && !obj.contains_key("type")
                && !obj.contains_key("column")
            {
                if let Some(expr) = obj.remove("expr") {
                    *item = expr;
                    changed = true;
                }
            }
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_bare_column() {
        let mut val = json!({"order_by": [{"column": "name", "descending": true}]});
        assert!(normalize(&mut val));
        let item = &val["order_by"][0];
        assert_eq!(item["expr"]["type"], "column_ref");
        assert_eq!(item["expr"]["column"], "name");
        assert_eq!(item["descending"], true);
        assert!(item.get("column").is_none());
    }

    #[test]
    fn normalizes_expr_missing_type() {
        let mut val = json!({"order_by": [{"expr": {"column": "name"}, "descending": true}]});
        assert!(normalize(&mut val));
        assert_eq!(val["order_by"][0]["expr"]["type"], "column_ref");
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({"order_by": [{"expr": {"type": "column_ref", "column": "name"}, "descending": true}]});
        assert!(!normalize(&mut val));
    }

    #[test]
    fn normalizes_multiple_items() {
        let mut val = json!({"order_by": [
            {"column": "name", "descending": true},
            {"column": "age", "descending": false}
        ]});
        assert!(normalize(&mut val));
        assert_eq!(val["order_by"][0]["expr"]["column"], "name");
        assert_eq!(val["order_by"][1]["expr"]["column"], "age");
    }
}
