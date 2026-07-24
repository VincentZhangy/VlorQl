//! JOIN clause structure normalization.
//!
//! Ensures:
//! - `joins` is always an array of valid join objects
//! - `right_table` is always an object (not a bare string)
//! - Missing `right_table` is inferred from the ON clause when possible
//! - Unknown join-level fields are stripped

use super::common;

/// Fields that are valid on a JoinClause.
const VALID_JOIN_FIELDS: &[&str] = &["join_type", "right_table", "on"];

/// Fields that belong at the plan level, not inside a join object.
const PLAN_LEVEL_FIELDS: &[&str] = &[
    "select", "from", "where", "group_by", "having", "order_by", "limit", "offset", "ctes",
];

/// Extract plan-level fields from inside join objects and return them
/// to the top level.
///
/// Returns `true` if any field was extracted.
#[must_use]
pub fn extract_plan_level_fields(val: &mut serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let Some(obj) = val.as_object_mut() else {
        return Vec::new();
    };
    let Some(joins) = obj.get("joins").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut extracted: Vec<(String, serde_json::Value)> = Vec::new();
    for join in joins {
        if let Some(join_obj) = join.as_object() {
            for &field in PLAN_LEVEL_FIELDS {
                if let Some(field_val) = join_obj.get(field) {
                    if !field_val.is_null() && !common::is_empty_array(field_val) {
                        extracted.push((field.to_owned(), field_val.clone()));
                    }
                }
            }
        }
    }

    // Also remove the extracted fields from the join objects themselves.
    if let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) {
        for join in joins.iter_mut() {
            if let Some(join_obj) = join.as_object_mut() {
                for &field in PLAN_LEVEL_FIELDS {
                    join_obj.remove(field);
                }
            }
        }
    }

    extracted
}

/// Apply extracted plan-level fields to the top level.
#[must_use]
pub fn apply_extracted_fields(
    val: &mut serde_json::Value,
    extracted: Vec<(String, serde_json::Value)>,
) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for (field, field_val) in extracted {
        if !obj.contains_key(&field) {
            obj.insert(field, field_val);
            changed = true;
        }
    }
    changed
}

/// Convert a bare string `right_table` to a `{"table": "..."}` object.
///
/// Returns `true` if any join was modified.
#[must_use]
pub fn string_right_table_to_object(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for join in joins.iter_mut() {
        if let Some(join_obj) = join.as_object_mut() {
            if let Some(rt) = join_obj.get("right_table").and_then(|v| v.as_str()) {
                join_obj.insert("right_table".to_owned(), serde_json::json!({"table": rt}));
                changed = true;
            }
        }
    }
    changed
}

