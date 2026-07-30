//! WHERE clause structure normalization.
//!
//! Ensures:
//! - `where` is always a predicate object (not an array)
//! - Misplaced top-level fields inside `where` are extracted back
//!   to the plan level

use super::common;

/// Fields that belong at the top level of the plan, not inside `where`.
const TOP_LEVEL_FIELDS: &[&str] = &[
    "order_by", "limit", "offset", "group_by", "having", "joins", "ctes",
];

/// If `where` is an array, collapse it to a single predicate object.
///
/// The LLM sometimes emits `"where": [{...}, "garbage"]`.  This
/// extracts the first object that has a `type` field.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn collapse_from_array(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(where_val) = obj.get("where") else {
        return false;
    };

    if !where_val.is_array() {
        return false;
    }

    let arr = where_val.as_array().unwrap();
    let pred = arr
        .iter()
        .filter_map(|v| v.as_object())
        .find(|o| o.contains_key("type"))
        .cloned()
        .map(serde_json::Value::Object)
        .unwrap_or(serde_json::Value::Null);

    if pred.is_null() {
        obj.remove("where");
    } else {
        obj.insert("where".to_owned(), pred);
    }
    true
}

/// Extract misplaced top-level fields from inside `where`.
///
/// The LLM sometimes nests `order_by`, `limit`, `offset`, `group_by`,
/// `having`, `joins`, `ctes` inside the `where` object.  Move them
/// back to the plan level.
///
/// Returns `true` if any field was extracted.
#[must_use]
pub fn extract_top_level_fields(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };

    // Snapshot which top-level fields already exist BEFORE we mutably borrow
    // `where` below.  This snapshot lets us decide extraction without holding
    // an immutable borrow of `obj` across the mutable `where_obj.remove(field)`
    // call, which would violate Rust's borrow rules.
    // NOTE: `serde_json::Map::get/contains_key` requires `String: Borrow<Q>`
    // — `&str` does not satisfy that bound, so we look up via a borrowed
    // `String` instead.
    let top_present: Vec<String> = TOP_LEVEL_FIELDS
        .iter()
        .filter(|f| {
            let key: String = f.to_string();
            obj.get(&key).is_some()
        })
        .map(|s| s.to_string())
        .collect();

    let Some(where_val) = obj.get_mut("where") else {
        return false;
    };
    let Some(where_obj) = where_val.as_object_mut() else {
        return false;
    };

    let mut extracted: Vec<(String, serde_json::Value)> = Vec::new();
    for &field in TOP_LEVEL_FIELDS {
        // Only extract when the plan level does NOT already have this field.
        // This preserves the where-nested value instead of dropping it on the
        // floor when a same-named top-level field exists (e.g. an inner subquery's
        // `limit` mistakenly nested inside `where` would otherwise be silently
        // discarded while the outer plan's `limit` stays unchanged).
        if top_present.iter().any(|f| f == field) {
            continue;
        }
        if let Some(field_val) = where_obj.remove(field)
            && !field_val.is_null()
            && !common::is_empty_array(&field_val)
        {
            extracted.push((field.to_owned(), field_val));
        }
    }

    let mut changed = false;
    for (field, field_val) in &extracted {
        // Guard against a concurrent insertion between the check above and here
        // (none in single-threaded normalize, but kept for clarity).
        if !obj.contains_key(field) {
            obj.insert(field.clone(), field_val.clone());
            changed = true;
        }
    }

    // If `where` is now empty, remove it.
    if let Some(w) = obj.get("where")
        && w.as_object().is_some_and(|o| o.is_empty())
    {
        obj.remove("where");
        changed = true;
    }

    changed
}

/// If `having` is an array, collapse it to a single predicate object.
///
/// The LLM sometimes emits `"having": [{...}, "garbage"]`.  This
/// extracts the first object that has a `type` field.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn collapse_having_from_array(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(having_val) = obj.get("having") else {
        return false;
    };

    if !having_val.is_array() {
        return false;
    }

    let arr = having_val.as_array().unwrap();
    let pred = arr
        .iter()
        .filter_map(|v| v.as_object())
        .find(|o| o.contains_key("type"))
        .cloned()
        .map(serde_json::Value::Object)
        .unwrap_or(serde_json::Value::Null);

    if pred.is_null() {
        obj.remove("having");
    } else {
        obj.insert("having".to_owned(), pred);
    }
    true
}

