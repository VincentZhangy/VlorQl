//! Query-level structure normalization.
//!
//! Handles top-level structural fixes:
//! - Strips unknown top-level fields that serde would reject
//! - Wraps top-level `descending` + `expr` into `order_by`

/// Fields that are valid on a QueryPlan.
const PLAN_FIELDS: &[&str] = &[
    "select", "from", "where", "group_by", "having", "order_by", "limit", "offset", "joins", "ctes",
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

/// Full query-level structure normalization.
///
/// 1. Wrap top-level `descending` + `expr` into `order_by` (must run
///    before `strip_unknown_fields` so these fields are preserved).
/// 2. Strip unknown top-level fields.
/// 3. Normalize string limit/offset to numbers.
/// 4. Auto-join tables referenced in `select` but missing from `from`/`joins`.
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let mut changed = false;
    // Wrap first: expr + descending → order_by, before they get stripped.
    changed |= wrap_descending_expr(val);
    // Then strip remaining unknown fields.
    changed |= strip_unknown_fields(val);
    // Normalize string limit/offset to numbers.
    changed |= normalize_limit_offset(val);
    // Auto-join tables referenced in select but missing from from/joins.
    changed |= auto_join_missing_tables(val);
    changed
}

/// Collect table names referenced in `select` items.
fn select_tables(val: &serde_json::Value) -> Vec<String> {
    let Some(obj) = val.as_object() else {
        return vec![];
    };
    let Some(select) = obj.get("select").and_then(|v| v.as_array()) else {
        return vec![];
    };
    let mut tables: Vec<String> = Vec::new();
    for item in select {
        if let Some(item_obj) = item.as_object() {
            if let Some(table) = item_obj.get("table").and_then(|v| v.as_str()) {
                if !tables.iter().any(|t| t == table) {
                    tables.push(table.to_owned());
                }
            }
        }
    }
    tables
}

/// Collect table names already referenced in `from` and `joins`.
fn existing_tables(val: &serde_json::Value) -> Vec<String> {
    let Some(obj) = val.as_object() else {
        return vec![];
    };
    let mut tables: Vec<String> = Vec::new();
    // FROM table
    if let Some(from) = obj.get("from").and_then(|v| v.as_object()) {
        if let Some(table) = from.get("table").and_then(|v| v.as_str()) {
            tables.push(table.to_owned());
        }
    }
    // JOIN tables
    if let Some(joins) = obj.get("joins").and_then(|v| v.as_array()) {
        for join in joins {
            if let Some(join_obj) = join.as_object() {
                if let Some(rt) = join_obj.get("right_table").and_then(|v| v.as_object()) {
                    if let Some(table) = rt.get("table").and_then(|v| v.as_str()) {
                        if !tables.iter().any(|t| t == table) {
                            tables.push(table.to_owned());
                        }
                    }
                }
            }
        }
    }
    tables
}

/// Crude singularization for FK column naming.
fn singularize(s: &str) -> &str {
    if s.ends_with("_items") {
        &s[..s.len() - 1]
    } else if s.ends_with('s') && s.len() > 1 {
        &s[..s.len() - 1]
    } else {
        s
    }
}

/// When a table is referenced in `select` but missing from `from`/`joins`,
/// auto-add an inner JOIN with an inferred ON clause.
///
/// Heuristic: the FK column is `<singular_missing>_id` in the `from` table
/// (e.g. `SELECT users.name FROM orders` → `JOIN users ON users.id = orders.user_id`).
#[must_use]
fn auto_join_missing_tables(val: &mut serde_json::Value) -> bool {
    // Collect all info upfront while we have an immutable borrow.
    let (from_table, missing): (Option<String>, Vec<String>) = {
        let Some(obj) = val.as_object() else {
            return false;
        };
        let select_tables = select_tables(val);
        let existing = existing_tables(val);
        let from_table = obj
            .get("from")
            .and_then(|f| f.as_object())
            .and_then(|f| f.get("table"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_owned());
        let missing: Vec<String> = select_tables
            .into_iter()
            .filter(|t| !existing.iter().any(|e| e == t))
            .collect();
        (from_table, missing)
    };

    if missing.is_empty() {
        return false;
    }

    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let mut changed = false;

    for table in &missing {
        let fk_col = format!("{}_{}", singularize(table), "id");
        let join = serde_json::json!({
            "join_type": "inner",
            "right_table": {"table": table},
            "on": {
                "type": "comparison",
                "left": {"type": "column_ref", "table": table, "column": "id"},
                "op": "eq",
                "right": {
                    "type": "column_ref",
                    "table": from_table.as_deref().unwrap_or(""),
                    "column": fk_col
                }
            }
        });
        if let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) {
            joins.push(join);
        } else {
            obj.insert("joins".to_owned(), serde_json::json!([join]));
        }
        changed = true;
    }
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