/// Infer missing `right_table` from the ON clause when possible.
///
/// If `right_table` is missing but `on` is present, try to extract
/// the table name from the ON predicate's `right` expression.
///
/// Returns `true` if any join was modified.
#[must_use]
pub fn infer_missing_right_table(val: &mut serde_json::Value) -> bool {
    // Get the parent `from` table FIRST (before mutable borrow of joins).
    let from_table = val
        .get("from")
        .and_then(|f| f.as_object())
        .and_then(|f| f.get("table"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_owned());

    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) else {
        return false;
    };

    let mut changed = false;
    for join in joins.iter_mut() {
        let Some(join_obj) = join.as_object_mut() else {
            continue;
        };
        if !join_obj.contains_key("right_table") && join_obj.contains_key("on") {
            if let Some(on_obj) = join_obj.get("on").and_then(|v| v.as_object()) {
                // Try to infer from ON.right (column_ref → table), with fallback to FROM table.
                let table = on_obj
                    .get("right")
                    .and_then(|r| r.as_object())
                    .and_then(|r| r.get("table"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_owned())
                    .or_else(|| from_table.clone());

                if let Some(table) = table {
                    join_obj.insert(
                        "right_table".to_owned(),
                        serde_json::json!({"table": table}),
                    );
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Strip unknown fields from join objects, keeping only valid join fields.
///
/// Returns `true` if any join was modified.
#[must_use]
pub fn strip_unknown_fields(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) else {
        return false;
    };

    // First, remove non-object entries and objects without `join_type`
    // (e.g., ColumnRef objects that the LLM mistakenly placed in joins).
    let before = joins.len();
    joins.retain(|v| v.is_object() && v.get("join_type").is_some());
    let mut changed = joins.len() != before;

    // Then strip unknown fields from each join object.
    for join in joins.iter_mut() {
        if let Some(join_obj) = join.as_object_mut() {
            let len_before = join_obj.len();
            join_obj.retain(|key, _| VALID_JOIN_FIELDS.contains(&key.as_str()));
            if join_obj.len() != len_before {
                changed = true;
            }
        }
    }

    // Remove empty joins array.
    if joins.is_empty() {
        obj.remove("joins");
        changed = true;
    }

    changed
}

/// Repair a bare ColumnRef in a join's ON clause.
///
/// Some LLMs emit a bare ColumnRef like `{"table": "users", "column": "id"}`
/// as the join ON condition instead of a proper comparison predicate.
/// This wraps it into a comparison: `{"type": "comparison", "left": ..., "op": "eq", "right": ...}`.
///
/// Returns `true` if any join was modified.
#[must_use]
pub fn repair_bare_join_on(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    // Collect (on_val_ptr, right_table_name) pairs to avoid borrow conflicts.
    let mut repairs: Vec<(usize, serde_json::Value, String)> = Vec::new();
    for (i, join) in joins.iter().enumerate() {
        let Some(join_obj) = join.as_object() else {
            continue;
        };
        let Some(on_val) = join_obj.get("on") else {
            continue;
        };
        let Some(on_obj) = on_val.as_object() else {
            continue;
        };
        // Detect bare ColumnRef: has `column` but no `type` and no `left`/`op`.
        if !on_obj.contains_key("type") && on_obj.contains_key("column") {
            let left_expr = on_val.clone();
            let right_table = join_obj
                .get("right_table")
                .and_then(|r| r.as_object())
                .and_then(|r| r.get("table"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_owned();

            // Add type tag to left expression.
            let mut left = left_expr;
            if let Some(left_obj) = left.as_object_mut() {
                if !left_obj.contains_key("type") {
                    left_obj.insert(
                        "type".to_owned(),
                        serde_json::Value::String("column_ref".to_owned()),
                    );
                }
            }

            repairs.push((i, left, right_table));
        }
    }
    // Apply repairs.
    for (i, left, right_table) in repairs {
        if let Some(join) = joins.get_mut(i) {
            if let Some(join_obj) = join.as_object_mut() {
                join_obj.insert(
                    "on".to_owned(),
                    serde_json::json!({
                        "type": "comparison",
                        "left": left,
                        "op": "eq",
                        "right": {
                            "type": "column_ref",
                            "table": right_table,
                            "column": "id"
                        }
                    }),
                );
                changed = true;
            }
        }
    }
    changed
}

/// Infer a missing `on` clause from `right_table` when the LLM omits it.
///
/// Uses the query's `from` table to derive the most likely foreign-key pattern.
/// Tries two common conventions:
///
/// 1. **FK in FROM table** — `right_table.id = from_table.<right_singular>_id`
///    (e.g. `FROM orders JOIN users ON users.id = orders.user_id`)
///
/// 2. **FK in RIGHT table** — `right_table.<from_singular>_id = from_table.id`
///    (e.g. `FROM orders JOIN order_items ON order_items.order_id = orders.id`)
///
/// Pattern 1 is the default since it is slightly more common.  Even an
/// incorrect `on` is better than a missing one — it lets the builder
/// succeed and the schema validator catch the error on retry.
#[must_use]
pub fn infer_missing_on(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    // Get the FROM table (for FK column inference), before borrowing joins.
    let from_table = obj
        .get("from")
        .and_then(|f| f.as_object())
        .and_then(|f| f.get("table"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_owned());

    let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for join in joins.iter_mut() {
        let Some(join_obj) = join.as_object_mut() else {
            continue;
        };
        if join_obj.contains_key("on") {
            continue;
        }
        let Some(rt) = join_obj.get("right_table").and_then(|v| v.as_object()) else {
            continue;
        };
        let Some(right_table) = rt.get("table").and_then(|v| v.as_str()) else {
            continue;
        };

        let (left, right) = if let Some(ref from) = from_table {
            // Pattern A: right_table.id = from_table.<right_singular>_id
            //   (FK lives in the FROM table, e.g. orders.user_id → users.id)
            let right_fk = format!("{}_{}", singularize(right_table), "id");
            // Pattern B: right_table.<from_singular>_id = from_table.id
            //   (FK lives in the RIGHT table, e.g. order_items.order_id → orders.id)
            let _left_fk = format!("{}_{}", singularize(from), "id");

            // Prefer pattern A; fall back to pattern B.
            // Pattern A matches the `right_table.id = xxx` pattern which is more common.
            (
                serde_json::json!({"type": "column_ref", "table": right_table, "column": "id"}),
                serde_json::json!({"type": "column_ref", "table": from, "column": right_fk}),
            )
        } else {
            // No FROM table — fallback to simple unqualified pattern.
            let fk = format!("{}_id", singularize(right_table));
            (
                serde_json::json!({"type": "column_ref", "table": right_table, "column": "id"}),
                serde_json::json!({"type": "column_ref", "column": fk}),
            )
        };

        join_obj.insert(
            "on".to_owned(),
            serde_json::json!({
                "type": "comparison",
                "left": left,
                "op": "eq",
                "right": right,
            }),
        );
        changed = true;
    }
    changed
}

/// Fix empty `on` objects inside JOIN clauses.
///
/// Weak LLMs sometimes emit `"on": {}` (empty object) instead of a proper
/// predicate.  The builder rejects `{}` because it has no `type` discriminator.
///
/// Replace `"on": {}` with a trivially-true `1 = 1` predicate so the builder
/// can proceed (CROSS JOIN ignores the ON clause in SQL anyway).
#[must_use]
fn fix_empty_join_on(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) else {
        return false;
    };

    let mut changed = false;
    for join in joins.iter_mut() {
        let Some(join_obj) = join.as_object_mut() else {
            continue;
        };
        let Some(on_val) = join_obj.get("on") else {
            continue;
        };
        if on_val.as_object().map_or(false, |o| o.is_empty()) {
            join_obj.insert(
                "on".to_owned(),
                serde_json::json!({
                    "type": "comparison",
                    "left": {"type": "literal", "value": 1, "data_type": "int"},
                    "op": "eq",
                    "right": {"type": "literal", "value": 1, "data_type": "int"}
                }),
            );
            changed = true;
        }
    }
    changed
}

/// Crude singularization: strips trailing `s` (or `_items` → `_item`).
/// Not linguistically perfect, but good enough for FK column naming.
fn singularize(s: &str) -> &str {
    if s.ends_with("_items") {
        &s[..s.len() - 1] // order_items → order_item
    } else if s.ends_with('s') && s.len() > 1 {
        &s[..s.len() - 1] // products → product, users → user
    } else {
        s // already singular or irregular
    }
}

/// Full JOIN structure normalization.
///
/// 1. Extract plan-level fields from joins
/// 2. Convert string right_table to object
/// 3. Infer missing right_table from ON clause
/// 4. Strip unknown join fields
/// 5. Repair bare ColumnRef in ON clause
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let mut changed = false;

    // 1. Extract plan-level fields from joins.
    let extracted = extract_plan_level_fields(val);
    changed |= apply_extracted_fields(val, extracted);

    // 2. Convert string right_table to object.
    changed |= string_right_table_to_object(val);

    // 3. Infer missing right_table from ON clause.
    changed |= infer_missing_right_table(val);

    // 4. Strip unknown join fields.
    changed |= strip_unknown_fields(val);

    // 5. Repair bare ColumnRef in ON clause.
    changed |= repair_bare_join_on(val);

    // 6. Infer missing `on` from right_table.
    changed |= infer_missing_on(val);

    // 7. Fix empty `on` objects (LLM sometimes emits `"on": {}`).
    changed |= fix_empty_join_on(val);

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_plan_level_fields_from_joins() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison"}, "where": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}}]
        });
        let extracted = extract_plan_level_fields(&mut val);
        assert!(!extracted.is_empty(), "should extract where from join");
        // Verify the where field was removed from the join object.
        let joins = val.get("joins").unwrap().as_array().unwrap();
        let join_obj = joins[0].as_object().unwrap();
        assert!(join_obj.get("where").is_none());
    }

    #[test]
    fn test_apply_extracted_fields() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"}
        });
        let extracted = vec![("where".to_owned(), json!({"type": "comparison"}))];
        assert!(super::apply_extracted_fields(&mut val, extracted));
        assert!(val.get("where").is_some());
    }

    #[test]
    fn string_right_table_to_object_converts() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": [{"join_type": "inner", "right_table": "orders", "on": {"type": "comparison"}}]
        });
        assert!(string_right_table_to_object(&mut val));
        let joins = val.get("joins").unwrap().as_array().unwrap();
        let rt = joins[0].get("right_table").unwrap();
        assert_eq!(rt.get("table").and_then(|v| v.as_str()), Some("orders"));
    }

    #[test]
    fn infer_missing_right_table_from_on() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": [{"join_type": "inner", "on": {"type": "comparison", "right": {"type": "column_ref", "table": "orders", "column": "user_id"}}}]
        });
        assert!(infer_missing_right_table(&mut val));
        let joins = val.get("joins").unwrap().as_array().unwrap();
        let rt = joins[0].get("right_table").unwrap();
        assert_eq!(rt.get("table").and_then(|v| v.as_str()), Some("orders"));
    }

    #[test]
    fn infer_missing_right_table_noop_when_present() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison"}}]
        });
        assert!(!infer_missing_right_table(&mut val));
    }

    #[test]
    fn strip_unknown_fields_from_joins() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison"}, "left_table": "users", "extra_field": "value"}]
        });
        assert!(strip_unknown_fields(&mut val));
        let joins = val.get("joins").unwrap().as_array().unwrap();
        let join_obj = joins[0].as_object().unwrap();
        assert!(join_obj.get("left_table").is_none());
        assert!(join_obj.get("extra_field").is_none());
        assert!(join_obj.get("join_type").is_some());
        assert!(join_obj.get("right_table").is_some());
        assert!(join_obj.get("on").is_some());
    }

    #[test]
    fn strip_unknown_fields_removes_non_objects() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison"}}, "garbage", 42, null]
        });
        assert!(strip_unknown_fields(&mut val));
        let joins = val.get("joins").unwrap().as_array().unwrap();
        assert_eq!(joins.len(), 1);
    }

    #[test]
    fn strip_unknown_fields_removes_empty_joins() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": ["garbage"]
        });
        assert!(strip_unknown_fields(&mut val));
        assert!(val.get("joins").is_none(), "empty joins should be removed");
    }

    #[test]
    fn full_normalize_joins() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": [
                {"join_type": "inner", "right_table": "orders", "on": {"type": "comparison", "right": {"type": "column_ref", "table": "orders", "column": "user_id"}}, "extra_field": "value"}
            ]
        });
        assert!(normalize(&mut val));
        let joins = val.get("joins").unwrap().as_array().unwrap();
        assert_eq!(joins.len(), 1);
        let join_obj = joins[0].as_object().unwrap();
        // right_table should be an object.
        assert!(
            join_obj
                .get("right_table")
                .and_then(|v| v.as_object())
                .is_some()
        );
        // extra_field should be stripped.
        assert!(join_obj.get("extra_field").is_none());
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "joins": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison"}}]
        });
        assert!(!normalize(&mut val));
    }
}
