//! Query-level structure normalization.
//!
//! Handles top-level structural fixes:
//! - Strips unknown top-level fields that serde would reject
//! - Wraps top-level `descending` + `expr` into `order_by`

/// Fields that are valid on a QueryPlan (used for stripping and lifting).
const PLAN_FIELDS: &[&str] = &[
    "select", "from", "where", "group_by", "having", "order_by", "limit", "offset", "joins", "ctes",
];

/// Fields that belong at the QueryPlan top level but the LLM sometimes
/// nests inside `where`.
const PLAN_LEVEL_FIELDS: &[&str] = &[
    "joins", "group_by", "having", "order_by", "limit", "offset", "ctes",
];

/// Strip unknown top-level fields that `QueryPlan` rejects.
///
/// `QueryPlan` uses `#[serde(deny_unknown_fields)]`, so fields like
/// `right`, `left`, `op`, `child`, `expr` at the plan level cause an
/// immediate deserialization error.  Remove them so the pipeline can
/// at least attempt to make `where` valid.
///
/// Returns `true` if any field was removed.
#[must_use]
pub fn strip_unknown_fields(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let before = obj.len();
    obj.retain(|key, _| PLAN_FIELDS.contains(&key.as_str()));
    obj.len() != before
}

/// Wrap top-level `descending` + `expr` into an `order_by` array.
///
/// The LLM sometimes emits `descending` and `expr` at the top level
/// of the QueryPlan instead of inside an `OrderByTerm` within the
/// `order_by` array.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn wrap_descending_expr(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };

    if obj.contains_key("order_by") {
        return false;
    }

    if let (Some(expr), Some(descending)) = (obj.remove("expr"), obj.remove("descending")) {
        if descending.is_boolean() {
            let term = serde_json::json!({
                "expr": expr,
                "descending": descending,
            });
            obj.insert("order_by".to_owned(), serde_json::json!([term]));
            return true;
        }
    }

    false
}

/// Convert string-formatted `limit` / `offset` values to numbers.
///
/// Some LLMs emit `"limit": "10"` (string) instead of `"limit": 10` (number).
/// The builder's `as_u64()` call would return `None` for a string, silently
/// dropping the limit.  This function converts string values to numbers.
///
/// Returns `true` if any field was converted.
#[must_use]
pub fn normalize_limit_offset(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for field in &["limit", "offset"] {
        if let Some(str_val) = obj.get(*field).and_then(|v| v.as_str()) {
            if let Ok(num) = str_val.parse::<u64>() {
                obj.insert(field.to_string(), serde_json::Value::Number(num.into()));
                changed = true;
            }
        }
    }
    changed
}

/// Lift plan-level fields (joins, group_by, having, order_by, limit, offset, ctes)
/// out of a nested `where` object back to the top level.
///
/// Weak LLMs sometimes put everything inside `where`:
///   `{"where": {"type": "left", "child": ..., "joins": [...], "limit": 10, ...}}`
/// instead of keeping them as top-level siblings.
///
/// Returns `true` if any field was lifted.
#[must_use]
fn lift_nested_plan_fields(val: &mut serde_json::Value) -> bool {
    // Collect fields to lift first, with their values.
    let (lifted, type_left) = {
        let Some(obj) = val.as_object() else {
            return false;
        };
        let Some(where_obj) = obj.get("where").and_then(|v| v.as_object()) else {
            return false;
        };

        let lifted: Vec<(String, serde_json::Value)> = PLAN_LEVEL_FIELDS
            .iter()
            .filter_map(|field| {
                if !obj.contains_key(*field) {
                    where_obj.get(*field).map(|v| (field.to_string(), v.clone()))
                } else {
                    None
                }
            })
            .collect();

        let type_left = where_obj
            .get("type")
            .and_then(|t| t.as_str())
            == Some("left")
            && where_obj.contains_key("child");

        (lifted, type_left)
    };

    if lifted.is_empty() && !type_left {
        return false;
    }

    let Some(obj) = val.as_object_mut() else {
        return false;
    };

    let mut changed = false;
    for (field, value) in lifted {
        obj.insert(field.clone(), value);
        // Remove the lifted field from `where`
        if let Some(where_obj) = obj.get_mut("where").and_then(|v| v.as_object_mut()) {
            where_obj.remove(&field);
        }
        changed = true;
    }

    // Fix `"type": "left"` → `"type": "not"` inside `where`.
    if type_left {
        if let Some(where_obj) = obj.get_mut("where").and_then(|v| v.as_object_mut()) {
            where_obj.insert(
                "type".to_owned(),
                serde_json::Value::String("not".to_owned()),
            );
            changed = true;
        }
    }

    changed
}