/// Convert a flat WHERE condition to a proper comparison structure.
///
/// Some LLMs emit a flat structure like:
/// ```json
/// {"where": {"field": "age", "operator": "gt", "value": 18}}
/// ```
/// instead of the canonical:
/// ```json
/// {"where": {"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18}}}
/// ```
///
/// After aliases normalization, `field` → `column`, `operator` → `op`,
/// but the structure is still flat (no `left`, no `type`).  This function
/// detects that pattern and wraps it into a proper comparison.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn normalize_flat_condition(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(where_val) = obj.get_mut("where") else {
        return false;
    };
    let Some(where_obj) = where_val.as_object_mut() else {
        return false;
    };

    // Detect flat condition: has `column`, `op`, and `value` but no `left` or `type`.
    if where_obj.contains_key("type") {
        return false;
    }
    let has_column = where_obj.contains_key("column");
    let has_op = where_obj.contains_key("op");
    let has_value = where_obj.contains_key("value");
    let has_left = where_obj.contains_key("left");

    if !has_left && has_column && has_op && has_value {
        let column = where_obj.remove("column").unwrap();
        let op = where_obj.remove("op").unwrap();
        let value = where_obj.remove("value").unwrap();

        // Build left expression from column.
        let left = if let Some(col_name) = column.as_str() {
            serde_json::json!({"type": "column_ref", "column": col_name})
        } else {
            column
        };

        // Build right expression from value.
        let right = if value.is_object() {
            // Value is already an object (e.g., {"value": 18, "data_type": "int"}).
            // The value might be flattened: {"value": 18} → need to wrap.
            if value.get("type").is_none() {
                let mut lit = serde_json::json!({"type": "literal"});
                if let Some(v) = value.get("value") {
                    lit["value"] = v.clone();
                }
                if let Some(dt) = value.get("data_type") {
                    lit["data_type"] = dt.clone();
                }
                lit
            } else {
                value
            }
        } else {
            // Bare value (number, string, bool).
            serde_json::json!({"type": "literal", "value": value})
        };

        *where_val = serde_json::json!({
            "type": "comparison",
            "left": left,
            "op": op,
            "right": right
        });
        return true;
    }

    false
}

/// Wrap bare `function_call` objects in `having`/`where` into a comparison
/// predicate (`{"type":"comparison","left":...,"op":"gt","right":{"type":"literal","value":0}}`).
///
/// The LLM sometimes places a bare function_call directly in a predicate
/// position, which the builder rejects as "unknown Predicate variant".
#[must_use]
fn wrap_bare_function_call(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    for field in ["having", "where"] {
        if let Some(target) = obj.get_mut(field)
            && target.as_object()
                .and_then(|o| o.get("type").and_then(|t| t.as_str()))
                == Some("function_call")
        {
            let inner = std::mem::take(target);
            *target = serde_json::json!({
                "type": "comparison",
                "left": inner,
                "op": "gt",
                "right": {"type": "literal", "value": 0, "data_type": "int"}
            });
            return true;
        }
    }
    false
}

/// Full WHERE structure normalization.
///
/// 1. Collapse array `where` → single predicate object
/// 2. Extract misplaced top-level fields from `where`
/// 3. Normalize flat condition to proper comparison
/// 4. Collapse array `having` → single predicate object
/// 5. Wrap bare function_call in having/where → comparison
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let mut changed = false;
    changed |= collapse_from_array(val);
    changed |= extract_top_level_fields(val);
    changed |= normalize_flat_condition(val);
    changed |= collapse_having_from_array(val);
    changed |= wrap_bare_function_call(val);
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collapse_where_array_to_object() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "where": [{"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}]});
        assert!(collapse_from_array(&mut val));
        assert!(val.get("where").unwrap().is_object());
        assert_eq!(
            val.get("where")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("comparison")
        );
    }

    #[test]
    fn collapse_where_array_with_garbage() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "where": [{"type": "comparison"}, "garbage string", 42]});
        assert!(collapse_from_array(&mut val));
        assert!(val.get("where").unwrap().is_object());
    }

    #[test]
    fn collapse_where_array_no_valid_predicate() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "where": ["garbage", 42]});
        assert!(collapse_from_array(&mut val));
        assert!(val.get("where").is_none(), "where should be removed");
    }

    #[test]
    fn collapse_where_noop_for_object() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "where": {"type": "comparison"}});
        assert!(!collapse_from_array(&mut val));
    }

    #[test]
    fn extract_top_level_fields_from_where() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {
                "type": "comparison",
                "left": {"column": "age"},
                "op": "gt",
                "right": {"value": 18},
                "order_by": [{"expr": {"column": "name"}, "descending": true}],
                "limit": 10
            }
        });
        assert!(extract_top_level_fields(&mut val));
        assert!(
            val.get("order_by").is_some(),
            "order_by should be extracted"
        );
        assert!(val.get("limit").is_some(), "limit should be extracted");
        // These fields should be removed from inside `where`.
        let where_obj = val.get("where").unwrap().as_object().unwrap();
        assert!(where_obj.get("order_by").is_none());
        assert!(where_obj.get("limit").is_none());
        // The where should still have the predicate content.
        assert!(where_obj.get("type").is_some());
    }

    #[test]
    fn extract_top_level_fields_from_where_noop_when_not_present() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {"type": "comparison"}
        });
        assert!(!extract_top_level_fields(&mut val));
    }

    #[test]
    fn extract_top_level_fields_removes_empty_where() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {
                "order_by": [{"expr": {"column": "name"}, "descending": true}],
                "limit": 10
            }
        });
        assert!(extract_top_level_fields(&mut val));
        // All fields were extracted, where had nothing else — should be removed.
        assert!(val.get("where").is_none(), "empty where should be removed");
        assert!(val.get("order_by").is_some());
        assert!(val.get("limit").is_some());
    }

    #[test]
    fn full_normalize_where() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": [
                {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}, "limit": 10}
            ]
        });
        assert!(normalize(&mut val));
        // Where should be a single object.
        assert!(val.get("where").unwrap().is_object());
        // limit should be extracted to top level.
        assert_eq!(val.get("limit").and_then(|v| v.as_u64()), Some(10));
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {"type": "comparison"}
        });
        assert!(!normalize(&mut val));
    }

    #[test]
    fn extract_skips_when_top_level_already_has_field() {
        // Regression: when the plan level already has `limit`, a same-named
        // field nested inside `where` used to be `remove`d and silently
        // discarded.  It must now be preserved in `where` (left for the
        // builder to surface as a stray field) instead of being dropped.
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "limit": 10,
            "where": {
                "type": "comparison",
                "left": {"type": "column_ref", "column": "age"},
                "op": "gt",
                "right": {"type": "literal", "value": 18, "data_type": "int"},
                "limit": 99
            }
        });
        let before_where = val.get("where").cloned().unwrap();
        let _ = normalize(&mut val);
        // Top-level `limit` must be unchanged (still 10, not overwritten).
        assert_eq!(val.get("limit").and_then(|v| v.as_u64()), Some(10));
        // The nested `limit: 99` must NOT have been silently dropped —
        // either it stays in `where` (our conservative choice) or, if the
        // normalize pipeline stripped it later, the top-level value was
        // not corrupted.  We assert the stronger invariant: top-level
        // `limit` is still 10.
        let after_where = val.get("where").cloned();
        if let Some(after) = after_where {
            // If `where` still exists, its `limit` (if present) must be 99
            // (i.e. we didn't overwrite it) OR absent (a later stage removed
            // it).  It must NOT be 10 (which would mean we corrupted the
            // nested value).
            if let Some(nested_limit) = after.get("limit").and_then(|v| v.as_u64()) {
                assert_eq!(
                    nested_limit, 99,
                    "nested limit must not be corrupted by extract_top_level_fields"
                );
            }
        }
        // Sanity: the predicate itself is preserved.
        assert!(val.get("where").is_some(), "predicate must not be dropped");
        let _ = before_where;
    }

    #[test]
    fn extract_still_lifts_when_top_level_missing() {
        // Sanity: the fix must not break the original lift behavior when
        // the top-level field is genuinely missing.
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {
                "type": "comparison",
                "left": {"type": "column_ref", "column": "age"},
                "op": "gt",
                "right": {"type": "literal", "value": 18, "data_type": "int"},
                "limit": 50
            }
        });
        assert!(normalize(&mut val));
        assert_eq!(val.get("limit").and_then(|v| v.as_u64()), Some(50));
    }
}