/// Remove null entries from array fields (order_by, group_by, having).
///
/// Weak LLMs sometimes emit `"order_by": [null]` or `"group_by": [null]`
/// which the builder rejects because it expects objects.
///
/// Returns `true` if any null entries were removed.
#[must_use]
fn sanitize_null_array_entries(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for field in &["order_by", "group_by", "having"] {
        if let Some(arr) = obj.get_mut(*field).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|v| !v.is_null());
            if arr.len() != before {
                changed = true;
            }
        }
    }
    changed
}

/// Full query-level structure normalization.
///
/// 1. Wrap top-level `descending` + `expr` into `order_by` (must run
///    before `strip_unknown_fields` so these fields are preserved).
/// 2. Lift nested plan fields from `where` back to top level.
/// 3. Strip unknown top-level fields.
/// 4. Remove null entries from array fields.
/// 5. Normalize string limit/offset to numbers.
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let mut changed = false;
    // Wrap first: expr + descending → order_by, before they get stripped.
    changed |= wrap_descending_expr(val);
    // Lift plan fields from within `where` before stripping.
    changed |= lift_nested_plan_fields(val);
    // Then strip remaining unknown fields.
    changed |= strip_unknown_fields(val);
    // Remove null entries from array fields.
    changed |= sanitize_null_array_entries(val);
    // Normalize string limit/offset to numbers.
    changed |= normalize_limit_offset(val);
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_unknown_fields_removes_extra() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "right": {"column": "id"},
            "left": {"column": "name"},
            "op": "eq"
        });
        assert!(strip_unknown_fields(&mut val));
        assert!(val.get("select").is_some());
        assert!(val.get("from").is_some());
        assert!(val.get("right").is_none());
        assert!(val.get("left").is_none());
        assert!(val.get("op").is_none());
    }

    #[test]
    fn strip_unknown_fields_noop_when_clean() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"}
        });
        assert!(!strip_unknown_fields(&mut val));
    }

    #[test]
    fn wrap_descending_expr_into_order_by() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "expr": {"column": "name"},
            "descending": true
        });
        assert!(wrap_descending_expr(&mut val));
        assert!(val.get("order_by").is_some());
        assert!(val.get("expr").is_none());
        assert!(val.get("descending").is_none());
        let order_by = val.get("order_by").unwrap().as_array().unwrap();
        assert_eq!(order_by.len(), 1);
        assert_eq!(
            order_by[0]
                .get("expr")
                .and_then(|v| v.get("column"))
                .and_then(|v| v.as_str()),
            Some("name")
        );
        assert_eq!(
            order_by[0].get("descending").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn wrap_descending_expr_noop_when_order_by_exists() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "order_by": [{"expr": {"column": "name"}, "descending": true}],
            "expr": {"column": "name"},
            "descending": true
        });
        assert!(!wrap_descending_expr(&mut val));
    }

    #[test]
    fn wrap_descending_expr_noop_when_missing_fields() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"}
        });
        assert!(!wrap_descending_expr(&mut val));
    }

    #[test]
    fn full_normalize() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "right": {"column": "id"},
            "expr": {"column": "name"},
            "descending": true
        });
        assert!(normalize(&mut val));
        // Unknown fields stripped.
        assert!(val.get("right").is_none());
        // expr + descending wrapped into order_by.
        assert!(val.get("order_by").is_some());
        assert!(val.get("expr").is_none());
        assert!(val.get("descending").is_none());
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
